//! Zero-copy parsing of zip archives held in memory.
//!
//! [`ZipSlice`] is a view over the archive bytes that allocates nothing: it locates the central
//! directory once and then walks it in place. Stored (uncompressed) entries can be borrowed
//! straight out of the archive with the lifetime of the bytes, not of the view.
//!
//! ```
//! # fn main() -> std::io::Result<()> {
//! use zipfs::ZipSlice;
//!
//! static ASSETS: &[u8] = include_bytes!("../test_data/stored_only.zip");
//!
//! let zip = ZipSlice::new(ASSETS)?;
//! let readme: &'static [u8] = zip.get("readme.txt")?;
//! assert_eq!(readme, b"hello from a stored zip\n");
//! # Ok(())
//! # }
//! ```

use std::borrow::Cow;
use std::io::{self, ErrorKind, Read};
use std::ops::Range;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::crc32;
use crate::paths::{self, normalize};
use crate::reader::EntryReader;

const EOCD_SIGNATURE: u32 = 0x0605_4b50;
const EOCD_LEN: usize = 22;
const ZIP64_LOCATOR_SIGNATURE: u32 = 0x0706_4b50;
const ZIP64_LOCATOR_LEN: usize = 20;
const ZIP64_EOCD_SIGNATURE: u32 = 0x0606_4b50;
const ZIP64_EOCD_LEN: usize = 56;
const CENTRAL_HEADER_SIGNATURE: u32 = 0x0201_4b50;
const CENTRAL_HEADER_LEN: usize = 46;
const LOCAL_HEADER_SIGNATURE: u32 = 0x0403_4b50;
const LOCAL_HEADER_LEN: usize = 30;

const EXTRA_ZIP64: u16 = 0x0001;
const EXTRA_EXTENDED_TIMESTAMP: u16 = 0x5455;
const EXTRA_UNICODE_PATH: u16 = 0x7075;

const FLAG_ENCRYPTED: u16 = 0x0001;
const FLAG_UTF8: u16 = 0x0800;

const HOST_UNIX: u8 = 3;
const HOST_OSX: u8 = 19;
const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;
const DOS_DIRECTORY: u32 = 0x10;

/// Compression method of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Method {
    /// No compression; the entry can be borrowed from the archive.
    Stored,
    /// Deflate (method 8), read with the `deflate` feature.
    Deflate,
    /// Zstandard (method 93), read with the `zstd` feature.
    Zstd,
    /// Any other method; not supported.
    Other(u16),
}

impl Method {
    fn from_id(id: u16) -> Self {
        match id {
            0 => Self::Stored,
            8 => Self::Deflate,
            93 => Self::Zstd,
            other => Self::Other(other),
        }
    }

    /// The method ID stored in the archive.
    pub fn id(self) -> u16 {
        match self {
            Self::Stored => 0,
            Self::Deflate => 8,
            Self::Zstd => 93,
            Self::Other(id) => id,
        }
    }
}

/// What an archive entry represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}

/// A zero-copy view over a zip archive in memory.
#[derive(Debug, Clone, Copy)]
pub struct ZipSlice<'a> {
    data: &'a [u8],
    central_directory: &'a [u8],
    entry_count: u64,
    /// Length of data prepended to the archive (e.g. a self-extractor stub); added to every
    /// offset stored in the archive.
    base_offset: u64,
    comment: &'a [u8],
}

