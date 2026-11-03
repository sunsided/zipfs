//! `ZipFS`: a [`lunchbox`] read-only filesystem over a zip archive, held in memory or read on
//! demand from an [`AsyncSource`].
//!
//! Enabled with the `lunchbox` feature. `ZipFS<D>` is built on [`ZipArchive`]: lookups use
//! its index, stored files are served straight from the archive bytes, and compressed files
//! are decompressed (and verified) when first read. `ZipFS<Source<S>>` is built on
//! [`SourceArchive`] and fetches file contents from the source when they are read.
//!
//! ```
//! # async fn example() -> std::io::Result<()> {
//! use lunchbox::ReadableFileSystem;
//! use zipfs::ZipFS;
//!
//! static ASSETS: &[u8] = include_bytes!("../test_data/stored_only.zip");
//!
//! let fs = ZipFS::new(ASSETS)?;
//! let readme = fs.read_to_string("readme.txt").await?;
//! let mut entries = fs.read_dir("images").await?;
//! while let Some(entry) = entries.next_entry().await? {
//!     println!("{}", entry.path());
//! }
//! # Ok(())
//! # }
//! ```

use std::future::Future;
use std::io::{self, Read, SeekFrom};
use std::marker::PhantomData;
use std::ops::Range;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, ready};

use async_trait::async_trait;
use lunchbox::ReadableFileSystem;
use lunchbox::path::PathBuf;
use lunchbox::types::{
    DirEntry, FileType, HasFileType, MaybeSend, MaybeSync, Metadata as LunchboxMetadata, PathType,
    Permissions, ReadDir, ReadDirPoller, ReadableFile,
};
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};

use crate::archive::{self, FileLocation, Metadata, ZipArchive};
use crate::format::{EntryKind, Method};
use crate::reader::{Decoding, OwnedSource};
use crate::source::{self, AsyncSource, SourceArchive, SourceLocation};

/// Where a [`ZipFS`] reads its archive from: the archive bytes `D` in memory, or an
/// [`AsyncSource`] wrapped in [`Source`].
///
/// This trait is sealed; it only names the archive and file types of each backend.
pub trait Backend: sealed::Sealed {
    /// The archive a [`ZipFS`] with this backend wraps.
    type Archive;
    /// The files a [`ZipFS`] with this backend opens.
    type File;
}

mod sealed {
    pub trait Sealed {}
}

impl<D: AsRef<[u8]>> sealed::Sealed for D {}

impl<D: AsRef<[u8]>> Backend for D {
    type Archive = ZipArchive<D>;
    type File = File<D>;
}

/// Selects the [`AsyncSource`] backend of [`ZipFS`]: `ZipFS<Source<S>>` reads its archive
/// from `S` on demand. Never constructed; see [`ZipFS::from_source`].
pub struct Source<S>(PhantomData<fn() -> S>);

impl<S> sealed::Sealed for Source<S> {}

impl<S: AsyncSource> Backend for Source<S> {
    type Archive = SourceArchive<S>;
    type File = SourceFile<S>;
}

/// A read-only [`lunchbox`] filesystem over a zip archive.
///
/// # In memory: `ZipFS<D>`
///
/// `D` holds the archive bytes and must be cheap to clone, since every opened [`File`] keeps
/// its own handle to them: `&'static [u8]` (e.g. from `include_bytes!`), `Arc<[u8]>` or
/// `bytes::Bytes`. [`ZipFS::from_file`] reads an archive file into an `Arc<[u8]>`.
///
/// Reads never wait: stored files are copied straight out of the archive bytes, and compressed
/// files are decompressed incrementally on the calling task, so memory per open file stays at
/// the decoder's working set rather than the file size. As with [`ZipArchive::read`],
/// stored files are not checked against their CRC-32 (that would mean scanning them on open);
/// decompressed files always are. Use [`ZipArchive::open`] via [`ZipFS::archive`] for a
/// verified streaming read of a stored file.
///
/// # From an async source: `ZipFS<Source<S>>`
///
/// [`ZipFS::from_source`] fetches the central directory from an [`AsyncSource`] and keeps
/// only the index in memory; see [`SourceArchive`]. Opening a file fetches its local header.
/// Stored files are then fetched in chunks of 64 KiB as they are read (not checked against
/// their CRC-32, as in memory). A compressed file's compressed bytes are fetched whole on its
/// first read and decoded incrementally from there, so a large compressed file needs its
/// compressed size in memory while it is open. [`read`](ReadableFileSystem::read) fetches and
/// verifies whole files.
pub struct ZipFS<B: Backend> {
    archive: B::Archive,
}

