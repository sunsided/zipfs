//! The path index shared by every backend: entry paths in sorted order, lookups, directories
//! and symlink resolution.
//!
//! The index never reads entry data itself. Reading a symlink's target goes through
//! [`LinkTargets`], implemented by `ZipArchive` (from the archive bytes) and by the async
//! source backend (from targets fetched when the archive was opened).

use std::borrow::Cow;
use std::io::{self, ErrorKind};
use std::ops::Range;
use std::time::SystemTime;

use crate::archive::{DirEntry, Metadata};
use crate::format::{self, EntryKind, Method};
use crate::paths::{self, normalize};
use crate::reader;

/// Reads the raw target bytes of a symlink entry.
pub(crate) trait LinkTargets {
    fn link_target_bytes(&self, entry: &IndexedEntry) -> io::Result<Cow<'_, [u8]>>;
}

/// What the index keeps about each entry.
#[derive(Debug, Clone)]
pub(crate) struct IndexedEntry {
    name: Range<usize>,
    pub(crate) kind: EntryKind,
    pub(crate) method: Method,
    pub(crate) crc32: u32,
    pub(crate) compressed_size: u64,
    pub(crate) uncompressed_size: u64,
    pub(crate) local_header_offset: u64,
    pub(crate) modified: Option<SystemTime>,
    pub(crate) unix_mode: Option<u32>,
}

pub(crate) struct Index {
    entries: Vec<IndexedEntry>,
    /// Indices into `entries`, sorted by path, one per distinct path. When several entries share
    /// a path, the last one in the central directory wins.
    sorted: Vec<usize>,
    /// Normalized paths of all entries, concatenated; `IndexedEntry::name` indexes into it.
    names: String,
    /// Whether any entry is a symlink; with `full-symlinks`, paths are only walked component
    /// by component when there is something to follow.
    #[cfg(feature = "full-symlinks")]
    has_symlinks: bool,
}

