//! Indexed, synchronous access to a zip archive held in memory.

use std::borrow::Cow;
use std::io::{self, ErrorKind, Read};
use std::ops::Range;
use std::time::SystemTime;

use crate::format::{self, EntryKind, Method, ZipSlice};
use crate::paths::{self, normalize};
use crate::reader::{self, EntryReader};

/// Upper bound for the up-front allocation when decompressing an entry, so a forged size in the
/// central directory cannot trigger a huge allocation before any data has been read.
const MAX_PREALLOCATION: u64 = 16 * 1024 * 1024;

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
    entries: Vec<IndexedEntry>,
    /// Indices into `entries`, sorted by path, one per distinct path. When several entries share
    /// a path, the last one in the central directory wins.
    sorted: Vec<usize>,
    /// Normalized paths of all entries, concatenated; `IndexedEntry::name` indexes into it.
    names: String,
}

#[derive(Debug, Clone)]
struct IndexedEntry {
    name: Range<usize>,
    kind: EntryKind,
    method: Method,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    local_header_offset: u64,
    modified: Option<SystemTime>,
    unix_mode: Option<u32>,
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
        let capacity = usize::try_from(slice.entry_count()).unwrap_or(0);
        let mut entries = Vec::with_capacity(capacity);
        let mut names = String::new();

        for entry in slice.entries() {
            let entry = entry?;
            let raw_name = entry.name()?;
            let path = paths::normalize_entry_name(&raw_name).ok_or_else(|| {
                format::invalid(&format!(
                    "entry name is not a portable relative path (NUL, drive letter or escapes the archive root): {raw_name:?}"
                ))
            })?;
            if path.is_empty() {
                // An entry for the root directory itself (`/` or `./`).
                continue;
            }

            let kind = entry.kind();
            if kind != EntryKind::Directory {
                if entry.is_encrypted() {
                    return Err(format::encrypted(&entry));
                }
                if !reader::is_supported(entry.method()) {
                    return Err(reader::unsupported_method(entry.method()));
                }
            }

            let start = names.len();
            names.push_str(&path);
            entries.push(IndexedEntry {
                name: start..names.len(),
                kind,
                method: entry.method(),
                crc32: entry.crc32(),
                compressed_size: entry.compressed_size(),
                uncompressed_size: entry.size(),
                local_header_offset: entry.local_header_offset(),
                modified: entry.modified(),
                unix_mode: entry.unix_mode(),
            });
        }

        // Sort by path, keeping central directory order among equal paths, then keep the last
        // entry of each run of equal paths.
        let mut sorted: Vec<usize> = (0..entries.len()).collect();
        sorted
            .sort_by(|&a, &b| names[entries[a].name.clone()].cmp(&names[entries[b].name.clone()]));
        let mut deduplicated: Vec<usize> = Vec::with_capacity(sorted.len());
        for index in sorted {
            match deduplicated.last_mut() {
                Some(last)
                    if names[entries[*last].name.clone()] == names[entries[index].name.clone()] =>
                {
                    *last = index;
                }
                _ => deduplicated.push(index),
            }
        }