impl<B: Backend> ZipFS<B> {
    /// The underlying archive, for synchronous lookups.
    pub fn archive(&self) -> &B::Archive {
        &self.archive
    }

    pub fn into_archive(self) -> B::Archive {
        self.archive
    }
}

impl<D: AsRef<[u8]> + Clone> ZipFS<D> {
    /// Reads the central directory of the archive in `data`.
    ///
    /// Fails like [`ZipArchive::new`]: [`io::ErrorKind::Unsupported`] for encrypted entries or
    /// compression methods this build cannot read, [`io::ErrorKind::InvalidData`] for malformed
    /// archives and entry names that are not portable relative paths.
    pub fn new(data: D) -> io::Result<Self> {
        Ok(Self {
            archive: ZipArchive::new(data)?,
        })
    }
}

impl<D: AsRef<[u8]>> From<ZipArchive<D>> for ZipFS<D> {
    fn from(archive: ZipArchive<D>) -> Self {
        Self { archive }
    }
}

#[cfg(not(target_family = "wasm"))]
impl ZipFS<Arc<[u8]>> {
    /// Reads the archive file at `path` into memory and opens it.
    ///
    /// Uses `tokio::fs`, so it must run within a tokio runtime. To read the file on demand
    /// instead, use [`ZipFS::from_source`] with a [`FileSource`](crate::FileSource).
    pub async fn from_file(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        let data = tokio::fs::read(path).await?;
        Self::new(Arc::from(data))
    }
}

impl<S: AsyncSource> ZipFS<Source<S>> {
    /// Fetches the central directory of the archive in `source`; see [`SourceArchive::open`].
    ///
    /// ```no_run
    /// # async fn example() -> std::io::Result<()> {
    /// use lunchbox::ReadableFileSystem;
    /// use zipfs::{FileSource, ZipFS};
    ///
    /// let fs = ZipFS::from_source(FileSource::open("assets.zip").await?).await?;
    /// let sprite = fs.read("images/sprite.png").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn from_source(source: S) -> io::Result<Self> {
        Ok(Self {
            archive: SourceArchive::open(source).await?,
        })
    }
}

impl<S: AsyncSource> From<SourceArchive<S>> for ZipFS<Source<S>> {
    fn from(archive: SourceArchive<S>) -> Self {
        Self { archive }
    }
}

/// A file opened from a [`ZipFS`] that holds its archive in memory.
///
/// Clones made with [`ReadableFile::try_clone`] share the read position (and decoder), like
/// duplicated file handles. The file also implements [`AsyncSeek`]; in a compressed file,
/// seeking forward decodes and discards the skipped data, and seeking backward restarts
/// decoding from the beginning.
pub struct File<D: AsRef<[u8]>> {
    data: D,
    location: FileLocation,
    state: Arc<Mutex<FileState<D>>>,
}

struct FileState<D: AsRef<[u8]>> {
    position: u64,
    /// Decoder for compressed files, created on first read; never used for stored files.
    stream: Option<Stream<D>>,
}

struct Stream<D: AsRef<[u8]>> {
    decoding: Decoding<OwnedSource<D>>,
    /// Uncompressed bytes produced so far.
    offset: u64,
}

/// How a file's contents are encoded.
#[derive(Debug, Clone, Copy)]
struct Encoding {
    method: Method,
    uncompressed_size: u64,
    crc32: u32,
}

/// Reads at `position` from the encoded contents in `data[range]` into `out`, decoding
/// incrementally with the decoder kept in `stream`.
fn read_decoded<D: AsRef<[u8]> + Clone>(
    stream: &mut Option<Stream<D>>,
    position: &mut u64,
    data: &D,
    range: Range<usize>,
    encoding: Encoding,
    out: &mut [u8],
) -> io::Result<usize> {
    // A backward seek restarts decoding; the first read starts it.
    if stream
        .as_ref()
        .is_none_or(|stream| stream.offset > *position)
    {
        *stream = Some(Stream {
            decoding: Decoding::new(
                OwnedSource::new(data.clone(), range),
                encoding.method,
                encoding.uncompressed_size,
                encoding.crc32,
            )?,
            offset: 0,
        });
    }
    let stream = stream.as_mut().expect("created above");

    // A forward seek decodes and discards the skipped bytes.
    let mut scratch = [0u8; 8 * 1024];
    while stream.offset < *position {
        let skip = (*position - stream.offset).min(scratch.len() as u64) as usize;
        let skipped = stream.decoding.read(&mut scratch[..skip])?;
        if skipped == 0 {
            // Positioned past the end of the file.
            return Ok(0);
        }
        stream.offset += skipped as u64;
    }

    let read = stream.decoding.read(out)?;
    stream.offset += read as u64;
    *position = stream.offset;
    Ok(read)
}