impl Index {
    /// Builds the index from a central directory.
    ///
    /// Fails with [`ErrorKind::Unsupported`] if an entry is encrypted or uses a compression
    /// method this build cannot read, and with [`ErrorKind::InvalidData`] if an entry is
    /// malformed or its name is not a portable relative path.
    pub(crate) fn build(entries: format::Entries<'_>, capacity: u64) -> io::Result<Self> {
        let mut indexed = Vec::with_capacity(usize::try_from(capacity).unwrap_or(0));
        let mut names = String::new();

        for entry in entries {
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
            indexed.push(IndexedEntry {
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
        let mut sorted: Vec<usize> = (0..indexed.len()).collect();
        sorted
            .sort_by(|&a, &b| names[indexed[a].name.clone()].cmp(&names[indexed[b].name.clone()]));
        let mut deduplicated: Vec<usize> = Vec::with_capacity(sorted.len());
        for index in sorted {
            match deduplicated.last_mut() {
                Some(last)
                    if names[indexed[*last].name.clone()] == names[indexed[index].name.clone()] =>
                {
                    *last = index;
                }
                _ => deduplicated.push(index),
            }
        }

        #[cfg(feature = "full-symlinks")]
        let has_symlinks = indexed.iter().any(|entry| entry.kind == EntryKind::Symlink);
        Ok(Self {
            entries: indexed,
            sorted: deduplicated,
            names,
            #[cfg(feature = "full-symlinks")]
            has_symlinks,
        })
    }

    /// Number of distinct paths with their own entry.
    pub(crate) fn len(&self) -> usize {
        self.sorted.len()
    }

    /// The reachable entries (one per path) in path order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &IndexedEntry)> + '_ {
        self.sorted
            .iter()
            .map(|&index| (self.name(index), &self.entries[index]))
    }

    pub(crate) fn find(&self, normalized: &str) -> Option<&IndexedEntry> {
        self.sorted
            .binary_search_by(|&index| self.name(index).cmp(normalized))
            .ok()
            .map(|position| &self.entries[self.sorted[position]])
    }

    pub(crate) fn metadata<L: LinkTargets + ?Sized>(
        &self,
        path: &str,
        links: &L,
    ) -> io::Result<Metadata> {
        let (path, entry) = self.resolve(path, links)?;
        self.metadata_for(&path, entry)
    }

    pub(crate) fn symlink_metadata<L: LinkTargets + ?Sized>(
        &self,
        path: &str,
        links: &L,
    ) -> io::Result<Metadata> {
        let path = self.resolve_parent(path, links)?;
        self.metadata_for(&path, self.find(&path))
    }

    pub(crate) fn read_link<L: LinkTargets + ?Sized>(
        &self,
        path: &str,
        links: &L,
    ) -> io::Result<String> {
        let path = self.resolve_parent(path, links)?;
        let entry = self.find(&path).ok_or_else(|| not_found(&path))?;
        if entry.kind != EntryKind::Symlink {
            return Err(io::Error::new(ErrorKind::InvalidInput, "not a symlink"));
        }
        #[cfg(feature = "full-symlinks")]
        if self.has_symlinks {
            paths::check_link_target_size(&path, entry.uncompressed_size)?;
            let target = links.link_target_bytes(entry)?;
            let target = paths::check_link_target(&target)?.replace('\\', "/");
            let joined = format!("{}/{target}", paths::parent(&path));
            return paths::resolve_walk(&joined, false, true, |p| {
                self.symlink_target_bytes(p, links)
            });
        }
        self.link_target(&path, entry, links)
    }

    pub(crate) fn canonicalize<L: LinkTargets + ?Sized>(
        &self,
        path: &str,
        links: &L,
    ) -> io::Result<String> {
        Ok(self.resolve(path, links)?.0.into_owned())
    }

    /// Normalizes `path` and follows symlinks in its final component (in every component with
    /// `full-symlinks`), returning the resolved path and its entry (`None` for implicit
    /// directories and paths that do not exist).
    ///
    /// The path stays borrowed unless normalization or a symlink changed it, so lookups of
    /// plain paths do not allocate.
    pub(crate) fn resolve<'p, L: LinkTargets + ?Sized>(
        &self,
        path: &'p str,
        links: &L,
    ) -> io::Result<(Cow<'p, str>, Option<&IndexedEntry>)> {
        #[cfg(feature = "full-symlinks")]
        if self.has_symlinks {
            let resolved =
                paths::resolve_walk(path, true, false, |p| self.symlink_target_bytes(p, links))?;
            let entry = self.find(&resolved);
            return Ok((Cow::Owned(resolved), entry));
        }
        let mut current = normalize(path).ok_or_else(|| not_found(path))?;
        for _ in 0..paths::MAX_SYMLINK_HOPS {
            match self.find(&current) {
                Some(entry) if entry.kind == EntryKind::Symlink => {
                    current = Cow::Owned(self.link_target(&current, entry, links)?);
                }
                entry => return Ok((current, entry)),
            }
        }
        Err(paths::too_many_links())
    }

    /// Resolves `path` to a regular file.
    pub(crate) fn resolve_file<L: LinkTargets + ?Sized>(
        &self,
        path: &str,
        links: &L,
    ) -> io::Result<&IndexedEntry> {
        let (path, entry) = self.resolve(path, links)?;
        match entry {
            Some(entry) if entry.kind == EntryKind::File => Ok(entry),
            Some(_) => Err(is_a_directory(&path)),
            None if self.is_dir(&path) => Err(is_a_directory(&path)),
            None => Err(not_found(&path)),
        }
    }

    /// Lists the direct children of a directory, sorted by name.
    ///
    /// Directories that only exist implicitly (because files below them do) are listed too.
    pub(crate) fn read_dir<L: LinkTargets + ?Sized>(
        &self,
        path: &str,
        links: &L,
    ) -> io::Result<Vec<DirEntry<'_>>> {
        let base = self.canonicalize(path, links)?;
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

    pub(crate) fn entry_metadata(entry: &IndexedEntry) -> Metadata {
        Metadata {
            kind: entry.kind,
            len: entry.uncompressed_size,
            compressed_len: entry.compressed_size,
            method: Some(entry.method),
            modified: entry.modified,
            unix_mode: entry.unix_mode,
        }
    }

    fn name(&self, index: usize) -> &str {
        &self.names[self.entries[index].name.clone()]
    }

    /// Position of the first path not less than `key` in `sorted`.
    fn lower_bound(&self, key: &str) -> usize {
        self.sorted.partition_point(|&index| self.name(index) < key)
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

    /// Metadata for a normalized path whose entry (if any) has already been looked up.
    fn metadata_for(&self, normalized: &str, entry: Option<&IndexedEntry>) -> io::Result<Metadata> {
        if let Some(entry) = entry {
            return Ok(Self::entry_metadata(entry));
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

    /// Normalizes `path` and, with `full-symlinks`, follows symlinks in every component but the
    /// last; for operations on a symlink itself (`symlink_metadata`, `read_link`).
    fn resolve_parent<'p, L: LinkTargets + ?Sized>(
        &self,
        path: &'p str,
        links: &L,
    ) -> io::Result<Cow<'p, str>> {
        #[cfg(feature = "full-symlinks")]
        if self.has_symlinks {
            return paths::resolve_walk(path, false, false, |p| {
                self.symlink_target_bytes(p, links)
            })
            .map(Cow::Owned);
        }
        #[cfg(not(feature = "full-symlinks"))]
        let _ = links;
        normalize(path).ok_or_else(|| not_found(path))
    }

    /// The raw target of the entry at a normalized path, if it is a symlink.
    #[cfg(feature = "full-symlinks")]
    fn symlink_target_bytes<L: LinkTargets + ?Sized>(
        &self,
        normalized: &str,
        links: &L,
    ) -> io::Result<Option<Vec<u8>>> {
        match self.find(normalized) {
            Some(entry) if entry.kind == EntryKind::Symlink => {
                paths::check_link_target_size(normalized, entry.uncompressed_size)?;
                Ok(Some(links.link_target_bytes(entry)?.into_owned()))
            }
            _ => Ok(None),
        }
    }

    /// Reads a symlink entry's target and resolves it relative to the link's directory.
    fn link_target<L: LinkTargets + ?Sized>(
        &self,
        link_path: &str,
        entry: &IndexedEntry,
        links: &L,
    ) -> io::Result<String> {
        // The declared size bounds the bytes actually read: decoders fail on any output beyond
        // it, and stored slices are exactly the declared size.
        paths::check_link_target_size(link_path, entry.uncompressed_size)?;
        let bytes = links.link_target_bytes(entry)?;
        paths::resolve_link(link_path, &bytes)
    }
}

pub(crate) fn not_found(path: &str) -> io::Error {
    io::Error::new(
        ErrorKind::NotFound,
        format!("file not found in zip archive: {path}"),
    )
}

fn is_a_directory(path: &str) -> io::Error {
    io::Error::new(ErrorKind::IsADirectory, format!("is a directory: {path}"))
}
