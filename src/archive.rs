//! Indexed, synchronous access to a zip archive held in memory.

use std::borrow::Cow;
use std::io::{self, ErrorKind};
use std::ops::Range;
use std::time::SystemTime;

use crate::format::{self, EntryKind, Method, ZipSlice};
use crate::index::{Index, IndexedEntry, LinkTargets};
use crate::reader::{self, EntryReader};

/// A zip archive held in memory, with an index for fast lookups.
///
/// `D` is anything that derefs to the archive bytes: `&'static [u8]` from `include_bytes!`,
/// `Vec<u8>`, `Arc<[u8]>`, or a memory map. Stored (uncompressed) entries are never copied:
/// [`read`](Self::read) returns them borrowed, and for `ZipArchive<&'a [u8]>`,
/// [`read_borrowed`](ZipArchive::read_borrowed) returns them with the lifetime of the bytes.
///
/// ```
/// # fn main() -> std::io::Result<()> {
/// use zipfs::ZipArchive;
///
/// static ASSETS: &[u8] = include_bytes!("../test_data/stored_only.zip");
///
/// let zip = ZipArchive::new(ASSETS)?;
/// let sprite: &'static [u8] = zip.read_borrowed("images/sprite.png")?;
/// for entry in zip.read_dir("")? {
///     println!("{} ({:?})", entry.name, entry.kind);
/// }
/// # Ok(())
/// # }
/// ```
pub struct ZipArchive<D> {
    data: D,
    index: Index,
    /// Largest uncompressed size that `read` decompresses in one call; see
    /// [`ZipArchive::with_one_shot_limit`].
    one_shot_limit: u64,
    /// Whether borrowed reads of stored files check their CRC-32; see
    /// [`ZipArchive::with_verified_stored_reads`].
    verify_stored: bool,
}

/// Where a file's (possibly compressed) contents live in the archive data.
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "lunchbox"), allow(dead_code))]
pub(crate) struct FileLocation {
    pub(crate) range: Range<usize>,
    pub(crate) method: Method,
    pub(crate) uncompressed_size: u64,
    pub(crate) crc32: u32,
    pub(crate) metadata: Metadata,
    /// Whether a stored file must be checked against its CRC-32 while it is read.
    pub(crate) verify: bool,
}

/// Metadata of a file, directory or symlink in the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Metadata {
    pub kind: EntryKind,
    /// Uncompressed size in bytes; 0 for directories.
    pub len: u64,
    /// Compressed size in bytes; 0 for directories that only exist implicitly.
    pub compressed_len: u64,
    /// Compression method; `None` for directories that only exist implicitly.
    pub method: Option<Method>,
    /// Last modification time, if recorded.
    pub modified: Option<SystemTime>,
    /// Unix mode bits, if the entry was written on a Unix-like system.
    pub unix_mode: Option<u32>,
}

/// An entry returned by [`ZipArchive::read_dir`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirEntry<'z> {
    /// File name within the directory.
    pub name: &'z str,
    pub kind: EntryKind,
}

impl<D: AsRef<[u8]>> ZipArchive<D> {
    /// Reads the central directory of the archive in `data` and builds the index.
    ///
    /// Fails with [`ErrorKind::Unsupported`] if an entry is encrypted or uses a compression
    /// method this build cannot read, and with [`ErrorKind::InvalidData`] if the archive is
    /// malformed or an entry name escapes the archive root (e.g. `../evil`).
    pub fn new(data: D) -> io::Result<Self> {
        let slice = ZipSlice::new(data.as_ref())?;
        let index = Index::build(slice.entries(), slice.entry_count())?;
        Ok(Self {
            data,
            index,
            one_shot_limit: u64::MAX,
            verify_stored: false,
        })
    }

    /// Checks the CRC-32 of stored (uncompressed) files on every read when `enabled`.
    ///
    /// By default, [`read`](Self::read), [`read_borrowed`](ZipArchive::read_borrowed) and
    /// `ZipFS` files return stored files without checking them, since that means reading every
    /// byte; compressed files are always checked as they are decompressed. With verification
    /// on, borrowed reads compute the CRC-32 before returning (and fail with
    /// [`ErrorKind::InvalidData`] on a mismatch), and `ZipFS` files check it when the end of
    /// the file is reached. To check a whole archive once instead, use
    /// [`verify_all`](Self::verify_all).
    #[must_use]
    pub fn with_verified_stored_reads(mut self, enabled: bool) -> Self {
        self.verify_stored = enabled;
        self
    }

