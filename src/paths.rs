//! Normalization of archive paths.

use std::borrow::Cow;
use std::io::{self, ErrorKind};

/// Maximum number of symlinks followed while resolving a single path.
pub(crate) const MAX_SYMLINK_HOPS: usize = 40;

/// Largest symlink target accepted. Real targets are short paths; the cap stops a forged
/// "symlink" entry from being used to read an arbitrarily large blob while resolving paths, and
/// lets `ZipSlice` decompress targets into a stack buffer.
pub(crate) const MAX_SYMLINK_TARGET: usize = 4096;

/// Normalizes an archive path: `/` is the only separator, empty and `.` components are dropped
/// and `..` removes the previous component. The result has no leading or trailing `/`, and the
/// archive root is the empty string.
///
/// Returns `None` if `..` would climb above the archive root. Already-normal paths, apart from
/// leading or trailing slashes, are returned borrowed.
pub(crate) fn normalize(path: &str) -> Option<Cow<'_, str>> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty()
        || trimmed
            .split('/')
            .all(|component| !matches!(component, "" | "." | ".."))
    {
        return Some(Cow::Borrowed(trimmed));
    }

    let mut components: Vec<&str> = Vec::new();
    for component in trimmed.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            component => components.push(component),
        }
    }
    Some(Cow::Owned(components.join("/")))
}

/// Normalizes an entry name read from an archive.
///
/// Backslashes are treated as separators, as some older Windows tools wrote them. Names that are
/// not portable relative paths yield `None`: names containing NUL, names starting with a drive
/// letter (`C:`), and names that climb above the archive root. Callers that hand entry names to
/// a host file system can therefore rely on them staying inside a directory.
pub(crate) fn normalize_entry_name(name: &str) -> Option<Cow<'_, str>> {
    if !is_portable(name) {
        return None;
    }
    if name.contains('\\') {
        Some(Cow::Owned(
            normalize(&name.replace('\\', "/"))?.into_owned(),
        ))
    } else {
        normalize(name)
    }
}

/// Rejects NUL bytes and a leading drive letter (`C:`, `C:\\`, `/C:/` after separators).
fn is_portable(name: &str) -> bool {
    if name.contains('\0') {
        return false;
    }
    let first = name
        .split(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or_default()
        .as_bytes();
    !(first.len() >= 2 && first[0].is_ascii_alphabetic() && first[1] == b':')
}

/// Checks a symlink's declared target size before its contents are read.
pub(crate) fn check_link_target_size(link_path: &str, size: u64) -> io::Result<()> {
    if size > MAX_SYMLINK_TARGET as u64 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("symlink target of {link_path} exceeds {MAX_SYMLINK_TARGET} bytes"),
        ));
    }
    Ok(())
}

/// Resolves a symlink's target relative to the directory of the (normalized) link path.
///
/// Absolute targets and targets that climb above the archive root are rejected.
pub(crate) fn resolve_link(link_path: &str, target: &[u8]) -> io::Result<String> {
    let target = std::str::from_utf8(target)
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "symlink target is not UTF-8"))?;
    if target.starts_with(['/', '\\']) || !is_portable(target) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "symlink target is absolute or not a portable path",
        ));
    }
    let joined = format!("{}/{target}", parent(link_path));
    normalize_entry_name(&joined)
        .map(Cow::into_owned)
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "symlink target points outside the archive",
            )
        })
}

pub(crate) fn too_many_links() -> io::Error {
    io::Error::other("too many levels of symbolic links")
}

/// The directory containing a normalized path (`""` for top-level entries).
pub(crate) fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_paths_are_borrowed() {
        for (input, expected) in [
            ("a/b.txt", "a/b.txt"),
            ("/a/b.txt", "a/b.txt"),
            ("a/", "a"),
            ("", ""),
            ("/", ""),
        ] {
            let normalized = normalize(input).unwrap();
            assert!(matches!(normalized, Cow::Borrowed(_)), "{input}");
            assert_eq!(normalized, expected, "{input}");
        }
    }

    #[test]
    fn dots_and_empty_components_are_resolved() {
        assert_eq!(normalize("a/./b//c.txt").unwrap(), "a/b/c.txt");
        assert_eq!(normalize("a/b/../c.txt").unwrap(), "a/c.txt");
        assert_eq!(normalize("./a").unwrap(), "a");
        assert_eq!(normalize("a/..").unwrap(), "");
    }

    #[test]
    fn escaping_the_root_is_rejected() {
        assert_eq!(normalize(".."), None);
        assert_eq!(normalize("a/../../b"), None);
        assert_eq!(normalize("/../etc/passwd"), None);
    }

    #[test]
    fn entry_names_must_be_portable() {
        assert_eq!(normalize_entry_name("a\\b\\c.txt").unwrap(), "a/b/c.txt");
        assert_eq!(normalize_entry_name("a/./b.txt").unwrap(), "a/b.txt");
        assert_eq!(normalize_entry_name("..\\..\\evil"), None);
        assert_eq!(normalize_entry_name("C:\\Windows\\evil.dll"), None);
        assert_eq!(normalize_entry_name("/c:/evil"), None);
        assert_eq!(normalize_entry_name("a\0b"), None);
        // A colon elsewhere is an ordinary character.
        assert_eq!(
            normalize_entry_name("logs/12:00.txt").unwrap(),
            "logs/12:00.txt"
        );
    }

    #[test]
    fn link_targets_must_be_portable_and_relative() {
        assert_eq!(resolve_link("dir/link", b"../a.txt").unwrap(), "a.txt");
        assert_eq!(resolve_link("dir/link", b"..\\a.txt").unwrap(), "a.txt");
        for target in [
            &b"/etc/passwd"[..],
            b"\\server\\share",
            b"C:/x",
            b"a\0b",
            b"../../x",
        ] {
            assert_eq!(
                resolve_link("dir/link", target).unwrap_err().kind(),
                ErrorKind::InvalidInput,
                "{}",
                String::from_utf8_lossy(target)
            );
        }
    }

    #[test]
    fn parent_of_normalized_paths() {
        assert_eq!(parent("a/b/c.txt"), "a/b");
        assert_eq!(parent("c.txt"), "");
    }
}