impl<'a> ZipSlice<'a> {
    /// Locates the central directory of the archive in `data`.
    ///
    /// Fails with [`ErrorKind::InvalidData`] if `data` is not a zip archive and with
    /// [`ErrorKind::Unsupported`] for multi-disk archives.
    pub fn new(data: &'a [u8]) -> io::Result<Self> {
        let eocd_at =
            find_eocd(data).ok_or_else(|| invalid("end of central directory record not found"))?;
        let eocd = &data[eocd_at..eocd_at + EOCD_LEN];
        let comment_len = usize::from(u16_at(eocd, 20));
        let comment = &data[eocd_at + EOCD_LEN..eocd_at + EOCD_LEN + comment_len];

        let mut disk = u32::from(u16_at(eocd, 4));
        let mut cd_disk = u32::from(u16_at(eocd, 6));
        let mut entries_on_disk = u64::from(u16_at(eocd, 8));
        let mut entry_count = u64::from(u16_at(eocd, 10));
        let mut cd_size = u64::from(u32_at(eocd, 12));
        let mut cd_offset = u64::from(u32_at(eocd, 16));
        // The central directory must end where the records after it begin.
        let mut cd_end = eocd_at as u64;

        if let Some(locator_at) = eocd_at.checked_sub(ZIP64_LOCATOR_LEN) {
            let locator = &data[locator_at..eocd_at];
            if u32_at(locator, 0) == ZIP64_LOCATOR_SIGNATURE {
                if u32_at(locator, 4) != 0 || u32_at(locator, 16) > 1 {
                    return Err(multi_disk());
                }
                let record_offset = u64_at(locator, 8);
                // With prepended data the stored offset is short by the prefix length. A v1
                // record without extensible data sits right before the locator, so fall back
                // to that position when the stored offset does not point at a record.
                let candidates = [
                    usize::try_from(record_offset).ok(),
                    locator_at.checked_sub(ZIP64_EOCD_LEN),
                ];
                let record_at = candidates
                    .into_iter()
                    .flatten()
                    .find(|&at| {
                        slice(data, at, ZIP64_EOCD_LEN)
                            .is_some_and(|r| u32_at(r, 0) == ZIP64_EOCD_SIGNATURE)
                    })
                    .ok_or_else(|| invalid("ZIP64 end of central directory record not found"))?;
                let record = &data[record_at..record_at + ZIP64_EOCD_LEN];
                disk = u32_at(record, 16);
                cd_disk = u32_at(record, 20);
                entries_on_disk = u64_at(record, 24);
                entry_count = u64_at(record, 32);
                cd_size = u64_at(record, 40);
                cd_offset = u64_at(record, 48);
                cd_end = record_at as u64;
            }
        }

        if disk != 0 || cd_disk != 0 || entries_on_disk != entry_count {
            return Err(multi_disk());
        }

        let stored_end = cd_offset
            .checked_add(cd_size)
            .ok_or_else(|| invalid("central directory size overflows"))?;
        if stored_end > cd_end {
            return Err(invalid("central directory overlaps its end record"));
        }
        let base_offset = cd_end - stored_end;
        let cd_start = usize::try_from(cd_offset + base_offset).map_err(|_| out_of_bounds())?;
        let cd_len = usize::try_from(cd_size).map_err(|_| out_of_bounds())?;
        let central_directory = slice(data, cd_start, cd_len).ok_or_else(out_of_bounds)?;

        if entry_count > (cd_size / CENTRAL_HEADER_LEN as u64) {
            return Err(invalid(
                "entry count does not fit in the central directory size",
            ));
        }

        Ok(Self {
            data,
            central_directory,
            entry_count,
            base_offset,
            comment,
        })
    }

    /// The archive bytes this view was created from.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Number of entries in the central directory.
    pub fn entry_count(&self) -> u64 {
        self.entry_count
    }

