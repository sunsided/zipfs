//! Zip archives read on demand from an asynchronous source, such as a file on disk or an
//! object store, instead of being held in memory.
//!
//! Enabled with the `lunchbox` feature. [`SourceArchive`] fetches the end records and the
//! central directory when it is opened and keeps only the index in memory. File contents are
//! fetched when they are read: stored files in chunks, compressed files as a whole compressed
//! payload that is then decoded incrementally.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use lunchbox::types::{MaybeSend, MaybeSync};

use crate::archive::{DirEntry, Metadata};
use crate::format::{self, EntryKind, Method};
use crate::index::{Index, IndexedEntry, LinkTargets};
use crate::paths;
use crate::reader;

/// Random-access reads from an archive that is not held in memory.
///
/// Implement this for a file handle, an HTTP range client or an object store, and open the
/// archive with [`SourceArchive::open`] or [`ZipFS::from_source`](crate::ZipFS::from_source).
/// [`FileSource`] implements it for local files.
///
/// Reads may run concurrently from several tasks.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait AsyncSource: MaybeSend + MaybeSync + 'static {
    /// Total size of the archive in bytes. Called once, when the archive is opened.
    async fn size(&self) -> io::Result<u64>;

    /// Reads `len` bytes starting at `offset`.
    ///
    /// Must return exactly `len` bytes; reads past the end are an error
    /// ([`ErrorKind::UnexpectedEof`]).
    async fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>>;
}

/// An archive file on the local filesystem, read with positional reads on tokio's blocking
/// thread pool, so concurrent reads do not contend for a file cursor.
///
/// Must be used within a tokio runtime.
#[cfg(all(not(target_family = "wasm"), any(unix, windows)))]
#[derive(Debug, Clone)]
pub struct FileSource {
    file: Arc<std::fs::File>,
}

#[cfg(all(not(target_family = "wasm"), any(unix, windows)))]
impl FileSource {
    /// Opens the file at `path` for reading.
    pub async fn open(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        let file = tokio::fs::File::open(path).await?.into_std().await;
        Ok(Self::from(file))
    }
}

#[cfg(all(not(target_family = "wasm"), any(unix, windows)))]
impl From<std::fs::File> for FileSource {
    fn from(file: std::fs::File) -> Self {
        Self {
            file: Arc::new(file),
        }
    }
}

#[cfg(all(not(target_family = "wasm"), any(unix, windows)))]
#[async_trait]
impl AsyncSource for FileSource {
    async fn size(&self) -> io::Result<u64> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || file.metadata().map(|metadata| metadata.len()))
            .await
            .map_err(io::Error::other)?
    }

    async fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; len];
            read_exact_at(&file, &mut buf, offset)?;
            Ok(buf)
        })
        .await
        .map_err(io::Error::other)?
    }
}

