//! `ZipFS`: a [`lunchbox`] read-only filesystem over a zip archive held in memory.
//!
//! Enabled with the `lunchbox` feature. `ZipFS` is built on [`ZipArchive`]: lookups use its
//! index, stored files are served straight from the archive bytes, and compressed files are
//! decompressed (and verified) when first read.
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

use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use async_trait::async_trait;
use lunchbox::ReadableFileSystem;
use lunchbox::path::PathBuf;
use lunchbox::types::{
    DirEntry, FileType, HasFileType, MaybeSend, MaybeSync, Metadata as LunchboxMetadata, PathType,
    Permissions, ReadDir, ReadDirPoller, ReadableFile,
};
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};

use crate::archive::{FileLocation, Metadata, ZipArchive};
use crate::format::{EntryKind, Method};
use crate::reader;

/// A read-only [`lunchbox`] filesystem over a zip archive held in memory.
///
/// `D` holds the archive bytes and must be cheap to clone, since every opened [`File`] keeps
/// its own handle to them: `&'static [u8]` (e.g. from `include_bytes!`), `Arc<[u8]>` or
/// `bytes::Bytes`. [`ZipFS::from_file`] reads an archive file into an `Arc<[u8]>`.
///
/// Reads never wait: stored files are copied straight out of the archive bytes, and compressed
/// files are decompressed on the calling task when first read. As with [`ZipArchive::read`],
/// stored files are not checked against their CRC-32 (that would mean scanning them on open);
/// decompressed files always are. Use [`ZipArchive::open`] via [`ZipFS::archive`] for a
/// verified streaming read of a stored file.
pub struct ZipFS<D> {
    archive: ZipArchive<D>,
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

    /// The underlying archive, for synchronous access.
    pub fn archive(&self) -> &ZipArchive<D> {
        &self.archive
    }

    pub fn into_archive(self) -> ZipArchive<D> {
        self.archive
    }
}

impl<D> From<ZipArchive<D>> for ZipFS<D> {
    fn from(archive: ZipArchive<D>) -> Self {
        Self { archive }
    }
}

#[cfg(not(target_family = "wasm"))]
impl ZipFS<Arc<[u8]>> {
    /// Reads the archive file at `path` into memory and opens it.
    ///
    /// Uses `tokio::fs`, so it must run within a tokio runtime.
    pub async fn from_file(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        let data = tokio::fs::read(path).await?;
        Self::new(Arc::from(data))
    }
}

/// A file opened from a [`ZipFS`].
///
/// Clones made with [`ReadableFile::try_clone`] share the read position, like duplicated file
/// handles. The file also implements [`AsyncSeek`].
pub struct File<D> {
    data: D,
    location: FileLocation,
    state: Arc<Mutex<FileState>>,
}

#[derive(Default)]
struct FileState {
    position: u64,
    /// Decompressed contents, produced on first read; `None` for stored files.
    decompressed: Option<Vec<u8>>,
}

impl<D: AsRef<[u8]>> File<D> {
    fn new(data: D, location: FileLocation) -> Self {
        Self {
            data,
            location,
            state: Arc::new(Mutex::new(FileState::default())),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FileState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<D: AsRef<[u8]> + Unpin> AsyncRead for File<D> {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let raw = &this.data.as_ref()[this.location.range.clone()];
        let mut state = this.lock();

        if this.location.method != Method::Stored && state.decompressed.is_none() {
            match reader::decompress(
                raw,
                this.location.method,
                this.location.uncompressed_size,
                this.location.crc32,
                u64::MAX,
            ) {
                Ok(contents) => state.decompressed = Some(contents),
                Err(err) => return Poll::Ready(Err(err)),
            }
        }

        let FileState {
            position,
            decompressed,
        } = &mut *state;
        let contents: &[u8] = decompressed.as_deref().unwrap_or(raw);
        let start = usize::try_from(*position)
            .unwrap_or(usize::MAX)
            .min(contents.len());
        let len = buf.remaining().min(contents.len() - start);
        buf.put_slice(&contents[start..start + len]);
        *position = (start + len) as u64;
        Poll::Ready(Ok(()))
    }
}

impl<D: AsRef<[u8]> + Unpin> AsyncSeek for File<D> {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let len = self.location.uncompressed_size;
        let mut state = self.lock();
        let target = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(offset) => len.checked_add_signed(offset),
            SeekFrom::Current(offset) => state.position.checked_add_signed(offset),
        };
        state.position = target.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative or overflowing position",
            )
        })?;
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.lock().position))
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

impl<D> HasFileType for ZipFS<D> {
    type FileType = File<D>;
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
        let mut entries: Vec<(String, PathBuf)> = self
            .archive
            .read_dir(path)?
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
        // Entries are handed out with `pop`, so reverse to keep them sorted by name.
        entries.reverse();
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

/// Yields the entries of a [`ZipFS`] directory; see [`ReadableFileSystem::read_dir`].
pub struct ZipReadDirPoller {
    /// `(file name, path from the archive root)`, in reverse order.
    entries: Vec<(String, PathBuf)>,
}

impl<D> ReadDirPoller<ZipFS<D>> for ZipReadDirPoller
where
    D: AsRef<[u8]> + Clone + Unpin + MaybeSend + MaybeSync,
{
    fn poll_next_entry<'a>(
        &mut self,
        _cx: &mut Context<'_>,
        fs: &'a ZipFS<D>,
    ) -> Poll<io::Result<Option<DirEntry<'a, ZipFS<D>>>>> {
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
