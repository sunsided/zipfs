//! Experimental synchronous, in-memory access to zip archives.
//!
//! **Spike.** This module evaluates [`rc-zip`](https://crates.io/crates/rc-zip) as the zip parser
//! behind zipfs. `rc-zip` is a sans-I/O state machine, so the same parser can be driven
//! synchronously (here) and asynchronously (see the `async_driver` tests), giving both front ends
//! one implementation of the zip format.
//!
//! ```
//! # fn main() -> std::io::Result<()> {
//! use std::borrow::Cow;
//! use zipfs::sync::ZipArchive;
//!
//! static ASSETS: &[u8] = include_bytes!("../test_data/stored_only.zip");
//!
//! let zip = ZipArchive::new(ASSETS)?;
//! let readme = zip.read("readme.txt")?;
//! assert!(matches!(readme, Cow::Borrowed(_))); // stored entries are not copied
//! for entry in zip.read_dir("images")? {
//!     println!("{} ({:?})", entry.path, entry.kind);
//! }
//! # Ok(())
//! # }
//! ```

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, ErrorKind, Read};

use rc_zip::Entry;
use rc_zip::chrono::{DateTime, Utc};
use rc_zip::fsm::{ArchiveFsm, EntryFsm, FsmResult};
use rc_zip::parse::Method;

pub use rc_zip::EntryKind;

/// Maximum number of symlinks followed while resolving a single path.
const MAX_SYMLINK_HOPS: usize = 40;

/// Upper bound for the up-front allocation when decompressing an entry, so a forged size in the
/// central directory cannot trigger a huge allocation before any data has been read.
const MAX_PREALLOCATION: u64 = 16 * 1024 * 1024;

/// Largest symlink target accepted. Real targets are short paths; the cap stops a forged
/// "symlink" entry from being used to read an arbitrarily large blob while resolving paths.
const MAX_SYMLINK_TARGET: u64 = 4096;

/// A zip archive held in memory, read without async.
///
/// `D` is anything that derefs to the archive bytes: `&'static [u8]` from `include_bytes!`,
/// `Vec<u8>`, `Arc<[u8]>`, or a memory map.
pub struct ZipArchive<D> {
    data: D,
    entries: Vec<Entry>,
    /// Normalized entry path -> index into `entries`.
    index: HashMap<String, usize>,
    /// Every directory, explicit (`a/` entries) or implied by a file path (`a/b.txt` implies `a`).
    dirs: HashSet<String>,
}

/// Metadata of a file, directory or symlink in the archive.
#[derive(Debug, Clone)]
pub struct Metadata {
    pub kind: EntryKind,
    /// Uncompressed size in bytes; 0 for directories.
    pub len: u64,
    /// Last modification time; `None` for directories that only exist implicitly.
    pub modified: Option<DateTime<Utc>>,
}

/// An entry returned by [`ZipArchive::read_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// File name within the directory.
    pub name: String,
    /// Normalized path from the archive root.
    pub path: String,
    pub kind: EntryKind,
}

impl<D: AsRef<[u8]>> ZipArchive<D> {
    /// Parses the central directory of the archive in `data`.
    ///
    /// Fails with [`ErrorKind::Unsupported`] if an entry is encrypted or uses a compression method
    /// whose crate feature is disabled, and with [`ErrorKind::InvalidData`] if the archive cannot
    /// be parsed.
    pub fn new(data: D) -> io::Result<Self> {
        let archive = parse_archive(data.as_ref())?;
        let entries: Vec<Entry> = archive.entries().cloned().collect();

        let mut index = HashMap::with_capacity(entries.len());
        let mut dirs = HashSet::from([String::new()]);
        for (i, entry) in entries.iter().enumerate() {
            check_supported(entry)?;
            let path = normalize(&entry.name);
            if path.is_empty() {
                continue;
            }
            if entry.kind() == EntryKind::Directory {
                dirs.insert(path.clone());
            }
            // Every ancestor of an entry is a directory, even without its own entry.
            let mut ancestor = path.as_str();
            while let Some((parent, _)) = ancestor.rsplit_once('/') {
                dirs.insert(parent.to_string());
                ancestor = parent;
            }
            index.insert(path, i);
        }

        Ok(Self {
            data,
            entries,
            index,
            dirs,
        })
    }