    /// The archive comment.
    pub fn comment(&self) -> &'a [u8] {
        self.comment
    }

    /// Iterates over the central directory in archive order.
    ///
    /// The iterator stops after the first malformed entry, which it yields as an error.
    pub fn entries(&self) -> Entries<'a> {
        Entries {
            central_directory: self.central_directory,
            position: 0,
            remaining: self.entry_count,
            base_offset: self.base_offset,
            failed: false,
        }
    }

    /// Finds the entry for `path` by scanning the central directory.
    ///
    /// Paths are compared after normalization (`./a//b` matches `a/b`, a trailing `/` is
    /// ignored). If several entries share a path, the last one wins. This is a linear scan;
    /// use [`ZipArchive`](crate::ZipArchive) for indexed lookups.
    pub fn find(&self, path: &str) -> io::Result<Option<Entry<'a>>> {
        let Some(key) = normalize(path) else {
            return Ok(None);
        };
        // The archive root is always a directory; entries for it (`/`, `./`) are ignored, as
        // in `ZipArchive`.
        if key.is_empty() {
            return Ok(None);
        }
        let mut found = None;
        for entry in self.entries() {
            let entry = entry?;
            if paths::normalize_entry_name(&entry.name()?).is_some_and(|name| name == key) {
                found = Some(entry);
            }
        }
        Ok(found)
    }

    /// Borrows the contents of the stored file at `path`, following symlinks.
    ///
    /// Symlinks in the final path component are resolved (see [`resolve`](Self::resolve)).
    /// Fails with [`ErrorKind::NotFound`] if there is no such entry,
    /// [`ErrorKind::IsADirectory`] for directories and [`ErrorKind::Unsupported`] for
    /// compressed or encrypted files.
    pub fn get(&self, path: &str) -> io::Result<&'a [u8]> {
        let (resolved, entry) = self.resolve(path)?;
        match entry.kind() {
            EntryKind::Directory => Err(io::Error::new(
                ErrorKind::IsADirectory,
                format!("is a directory: {resolved}"),
            )),
            _ => self.stored_data(&entry),
        }
    }

    /// Finds the entry for `path`, following symlinks in the final path component.
    ///
    /// Returns the normalized path that was resolved to, together with its entry. Lookups
    /// allocate only to resolve a symlink: a stored target is borrowed from the archive, a
    /// compressed one is decompressed into a stack buffer, and the joined path is the only new
    /// `String`. Targets are limited to 4 KiB, may not be absolute or leave the archive root,
    /// and at most 40 links are followed.
    pub fn resolve<'p>(&self, path: &'p str) -> io::Result<(Cow<'p, str>, Entry<'a>)> {
        let mut current = normalize(path).ok_or_else(|| not_found(path))?;
        for _ in 0..paths::MAX_SYMLINK_HOPS {
            let entry = match self.find(&current)? {
                Some(entry) => entry,
                None if self.has_children(&current)? => {
                    return Err(io::Error::new(
                        ErrorKind::IsADirectory,
                        format!("is a directory: {current}"),
                    ));
                }
                None => return Err(not_found(&current)),
            };
            if entry.kind() != EntryKind::Symlink {
                return Ok((current, entry));
            }
            let mut buffer = [0u8; paths::MAX_SYMLINK_TARGET];
            let target = self.link_target(&current, &entry, &mut buffer)?;
            current = Cow::Owned(paths::resolve_link(&current, target)?);
        }
        Err(paths::too_many_links())
    }

    /// Returns the target of the symlink at `path` as a normalized path from the archive root.
    pub fn read_link(&self, path: &str) -> io::Result<String> {
        let link_path = normalize(path).ok_or_else(|| not_found(path))?;
        let entry = self
            .find(&link_path)?
            .ok_or_else(|| not_found(&link_path))?;
        if entry.kind() != EntryKind::Symlink {
            return Err(io::Error::new(ErrorKind::InvalidInput, "not a symlink"));
        }
        let mut buffer = [0u8; paths::MAX_SYMLINK_TARGET];
        let target = self.link_target(&link_path, &entry, &mut buffer)?;
        paths::resolve_link(&link_path, target)
    }

    /// Whether any entry lies below the normalized directory path `dir`, which makes `dir` a
    /// directory even without an entry of its own.
    fn has_children(&self, dir: &str) -> io::Result<bool> {
        for entry in self.entries() {
            let name = entry?.name()?;
            if let Some(name) = paths::normalize_entry_name(&name) {
                if name.len() > dir.len()
                    && name.starts_with(dir)
                    && (dir.is_empty() || name.as_bytes()[dir.len()] == b'/')
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Reads a symlink's target without heap allocation: stored targets are borrowed from the
    /// archive, compressed ones are decompressed into `buffer` with size and CRC-32 checks.
    fn link_target<'b>(
        &self,
        link_path: &str,
        entry: &Entry<'a>,
        buffer: &'b mut [u8; paths::MAX_SYMLINK_TARGET],
    ) -> io::Result<&'b [u8]>
    where
        'a: 'b,
    {
        paths::check_link_target_size(link_path, entry.size())?;
        if entry.is_encrypted() {
            return Err(encrypted(entry));
        }
        let raw = self.raw_data(entry)?;
        if entry.method() == Method::Stored {
            return Ok(raw);
        }
        // At most MAX_SYMLINK_TARGET, checked above.
        let len = entry.size() as usize;
        let mut reader = EntryReader::new(raw, entry.method(), entry.size(), entry.crc32())?;
        reader.read_exact(&mut buffer[..len])?;
        // Reading past the end makes the reader verify the size and CRC-32.
        if reader.read(&mut [0u8; 1])? != 0 {
            return Err(invalid("symlink target is longer than declared"));
        }
        Ok(&buffer[..len])
    }

    /// Like [`get`](Self::get), but checks the file's CRC-32 before returning it.
    ///
    /// Fails with [`ErrorKind::InvalidData`] on a mismatch. This reads every byte of the file;
    /// [`get`](Self::get) does not.
    pub fn get_verified(&self, path: &str) -> io::Result<&'a [u8]> {
        let (resolved, entry) = self.resolve(path)?;
        if entry.kind() == EntryKind::Directory {
            return Err(io::Error::new(
                ErrorKind::IsADirectory,
                format!("is a directory: {resolved}"),
            ));
        }
        let contents = self.stored_data(&entry)?;
        crate::reader::verify_crc(contents, entry.crc32())?;
        Ok(contents)
    }

    /// Borrows the contents of a stored (uncompressed) entry.
    pub fn stored_data(&self, entry: &Entry<'a>) -> io::Result<&'a [u8]> {
        if entry.is_encrypted() {
            return Err(encrypted(entry));
        }
        if entry.method() != Method::Stored {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                format!(
                    "entry is compressed ({:?}) and cannot be borrowed; read it through ZipArchive or repack the archive with stored entries",
                    entry.method()
                ),
            ));
        }
        self.raw_data(entry)
    }

    /// Borrows the raw (possibly compressed) bytes of an entry.
    pub fn raw_data(&self, entry: &Entry<'a>) -> io::Result<&'a [u8]> {
        let range = data_range(
            self.data,
            entry.local_header_offset,
            entry.compressed_size,
            entry.method,
            entry.uncompressed_size,
        )?;
        Ok(&self.data[range])
    }
}

