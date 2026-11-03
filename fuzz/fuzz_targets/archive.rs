//! Raw bytes into `ZipArchive`, exercising every lookup and read path.

#![no_main]

mod common;

use std::borrow::Cow;
use std::io::Read;

use libfuzzer_sys::fuzz_target;
use zipfs::{EntryKind, ZipArchive};

/// Skip reading entries declared larger than this, to keep iterations fast.
const MAX_READ: u64 = 16 * 1024 * 1024;

fuzz_target!(|data: &[u8]| {
    let Ok(zip) = ZipArchive::new(data) else {
        return;
    };

    if let Ok(children) = zip.read_dir("") {
        for child in children {
            assert!(!child.name.contains('/'), "{:?}", child.name);
            assert!(zip.symlink_metadata(child.name).is_ok(), "{:?}", child.name);
        }
    }

    let entries: Vec<_> = zip
        .iter()
        .map(|(path, metadata)| (path.to_string(), metadata))
        .collect();
    assert_eq!(entries.len(), zip.len());

    for (path, metadata) in &entries {
        common::assert_normalized(path);
        assert_eq!(&zip.symlink_metadata(path).unwrap(), metadata);

        if let Ok(target) = zip.read_link(path) {
            assert_eq!(metadata.kind, EntryKind::Symlink);
            common::assert_normalized(&target);
        }
        if let Ok(canonical) = zip.canonicalize(path) {
            common::assert_normalized(&canonical);
        }
        if let Ok(children) = zip.read_dir(path) {
            // `read_dir` follows a symlink at `path` and lists the target directory, and only
            // the final path component is resolved, so children live under the canonical path.
            let base = zip.canonicalize(path).expect("read_dir canonicalized it");
            for child in children {
                assert!(!child.name.contains('/'), "{:?}", child.name);
                let child_path = common::join(&base, child.name);
                assert!(zip.symlink_metadata(&child_path).is_ok(), "{child_path:?}");
            }
        }

        let resolved = zip.metadata(path);
        if resolved.as_ref().is_ok_and(|m| m.len > MAX_READ) {
            continue;
        }
        if let Ok(bytes) = zip.read(path) {
            let resolved = resolved.expect("a readable file has metadata");
            assert_eq!(resolved.kind, EntryKind::File);
            assert_eq!(bytes.len() as u64, resolved.len);
            if let Cow::Borrowed(borrowed) = &bytes {
                common::assert_within(data, borrowed);
            }
            // The streaming reader also checks CRC-32 for stored entries, so it may fail where
            // `read` succeeded; when both succeed they must agree.
            let mut streamed = Vec::new();
            if zip
                .open(path)
                .and_then(|mut reader| reader.read_to_end(&mut streamed))
                .is_ok()
            {
                assert_eq!(streamed, *bytes);
            }
        }
        if let Ok(borrowed) = zip.read_borrowed(path) {
            common::assert_within(data, borrowed);
        }
    }
});