    /// Reads a whole file, following symlinks.
    ///
    /// Stored (uncompressed) files are returned as [`Cow::Borrowed`] slices of the archive bytes;
    /// compressed files are decompressed into a new buffer.
    pub fn read(&self, path: &str) -> io::Result<Cow<'_, [u8]>> {
        let entry = self.resolve_file(path)?;
        self.entry_bytes(entry)
    }

    /// Reads a whole file as UTF-8, following symlinks.
    pub fn read_to_string(&self, path: &str) -> io::Result<String> {
        let bytes = self.read(path)?.into_owned();
        String::from_utf8(bytes)
            .map_err(|_| io::Error::new(ErrorKind::InvalidData, "file did not contain valid UTF-8"))
    }

    /// Opens a file for streaming reads, following symlinks.
    pub fn open(&self, path: &str) -> io::Result<EntryReader<'_>> {
        let entry = self.resolve_file(path)?;
        self.entry_reader(entry)
    }

    /// Returns metadata, following symlinks.
    pub fn metadata(&self, path: &str) -> io::Result<Metadata> {
        let path = self.canonicalize(path)?;
        self.metadata_no_follow(&path)
    }

    /// Returns metadata without following a symlink in the final path component.
    pub fn symlink_metadata(&self, path: &str) -> io::Result<Metadata> {
        self.metadata_no_follow(&normalize(path))
    }

    /// Returns the target of a symlink, as a normalized path from the archive root.
    pub fn read_link(&self, path: &str) -> io::Result<String> {
        let path = normalize(path);
        let entry = self.entry(&path).ok_or_else(|| not_found(&path))?;
        if entry.kind() != EntryKind::Symlink {
            return Err(io::Error::new(ErrorKind::InvalidInput, "not a symlink"));
        }
        self.link_target(&path, entry)
    }

    /// Resolves symlinks in the final path component and normalizes the path.
    ///
    /// Paths that do not exist are returned normalized but otherwise unchanged.
    pub fn canonicalize(&self, path: &str) -> io::Result<String> {
        let mut path = normalize(path);
        for _ in 0..MAX_SYMLINK_HOPS {
            match self.entry(&path) {
                Some(entry) if entry.kind() == EntryKind::Symlink => {
                    path = self.link_target(&path, entry)?;
                }
                _ => return Ok(path),
            }
        }
        Err(io::Error::other("too many levels of symbolic links"))
    }

    /// Lists the direct children of a directory, sorted by name.
    ///
    /// Directories that only exist implicitly (because files below them do) are listed too.
    pub fn read_dir(&self, path: &str) -> io::Result<Vec<DirEntry>> {
        let base = self.canonicalize(path)?;
        if !self.dirs.contains(&base) {
            return Err(match self.entry(&base) {
                Some(_) => {
                    io::Error::new(ErrorKind::NotADirectory, format!("not a directory: {base}"))
                }
                None => not_found(&base),
            });
        }

        let prefix = if base.is_empty() {
            String::new()
        } else {
            format!("{base}/")
        };
        let mut children: BTreeMap<&str, EntryKind> = BTreeMap::new();
        for (path, &i) in &self.index {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            match rest.split_once('/') {
                Some((dir, _)) => {
                    children.insert(dir, EntryKind::Directory);
                }
                None => {
                    children.entry(rest).or_insert(self.entries[i].kind());
                }
            }
        }

        Ok(children
            .into_iter()
            .map(|(name, kind)| DirEntry {
                name: name.to_string(),
                path: format!("{prefix}{name}"),
                kind,
            })
            .collect())
    }

    fn entry(&self, normalized: &str) -> Option<&Entry> {
        self.index.get(normalized).map(|&i| &self.entries[i])
    }

    fn metadata_no_follow(&self, path: &str) -> io::Result<Metadata> {
        if let Some(entry) = self.entry(path) {
            return Ok(Metadata {
                kind: entry.kind(),
                len: entry.uncompressed_size,
                modified: Some(entry.modified),
            });
        }
        if self.dirs.contains(path) {
            return Ok(Metadata {
                kind: EntryKind::Directory,
                len: 0,
                modified: None,
            });
        }
        Err(not_found(path))
    }

    fn resolve_file(&self, path: &str) -> io::Result<&Entry> {
        let path = self.canonicalize(path)?;
        match self.entry(&path) {
            Some(entry) if entry.kind() == EntryKind::File => Ok(entry),
            Some(_) => Err(io::Error::new(
                ErrorKind::IsADirectory,
                format!("is a directory: {path}"),
            )),
            None if self.dirs.contains(&path) => Err(io::Error::new(
                ErrorKind::IsADirectory,
                format!("is a directory: {path}"),
            )),
            None => Err(not_found(&path)),
        }
    }

    /// Reads a symlink entry's target and resolves it relative to the link's directory.
    fn link_target(&self, link_path: &str, entry: &Entry) -> io::Result<String> {
        // The declared size bounds the bytes actually read: `EntryReader` fails on any output
        // beyond it, and stored slices are exactly `compressed_size == uncompressed_size` long.
        if entry.uncompressed_size > MAX_SYMLINK_TARGET {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "symlink target of {} exceeds {MAX_SYMLINK_TARGET} bytes",
                    entry.name
                ),
            ));
        }
        let target = String::from_utf8(self.entry_bytes(entry)?.into_owned())
            .map_err(|_| io::Error::new(ErrorKind::InvalidData, "symlink target is not UTF-8"))?;
        if target.starts_with('/') {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "absolute symlink targets are not supported",
            ));
        }
        let parent = link_path.rsplit_once('/').map_or("", |(parent, _)| parent);
        let joined = normalize(&format!("{parent}/{target}"));
        if joined == ".." || joined.starts_with("../") {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "symlink target points outside the archive",
            ));
        }
        Ok(joined)
    }

    fn entry_bytes(&self, entry: &Entry) -> io::Result<Cow<'_, [u8]>> {
        if entry.method == Method::Store {
            return self.stored_slice(entry).map(Cow::Borrowed);
        }
        let capacity = entry.uncompressed_size.min(MAX_PREALLOCATION) as usize;
        let mut out = Vec::with_capacity(capacity);
        self.entry_reader(entry)?.read_to_end(&mut out)?;
        Ok(Cow::Owned(out))
    }

    fn entry_reader(&self, entry: &Entry) -> io::Result<EntryReader<'_>> {
        let offset = usize::try_from(entry.header_offset).map_err(|_| out_of_bounds())?;
        let source = self.data.as_ref().get(offset..).ok_or_else(out_of_bounds)?;
        Ok(EntryReader {
            source,
            fsm: Some(EntryFsm::new(Some(entry.clone()), None)),
            remaining: entry.uncompressed_size,
        })
    }

    /// Locates a stored entry's data in the archive bytes without copying it.
    ///
    /// The data starts after the local file header, whose name and extra field lengths can
    /// differ from the central directory, so they are read from the local header itself.
    /// Unlike [`EntryReader`], this path does not verify the CRC-32.
    fn stored_slice(&self, entry: &Entry) -> io::Result<&[u8]> {
        const LOCAL_HEADER_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
        const LOCAL_HEADER_LEN: usize = 30;

        if entry.compressed_size != entry.uncompressed_size {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "stored entry has differing compressed and uncompressed sizes",
            ));
        }

        let bytes = self.data.as_ref();
        let header_start = usize::try_from(entry.header_offset).map_err(|_| out_of_bounds())?;
        let header = header_start
            .checked_add(LOCAL_HEADER_LEN)
            .and_then(|end| bytes.get(header_start..end))
            .ok_or_else(out_of_bounds)?;
        if header[..4] != LOCAL_HEADER_SIGNATURE {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "missing local file header signature",
            ));
        }
        let name_len = u16::from_le_bytes([header[26], header[27]]) as usize;
        let extra_len = u16::from_le_bytes([header[28], header[29]]) as usize;
        let len = usize::try_from(entry.compressed_size).map_err(|_| out_of_bounds())?;

        let data_start = header_start + LOCAL_HEADER_LEN + name_len + extra_len;
        data_start
            .checked_add(len)
            .and_then(|end| bytes.get(data_start..end))
            .ok_or_else(out_of_bounds)
    }
}