/// Iterator over the central directory; see [`ZipSlice::entries`].
#[derive(Debug, Clone)]
pub struct Entries<'a> {
    central_directory: &'a [u8],
    position: usize,
    remaining: u64,
    base_offset: u64,
    failed: bool,
}

impl<'a> Iterator for Entries<'a> {
    type Item = io::Result<Entry<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.remaining == 0 {
            return None;
        }
        match parse_central_header(self.central_directory, &mut self.position, self.base_offset) {
            Ok(entry) => {
                self.remaining -= 1;
                Some(Ok(entry))
            }
            Err(err) => {
                self.failed = true;
                Some(Err(err))
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.failed {
            return (0, Some(0));
        }
        (0, usize::try_from(self.remaining).ok())
    }
}

/// One central directory entry, borrowing from the archive bytes.
#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    raw_name: &'a [u8],
    extra: &'a [u8],
    comment: &'a [u8],
    version_made_by: u16,
    flags: u16,
    method: Method,
    dos_time: u16,
    dos_date: u16,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    external_attributes: u32,
    local_header_offset: u64,
}

impl<'a> Entry<'a> {
    /// The entry name as stored in the archive, decoded to text.
    ///
    /// Uses the Info-ZIP Unicode Path extra field when present and current, UTF-8 when the
    /// entry is flagged as UTF-8 or the name is valid UTF-8, and code page 437 otherwise.
    /// Borrowed unless the name had to be transcoded from code page 437.
    pub fn name(&self) -> io::Result<Cow<'a, str>> {
        for (id, body) in extra_fields(self.extra) {
            if id == EXTRA_UNICODE_PATH
                && body.len() > 5
                && body[0] == 1
                && u32_at(body, 1) == crc32::checksum(self.raw_name)
            {
                if let Ok(name) = std::str::from_utf8(&body[5..]) {
                    return Ok(Cow::Borrowed(name));
                }
            }
        }
        match std::str::from_utf8(self.raw_name) {
            Ok(name) => Ok(Cow::Borrowed(name)),
            Err(_) if self.flags & FLAG_UTF8 != 0 => Err(invalid(
                "entry name is flagged as UTF-8 but is not valid UTF-8",
            )),
            Err(_) => Ok(Cow::Owned(decode_cp437(self.raw_name))),
        }
    }

    /// The entry name bytes exactly as stored.
    pub fn raw_name(&self) -> &'a [u8] {
        self.raw_name
    }

    /// The entry comment bytes.
    pub fn comment(&self) -> &'a [u8] {
        self.comment
    }

    pub fn method(&self) -> Method {
        self.method
    }

    pub fn kind(&self) -> EntryKind {
        if let Some(mode) = self.unix_mode() {
            match mode & S_IFMT {
                S_IFDIR => return EntryKind::Directory,
                S_IFLNK => return EntryKind::Symlink,
                _ => {}
            }
        }
        if self.raw_name.ends_with(b"/") || self.external_attributes & DOS_DIRECTORY != 0 {
            EntryKind::Directory
        } else {
            EntryKind::File
        }
    }

    /// Unix permission and file type bits, if the archive records them.
    ///
    /// Unix and macOS writers store the mode in the upper 16 bits of the external attributes.
    /// Windows writers (the `zip` crate among them) mark entries as made by MS-DOS but often
    /// store a Unix mode there too, which is the only way to recognize their symlinks; for
    /// other hosts the bits are used when they name a valid Unix file type.
    pub fn unix_mode(&self) -> Option<u32> {
        let host = (self.version_made_by >> 8) as u8;
        let mode = self.external_attributes >> 16;
        if mode == 0 {
            return None;
        }
        (matches!(host, HOST_UNIX | HOST_OSX) || is_unix_file_type(mode)).then_some(mode)
    }

    /// Uncompressed size in bytes.
    pub fn size(&self) -> u64 {
        self.uncompressed_size
    }

    /// Compressed size in bytes.
    pub fn compressed_size(&self) -> u64 {
        self.compressed_size
    }

    /// CRC-32 of the uncompressed contents.
    pub fn crc32(&self) -> u32 {
        self.crc32
    }

    pub fn is_encrypted(&self) -> bool {
        self.flags & FLAG_ENCRYPTED != 0
    }

    /// Last modification time.
    ///
    /// Uses the extended timestamp extra field (UTC) when present, and otherwise the MS-DOS
    /// timestamp, which has no time zone and is interpreted as UTC.
    pub fn modified(&self) -> Option<SystemTime> {
        for (id, body) in extra_fields(self.extra) {
            if id == EXTRA_EXTENDED_TIMESTAMP && body.len() >= 5 && body[0] & 1 != 0 {
                let seconds = i32::from_le_bytes([body[1], body[2], body[3], body[4]]);
                let offset = Duration::from_secs(u64::from(seconds.unsigned_abs()));
                return if seconds >= 0 {
                    UNIX_EPOCH.checked_add(offset)
                } else {
                    UNIX_EPOCH.checked_sub(offset)
                };
            }
        }
        dos_to_system_time(self.dos_date, self.dos_time)
    }

    pub(crate) fn local_header_offset(&self) -> u64 {
        self.local_header_offset
    }
}