/// Copies from `contents`, which start at `start` in the file, at `position` into `out`.
fn copy_at(contents: &[u8], start: u64, position: &mut u64, out: &mut [u8]) -> usize {
    let from = usize::try_from(*position - start)
        .unwrap_or(usize::MAX)
        .min(contents.len());
    let len = out.len().min(contents.len() - from);
    out[..len].copy_from_slice(&contents[from..from + len]);
    *position += len as u64;
    len
}

/// The position a seek from `position` in a file of `len` bytes moves to.
fn seek_target(position: u64, len: u64, seek: SeekFrom) -> io::Result<u64> {
    let target = match seek {
        SeekFrom::Start(offset) => Some(offset),
        SeekFrom::End(offset) => len.checked_add_signed(offset),
        SeekFrom::Current(offset) => position.checked_add_signed(offset),
    };
    target.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid seek to a negative or overflowing position",
        )
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<D: AsRef<[u8]>> File<D> {
    fn new(data: D, location: FileLocation) -> Self {
        Self {
            data,
            location,
            state: Arc::new(Mutex::new(FileState {
                position: 0,
                stream: None,
            })),
        }
    }
}

impl<D: AsRef<[u8]> + Clone> File<D> {
    /// Reads at the shared position into `out`, decoding compressed files incrementally.
    fn read_at_position(&self, out: &mut [u8]) -> io::Result<usize> {
        let location = &self.location;
        let mut state = lock(&self.state);
        let FileState { position, stream } = &mut *state;

        // Stored files are served straight from the archive bytes, unless they must be checked
        // against their CRC-32, in which case they go through the decoder like compressed ones.
        if location.method == Method::Stored && !location.verify {
            let contents = &self.data.as_ref()[location.range.clone()];
            if *position >= contents.len() as u64 {
                return Ok(0);
            }
            return Ok(copy_at(contents, 0, position, out));
        }

        let encoding = Encoding {
            method: location.method,
            uncompressed_size: location.uncompressed_size,
            crc32: location.crc32,
        };
        read_decoded(
            stream,
            position,
            &self.data,
            location.range.clone(),
            encoding,
            out,
        )
    }
}

impl<D: AsRef<[u8]> + Clone + Unpin> AsyncRead for File<D> {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let read = self.read_at_position(buf.initialize_unfilled());
        Poll::Ready(read.map(|count| buf.advance(count)))
    }
}

impl<D: AsRef<[u8]> + Unpin> AsyncSeek for File<D> {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.position = seek_target(state.position, self.location.uncompressed_size, position)?;
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(lock(&self.state).position))
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl<D> ReadableFile for File<D>
where
    D: AsRef<[u8]> + Clone + Unpin + MaybeSend + MaybeSync,
{
    async fn metadata(&self) -> io::Result<LunchboxMetadata> {
        Ok(to_lunchbox(&self.location.metadata))
    }

    async fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            data: self.data.clone(),
            location: self.location.clone(),
            state: Arc::clone(&self.state),
        })
    }
}

/// Bytes fetched per read of a stored file from an [`AsyncSource`].
const CHUNK_SIZE: u64 = 64 * 1024;

#[cfg(not(target_family = "wasm"))]
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
#[cfg(target_family = "wasm")]
type BoxFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// A file opened from a [`ZipFS`] that reads its archive from an [`AsyncSource`].
///
/// Behaves like [`File`]: clones share the read position and decoder, and seeking works
/// anywhere. Reads wait for the source: a stored file is fetched in 64 KiB chunks around the
/// read position, a compressed file's compressed bytes are fetched whole on its first read.
pub struct SourceFile<S> {
    source: Arc<S>,
    location: Arc<SourceLocation>,
    state: Arc<Mutex<SourceState>>,
    /// This handle's fetch in progress. Behind a mutex only so the file is `Sync`; it is
    /// accessed through `&mut self`.
    pending: Mutex<Option<Pending>>,
}

