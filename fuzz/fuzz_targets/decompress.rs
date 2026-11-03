//! Fuzzed entry data wrapped in a structurally valid archive, aimed at the decompressors and the
//! declared-size cap. Random bytes rarely form a valid zip, so the archive structure is built
//! here and only the entry's method, sizes, checksum and data are fuzzed.

#![no_main]

mod common;

use std::io::Read;

use libfuzzer_sys::arbitrary::{self, Arbitrary};
use libfuzzer_sys::fuzz_target;
use zipfs::{ZipArchive, ZipSlice};

#[derive(Debug, Arbitrary)]
enum Method {
    Stored,
    Deflate,
    Zstd,
    Other(u16),
}

impl Method {
    fn id(&self) -> u16 {
        match self {
            Self::Stored => 0,
            Self::Deflate => 8,
            Self::Zstd => 93,
            Self::Other(id) => *id,
        }
    }
}

#[derive(Debug, Arbitrary)]
struct Input {
    method: Method,
    declared_size: u32,
    crc32: u32,
    symlink: bool,
    data: Vec<u8>,
}

/// Bound the declared size so iterations stay fast.
const MAX_DECLARED: u32 = 1 << 20;

fuzz_target!(|input: Input| {
    let declared = input.declared_size % MAX_DECLARED;
    let mode = if input.symlink { 0o120_777 } else { 0o100_644 };
    let bytes = single_entry_zip(input.method.id(), input.crc32, &input.data, declared, mode);

    let Ok(zip) = ZipArchive::new(&bytes[..]) else {
        return;
    };
    let slice = ZipSlice::new(&bytes).expect("ZipArchive accepted it");

    if input.symlink {
        let ours = zip.read_link("f");
        let theirs = slice.read_link("f");
        if let (Ok(ours), Ok(theirs)) = (&ours, &theirs) {
            common::assert_normalized(ours);
            assert_eq!(ours, theirs);
        }
        return;
    }

    // The declared size is a hard cap, even when decompression fails.
    let mut out = Vec::new();
    if let Ok(mut reader) = zip.open("f") {
        let result = reader.read_to_end(&mut out);
        assert!(
            out.len() as u64 <= u64::from(declared),
            "read {} bytes past the declared {declared}",
            out.len()
        );
        if result.is_ok() {
            assert_eq!(out.len() as u64, u64::from(declared));
            assert_eq!(common::crc32(&out), input.crc32);
        }
    }

    if let Ok(read) = zip.read("f") {
        assert_eq!(read.len() as u64, u64::from(declared));
        if input.method.id() != 0 {
            // Stored entries are borrowed without a CRC check; decompressed ones are verified.
            assert_eq!(common::crc32(&read), input.crc32);
        }
    }
});

/// One entry named `f` with the given method, CRC-32 and data, written by a Unix host.
fn single_entry_zip(method: u16, crc: u32, data: &[u8], declared: u32, mode: u32) -> Vec<u8> {
    let name = b"f";
    let compressed = u32::try_from(data.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(data.len() + 128);

    out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes()); // version needed
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&method.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // time
    out.extend_from_slice(&0x21u16.to_le_bytes()); // date: 1980-01-01
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&compressed.to_le_bytes());
    out.extend_from_slice(&declared.to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // extra length
    out.extend_from_slice(name);
    out.extend_from_slice(data);

    let central_offset = out.len() as u32;
    out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    out.extend_from_slice(&((3u16 << 8) | 20).to_le_bytes()); // made by: Unix
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&method.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0x21u16.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&compressed.to_le_bytes());
    out.extend_from_slice(&declared.to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // extra length
    out.extend_from_slice(&0u16.to_le_bytes()); // comment length
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
    out.extend_from_slice(&(mode << 16).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // local header offset
    out.extend_from_slice(name);
    let central_size = out.len() as u32 - central_offset;

    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}