/// Whether `mode` carries one of the Unix file types (FIFO, character device, directory, block
/// device, regular file, symlink, socket).
fn is_unix_file_type(mode: u32) -> bool {
    matches!(
        mode & S_IFMT,
        0o010_000 | 0o020_000 | S_IFDIR | 0o060_000 | 0o100_000 | S_IFLNK | 0o140_000
    )
}

/// Finds the byte range of an entry's data by reading its local file header.
///
/// The name and extra field lengths in the local header can differ from the central directory,
/// so they are read from the local header itself. For stored entries the compressed and
/// uncompressed sizes must agree, so the range is exactly the file contents.
pub(crate) fn data_range(
    data: &[u8],
    local_header_offset: u64,
    compressed_size: u64,
    method: Method,
    uncompressed_size: u64,
) -> io::Result<Range<usize>> {
    if method == Method::Stored && compressed_size != uncompressed_size {
        return Err(invalid(
            "stored entry has differing compressed and uncompressed sizes",
        ));
    }
    let header_at = usize::try_from(local_header_offset).map_err(|_| out_of_bounds())?;
    let header = slice(data, header_at, LOCAL_HEADER_LEN).ok_or_else(out_of_bounds)?;
    if u32_at(header, 0) != LOCAL_HEADER_SIGNATURE {
        return Err(invalid("local file header signature not found"));
    }
    let name_len = usize::from(u16_at(header, 26));
    let extra_len = usize::from(u16_at(header, 28));
    let len = usize::try_from(compressed_size).map_err(|_| out_of_bounds())?;
    let start = header_at
        .checked_add(LOCAL_HEADER_LEN)
        .and_then(|at| at.checked_add(name_len))
        .and_then(|at| at.checked_add(extra_len))
        .ok_or_else(out_of_bounds)?;
    let end = start.checked_add(len).ok_or_else(out_of_bounds)?;
    if end > data.len() {
        return Err(out_of_bounds());
    }
    Ok(start..end)
}

