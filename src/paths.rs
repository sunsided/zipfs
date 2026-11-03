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
    if target.starts_with('/') {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "absolute symlink targets are not supported",
        ));
    }
    let joined = format!("{}/{target}", parent(link_path));
    normalize(&joined).map(Cow::into_owned).ok_or_else(|| {
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
    fn parent_of_normalized_paths() {
        assert_eq!(parent("a/b/c.txt"), "a/b");
        assert_eq!(parent("c.txt"), "");
    }
}
