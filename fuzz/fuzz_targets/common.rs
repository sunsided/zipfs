//! Invariants shared by the fuzz targets.

#![allow(dead_code)]

/// Paths handed out by zipfs are normalized and portable: no empty, `.` or `..` components,
/// no leading or trailing `/`, no backslashes, NUL bytes or drive-letter components.
pub fn assert_normalized(path: &str) {
    if path.is_empty() {
        return;
    }
    assert!(
        !path.starts_with('/') && !path.ends_with('/'),
        "not normalized: {path:?}"
    );
    for component in path.split('/') {
        assert!(
            !matches!(component, "" | "." | ".."),
            "not normalized: {path:?}"
        );
        assert!(!component.contains(['\\', '\0']), "not portable: {path:?}");
        let bytes = component.as_bytes();
        assert!(
            !(bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'),
            "drive letter: {path:?}"
        );
    }
}

/// A borrowed slice must point into the archive bytes it was read from.
pub fn assert_within(data: &[u8], slice: &[u8]) {
    if slice.is_empty() {
        return;
    }
    let outer = data.as_ptr_range();
    let inner = slice.as_ptr_range();
    assert!(
        outer.start <= inner.start && inner.end <= outer.end,
        "borrowed slice is not inside the archive data"
    );
}

/// The child path of a `read_dir` entry.
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