/// Streaming reader for one entry, decompressing (and CRC-checking) as it goes.
///
/// The driving loop follows `rc_zip_sync::EntryReader` (Apache-2.0 OR MIT), whose constructor is
/// private to that crate.
///
/// Output is capped at the uncompressed size declared in the central directory. rc-zip only
/// compares the actual size with the declared one after all compressed input is consumed, so
/// without the cap a small, highly compressed entry could expand without bound before that check
/// (a decompression bomb).
pub struct EntryReader<'a> {
    source: &'a [u8],
    fsm: Option<EntryFsm>,
    /// Declared uncompressed bytes not yet returned.
    remaining: u64,
}

impl Read for EntryReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        // Let the decompressor produce at most one byte more than declared, which is enough to
        // detect an overrun without decompressing further.
        let limit = usize::try_from(self.remaining.saturating_add(1))
            .map_or(buf.len(), |l| l.min(buf.len()));
        let buf = &mut buf[..limit];

        loop {
            let Some(mut fsm) = self.fsm.take() else {
                return Ok(0);
            };

            let mut filled = 0;
            if fsm.wants_read() {
                filled = self.source.read(fsm.space())?;
                fsm.fill(filled);
            }

            match fsm.process(buf).map_err(rc_zip_error_to_io)? {
                FsmResult::Continue((fsm, outcome)) => {
                    let written = outcome.bytes_written as u64;
                    if written > self.remaining {
                        // `self.fsm` stays `None`: the reader is poisoned and yields no more data.
                        return Err(io::Error::new(
                            ErrorKind::InvalidData,
                            "zip entry decompresses to more than its declared size",
                        ));
                    }
                    self.remaining -= written;
                    self.fsm = Some(fsm);
                    if outcome.bytes_written > 0 {
                        return Ok(outcome.bytes_written);
                    }
                    if filled == 0 && outcome.bytes_read == 0 {
                        return Err(io::Error::new(
                            ErrorKind::UnexpectedEof,
                            "zip entry ended before its declared size",
                        ));
                    }
                }
                FsmResult::Done(_) => return Ok(0),
            }
        }
    }
}