pub(crate) fn encrypted(entry: &Entry<'_>) -> io::Error {
    io::Error::new(
        ErrorKind::Unsupported,
        format!(
            "entry {} is encrypted, which is not supported",
            String::from_utf8_lossy(entry.raw_name)
        ),
    )
}

fn parse_central_header<'a>(
    cd: &'a [u8],
    position: &mut usize,
    base_offset: u64,
) -> io::Result<Entry<'a>> {
    let at = *position;
    let header = slice(cd, at, CENTRAL_HEADER_LEN)
        .ok_or_else(|| invalid("central directory is truncated"))?;
    if u32_at(header, 0) != CENTRAL_HEADER_SIGNATURE {
        return Err(invalid("central directory header signature not found"));
    }
    let name_len = usize::from(u16_at(header, 28));
    let extra_len = usize::from(u16_at(header, 30));
    let comment_len = usize::from(u16_at(header, 32));
    let variable = slice(
        cd,
        at + CENTRAL_HEADER_LEN,
        name_len + extra_len + comment_len,
    )
    .ok_or_else(|| invalid("central directory is truncated"))?;
    let (raw_name, rest) = variable.split_at(name_len);
    let (extra, comment) = rest.split_at(extra_len);
    *position = at + CENTRAL_HEADER_LEN + variable.len();

    let mut compressed_size = u64::from(u32_at(header, 20));
    let mut uncompressed_size = u64::from(u32_at(header, 24));
    let mut local_header_offset = u64::from(u32_at(header, 42));

    // ZIP64: 32-bit fields set to 0xFFFFFFFF are stored in the ZIP64 extra field, in this
    // fixed order, and only those that overflowed.
    let needs_uncompressed = uncompressed_size == 0xFFFF_FFFF;
    let needs_compressed = compressed_size == 0xFFFF_FFFF;
    let needs_offset = local_header_offset == 0xFFFF_FFFF;
    if needs_uncompressed || needs_compressed || needs_offset {
        let body = extra_fields(extra)
            .find(|&(id, _)| id == EXTRA_ZIP64)
            .map(|(_, body)| body)
            .ok_or_else(|| invalid("ZIP64 extra field missing"))?;
        let mut values = body
            .chunks_exact(8)
            .map(|chunk| u64::from_le_bytes(chunk.try_into().expect("chunk of 8")));
        let missing = || invalid("ZIP64 extra field is truncated");
        if needs_uncompressed {
            uncompressed_size = values.next().ok_or_else(missing)?;
        }
        if needs_compressed {
            compressed_size = values.next().ok_or_else(missing)?;
        }
        if needs_offset {
            local_header_offset = values.next().ok_or_else(missing)?;
        }
    }

    let local_header_offset = local_header_offset
        .checked_add(base_offset)
        .ok_or_else(out_of_bounds)?;

    Ok(Entry {
        raw_name,
        extra,
        comment,
        version_made_by: u16_at(header, 4),
        flags: u16_at(header, 8),
        method: Method::from_id(u16_at(header, 10)),
        dos_time: u16_at(header, 12),
        dos_date: u16_at(header, 14),
        crc32: u32_at(header, 16),
        compressed_size,
        uncompressed_size,
        external_attributes: u32_at(header, 38),
        local_header_offset,
    })
}