    /// Whether stored files are checked on every read; see
    /// [`with_verified_stored_reads`](Self::with_verified_stored_reads).
    pub fn verifies_stored_reads(&self) -> bool {
        self.verify_stored
    }

    /// Checks every file and symlink in the archive: each is decompressed (or read, if stored)
    /// and its size and CRC-32 compared with the central directory. Memory use is constant.
    ///
    /// The first failure is returned, with the path in its message. Useful once at startup to
    /// trust an asset archive before serving borrowed, unchecked reads from it.
    pub fn verify_all(&self) -> io::Result<()> {
        for (path, entry) in self.index.iter() {
            if entry.kind == EntryKind::Directory {
                continue;
            }
            let with_path = |err: io::Error| io::Error::new(err.kind(), format!("{path}: {err}"));
            let raw = self.raw_data(entry).map_err(with_path)?;
            let mut reader =
                EntryReader::new(raw, entry.method, entry.uncompressed_size, entry.crc32)
                    .map_err(with_path)?;
            io::copy(&mut reader, &mut io::sink()).map_err(with_path)?;
        }
        Ok(())
    }

    /// Sets the largest file, in uncompressed bytes, that [`read`](Self::read) decompresses in
    /// one call. `0` turns one-shot decompression off.
    ///
    /// One-shot decompression allocates the file's declared size up front and decodes into it
    /// in a single call. It is usually the faster path and also the leaner one: the result
    /// needs exactly the file size, whereas the streaming path grows its buffer as output
    /// arrives, which can briefly need about twice the file size. Its cost is that the declared
    /// size is reserved before any data has been decoded. Lower the limit when archives may
    /// carry forged sizes, or on targets where reserving memory commits it (such as
    /// WebAssembly). Independently of the limit, one-shot decoding is only used up to 1032
    /// times the compressed size, the most Deflate can expand data.
    ///
    /// To keep memory low with large or many files, read them with [`open`](Self::open), which
    /// always streams and holds only the decoder's working set. The default is no limit.
    #[must_use]
    pub fn with_one_shot_limit(mut self, max_size: u64) -> Self {
        self.one_shot_limit = max_size;
        self
    }

    /// The current one-shot limit; see [`with_one_shot_limit`](Self::with_one_shot_limit).
    pub fn one_shot_limit(&self) -> u64 {
        self.one_shot_limit
    }

    /// The archive bytes.
    pub fn data(&self) -> &D {
        &self.data
    }

    /// Returns the archive bytes, dropping the index.
    pub fn into_inner(self) -> D {
        self.data
    }