#[cfg(all(not(target_family = "wasm"), unix))]
fn read_exact_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(all(not(target_family = "wasm"), windows))]
fn read_exact_at(file: &std::fs::File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(read) => {
                buf = &mut buf[read..];
                offset += read as u64;
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// A zip archive read on demand from an [`AsyncSource`], with an index for fast lookups.
///
/// Opening fetches the end records and the central directory (and the targets of any
/// symlinks, so path resolution never waits); everything else is fetched when read.
/// Lookups ([`metadata`](Self::metadata), [`read_dir`](Self::read_dir), ...) are
/// synchronous and work like those of [`ZipArchive`](crate::ZipArchive).
pub struct SourceArchive<S> {
    source: Arc<S>,
    size: u64,
    index: Index,
    /// Symlink targets by local header offset, fetched when the archive was opened.
    links: HashMap<u64, Result<Vec<u8>, (ErrorKind, String)>>,
    one_shot_limit: u64,
}

/// Where a file's (possibly compressed) contents live in the source.
#[derive(Debug, Clone)]
pub(crate) struct SourceLocation {
    pub(crate) range: Range<u64>,
    pub(crate) method: Method,
    pub(crate) uncompressed_size: u64,
    pub(crate) crc32: u32,
    pub(crate) metadata: Metadata,
}

impl<S: AsyncSource> SourceArchive<S> {
    /// Fetches the central directory of the archive in `source` and builds the index.
    ///
    /// Fails like [`ZipArchive::new`](crate::ZipArchive::new), and with the source's errors.
    pub async fn open(source: S) -> io::Result<Self> {
        let source = Arc::new(source);
        let size = source.size().await?;

        let window_start = size.saturating_sub(format::END_RECORDS_WINDOW);
        let window = fetch(&*source, size, window_start..size).await?;
        let zip64_record = match format::zip64_record_outside_window(&window, window_start) {
            Some(offset) => {
                let end = offset.saturating_add(format::ZIP64_RECORD_LEN as u64);
                Some((offset, fetch(&*source, size, offset..end).await?))
            }
            None => None,
        };
        let location = format::locate_central_directory(
            &window,
            window_start,
            zip64_record
                .as_ref()
                .map(|(offset, record)| (*offset, record.as_slice())),
        )?;

        let directory = location.central_directory;
        let central_directory = if directory.start >= window_start {
            let start = (directory.start - window_start) as usize;
            let end = (directory.end - window_start) as usize;
            Cow::Borrowed(&window[start..end])
        } else {
            Cow::Owned(fetch(&*source, size, directory).await?)
        };
        let entries = format::Entries::new(
            &central_directory,
            location.entry_count,
            location.base_offset,
        );
        let index = Index::build(entries, location.entry_count)?;
        drop(central_directory);
        drop(window);

        let mut links = HashMap::new();
        for (path, entry) in index.iter() {
            if entry.kind != EntryKind::Symlink
                || links.contains_key(&entry.local_header_offset)
                // Oversized targets are rejected before their bytes are asked for.
                || paths::check_link_target_size(path, entry.uncompressed_size).is_err()
            {
                continue;
            }
            let target = fetch_entry(&*source, size, entry, u64::MAX)
                .await
                .map_err(|error| (error.kind(), error.to_string()));
            links.insert(entry.local_header_offset, target);
        }

        Ok(Self {
            source,
            size,
            index,
            links,
            one_shot_limit: u64::MAX,
        })
    }
}

impl<S> SourceArchive<S> {
    /// Sets the largest uncompressed size that [`read`](Self::read) decompresses in one call;
    /// see [`ZipArchive::with_one_shot_limit`](crate::ZipArchive::with_one_shot_limit).
    pub fn with_one_shot_limit(mut self, limit: u64) -> Self {
        self.one_shot_limit = limit;
        self
    }

    pub fn one_shot_limit(&self) -> u64 {
        self.one_shot_limit
    }

    /// The source the archive is read from.
    pub fn source(&self) -> &S {
        &self.source
    }

    /// Size of the archive in bytes, as reported by the source when it was opened.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Number of files, directories and symlinks in the archive.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Normalized paths and metadata of all entries, sorted by path.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Metadata)> + '_ {
        self.index
            .iter()
            .map(|(name, entry)| (name, Index::entry_metadata(entry)))
    }

    /// Whether `path` exists, following symlinks.
    pub fn exists(&self, path: &str) -> bool {
        self.metadata(path).is_ok()
    }

    /// Metadata of `path`, following symlinks.
    pub fn metadata(&self, path: &str) -> io::Result<Metadata> {
        self.index.metadata(path, self)
    }

    /// Metadata of `path` itself, without following a final symlink.
    pub fn symlink_metadata(&self, path: &str) -> io::Result<Metadata> {
        self.index.symlink_metadata(path, self)
    }

    /// Target of the symlink at `path`, resolved to a path from the archive root.
    pub fn read_link(&self, path: &str) -> io::Result<String> {
        self.index.read_link(path, self)
    }

    /// `path` with symlinks resolved, relative to the archive root.
    pub fn canonicalize(&self, path: &str) -> io::Result<String> {
        self.index.canonicalize(path, self)
    }

    /// Entries of the directory at `path`, sorted by name.
    pub fn read_dir(&self, path: &str) -> io::Result<Vec<DirEntry<'_>>> {
        self.index.read_dir(path, self)
    }
}