/// Drives rc-zip's archive state machine over an in-memory buffer.
fn parse_archive(bytes: &[u8]) -> io::Result<rc_zip::Archive> {
    let mut fsm = ArchiveFsm::new(bytes.len() as u64);
    loop {
        if let Some(offset) = fsm.wants_read() {
            let available = usize::try_from(offset)
                .ok()
                .and_then(|offset| bytes.get(offset..))
                .ok_or_else(out_of_bounds)?;
            let space = fsm.space();
            let n = space.len().min(available.len());
            if n == 0 {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "zip archive is truncated",
                ));
            }
            space[..n].copy_from_slice(&available[..n]);
            fsm.fill(n);
        }

        fsm = match fsm.process().map_err(rc_zip_error_to_io)? {
            FsmResult::Done(archive) => return Ok(archive),
            FsmResult::Continue(fsm) => fsm,
        };
    }
}

/// Rejects entries this build cannot read, so failures happen when the archive is opened.
fn check_supported(entry: &Entry) -> io::Result<()> {
    if entry.flags & 0x1 != 0 {
        return Err(io::Error::new(
            ErrorKind::Unsupported,
            format!("zip entry {} is encrypted", entry.name),
        ));
    }
    if entry.kind() == EntryKind::Directory {
        return Ok(());
    }
    let feature = match entry.method {
        Method::Store => return Ok(()),
        Method::Deflate if cfg!(feature = "deflate") => return Ok(()),
        Method::Zstd if cfg!(feature = "zstd") => return Ok(()),
        Method::Deflate => Some("deflate"),
        Method::Zstd => Some("zstd"),
        _ => None,
    };
    let hint = match feature {
        Some(feature) => format!("enable the `{feature}` feature of zipfs"),
        None => "zipfs cannot read this method".to_string(),
    };
    Err(io::Error::new(
        ErrorKind::Unsupported,
        format!(
            "zip entry {} is compressed with {:?}; {hint}, or repack the archive with stored (uncompressed) entries",
            entry.name, entry.method
        ),
    ))
}