/// Searches backwards for the end of central directory record whose comment fits exactly
/// within the remaining data.
fn find_eocd(data: &[u8]) -> Option<usize> {
    let last = data.len().checked_sub(EOCD_LEN)?;
    let first = last.saturating_sub(usize::from(u16::MAX));
    (first..=last).rev().find(|&at| {
        u32_at(data, at) == EOCD_SIGNATURE
            && at + EOCD_LEN + usize::from(u16_at(data, at + 20)) <= data.len()
    })
}

/// Iterates over `(id, body)` pairs of an extra field block, stopping at the first malformed
/// field.
fn extra_fields(mut extra: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        if extra.len() < 4 {
            return None;
        }
        let id = u16_at(extra, 0);
        let len = usize::from(u16_at(extra, 2));
        let body = extra.get(4..4 + len)?;
        extra = &extra[4 + len..];
        Some((id, body))
    })
}

/// Converts an MS-DOS date and time to a `SystemTime`, treating it as UTC.
fn dos_to_system_time(date: u16, time: u16) -> Option<SystemTime> {
    let year = 1980 + i64::from(date >> 9);
    let month = u32::from((date >> 5) & 0x0F);
    let day = u32::from(date & 0x1F);
    let hour = u64::from(time >> 11);
    let minute = u64::from((time >> 5) & 0x3F);
    let second = u64::from(time & 0x1F) * 2;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = u64::try_from(days_from_civil(year, month, day)).ok()?;
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Code page 437, bytes 0x80 to 0xFF.
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', //
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

fn decode_cp437(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&byte| {
            if byte < 0x80 {
                char::from(byte)
            } else {
                CP437_HIGH[usize::from(byte - 0x80)]
            }
        })
        .collect()
}

fn slice(data: &[u8], at: usize, len: usize) -> Option<&[u8]> {
    data.get(at..at.checked_add(len)?)
}

// The fixed-offset readers below are only called on slices whose length was checked first.
fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.to_string())
}

fn not_found(path: &str) -> io::Error {
    io::Error::new(
        ErrorKind::NotFound,
        format!("file not found in zip archive: {path}"),
    )
}

fn out_of_bounds() -> io::Error {
    invalid("zip entry points outside the archive data")
}

fn multi_disk() -> io::Error {
    io::Error::new(
        ErrorKind::Unsupported,
        "multi-disk zip archives are not supported",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dos_timestamps() {
        // 2023-08-29 22:53:10
        let date = ((2023 - 1980) << 9) | (8 << 5) | 29;
        let time = (22 << 11) | (53 << 5) | (10 / 2);
        let expected = UNIX_EPOCH + Duration::from_secs(1_693_349_590);
        assert_eq!(dos_to_system_time(date, time), Some(expected));
        // Month 0 is invalid.
        assert_eq!(dos_to_system_time(0, 0), None);
    }

    #[test]
    fn civil_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(1980, 1, 1), 3_652);
    }

    #[test]
    fn cp437_high_half() {
        assert_eq!(decode_cp437(b"caf\x82"), "café");
        assert_eq!(decode_cp437(&[0x80, 0xE1, 0xFF]), "Çß\u{a0}");
    }

    #[test]
    fn not_a_zip() {
        for data in [&b""[..], b"PK", b"definitely not a zip archive at all, no"] {
            assert_eq!(
                ZipSlice::new(data).unwrap_err().kind(),
                ErrorKind::InvalidData
            );
        }
    }
}