impl<S: AsyncSource> SourceArchive<S> {
    /// Fetches and decompresses the file at `path`, following symlinks, and checks it against
    /// its CRC-32.
    pub async fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        let entry = self.index.resolve_file(path, self)?;
        fetch_entry(&*self.source, self.size, entry, self.one_shot_limit).await
    }

    /// Like [`read`](Self::read), and checks that the contents are UTF-8.
    pub async fn read_to_string(&self, path: &str) -> io::Result<String> {
        String::from_utf8(self.read(path).await?)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))
    }

    /// Resolves `path` to a file and fetches its local header to find its contents.
    pub(crate) async fn locate_file(&self, path: &str) -> io::Result<SourceLocation> {
        let entry = self.index.resolve_file(path, self)?;
        Ok(SourceLocation {
            range: locate_data(&*self.source, self.size, entry).await?,
            method: entry.method,
            uncompressed_size: entry.uncompressed_size,
            crc32: entry.crc32,
            metadata: Index::entry_metadata(entry),
        })
    }

    /// The shared handle to the source, for files that fetch on their own.
    pub(crate) fn shared_source(&self) -> &Arc<S> {
        &self.source
    }
}

impl<S> LinkTargets for SourceArchive<S> {
    fn link_target_bytes(&self, entry: &IndexedEntry) -> io::Result<Cow<'_, [u8]>> {
        match self.links.get(&entry.local_header_offset) {
            Some(Ok(target)) => Ok(Cow::Borrowed(target)),
            Some(Err((kind, message))) => Err(io::Error::new(*kind, message.clone())),
            None => Err(io::Error::other("symlink target was not loaded")),
        }
    }
}

/// Fetches `range` from `source`, which is `size` bytes long.
pub(crate) async fn fetch<S: AsyncSource + ?Sized>(
    source: &S,
    size: u64,
    range: Range<u64>,
) -> io::Result<Vec<u8>> {
    if range.end > size || range.start > range.end {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "zip structure points outside the archive",
        ));
    }
    let len = usize::try_from(range.end - range.start).map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidData,
            "zip entry is too large for this platform",
        )
    })?;
    let bytes = source.read_at(range.start, len).await?;
    if bytes.len() != len {
        return Err(io::Error::new(
            ErrorKind::UnexpectedEof,
            format!(
                "source returned {} bytes instead of {len} at offset {}",
                bytes.len(),
                range.start
            ),
        ));
    }
    Ok(bytes)
}

/// Fetches the local header of `entry` and returns where its data lies.
async fn locate_data<S: AsyncSource + ?Sized>(
    source: &S,
    size: u64,
    entry: &IndexedEntry,
) -> io::Result<Range<u64>> {
    let header_end = entry
        .local_header_offset
        .saturating_add(format::LOCAL_HEADER_LEN as u64);
    let header = fetch(source, size, entry.local_header_offset..header_end).await?;
    let range = format::local_data_range(
        &header,
        entry.local_header_offset,
        entry.compressed_size,
        entry.method,
        entry.uncompressed_size,
    )?;
    if range.end > size {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "zip entry data extends past the end of the archive",
        ));
    }
    Ok(range)
}

/// Fetches, decompresses and verifies the whole contents of `entry`.
async fn fetch_entry<S: AsyncSource + ?Sized>(
    source: &S,
    size: u64,
    entry: &IndexedEntry,
    one_shot_limit: u64,
) -> io::Result<Vec<u8>> {
    if !reader::is_supported(entry.method) {
        return Err(reader::unsupported_method(entry.method));
    }
    let range = locate_data(source, size, entry).await?;
    let raw = fetch(source, size, range).await?;
    if entry.method == Method::Stored {
        reader::verify_crc(&raw, entry.crc32)?;
        return Ok(raw);
    }
    reader::decompress(
        &raw,
        entry.method,
        entry.uncompressed_size,
        entry.crc32,
        one_shot_limit,
    )
}