    /// Number of distinct paths with their own entry (implicit directories are not counted).
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.len() == 0
    }

    /// Iterates over all entries in path order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Metadata)> + '_ {
        self.index
            .iter()
            .map(|(path, entry)| (path, Index::entry_metadata(entry)))
    }

    /// Whether `path` exists, following symlinks.
    pub fn exists(&self, path: &str) -> bool {
        self.metadata(path).is_ok()
    }

    /// Reads a whole file, following symlinks.
    ///
    /// Stored files are returned as [`Cow::Borrowed`] slices of the archive bytes without
    /// copying (and without a CRC-32 check, which would mean reading them); compressed files
    /// are decompressed into a new, verified buffer.
    pub fn read(&self, path: &str) -> io::Result<Cow<'_, [u8]>> {
        let entry = self.index.resolve_file(path, self)?;
        self.entry_bytes(entry)
    }

    /// Reads a whole file as UTF-8, following symlinks.
    pub fn read_to_string(&self, path: &str) -> io::Result<String> {
        match self.read(path)? {
            Cow::Borrowed(bytes) => std::str::from_utf8(bytes)
                .map(str::to_string)
                .map_err(|_| not_utf8()),
            Cow::Owned(bytes) => String::from_utf8(bytes).map_err(|_| not_utf8()),
        }
    }

    /// Opens a file for streaming reads, following symlinks.
    ///
    /// The reader decompresses on the fly and verifies size and CRC-32 at the end, for stored
    /// entries too.
    pub fn open(&self, path: &str) -> io::Result<EntryReader<'_>> {
        let entry = self.index.resolve_file(path, self)?;
        let raw = self.raw_data(entry)?;
        EntryReader::new(raw, entry.method, entry.uncompressed_size, entry.crc32)
    }

    /// Returns metadata, following symlinks.
    pub fn metadata(&self, path: &str) -> io::Result<Metadata> {
        self.index.metadata(path, self)
    }

    /// Returns metadata without following a symlink in the final path component.
    pub fn symlink_metadata(&self, path: &str) -> io::Result<Metadata> {
        self.index.symlink_metadata(path, self)
    }

    /// Returns the target of a symlink as a normalized path from the archive root.
    pub fn read_link(&self, path: &str) -> io::Result<String> {
        self.index.read_link(path, self)
    }

    /// Resolves symlinks in the final path component (in every component with the
    /// `full-symlinks` feature) and normalizes the path.
    ///
    /// Paths that do not exist are returned normalized but otherwise unchanged.
    pub fn canonicalize(&self, path: &str) -> io::Result<String> {
        self.index.canonicalize(path, self)
    }

    /// Lists the direct children of a directory, sorted by name.
    ///
    /// Directories that only exist implicitly (because files below them do) are listed too.
    pub fn read_dir(&self, path: &str) -> io::Result<Vec<DirEntry<'_>>> {
        self.index.read_dir(path, self)
    }

    fn raw_data(&self, entry: &IndexedEntry) -> io::Result<&[u8]> {
        let data = self.data.as_ref();
        let range = format::data_range(
            data,
            entry.local_header_offset,
            entry.compressed_size,
            entry.method,
            entry.uncompressed_size,
        )?;
        Ok(&data[range])
    }

    fn entry_bytes(&self, entry: &IndexedEntry) -> io::Result<Cow<'_, [u8]>> {
        let raw = self.raw_data(entry)?;
        if entry.method == Method::Stored {
            if self.verify_stored {
                reader::verify_crc(raw, entry.crc32)?;
            }
            return Ok(Cow::Borrowed(raw));
        }
        reader::decompress(
            raw,
            entry.method,
            entry.uncompressed_size,
            entry.crc32,
            self.one_shot_limit,
        )
        .map(Cow::Owned)
    }

    /// Resolves `path` to a file and returns where its contents live in the archive data, for
    /// readers that keep their own handle to the data (such as `ZipFS` files).
    #[cfg_attr(not(feature = "lunchbox"), allow(dead_code))]
    pub(crate) fn locate_file(&self, path: &str) -> io::Result<FileLocation> {
        let entry = self.index.resolve_file(path, self)?;
        let range = format::data_range(
            self.data.as_ref(),
            entry.local_header_offset,
            entry.compressed_size,
            entry.method,
            entry.uncompressed_size,
        )?;
        Ok(FileLocation {
            range,
            method: entry.method,
            uncompressed_size: entry.uncompressed_size,
            crc32: entry.crc32,
            metadata: Index::entry_metadata(entry),
            verify: self.verify_stored,
        })
    }
}

impl<'a> ZipArchive<&'a [u8]> {
    /// Borrows a stored file for the lifetime of the archive bytes, following symlinks.
    ///
    /// The returned slice outlives the `ZipArchive` itself. Fails with
    /// [`ErrorKind::Unsupported`] for compressed entries; use [`read`](Self::read) for those.
    pub fn read_borrowed(&self, path: &str) -> io::Result<&'a [u8]> {
        let entry = self.index.resolve_file(path, self)?;
        if entry.method != Method::Stored {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                format!(
                    "{path} is compressed ({:?}) and cannot be borrowed; use read() or repack the archive with stored entries",
                    entry.method
                ),
            ));
        }
        let data: &'a [u8] = self.data;
        let range = format::data_range(
            data,
            entry.local_header_offset,
            entry.compressed_size,
            entry.method,
            entry.uncompressed_size,
        )?;
        let contents = &data[range];
        if self.verify_stored {
            reader::verify_crc(contents, entry.crc32)?;
        }
        Ok(contents)
    }
}

impl<D: AsRef<[u8]>> LinkTargets for ZipArchive<D> {
    fn link_target_bytes(&self, entry: &IndexedEntry) -> io::Result<Cow<'_, [u8]>> {
        self.entry_bytes(entry)
    }
}

fn not_utf8() -> io::Error {
    io::Error::new(ErrorKind::InvalidData, "file did not contain valid UTF-8")
}