struct SourceState {
    position: u64,
    /// The last chunk fetched from a stored file, with its offset in the file.
    chunk: Option<(u64, Vec<u8>)>,
    /// A compressed file's compressed bytes, fetched on first read.
    compressed: Option<Arc<[u8]>>,
    stream: Option<Stream<Arc<[u8]>>>,
}

struct Pending {
    /// Offset in the file of the chunk being fetched; `None` for compressed bytes.
    chunk_at: Option<u64>,
    future: BoxFuture<io::Result<Vec<u8>>>,
}

impl<S: AsyncSource> SourceFile<S> {
    fn new(source: Arc<S>, location: SourceLocation) -> Self {
        Self {
            source,
            location: Arc::new(location),
            state: Arc::new(Mutex::new(SourceState {
                position: 0,
                chunk: None,
                compressed: None,
                stream: None,
            })),
            pending: Mutex::new(None),
        }
    }

    fn fetch(&self, range: Range<u64>, chunk_at: Option<u64>) -> Pending {
        let source = Arc::clone(&self.source);
        let size = self.location.range.end;
        Pending {
            chunk_at,
            future: Box::pin(async move { source::fetch(&*source, size, range).await }),
        }
    }

    /// Reads at the shared position into `out` from what was fetched, or returns the fetch
    /// that the read needs.
    fn try_read(&self, out: &mut [u8]) -> Result<io::Result<usize>, Pending> {
        let location = &*self.location;
        let mut state = lock(&self.state);
        let SourceState {
            position,
            chunk,
            compressed,
            stream,
        } = &mut *state;
        if out.is_empty() || *position >= location.uncompressed_size {
            return Ok(Ok(0));
        }

        if location.method == Method::Stored {
            let len = location.uncompressed_size;
            if let Some((start, bytes)) = chunk {
                if (*start..*start + bytes.len() as u64).contains(position) {
                    return Ok(Ok(copy_at(bytes, *start, position, out)));
                }
            }
            let start = *position;
            let end = (start + CHUNK_SIZE).min(len);
            let range = location.range.start + start..location.range.start + end;
            return Err(self.fetch(range, Some(start)));
        }

        let Some(data) = compressed else {
            return Err(self.fetch(location.range.clone(), None));
        };
        let encoding = Encoding {
            method: location.method,
            uncompressed_size: location.uncompressed_size,
            crc32: location.crc32,
        };
        let range = 0..data.len();
        Ok(read_decoded(stream, position, data, range, encoding, out))
    }
}

impl<S: AsyncSource> AsyncRead for SourceFile<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            let pending = this
                .pending
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(fetch) = pending {
                let fetched = ready!(fetch.future.as_mut().poll(cx));
                let chunk_at = fetch.chunk_at;
                *pending = None;
                let bytes = fetched?;
                let mut state = lock(&this.state);
                match chunk_at {
                    Some(start) => state.chunk = Some((start, bytes)),
                    None => state.compressed = Some(Arc::from(bytes)),
                }
            }
            match this.try_read(buf.initialize_unfilled()) {
                Ok(read) => return Poll::Ready(read.map(|count| buf.advance(count))),
                Err(fetch) => {
                    *this
                        .pending
                        .get_mut()
                        .unwrap_or_else(PoisonError::into_inner) = Some(fetch);
                }
            }
        }
    }
}

impl<S: AsyncSource> AsyncSeek for SourceFile<S> {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.position = seek_target(state.position, self.location.uncompressed_size, position)?;
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(lock(&self.state).position))
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl<S: AsyncSource> ReadableFile for SourceFile<S> {
    async fn metadata(&self) -> io::Result<LunchboxMetadata> {
        Ok(to_lunchbox(&self.location.metadata))
    }

    async fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            source: Arc::clone(&self.source),
            location: Arc::clone(&self.location),
            state: Arc::clone(&self.state),
            pending: Mutex::new(None),
        })
    }
}

impl<B: Backend> HasFileType for ZipFS<B> {
    type FileType = B::File;
}

