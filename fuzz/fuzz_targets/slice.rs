//! Raw bytes into `ZipSlice`, cross-checked against `ZipArchive` where both succeed.

#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;
use zipfs::{EntryKind, ZipArchive, ZipSlice};

/// `ZipSlice` lookups scan the whole central directory; bound the work per input.
const MAX_ENTRIES: usize = 256;

fuzz_target!(|data: &[u8]| {
    let Ok(slice) = ZipSlice::new(data) else {
        return;
    };
    let archive = ZipArchive::new(data).ok();

    for entry in slice.entries().take(MAX_ENTRIES) {
        let Ok(entry) = entry else {
            break;
        };
        let _ = (
            entry.kind(),
            entry.unix_mode(),
            entry.modified(),
            entry.comment(),
        );
        if let Ok(raw) = slice.raw_data(&entry) {
            common::assert_within(data, raw);
            assert_eq!(raw.len() as u64, entry.compressed_size());
        }
        if let Ok(stored) = slice.stored_data(&entry) {
            common::assert_within(data, stored);
        }

        let Ok(name) = entry.name() else {
            continue;
        };
        if let Ok(bytes) = slice.get(&name) {
            common::assert_within(data, bytes);
            if let Some(Ok(other)) = archive.as_ref().map(|a| a.read_borrowed(&name)) {
                assert_eq!(bytes, other, "{name:?}");
            }
        }
        if let Ok((resolved, target)) = slice.resolve(&name) {
            assert_ne!(target.kind(), EntryKind::Symlink);
            common::assert_normalized(&resolved);
        }
        if let Ok(link) = slice.read_link(&name) {
            common::assert_normalized(&link);
            if let Some(Ok(other)) = archive.as_ref().map(|a| a.read_link(&name)) {
                assert_eq!(link, other, "{name:?}");
            }
        }
    }
});
