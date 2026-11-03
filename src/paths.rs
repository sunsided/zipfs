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
/// not portable relative paths yield `None`: names containing NUL, names with a component that
/// starts with a drive letter (`C:`) after normalization, and names that climb above the archive
/// root. Callers that hand entry names to a host file system can therefore rely on them staying
/// inside a directory.
pub(crate) fn normalize_entry_name(name: &str) -> Option<Cow<'_, str>> {
    if name.contains('\0') {
        return None;
    }
    let normalized = if name.contains('\\') {
        Cow::Owned(normalize(&name.replace('\\', "/"))?.into_owned())
    } else {
        normalize(name)?
    };
    // Validate the normalized result: `x/../C:/evil` only shows its drive letter after
    // normalization.
    if normalized.split('/').any(is_drive_letter) {
        return None;
    }
    Some(normalized)
}

/// Whether a path component starts with a drive letter (`C:`, `c:evil`).
fn is_drive_letter(component: &str) -> bool {
    let bytes = component.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
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
/// Empty targets are rejected with [`ErrorKind::InvalidData`]; absolute targets, targets that
/// are not portable (NUL, drive letters) and targets that climb above the archive root with
/// [`ErrorKind::InvalidInput`].
pub(crate) fn resolve_link(link_path: &str, target: &[u8]) -> io::Result<String> {
    let target = check_link_target(target)?;
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

/// Validates a symlink target: UTF-8, not empty, relative and portable (no NUL, no drive
/// letters).
pub(crate) fn check_link_target(target: &[u8]) -> io::Result<&str> {
    let target = std::str::from_utf8(target)
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "symlink target is not UTF-8"))?;
    if target.is_empty() {
        // POSIX does not allow empty symlink targets; do not resolve them to the link's parent.
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "symlink target is empty",
        ));
    }
    if target.starts_with(['/', '\\'])
        || target.contains('\0')
        || target.split(['/', '\\']).any(is_drive_letter)
    {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "symlink target is absolute or not a portable path",
        ));
    }
    Ok(target)
}

/// Upper bound on the path components one resolution may process, counting the components
/// symlink targets add. Legitimate paths stay far below it; without it, a few long targets that
/// refer to each other would make resolution do work quadratic in their length.
#[cfg(feature = "full-symlinks")]
const MAX_WALK_COMPONENTS: usize = 4096;

/// Resolves `path` component by component, following a symlink wherever one occurs (the
/// `full-symlinks` feature). The final component is followed only if `follow_final` is set.
///
/// A symlink is replaced by its target, read relative to the link's directory, and the walk
/// continues with the target's components followed by the rest of the path. `..` is applied
/// to the path resolved so far, so it climbs out of a symlink's target rather than out of the
/// link's own directory, as in POSIX path resolution. `symlink_target` returns the raw target
/// of the entry at a normalized path if that entry is a symlink.
///
/// `from_link` marks paths built from a symlink target, for which climbing above the archive
/// root is an invalid link rather than a missing file. Work is bounded: at most
/// [`MAX_SYMLINK_HOPS`] links and [`MAX_WALK_COMPONENTS`] components per call.
#[cfg(feature = "full-symlinks")]
pub(crate) fn resolve_walk(
    path: &str,
    follow_final: bool,
    from_link: bool,
    mut symlink_target: impl FnMut(&str) -> io::Result<Option<Vec<u8>>>,
) -> io::Result<String> {
    /// Queues the components of `text` so that they pop in order, skipping empty and `.`
    /// components, so an empty queue means the last real component has been reached.
    fn queue(pending: &mut Vec<String>, text: &str, separators: &[char]) {
        pending.extend(
            text.split(separators)
                .rev()
                .filter(|component| !matches!(*component, "" | "."))
                .map(str::to_string),
        );
    }

    /// Removes the last component (and its separator) from `resolved`.
    fn pop_component(resolved: &mut String, starts: &mut Vec<usize>) -> bool {
        let Some(start) = starts.pop() else {
            return false;
        };
        resolved.truncate(start.saturating_sub(1));
        true
    }

    let mut pending = Vec::new();
    queue(&mut pending, path, &['/']);
    // The resolved path so far, built in place, and where each of its components starts.
    let mut resolved = String::new();
    let mut starts: Vec<usize> = Vec::new();
    let mut hops = 0;
    let mut steps = 0;

    while let Some(component) = pending.pop() {
        steps += 1;
        if steps > MAX_WALK_COMPONENTS {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "path has too many components after following symlinks",
            ));
        }
        if component == ".." {
            if !pop_component(&mut resolved, &mut starts) {
                return Err(if hops == 0 && !from_link {
                    io::Error::new(
                        ErrorKind::NotFound,
                        format!("file not found in zip archive: {path}"),
                    )
                } else {
                    io::Error::new(
                        ErrorKind::InvalidInput,
                        "symlink target points outside the archive",
                    )
                });
            }
            continue;
        }

        if !resolved.is_empty() {
            resolved.push('/');
        }
        starts.push(resolved.len());
        resolved.push_str(&component);

        if pending.is_empty() && !follow_final {
            break;
        }
        let Some(target) = symlink_target(&resolved)? else {
            continue;
        };
        hops += 1;
        if hops > MAX_SYMLINK_HOPS {
            return Err(too_many_links());
        }
        let target = check_link_target(&target)?;
        pop_component(&mut resolved, &mut starts);
        queue(&mut pending, target, &['/', '\\']);
    }
    Ok(resolved)
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
        // Normalization must not be able to bring a drive letter to the front.
        assert_eq!(normalize_entry_name("x/../C:/evil"), None);
        assert_eq!(normalize_entry_name("./C:/evil"), None);
        assert_eq!(normalize_entry_name("a/..\\C:\\evil"), None);
        assert_eq!(normalize_entry_name("dir/C:/evil"), None);
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
        assert_eq!(resolve_link("dir/link", b".").unwrap(), "dir");
        assert_eq!(
            resolve_link("dir/link", b"").unwrap_err().kind(),
            ErrorKind::InvalidData
        );
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