/// Lists `path` for [`ReadableFileSystem::read_dir`]: `(file name, path from the root)` in
/// reverse order, since entries are handed out with `pop`.
fn dir_listing(base: &str, entries: Vec<archive::DirEntry<'_>>) -> Vec<(String, PathBuf)> {
    let mut listing: Vec<(String, PathBuf)> = entries
        .into_iter()
        .map(|entry| {
            let child = if base.is_empty() {
                entry.name.to_string()
            } else {
                format!("{base}/{}", entry.name)
            };
            (entry.name.to_string(), PathBuf::from(child))
        })
        .collect();
    listing.reverse();
    listing
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl<D> ReadableFileSystem for ZipFS<D>
where
    D: AsRef<[u8]> + Clone + Unpin + MaybeSend + MaybeSync,
{
    async fn open(&self, path: impl PathType) -> io::Result<Self::FileType>
    where
        Self::FileType: ReadableFile,
    {
        let location = self.archive.locate_file(path.as_ref().as_str())?;
        Ok(File::new(self.archive.data().clone(), location))
    }

    async fn canonicalize(&self, path: impl PathType) -> io::Result<PathBuf> {
        self.archive
            .canonicalize(path.as_ref().as_str())
            .map(PathBuf::from)
    }

    async fn metadata(&self, path: impl PathType) -> io::Result<LunchboxMetadata> {
        self.archive
            .metadata(path.as_ref().as_str())
            .map(|metadata| to_lunchbox(&metadata))
    }

    async fn read(&self, path: impl PathType) -> io::Result<Vec<u8>> {
        self.archive
            .read(path.as_ref().as_str())
            .map(|contents| contents.into_owned())
    }

    type ReadDirPollerType = ZipReadDirPoller;

    async fn read_dir(
        &self,
        path: impl PathType,
    ) -> io::Result<ReadDir<Self::ReadDirPollerType, Self>> {
        let path = path.as_ref().as_str();
        let base = self.archive.canonicalize(path)?;
        let entries = dir_listing(&base, self.archive.read_dir(path)?);
        Ok(ReadDir::new(ZipReadDirPoller { entries }, self))
    }

    async fn read_link(&self, path: impl PathType) -> io::Result<PathBuf> {
        self.archive
            .read_link(path.as_ref().as_str())
            .map(PathBuf::from)
    }

    async fn read_to_string(&self, path: impl PathType) -> io::Result<String> {
        self.archive.read_to_string(path.as_ref().as_str())
    }

    async fn symlink_metadata(&self, path: impl PathType) -> io::Result<LunchboxMetadata> {
        self.archive
            .symlink_metadata(path.as_ref().as_str())
            .map(|metadata| to_lunchbox(&metadata))
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl<S: AsyncSource> ReadableFileSystem for ZipFS<Source<S>> {
    async fn open(&self, path: impl PathType) -> io::Result<Self::FileType>
    where
        Self::FileType: ReadableFile,
    {
        let location = self.archive.locate_file(path.as_ref().as_str()).await?;
        Ok(SourceFile::new(
            Arc::clone(self.archive.shared_source()),
            location,
        ))
    }

    async fn canonicalize(&self, path: impl PathType) -> io::Result<PathBuf> {
        self.archive
            .canonicalize(path.as_ref().as_str())
            .map(PathBuf::from)
    }

    async fn metadata(&self, path: impl PathType) -> io::Result<LunchboxMetadata> {
        self.archive
            .metadata(path.as_ref().as_str())
            .map(|metadata| to_lunchbox(&metadata))
    }

    async fn read(&self, path: impl PathType) -> io::Result<Vec<u8>> {
        self.archive.read(path.as_ref().as_str()).await
    }

    type ReadDirPollerType = ZipReadDirPoller;

    async fn read_dir(
        &self,
        path: impl PathType,
    ) -> io::Result<ReadDir<Self::ReadDirPollerType, Self>> {
        let path = path.as_ref().as_str();
        let base = self.archive.canonicalize(path)?;
        let entries = dir_listing(&base, self.archive.read_dir(path)?);
        Ok(ReadDir::new(ZipReadDirPoller { entries }, self))
    }

    async fn read_link(&self, path: impl PathType) -> io::Result<PathBuf> {
        self.archive
            .read_link(path.as_ref().as_str())
            .map(PathBuf::from)
    }

    async fn read_to_string(&self, path: impl PathType) -> io::Result<String> {
        self.archive.read_to_string(path.as_ref().as_str()).await
    }

    async fn symlink_metadata(&self, path: impl PathType) -> io::Result<LunchboxMetadata> {
        self.archive
            .symlink_metadata(path.as_ref().as_str())
            .map(|metadata| to_lunchbox(&metadata))
    }
}

/// Yields the entries of a [`ZipFS`] directory; see [`ReadableFileSystem::read_dir`].
pub struct ZipReadDirPoller {
    /// `(file name, path from the archive root)`, in reverse order.
    entries: Vec<(String, PathBuf)>,
}

impl<B> ReadDirPoller<ZipFS<B>> for ZipReadDirPoller
where
    B: Backend,
    <ZipFS<B> as HasFileType>::FileType: ReadableFile,
    ZipFS<B>: ReadableFileSystem,
{
    fn poll_next_entry<'a>(
        &mut self,
        _cx: &mut Context<'_>,
        fs: &'a ZipFS<B>,
    ) -> Poll<io::Result<Option<DirEntry<'a, ZipFS<B>>>>> {
        Poll::Ready(Ok(self
            .entries
            .pop()
            .map(|(name, path)| DirEntry::new(fs, name, path))))
    }
}

fn to_lunchbox(metadata: &Metadata) -> LunchboxMetadata {
    let file_type = FileType::new(
        metadata.kind == EntryKind::Directory,
        metadata.kind == EntryKind::File,
        metadata.kind == EntryKind::Symlink,
    );
    LunchboxMetadata::new(
        Err(io::Error::other("ZipFS does not record access times")),
        Err(io::Error::other("ZipFS does not record creation times")),
        metadata
            .modified
            .ok_or_else(|| io::Error::other("no modification time recorded")),
        file_type,
        metadata.len,
        Permissions::new(true),
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf as StdPathBuf;

    use lunchbox::ReadableFileSystem;
    use tokio::io::AsyncReadExt;

    use super::*;

    static STORED: &[u8] = include_bytes!("../test_data/stored_only.zip");
    static SYMLINKS: &[u8] = include_bytes!("../test_data/symlink_test.zip");
    static EXTERNAL_SYMLINK: &[u8] = include_bytes!("../test_data/external_symlink.zip");
    static CORRUPT_LOCAL_HEADER: &[u8] = include_bytes!("../test_data/corrupt_local_header.zip");

    async fn read_file(fs: &ZipFS<&'static [u8]>, path: &str) -> String {
        let mut contents = String::new();
        fs.open(path)
            .await
            .unwrap()
            .read_to_string(&mut contents)
            .await
            .unwrap();
        contents
    }

    async fn dir_names(fs: &ZipFS<&'static [u8]>, path: &str) -> Vec<String> {
        let mut entries = fs.read_dir(path).await.unwrap();
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            names.push(entry.path().to_string());
        }
        names
    }

    #[tokio::test]
    async fn stored_files() {
        let fs = ZipFS::new(STORED).unwrap();
        assert_eq!(
            read_file(&fs, "readme.txt").await,
            "hello from a stored zip\n"
        );
        assert!(fs.metadata("images").await.unwrap().is_dir());
        let sprite = fs.read("images/sprite.png").await.unwrap();
        assert!(sprite.starts_with(b"\x89PNG"));
        assert_eq!(dir_names(&fs, "").await, ["images", "readme.txt"]);
        assert_eq!(dir_names(&fs, "images").await, ["images/sprite.png"]);
    }

    #[cfg(feature = "deflate")]
    #[tokio::test]
    async fn symlinks() {
        let fs = ZipFS::new(SYMLINKS).unwrap();

        assert!(fs.metadata("a/a.txt").await.unwrap().is_file());
        assert!(fs.symlink_metadata("a/b.txt").await.unwrap().is_symlink());
        assert_eq!(fs.read_link("a/b.txt").await.unwrap(), "a/a.txt");
        assert_eq!(fs.read_link("a/c.txt").await.unwrap(), "a/a.txt");
        assert_eq!(fs.read_link("a/d.txt").await.unwrap(), "a/a.txt");
        assert_eq!(
            fs.read_link("b/c/symlink_to_a.txt").await.unwrap(),
            "a/a.txt"
        );
        assert_eq!(fs.read_link("symlink_to_d.txt").await.unwrap(), "b/c/d.txt");
        assert_eq!(
            fs.read_link("/symlink_to_d.txt").await.unwrap(),
            "b/c/d.txt"
        );
        assert_eq!(
            fs.read_link("b/./symlink_to_d.txt").await.unwrap(),
            "b/c/d.txt"
        );

        assert_eq!(read_file(&fs, "a/b.txt").await, "a");
        assert_eq!(read_file(&fs, "b/c/symlink_to_a.txt").await, "a");
        assert_eq!(read_file(&fs, "/symlink_to_d.txt").await, "d");
        assert_eq!(
            fs.canonicalize("b/symlink_to_d.txt").await.unwrap(),
            "b/c/d.txt"
        );
        assert_eq!(dir_names(&fs, "b").await, ["b/c", "b/symlink_to_d.txt"]);
    }

    #[tokio::test]
    async fn external_symlinks_are_rejected() {
        let fs = ZipFS::new(EXTERNAL_SYMLINK).unwrap();
        assert!(fs.read_link("/symlink_to_etc_passwd").await.is_err());
        assert!(fs.open("/symlink_to_etc_passwd").await.is_err());
    }

    #[tokio::test]
    async fn corrupt_entries_fail_on_read() {
        let fs = ZipFS::new(CORRUPT_LOCAL_HEADER).unwrap();
        assert_eq!(
            fs.read("readme.txt").await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(fs.read("images/sprite.png").await.is_ok());
    }

    #[tokio::test]
    async fn invalid_archives_are_rejected() {
        let err = ZipFS::new(&b"not a zip archive"[..]).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(not(feature = "deflate"))]
    #[tokio::test]
    async fn deflate_archives_need_the_feature() {
        let err = ZipFS::new(SYMLINKS).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[cfg(feature = "deflate")]
    #[tokio::test]
    async fn clones_share_position_and_seek_works() {
        use lunchbox::types::ReadableFile;
        use tokio::io::AsyncSeekExt;

        let fs = ZipFS::new(SYMLINKS).unwrap();
        // A deflated file: decompressed on first read, shared by the clone.
        let mut file = fs.open("b/c/d.txt").await.unwrap();
        let mut clone = file.try_clone().await.unwrap();
        let mut byte = [0u8; 1];
        assert_eq!(file.read(&mut byte).await.unwrap(), 1);
        assert_eq!(&byte, b"d");
        assert_eq!(
            clone.read(&mut byte).await.unwrap(),
            0,
            "position is shared"
        );

        assert_eq!(clone.seek(SeekFrom::Start(0)).await.unwrap(), 0);
        let mut contents = String::new();
        file.read_to_string(&mut contents).await.unwrap();
        assert_eq!(contents, "d");
        assert!(file.seek(SeekFrom::Current(-5)).await.is_err());
        assert!(file.metadata().await.unwrap().is_file());
    }

    #[tokio::test]
    async fn verified_stored_reads_check_the_crc() {
        use tokio::io::AsyncSeekExt;

        // stored_only.zip with the CRC-32 of its first entry (readme.txt) altered.
        let mut bytes = STORED.to_vec();
        let crc_at = 14; // local header
        bytes[crc_at] ^= 0xFF;
        let central = bytes
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .unwrap();
        bytes[central + 16] ^= 0xFF;
        let bytes: &'static [u8] = bytes.leak();

        let fs = ZipFS::new(bytes).unwrap();
        assert_eq!(
            read_file(&fs, "readme.txt").await,
            "hello from a stored zip\n"
        );

        let fs = ZipFS::from(
            ZipArchive::new(bytes)
                .unwrap()
                .with_verified_stored_reads(true),
        );
        let mut file = fs.open("readme.txt").await.unwrap();
        let mut contents = Vec::new();
        let err = file.read_to_end(&mut contents).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // Intact stored files still read, and seeking works through the checking path.
        let mut file = fs.open("images/sprite.png").await.unwrap();
        file.seek(SeekFrom::Start(1)).await.unwrap();
        let mut rest = Vec::new();
        file.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"PNG\r\n\x1a\nnot-really-a-png");
    }

    #[tokio::test]
    async fn from_file_reads_the_archive() {
        let path = StdPathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test_data/stored_only.zip");
        let fs = ZipFS::from_file(path).await.unwrap();
        assert_eq!(
            fs.read_to_string("readme.txt").await.unwrap(),
            "hello from a stored zip\n"
        );
        let missing = ZipFS::from_file("/definitely/not/here.zip").await;
        assert_eq!(missing.err().unwrap().kind(), io::ErrorKind::NotFound);
    }
}