        Ok(Self {
            data,
            entries,
            sorted: deduplicated,
            names,
        })
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
        self.sorted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }

    /// Iterates over all entries in path order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Metadata)> + '_ {
        self.sorted
            .iter()
            .map(|&index| (self.name(index), self.entry_metadata(&self.entries[index])))
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
        let entry = self.resolve_file(path)?;
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
        let entry = self.resolve_file(path)?;
        let raw = self.raw_data(entry)?;
        EntryReader::new(raw, entry.method, entry.uncompressed_size, entry.crc32)
    }

    /// Returns metadata, following symlinks.
    pub fn metadata(&self, path: &str) -> io::Result<Metadata> {
        let (path, entry) = self.resolve(path)?;
        self.metadata_for(&path, entry)
    }

    /// Returns metadata without following a symlink in the final path component.
    pub fn symlink_metadata(&self, path: &str) -> io::Result<Metadata> {
        let path = normalize(path).ok_or_else(|| not_found(path))?;
        self.metadata_for(&path, self.find(&path))
    }

    /// Returns the target of a symlink as a normalized path from the archive root.
    pub fn read_link(&self, path: &str) -> io::Result<String> {
        let path = normalize(path).ok_or_else(|| not_found(path))?;
        let entry = self.find(&path).ok_or_else(|| not_found(&path))?;
        if entry.kind != EntryKind::Symlink {
            return Err(io::Error::new(ErrorKind::InvalidInput, "not a symlink"));
        }
        self.link_target(&path, entry)
    }

    /// Resolves symlinks in the final path component and normalizes the path.
    ///
    /// Paths that do not exist are returned normalized but otherwise unchanged.
    pub fn canonicalize(&self, path: &str) -> io::Result<String> {
        Ok(self.resolve(path)?.0.into_owned())
    }

    /// Normalizes `path` and follows symlinks in its final component, returning the resolved
    /// path and its entry (`None` for implicit directories and paths that do not exist).
    ///
    /// The path stays borrowed unless normalization or a symlink changed it, so lookups of
    /// plain paths do not allocate.
    fn resolve<'p>(&self, path: &'p str) -> io::Result<(Cow<'p, str>, Option<&IndexedEntry>)> {
        let mut current = normalize(path).ok_or_else(|| not_found(path))?;
        for _ in 0..paths::MAX_SYMLINK_HOPS {
            match self.find(&current) {
                Some(entry) if entry.kind == EntryKind::Symlink => {
                    current = Cow::Owned(self.link_target(&current, entry)?);
                }
                entry => return Ok((current, entry)),
            }
        }
        Err(paths::too_many_links())
    }

    /// Lists the direct children of a directory, sorted by name.
    ///
    /// Directories that only exist implicitly (because files below them do) are listed too.
    pub fn read_dir(&self, path: &str) -> io::Result<Vec<DirEntry<'_>>> {
        let base = self.canonicalize(path)?;
        if !self.is_dir(&base) {
            return Err(match self.find(&base) {
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
        let mut children = Vec::new();
        for &index in &self.sorted[self.lower_bound(&prefix)..] {
            let name = self.name(index);
            let Some(rest) = name.strip_prefix(prefix.as_str()) else {
                break;
            };
            let child = match rest.split_once('/') {
                Some((directory, _)) => DirEntry {
                    name: directory,
                    kind: EntryKind::Directory,
                },
                None => DirEntry {
                    name: rest,
                    kind: self.entries[index].kind,
                },
            };
            children.push(child);
        }
        // Names of implicit directories can repeat and interleave with sibling files; prefer
        // the directory when a name appears both ways.
        children.sort_by(|a, b| {
            a.name.cmp(b.name).then_with(|| {
                (a.kind != EntryKind::Directory).cmp(&(b.kind != EntryKind::Directory))
            })
        });
        children.dedup_by(|later, earlier| later.name == earlier.name);
        Ok(children)
    }

    fn name(&self, index: usize) -> &str {
        &self.names[self.entries[index].name.clone()]
    }

    /// Position of the first path not less than `key` in `sorted`.
    fn lower_bound(&self, key: &str) -> usize {
        self.sorted.partition_point(|&index| self.name(index) < key)
    }

    fn find(&self, normalized: &str) -> Option<&IndexedEntry> {
        self.sorted
            .binary_search_by(|&index| self.name(index).cmp(normalized))
            .ok()
            .map(|position| &self.entries[self.sorted[position]])
    }

    fn is_dir(&self, normalized: &str) -> bool {
        normalized.is_empty()
            || self
                .find(normalized)
                .is_some_and(|entry| entry.kind == EntryKind::Directory)
            || self.has_children(normalized)
    }

    /// Whether any entry lies below the normalized, non-empty directory path `dir`.
    ///
    /// Compares against `dir` followed by `/` without building that string.
    fn has_children(&self, dir: &str) -> bool {
        let key = || dir.bytes().chain(std::iter::once(b'/'));
        let start = self
            .sorted
            .partition_point(|&index| self.name(index).bytes().lt(key()));
        self.sorted.get(start).is_some_and(|&index| {
            let name = self.name(index).as_bytes();
            name.len() > dir.len() && name.starts_with(dir.as_bytes()) && name[dir.len()] == b'/'
        })
    }

    fn entry_metadata(&self, entry: &IndexedEntry) -> Metadata {
        Metadata {
            kind: entry.kind,
            len: entry.uncompressed_size,
            compressed_len: entry.compressed_size,
            method: Some(entry.method),
            modified: entry.modified,
            unix_mode: entry.unix_mode,
        }
    }

    /// Metadata for a normalized path whose entry (if any) has already been looked up.
    fn metadata_for(&self, normalized: &str, entry: Option<&IndexedEntry>) -> io::Result<Metadata> {
        if let Some(entry) = entry {
            return Ok(self.entry_metadata(entry));
        }
        if normalized.is_empty() || self.has_children(normalized) {
            return Ok(Metadata {
                kind: EntryKind::Directory,
                len: 0,
                compressed_len: 0,
                method: None,
                modified: None,
                unix_mode: None,
            });
        }
        Err(not_found(normalized))
    }

    fn resolve_file(&self, path: &str) -> io::Result<&IndexedEntry> {
        let (path, entry) = self.resolve(path)?;
        match entry {
            Some(entry) if entry.kind == EntryKind::File => Ok(entry),
            Some(_) => Err(is_a_directory(&path)),
            None if self.is_dir(&path) => Err(is_a_directory(&path)),
            None => Err(not_found(&path)),
        }
    }

    /// Reads a symlink entry's target and resolves it relative to the link's directory.
    fn link_target(&self, link_path: &str, entry: &IndexedEntry) -> io::Result<String> {
        // The declared size bounds the bytes actually read: `EntryReader` fails on any output
        // beyond it, and stored slices are exactly the declared size.
        paths::check_link_target_size(link_path, entry.uncompressed_size)?;
        let bytes = self.entry_bytes(entry)?;
        paths::resolve_link(link_path, &bytes)
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
            return Ok(Cow::Borrowed(raw));
        }
        if reader::fits_one_shot(entry.compressed_size, entry.uncompressed_size) {
            return reader::decompress_to_vec(
                raw,
                entry.method,
                entry.uncompressed_size,
                entry.crc32,
            )
            .map(Cow::Owned);
        }
        let capacity = entry.uncompressed_size.min(MAX_PREALLOCATION) as usize;
        let mut out = Vec::with_capacity(capacity);
        EntryReader::new(raw, entry.method, entry.uncompressed_size, entry.crc32)?
            .read_to_end(&mut out)?;
        Ok(Cow::Owned(out))
    }
}

impl<'a> ZipArchive<&'a [u8]> {
    /// Borrows a stored file for the lifetime of the archive bytes, following symlinks.
    ///
    /// The returned slice outlives the `ZipArchive` itself. Fails with
    /// [`ErrorKind::Unsupported`] for compressed entries; use [`read`](Self::read) for those.
    pub fn read_borrowed(&self, path: &str) -> io::Result<&'a [u8]> {
        let entry = self.resolve_file(path)?;
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
        Ok(&data[range])
    }
}

fn not_found(path: &str) -> io::Error {
    io::Error::new(
        ErrorKind::NotFound,
        format!("file not found in zip archive: {path}"),
    )
}

fn is_a_directory(path: &str) -> io::Error {
    io::Error::new(ErrorKind::IsADirectory, format!("is a directory: {path}"))
}

fn not_utf8() -> io::Error {
    io::Error::new(ErrorKind::InvalidData, "file did not contain valid UTF-8")
}