/// Normalizes a path the same way as the async `ZipFS`: no leading or trailing `/`, `.` and
/// `..` resolved.
fn normalize(path: &str) -> String {
    lunchbox::path::Path::new(path).normalize().into_string()
}

fn rc_zip_error_to_io(err: rc_zip::Error) -> io::Error {
    match err {
        rc_zip::Error::IO(err) => err,
        rc_zip::Error::Unsupported(err) => io::Error::new(ErrorKind::Unsupported, err.to_string()),
        other => io::Error::new(ErrorKind::InvalidData, other.to_string()),
    }
}

fn not_found(path: &str) -> io::Error {
    io::Error::new(
        ErrorKind::NotFound,
        format!("file not found in zipfile: {path}"),
    )
}

fn out_of_bounds() -> io::Error {
    io::Error::new(
        ErrorKind::InvalidData,
        "zip entry points outside the archive data",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    static STORED: &[u8] = include_bytes!("../test_data/stored_only.zip");
    static NO_DIR_ENTRIES: &[u8] = include_bytes!("../test_data/no_dir_entries.zip");
    static SYMLINKS: &[u8] = include_bytes!("../test_data/symlink_test.zip");
    static EXTERNAL_SYMLINK: &[u8] = include_bytes!("../test_data/external_symlink.zip");
    /// 8 MiB of zeros, deflated, with the declared uncompressed size patched to 16 bytes.
    #[cfg(feature = "deflate")]
    static SIZE_BOMB: &[u8] = include_bytes!("../test_data/size_bomb.zip");
    /// A symlink entry whose stored target is 5000 bytes long.
    static LONG_SYMLINK: &[u8] = include_bytes!("../test_data/long_symlink.zip");

    fn names(entries: &[DirEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn stored_reads_borrow_from_the_archive() {
        let zip = ZipArchive::new(STORED).unwrap();

        let readme = zip.read("readme.txt").unwrap();
        let Cow::Borrowed(slice) = readme else {
            panic!("stored entry was copied");
        };
        assert_eq!(slice, b"hello from a stored zip\n");
        let archive = STORED.as_ptr_range();
        assert!(archive.contains(&slice.as_ptr()));

        let sprite = zip.read("/images/./sprite.png").unwrap();
        assert!(sprite.starts_with(b"\x89PNG"));
    }

    #[test]
    fn streaming_reader_matches_slice() {
        let zip = ZipArchive::new(STORED).unwrap();
        let mut streamed = String::new();
        zip.open("readme.txt")
            .unwrap()
            .read_to_string(&mut streamed)
            .unwrap();
        assert_eq!(streamed.as_bytes(), &*zip.read("readme.txt").unwrap());
    }

    #[test]
    fn read_dir_lists_direct_children_only() {
        let zip = ZipArchive::new(STORED).unwrap();
        assert_eq!(names(&zip.read_dir("").unwrap()), ["images", "readme.txt"]);
        assert_eq!(names(&zip.read_dir("images").unwrap()), ["sprite.png"]);
        assert_eq!(
            zip.read_dir("readme.txt").unwrap_err().kind(),
            ErrorKind::NotADirectory
        );
        assert_eq!(
            zip.read_dir("nope").unwrap_err().kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn implicit_directories_exist() {
        let zip = ZipArchive::new(NO_DIR_ENTRIES).unwrap();
        let meta = zip.metadata("images").unwrap();
        assert_eq!(meta.kind, EntryKind::Directory);
        assert!(meta.modified.is_none());
        assert_eq!(names(&zip.read_dir("").unwrap()), ["images", "readme.txt"]);
        assert_eq!(names(&zip.read_dir("images").unwrap()), ["sprite.png"]);
        assert_eq!(
            zip.read("images").unwrap_err().kind(),
            ErrorKind::IsADirectory
        );
    }

    #[test]
    fn external_symlinks_are_rejected() {
        let zip = ZipArchive::new(EXTERNAL_SYMLINK).unwrap();
        assert!(zip.read_link("symlink_to_etc_passwd").is_err());
        assert!(zip.read("symlink_to_etc_passwd").is_err());
    }

    #[cfg(feature = "deflate")]
    #[test]
    fn output_is_capped_at_declared_size() {
        let zip = ZipArchive::new(SIZE_BOMB).unwrap();

        let err = zip.read("bomb.bin").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData, "{err}");

        let mut out = Vec::new();
        let err = zip
            .open("bomb.bin")
            .unwrap()
            .read_to_end(&mut out)
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData, "{err}");
        assert!(
            out.len() <= 16,
            "read {} bytes past the declared 16",
            out.len()
        );
    }

    #[test]
    fn oversized_symlink_targets_are_rejected() {
        let zip = ZipArchive::new(LONG_SYMLINK).unwrap();
        assert_eq!(
            zip.read_link("link").unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(zip.read("link").unwrap_err().kind(), ErrorKind::InvalidData);
        assert_eq!(zip.read_to_string("a").unwrap(), "target");
    }

    #[cfg(feature = "deflate")]
    #[test]
    fn deflated_entries_and_symlinks() {
        let zip = ZipArchive::new(SYMLINKS).unwrap();

        assert!(matches!(zip.read("b/c/d.txt").unwrap(), Cow::Owned(_)));
        assert_eq!(zip.read_to_string("b/c/d.txt").unwrap(), "d");
        assert_eq!(zip.read_to_string("symlink_to_d.txt").unwrap(), "d");
        assert_eq!(zip.read_to_string("a/d.txt").unwrap(), "a");
        assert_eq!(zip.read_link("b/c/symlink_to_a.txt").unwrap(), "a/a.txt");
        assert_eq!(zip.read_link("/symlink_to_d.txt").unwrap(), "b/c/d.txt");
        assert_eq!(
            zip.symlink_metadata("a/b.txt").unwrap().kind,
            EntryKind::Symlink
        );
        assert_eq!(zip.metadata("a/b.txt").unwrap().kind, EntryKind::File);

        assert_eq!(
            names(&zip.read_dir("b").unwrap()),
            ["c", "symlink_to_d.txt"]
        );
        assert_eq!(
            names(&zip.read_dir("").unwrap()),
            ["a", "b", "symlink_to_d.txt"]
        );
    }

    #[cfg(not(feature = "deflate"))]
    #[test]
    fn deflated_archive_rejected_without_deflate() {
        let err = ZipArchive::new(SYMLINKS).err().expect("must be rejected");
        assert_eq!(err.kind(), ErrorKind::Unsupported);
        assert!(err.to_string().contains("`deflate` feature"), "{err}");
    }

    /// Checks that every entry gets the same kind and size from rc-zip as from the existing
    /// async_zip-based `ZipFS`.
    #[cfg(feature = "deflate")]
    #[tokio::test]
    async fn metadata_matches_async_zip() {
        use lunchbox::ReadableFileSystem;

        for (name, bytes) in [("symlink_test.zip", SYMLINKS), ("stored_only.zip", STORED)] {
            let sync = ZipArchive::new(bytes).unwrap();
            let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("test_data")
                .join(name);
            let fs = crate::ZipFS::new(path).await.unwrap();

            for entry in &sync.entries {
                let ours = sync.symlink_metadata(&entry.name).unwrap();
                let theirs = fs.symlink_metadata(entry.name.as_str()).await.unwrap();
                let kind = match ours.kind {
                    EntryKind::Directory => theirs.is_dir(),
                    EntryKind::File => theirs.is_file(),
                    EntryKind::Symlink => theirs.is_symlink(),
                };
                assert!(kind, "{name}: kind mismatch for {}", entry.name);
                assert_eq!(
                    ours.len,
                    theirs.len(),
                    "{name}: size mismatch for {}",
                    entry.name
                );
            }
        }
    }

    /// The same rc-zip state machines driven over tokio's `AsyncRead + AsyncSeek`, the reader
    /// shape `GetReader` produces. Not wired into `ZipFS` yet; this checks that the parser fits.
    mod async_driver {
        use super::*;
        use std::io::SeekFrom;
        use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};

        async fn parse_archive_async<R>(reader: &mut R, size: u64) -> io::Result<rc_zip::Archive>
        where
            R: AsyncRead + AsyncSeek + Unpin,
        {
            let mut fsm = ArchiveFsm::new(size);
            loop {
                if let Some(offset) = fsm.wants_read() {
                    reader.seek(SeekFrom::Start(offset)).await?;
                    let n = reader.read(fsm.space()).await?;
                    if n == 0 {
                        return Err(ErrorKind::UnexpectedEof.into());
                    }
                    fsm.fill(n);
                }
                fsm = match fsm.process().map_err(rc_zip_error_to_io)? {
                    FsmResult::Done(archive) => return Ok(archive),
                    FsmResult::Continue(fsm) => fsm,
                };
            }
        }

        async fn read_entry_async<R>(reader: &mut R, entry: &Entry) -> io::Result<Vec<u8>>
        where
            R: AsyncRead + AsyncSeek + Unpin,
        {
            reader.seek(SeekFrom::Start(entry.header_offset)).await?;
            let mut fsm = Some(EntryFsm::new(Some(entry.clone()), None));
            let mut out = Vec::new();
            let mut buf = vec![0; 8192];
            loop {
                let mut current = fsm.take().expect("state machine present");
                let mut filled = 0;
                if current.wants_read() {
                    filled = reader.read(current.space()).await?;
                    current.fill(filled);
                }
                match current.process(&mut buf).map_err(rc_zip_error_to_io)? {
                    FsmResult::Continue((next, outcome)) => {
                        out.extend_from_slice(&buf[..outcome.bytes_written]);
                        if outcome.bytes_written == 0 && outcome.bytes_read == 0 && filled == 0 {
                            return Err(ErrorKind::UnexpectedEof.into());
                        }
                        fsm = Some(next);
                    }
                    FsmResult::Done(_) => return Ok(out),
                }
            }
        }

        #[tokio::test]
        async fn async_driver_matches_sync_driver() {
            // Without `deflate`, `ZipArchive::new` rejects SYMLINKS by design.
            let archives: &[&[u8]] = if cfg!(feature = "deflate") {
                &[STORED, SYMLINKS]
            } else {
                &[STORED]
            };
            for &bytes in archives {
                let sync = ZipArchive::new(bytes).unwrap();
                let mut reader = std::io::Cursor::new(bytes);
                let archive = parse_archive_async(&mut reader, bytes.len() as u64)
                    .await
                    .unwrap();

                let async_names: Vec<_> = archive.entries().map(|e| e.name.clone()).collect();
                let sync_names: Vec<_> = sync.entries.iter().map(|e| e.name.clone()).collect();
                assert_eq!(async_names, sync_names);

                for entry in archive.entries() {
                    if entry.kind() != EntryKind::File {
                        continue;
                    }
                    let async_bytes = read_entry_async(&mut reader, entry).await.unwrap();
                    let sync_bytes = sync.entry_bytes(entry).unwrap();
                    assert_eq!(async_bytes, &*sync_bytes, "{}", entry.name);
                }
            }
        }
    }
}
