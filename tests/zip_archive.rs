//! Tests for the synchronous reader (`ZipArchive`, `ZipSlice`).
//!
//! Archives written by the `zip` crate are read back by both implementations and compared
//! field by field. Hand-built archives cover cases no regular writer produces.

use std::borrow::Cow;
use std::io::{self, Cursor, ErrorKind, Read, Write};

use zip::CompressionMethod;
use zip::write::SimpleFileOptions;
use zipfs::{EntryKind, Method, ZipArchive, ZipSlice};

static STORED: &[u8] = include_bytes!("../test_data/stored_only.zip");
static NO_DIR_ENTRIES: &[u8] = include_bytes!("../test_data/no_dir_entries.zip");
static SYMLINKS: &[u8] = include_bytes!("../test_data/symlink_test.zip");
static EXTERNAL_SYMLINK: &[u8] = include_bytes!("../test_data/external_symlink.zip");
static CORRUPT_LOCAL_HEADER: &[u8] = include_bytes!("../test_data/corrupt_local_header.zip");
/// 8 MiB of zeros, deflated, with the declared uncompressed size patched to 16 bytes.
#[cfg(feature = "deflate")]
static SIZE_BOMB: &[u8] = include_bytes!("../test_data/size_bomb.zip");
/// A symlink entry whose stored target is 5000 bytes long.
static LONG_SYMLINK: &[u8] = include_bytes!("../test_data/long_symlink.zip");

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Deterministic pseudo-random bytes (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

/// Compression methods this build of zipfs can read.
fn supported_methods() -> Vec<CompressionMethod> {
    let mut methods = vec![CompressionMethod::Stored];
    if cfg!(feature = "deflate") {
        methods.push(CompressionMethod::Deflated);
    }
    if cfg!(feature = "zstd") {
        methods.push(CompressionMethod::Zstd);
    }
    methods
}

fn options(method: CompressionMethod) -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(method)
        .unix_permissions(0o644)
}

/// Writes an archive with the `zip` crate.
fn write_zip(build: impl FnOnce(&mut zip::ZipWriter<Cursor<Vec<u8>>>)) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    build(&mut writer);
    writer.finish().unwrap().into_inner()
}

/// Normalizes like zipfs: no leading/trailing `/`, `.` and empty components dropped.
fn normalized(name: &str) -> String {
    name.split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Reads `bytes` with zipfs and with the `zip` crate and compares every entry.
fn assert_matches_reference(bytes: &[u8]) {
    let slice = ZipSlice::new(bytes).unwrap();
    let archive = ZipArchive::new(bytes).unwrap();
    let mut reference = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();

    assert_eq!(slice.entry_count() as usize, reference.len());
    let entries: Vec<_> = slice.entries().collect::<io::Result<_>>().unwrap();
    assert_eq!(entries.len(), reference.len());

    for (index, entry) in entries.iter().enumerate() {
        let mut expected = reference.by_index(index).unwrap();
        let name = entry.name().unwrap();
        assert_eq!(name, expected.name(), "entry {index}");
        assert_eq!(entry.size(), expected.size(), "{name}: size");
        assert_eq!(
            entry.compressed_size(),
            expected.compressed_size(),
            "{name}: compressed size"
        );
        assert_eq!(entry.crc32(), expected.crc32(), "{name}: crc32");
        assert_eq!(
            entry.method().id(),
            compression_id(expected.compression()),
            "{name}: method"
        );
        assert_eq!(
            entry.kind() == EntryKind::Directory,
            expected.is_dir(),
            "{name}: is_dir"
        );
        assert_eq!(
            entry.kind() == EntryKind::Symlink,
            expected.is_symlink(),
            "{name}: is_symlink"
        );
        assert_eq!(entry.unix_mode(), expected.unix_mode(), "{name}: unix mode");

        let mut contents = Vec::new();
        expected.read_to_end(&mut contents).unwrap();
        drop(expected);

        let path = normalized(&name);
        match entry.kind() {
            EntryKind::File => {
                let ours = archive.read(&path).unwrap();
                assert_eq!(&*ours, &contents[..], "{name}: read");
                assert_eq!(
                    matches!(ours, Cow::Borrowed(_)),
                    entry.method() == Method::Stored,
                    "{name}: borrowed iff stored"
                );
                let mut streamed = Vec::new();
                archive
                    .open(&path)
                    .unwrap()
                    .read_to_end(&mut streamed)
                    .unwrap();
                assert_eq!(streamed, contents, "{name}: open");
            }
            EntryKind::Symlink => {
                assert_eq!(
                    slice.raw_data(entry).unwrap(),
                    &contents[..],
                    "{name}: target"
                );
            }
            EntryKind::Directory => {
                assert_eq!(archive.metadata(&path).unwrap().kind, EntryKind::Directory);
            }
        }
    }
}

fn compression_id(method: CompressionMethod) -> u16 {
    #[allow(deprecated)]
    match method {
        CompressionMethod::Stored => 0,
        CompressionMethod::Deflated => 8,
        CompressionMethod::Zstd => 93,
        other => other.to_u16(),
    }
}

fn names<'z>(entries: &[zipfs::archive::DirEntry<'z>]) -> Vec<&'z str> {
    entries.iter().map(|e| e.name).collect()
}

// ---------------------------------------------------------------------------------------------
// Hand-built archives
// ---------------------------------------------------------------------------------------------

fn crc32(data: &[u8]) -> u32 {
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

struct Hand<'a> {
    name: &'a [u8],
    data: &'a [u8],
    method: u16,
    flags: u16,
    crc: Option<u32>,
    zip64: bool,
    extra: Vec<u8>,
    mode: u32,
    /// Uncompressed size when `data` is compressed; defaults to `data.len()`.
    uncompressed_size: Option<u32>,
    /// "Version made by" host system: 3 is Unix, 0 is MS-DOS (used by Windows writers).
    host: u8,
}

impl<'a> Hand<'a> {
    fn file(name: &'a [u8], data: &'a [u8]) -> Self {
        Self {
            name,
            data,
            method: 0,
            flags: 0,
            crc: None,
            zip64: false,
            extra: Vec::new(),
            mode: 0o100_644,
            uncompressed_size: None,
            host: 3,
        }
    }

    fn symlink(name: &'a [u8], target: &'a [u8]) -> Self {
        Self {
            mode: 0o120_777,
            ..Self::file(name, target)
        }
    }
}

/// Builds an archive from raw entries. Offsets are relative to the start of the archive, so a
/// non-empty `prefix` simulates data prepended with `cat stub archive.zip`.
fn build(entries: &[Hand<'_>], prefix: &[u8]) -> Vec<u8> {
    let mut out = prefix.to_vec();
    let mut central = Vec::new();
    for entry in entries {
        let offset = (out.len() - prefix.len()) as u64;
        let crc = entry.crc.unwrap_or_else(|| crc32(entry.data));
        let size = entry.data.len() as u64;
        let uncompressed32 = entry.uncompressed_size.unwrap_or(size as u32);
        let (size32, offset32) = if entry.zip64 {
            (0xFFFF_FFFFu32, 0xFFFF_FFFFu32)
        } else {
            (size as u32, offset as u32)
        };

        let mut local_extra = Vec::new();
        if entry.zip64 {
            local_extra.extend_from_slice(&1u16.to_le_bytes());
            local_extra.extend_from_slice(&16u16.to_le_bytes());
            local_extra.extend_from_slice(&size.to_le_bytes());
            local_extra.extend_from_slice(&size.to_le_bytes());
        }
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&45u16.to_le_bytes());
        out.extend_from_slice(&entry.flags.to_le_bytes());
        out.extend_from_slice(&entry.method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0x21u16.to_le_bytes()); // 1980-01-01
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size32.to_le_bytes());
        out.extend_from_slice(&(if entry.zip64 { size32 } else { uncompressed32 }).to_le_bytes());
        out.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(local_extra.len() as u16).to_le_bytes());
        out.extend_from_slice(entry.name);
        out.extend_from_slice(&local_extra);
        out.extend_from_slice(entry.data);

        let mut extra = Vec::new();
        if entry.zip64 {
            extra.extend_from_slice(&1u16.to_le_bytes());
            extra.extend_from_slice(&24u16.to_le_bytes());
            extra.extend_from_slice(&size.to_le_bytes());
            extra.extend_from_slice(&size.to_le_bytes());
            extra.extend_from_slice(&offset.to_le_bytes());
        }
        extra.extend_from_slice(&entry.extra);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&((u16::from(entry.host) << 8) | 63).to_le_bytes()); // made by
        central.extend_from_slice(&45u16.to_le_bytes());
        central.extend_from_slice(&entry.flags.to_le_bytes());
        central.extend_from_slice(&entry.method.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0x21u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size32.to_le_bytes());
        central
            .extend_from_slice(&(if entry.zip64 { size32 } else { uncompressed32 }).to_le_bytes());
        central.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
        central.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // comment length
        central.extend_from_slice(&0u16.to_le_bytes()); // disk
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        central.extend_from_slice(&(entry.mode << 16).to_le_bytes());
        central.extend_from_slice(&offset32.to_le_bytes());
        central.extend_from_slice(entry.name);
        central.extend_from_slice(&extra);
    }
    let cd_offset = (out.len() - prefix.len()) as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

// ---------------------------------------------------------------------------------------------
// Reference comparisons
// ---------------------------------------------------------------------------------------------

#[test]
fn mixed_archive_matches_reference() {
    let mut rng = Rng(0x5EED);
    let blob = rng.bytes(256 * 1024);
    let text = "lorem ipsum dolor sit amet ".repeat(400);

    let bytes = write_zip(|w| {
        w.add_directory("assets/", options(CompressionMethod::Stored))
            .unwrap();
        w.add_directory("assets/images/", options(CompressionMethod::Stored))
            .unwrap();
        for (i, method) in supported_methods().into_iter().enumerate() {
            w.start_file(format!("assets/text_{i}.txt"), options(method))
                .unwrap();
            w.write_all(text.as_bytes()).unwrap();
            w.start_file(format!("assets/images/blob_{i}.bin"), options(method))
                .unwrap();
            w.write_all(&blob).unwrap();
            w.start_file(format!("assets/empty_{i}.txt"), options(method))
                .unwrap();
        }
        w.start_file("assets/ünïcödé ✓.txt", options(CompressionMethod::Stored))
            .unwrap();
        w.write_all(b"unicode").unwrap();
        w.start_file("deep/a/b/c/d.txt", options(CompressionMethod::Stored))
            .unwrap();
        w.write_all(b"deep").unwrap();
        w.add_symlink(
            "latest.txt",
            "assets/text_0.txt",
            options(CompressionMethod::Stored),
        )
        .unwrap();
    });

    assert_matches_reference(&bytes);

    let zip = ZipArchive::new(&bytes[..]).unwrap();
    assert_eq!(
        names(&zip.read_dir("").unwrap()),
        ["assets", "deep", "latest.txt"]
    );
    assert_eq!(names(&zip.read_dir("deep/a").unwrap()), ["b"]);
    assert_eq!(zip.metadata("deep/a/b").unwrap().kind, EntryKind::Directory);
    assert_eq!(zip.read_link("latest.txt").unwrap(), "assets/text_0.txt");
    assert_eq!(
        ZipSlice::new(&bytes).unwrap().get("latest.txt").unwrap(),
        text.as_bytes()
    );
    assert_eq!(zip.read_to_string("latest.txt").unwrap(), text);
    assert_eq!(&*zip.read("assets/ünïcödé ✓.txt").unwrap(), b"unicode");
    assert_eq!(
        zip.read_borrowed("assets/text_0.txt").unwrap(),
        text.as_bytes()
    );
}

#[test]
fn many_entries_use_zip64_end_records() {
    // More than 65535 entries require the ZIP64 end of central directory record.
    let bytes = write_zip(|w| {
        for i in 0..70_000u32 {
            w.start_file(
                format!("{:02}/{i}", i % 97),
                options(CompressionMethod::Stored),
            )
            .unwrap();
            w.write_all(&i.to_le_bytes()).unwrap();
        }
    });
    let slice = ZipSlice::new(&bytes).unwrap();
    assert_eq!(slice.entry_count(), 70_000);

    let zip = ZipArchive::new(&bytes[..]).unwrap();
    assert_eq!(zip.len(), 70_000);
    let last = 69_999u32;
    let path = format!("{:02}/{last}", last % 97);
    assert_eq!(&*zip.read(&path).unwrap(), &last.to_le_bytes());
    assert_eq!(zip.read_dir("").unwrap().len(), 97);
}

#[test]
fn prepended_data_is_skipped() {
    let archive = write_zip(|w| {
        w.start_file("hello.txt", options(CompressionMethod::Stored))
            .unwrap();
        w.write_all(b"hello").unwrap();
    });
    let mut bytes = b"#!/bin/sh\necho self-extracting stub\nexit 0\n".to_vec();
    bytes.extend_from_slice(&archive);

    assert_matches_reference(&bytes);
    assert_eq!(
        ZipSlice::new(&bytes).unwrap().get("hello.txt").unwrap(),
        b"hello"
    );
}

#[test]
fn archive_comment() {
    let bytes = write_zip(|w| {
        w.set_comment("built for tests");
        w.start_file("a.txt", options(CompressionMethod::Stored))
            .unwrap();
    });
    assert_eq!(ZipSlice::new(&bytes).unwrap().comment(), b"built for tests");
}

#[test]
fn fixtures_match_reference() {
    for bytes in [STORED, NO_DIR_ENTRIES, EXTERNAL_SYMLINK, LONG_SYMLINK] {
        assert_matches_reference(bytes);
    }
    if cfg!(feature = "deflate") {
        assert_matches_reference(SYMLINKS);
    }
}

#[test]
fn cp437_names_are_decoded() {
    // Rewrite "readme.txt" to "r\x82adme.txt" (0x82 is 'é' in code page 437) in both the local
    // and the central header; the UTF-8 flag is not set.
    let mut bytes = STORED.to_vec();
    let mut replaced = 0;
    for at in 0..bytes.len() - 10 {
        if &bytes[at..at + 10] == b"readme.txt" {
            bytes[at + 1] = 0x82;
            replaced += 1;
        }
    }
    assert_eq!(replaced, 2);

    let mut reference = zip::ZipArchive::new(Cursor::new(&bytes[..])).unwrap();
    let expected = reference.by_index(0).unwrap().name().to_string();
    assert_eq!(expected, "réadme.txt");

    let entry = ZipSlice::new(&bytes)
        .unwrap()
        .entries()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(entry.name().unwrap(), expected);
    assert!(matches!(entry.name().unwrap(), Cow::Owned(_)));
    let zip = ZipArchive::new(&bytes[..]).unwrap();
    assert_eq!(
        &*zip.read("réadme.txt").unwrap(),
        b"hello from a stored zip\n"
    );
}

// ---------------------------------------------------------------------------------------------
// Hand-built archives
// ---------------------------------------------------------------------------------------------

#[test]
fn zip64_extra_fields_are_used() {
    let mut entry = Hand::file(b"big.bin", b"pretend this is large");
    entry.zip64 = true;
    let bytes = build(&[entry], b"");

    let slice = ZipSlice::new(&bytes).unwrap();
    let parsed = slice.entries().next().unwrap().unwrap();
    assert_eq!(parsed.size(), 21);
    assert_eq!(slice.get("big.bin").unwrap(), b"pretend this is large");
    assert_matches_reference(&bytes);
}

#[test]
fn unicode_path_extra_field_wins() {
    let mut extra = Vec::new();
    let unicode = "naïve.txt".as_bytes();
    extra.extend_from_slice(&0x7075u16.to_le_bytes());
    extra.extend_from_slice(&((5 + unicode.len()) as u16).to_le_bytes());
    extra.push(1);
    extra.extend_from_slice(&crc32(b"naive.txt").to_le_bytes());
    extra.extend_from_slice(unicode);
    let mut entry = Hand::file(b"naive.txt", b"x");
    entry.extra = extra;
    let bytes = build(&[entry], b"");

    let zip = ZipArchive::new(&bytes[..]).unwrap();
    assert_eq!(&*zip.read("naïve.txt").unwrap(), b"x");
    assert!(!zip.exists("naive.txt"));
}

#[test]
fn duplicate_paths_keep_the_last_entry() {
    let bytes = build(
        &[
            Hand::file(b"dup.txt", b"first"),
            Hand::file(b"./dup.txt", b"second"),
        ],
        b"",
    );
    let zip = ZipArchive::new(&bytes[..]).unwrap();
    assert_eq!(zip.len(), 1);
    assert_eq!(&*zip.read("dup.txt").unwrap(), b"second");
    assert_eq!(
        ZipSlice::new(&bytes).unwrap().get("dup.txt").unwrap(),
        b"second"
    );
}

#[test]
fn names_escaping_the_root_are_rejected() {
    let bytes = build(&[Hand::file(b"../evil.txt", b"x")], b"");
    let err = ZipArchive::new(&bytes[..]).err().expect("must be rejected");
    assert_eq!(err.kind(), ErrorKind::InvalidData);

    // The raw view still lists it, but path lookups cannot reach it.
    let slice = ZipSlice::new(&bytes).unwrap();
    assert_eq!(slice.entries().count(), 1);
    assert!(slice.find("../evil.txt").unwrap().is_none());
}

#[test]
fn backslash_names_are_normalized() {
    // Older Windows tools wrote backslash separators.
    let bytes = build(&[Hand::file(b"assets\\images\\a.png", b"png")], b"");
    let zip = ZipArchive::new(&bytes[..]).unwrap();
    assert_eq!(&*zip.read("assets/images/a.png").unwrap(), b"png");
    assert_eq!(names(&zip.read_dir("assets").unwrap()), ["images"]);
    assert_eq!(
        ZipSlice::new(&bytes)
            .unwrap()
            .get("assets/images/a.png")
            .unwrap(),
        b"png"
    );
}

#[test]
fn non_portable_names_are_rejected() {
    for name in [
        &b"..\\..\\evil.dll"[..],
        b"C:\\Windows\\evil.dll",
        b"c:/evil",
        b"a\0b.txt",
        b"x/../C:/evil.dll",
        b"a\\..\\c:\\evil.dll",
    ] {
        let bytes = build(&[Hand::file(name, b"x")], b"");
        let err = ZipArchive::new(&bytes[..]).err().expect("must be rejected");
        assert_eq!(
            err.kind(),
            ErrorKind::InvalidData,
            "{}",
            String::from_utf8_lossy(name)
        );
        // The raw view lists the entry, but no lookup reaches it.
        let slice = ZipSlice::new(&bytes).unwrap();
        assert_eq!(slice.entries().count(), 1);
        assert!(slice.find("evil.dll").unwrap().is_none());
    }
}

/// Windows writers (the `zip` crate among them) mark entries as made by MS-DOS but still store
/// the Unix mode in the upper 16 bits of the external attributes; that is the only way to
/// recognize symlinks in such archives.
#[test]
fn unix_modes_from_windows_writers_are_used() {
    let mut file = Hand::file(b"dir/a.txt", b"payload");
    file.host = 0;
    let mut link = Hand::symlink(b"link", b"dir/a.txt");
    link.host = 0;
    let bytes = build(&[file, link], b"");

    assert_matches_reference(&bytes[..]);
    let zip = ZipArchive::new(&bytes[..]).unwrap();
    assert_eq!(
        zip.symlink_metadata("link").unwrap().kind,
        EntryKind::Symlink
    );
    assert_eq!(
        zip.symlink_metadata("link").unwrap().unix_mode,
        Some(0o120_777)
    );
    assert_eq!(&*zip.read("link").unwrap(), b"payload");
    assert_eq!(
        ZipSlice::new(&bytes).unwrap().get("link").unwrap(),
        b"payload"
    );

    // Upper bits that do not name a file type are not taken as a mode (the zip crate trusts any
    // non-zero value, so this case is not compared against it).
    let mut noise = Hand::file(b"noise.txt", b"x");
    noise.host = 0;
    noise.mode = 0xFFFF; // File type bits 0o170000: not a valid type.
    let bytes = build(&[noise], b"");
    let zip = ZipArchive::new(&bytes[..]).unwrap();
    assert_eq!(zip.metadata("noise.txt").unwrap().unix_mode, None);
    assert_eq!(zip.metadata("noise.txt").unwrap().kind, EntryKind::File);
}

/// Raw deflate data holding `payload` in one stored block; `last` sets the BFINAL bit.
#[cfg(feature = "deflate")]
fn deflate_stored_block(payload: &[u8], last: bool) -> Vec<u8> {
    let len = payload.len() as u16;
    let mut out = vec![u8::from(last)]; // BFINAL, BTYPE = 00 (stored), padded to a byte
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// A deflate stream must reach its end marker; producing the declared bytes with the right
/// CRC is not enough.
#[cfg(feature = "deflate")]
#[test]
fn truncated_deflate_streams_are_rejected() {
    let payload = b"complete output, missing final block";
    let complete = deflate_stored_block(payload, true);
    let truncated = deflate_stored_block(payload, false);
    let entry = |name: &'static [u8], data: &'static [u8]| {
        let mut entry = Hand::file(name, data);
        entry.method = 8;
        entry.crc = Some(crc32(payload));
        entry.uncompressed_size = Some(payload.len() as u32);
        entry
    };
    let complete: &'static [u8] = complete.leak();
    let truncated: &'static [u8] = truncated.leak();
    let bytes = build(
        &[entry(b"complete", complete), entry(b"truncated", truncated)],
        b"",
    );

    let zip = ZipArchive::new(&bytes[..]).unwrap();
    assert_eq!(&*zip.read("complete").unwrap(), payload);
    let err = zip.read("truncated").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnexpectedEof, "{err}");
    let mut out = Vec::new();
    let err = zip
        .open("truncated")
        .unwrap()
        .read_to_end(&mut out)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnexpectedEof, "{err}");
}

#[test]
fn encrypted_entries_are_rejected() {
    let mut entry = Hand::file(b"secret.txt", b"x");
    entry.flags = 1;
    let bytes = build(&[entry], b"");
    let err = ZipArchive::new(&bytes[..]).err().expect("must be rejected");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(
        ZipSlice::new(&bytes)
            .unwrap()
            .get("secret.txt")
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported
    );
}

#[test]
fn unknown_methods_are_rejected() {
    let mut entry = Hand::file(b"a.bz2", b"not really bzip2");
    entry.method = 12;
    let bytes = build(&[entry], b"");
    let err = ZipArchive::new(&bytes[..]).err().expect("must be rejected");
    assert_eq!(err.kind(), ErrorKind::Unsupported, "{err}");
}

#[test]
fn crc_mismatch_fails_the_streaming_reader() {
    let mut entry = Hand::file(b"a.txt", b"payload");
    entry.crc = Some(0xDEAD_BEEF);
    let bytes = build(&[entry], b"");
    let zip = ZipArchive::new(&bytes[..]).unwrap();

    let mut out = Vec::new();
    let err = zip
        .open("a.txt")
        .unwrap()
        .read_to_end(&mut out)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidData, "{err}");
    // Borrowed reads do not touch the data, so they do not check the CRC.
    assert_eq!(&*zip.read("a.txt").unwrap(), b"payload");
}

#[test]
fn directory_entries_from_dos_attributes_and_root_entries() {
    let mut root = Hand::file(b"./", b"");
    root.mode = 0o040_755;
    let bytes = build(&[root, Hand::file(b"a/b.txt", b"b")], b"");
    let zip = ZipArchive::new(&bytes[..]).unwrap();
    assert_eq!(zip.len(), 1);
    assert_eq!(names(&zip.read_dir("/").unwrap()), ["a"]);
}

// ---------------------------------------------------------------------------------------------
// Borrowing
// ---------------------------------------------------------------------------------------------

/// The returned slice outlives the archive object; this only compiles if it borrows from the
/// `'static` bytes rather than from the `ZipArchive`.
fn borrowed_from_dropped_archive() -> &'static [u8] {
    let zip = ZipArchive::new(STORED).unwrap();
    zip.read_borrowed("images/sprite.png").unwrap()
}

fn borrowed_from_dropped_slice() -> &'static [u8] {
    let zip = ZipSlice::new(STORED).unwrap();
    zip.get("readme.txt").unwrap()
}

#[test]
fn borrowed_reads_point_into_the_archive() {
    let range = STORED.as_ptr_range();
    let sprite = borrowed_from_dropped_archive();
    assert!(sprite.starts_with(b"\x89PNG"));
    assert!(range.contains(&sprite.as_ptr()));

    let readme = borrowed_from_dropped_slice();
    assert_eq!(readme, b"hello from a stored zip\n");
    assert!(range.contains(&readme.as_ptr()));

    let zip = ZipArchive::new(STORED).unwrap();
    let Cow::Borrowed(read) = zip.read("readme.txt").unwrap() else {
        panic!("stored entry was copied");
    };
    assert!(range.contains(&read.as_ptr()));
}

#[test]
fn owned_buffers_work_too() {
    let zip = ZipArchive::new(STORED.to_vec()).unwrap();
    assert_eq!(
        &*zip.read("readme.txt").unwrap(),
        b"hello from a stored zip\n"
    );
    let zip = ZipArchive::new(std::sync::Arc::<[u8]>::from(STORED)).unwrap();
    assert!(zip.exists("images/sprite.png"));
}

#[cfg(feature = "deflate")]
#[test]
fn compressed_entries_cannot_be_borrowed() {
    let zip = ZipArchive::new(SYMLINKS).unwrap();
    assert_eq!(
        zip.read_borrowed("b/c/d.txt").unwrap_err().kind(),
        ErrorKind::Unsupported
    );
    assert_eq!(zip.read_to_string("b/c/d.txt").unwrap(), "d");
    assert_eq!(
        ZipSlice::new(SYMLINKS)
            .unwrap()
            .get("b/c/d.txt")
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported
    );
}

// ---------------------------------------------------------------------------------------------
// Behaviour
// ---------------------------------------------------------------------------------------------

#[test]
fn zip_slice_lookups() {
    let zip = ZipSlice::new(STORED).unwrap();
    assert_eq!(zip.get("/images/./sprite.png").unwrap().len(), 24);
    assert_eq!(
        zip.get("images").unwrap_err().kind(),
        ErrorKind::IsADirectory
    );
    assert_eq!(zip.get("nope").unwrap_err().kind(), ErrorKind::NotFound);
    // Directories without an entry of their own are recognized too.
    assert_eq!(
        ZipSlice::new(NO_DIR_ENTRIES)
            .unwrap()
            .get("images")
            .unwrap_err()
            .kind(),
        ErrorKind::IsADirectory
    );
    assert_eq!(
        ZipSlice::new(LONG_SYMLINK)
            .unwrap()
            .get("link")
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData
    );
}

#[test]
fn zip_slice_follows_symlinks() {
    let bytes = build(
        &[
            Hand::file(b"dir/a.txt", b"payload"),
            Hand::symlink(b"first", b"second"),
            Hand::symlink(b"second", b"dir/../dir/a.txt"),
            Hand::symlink(b"dir/sibling", b"a.txt"),
            Hand::symlink(b"loop_a", b"loop_b"),
            Hand::symlink(b"loop_b", b"loop_a"),
            Hand::symlink(b"escape", b"../outside"),
            Hand::symlink(b"absolute", b"/etc/passwd"),
            Hand::symlink(b"dangling", b"missing.txt"),
            Hand::symlink(b"to_dir", b"dir"),
        ],
        b"",
    );
    let zip = ZipSlice::new(&bytes).unwrap();

    assert_eq!(zip.get("first").unwrap(), b"payload");
    assert_eq!(zip.get("dir/sibling").unwrap(), b"payload");
    let (resolved, entry) = zip.resolve("first").unwrap();
    assert_eq!(resolved, "dir/a.txt");
    assert_eq!(entry.kind(), EntryKind::File);
    assert_eq!(zip.read_link("first").unwrap(), "second");
    assert_eq!(zip.read_link("second").unwrap(), "dir/a.txt");
    assert_eq!(
        zip.read_link("dir/a.txt").unwrap_err().kind(),
        ErrorKind::InvalidInput
    );

    assert_eq!(
        zip.get("loop_a").unwrap_err().to_string(),
        "too many levels of symbolic links"
    );
    assert_eq!(
        zip.get("escape").unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        zip.get("absolute").unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(zip.get("dangling").unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(
        zip.get("to_dir").unwrap_err().kind(),
        ErrorKind::IsADirectory
    );

    // Same answers as ZipArchive for the links both can resolve.
    let archive = ZipArchive::new(&bytes[..]).unwrap();
    for link in ["first", "second", "dir/sibling", "to_dir", "dangling"] {
        assert_eq!(
            zip.read_link(link).unwrap(),
            archive.read_link(link).unwrap(),
            "{link}"
        );
    }
    assert_eq!(
        archive.read_borrowed("first").unwrap(),
        zip.get("first").unwrap()
    );
}

#[test]
fn zip_slice_resolves_fixture_symlinks() {
    // The links are stored; their targets are deflated files, which ZipSlice cannot borrow.
    let zip = ZipSlice::new(SYMLINKS).unwrap();
    let (resolved, entry) = zip.resolve("symlink_to_d.txt").unwrap();
    assert_eq!(resolved, "b/c/d.txt");
    assert_eq!(entry.method(), Method::Deflate);
    assert_eq!(
        zip.get("symlink_to_d.txt").unwrap_err().kind(),
        ErrorKind::Unsupported
    );
    assert_eq!(zip.read_link("b/c/symlink_to_a.txt").unwrap(), "a/a.txt");
    assert!(zip.get("../escape").is_err());
    assert!(
        ZipSlice::new(EXTERNAL_SYMLINK)
            .unwrap()
            .get("symlink_to_etc_passwd")
            .is_err()
    );
}

/// A deflate-compressed symlink target is decompressed (into a stack buffer) and checked.
#[cfg(feature = "deflate")]
#[test]
fn zip_slice_follows_compressed_symlinks() {
    let target = b"dir/a.txt".to_vec();
    let compressed = write_zip(|w| {
        w.start_file("t", options(CompressionMethod::Deflated))
            .unwrap();
        w.write_all(&target).unwrap();
    });
    let source = ZipSlice::new(&compressed).unwrap();
    let source_entry = source.entries().next().unwrap().unwrap();
    assert_eq!(source_entry.method(), Method::Deflate);
    let raw = source.raw_data(&source_entry).unwrap();

    let mut link = Hand::symlink(b"link", raw);
    link.method = 8;
    link.crc = Some(source_entry.crc32());
    link.uncompressed_size = Some(target.len() as u32);
    let mut corrupt = Hand::symlink(b"bad_crc", raw);
    corrupt.method = 8;
    corrupt.crc = Some(source_entry.crc32() ^ 1);
    corrupt.uncompressed_size = Some(target.len() as u32);
    let bytes = build(&[Hand::file(b"dir/a.txt", b"payload"), link, corrupt], b"");

    let zip = ZipSlice::new(&bytes).unwrap();
    assert_eq!(zip.get("link").unwrap(), b"payload");
    assert_eq!(zip.read_link("link").unwrap(), "dir/a.txt");
    assert_eq!(
        zip.get("bad_crc").unwrap_err().kind(),
        ErrorKind::InvalidData
    );

    let archive = ZipArchive::new(&bytes[..]).unwrap();
    assert_eq!(&*archive.read("link").unwrap(), b"payload");
    assert_eq!(
        archive.read("bad_crc").unwrap_err().kind(),
        ErrorKind::InvalidData
    );
}

#[test]
fn implicit_directories() {
    let zip = ZipArchive::new(NO_DIR_ENTRIES).unwrap();
    let meta = zip.metadata("images").unwrap();
    assert_eq!(meta.kind, EntryKind::Directory);
    assert_eq!(meta.method, None);
    assert_eq!(names(&zip.read_dir("").unwrap()), ["images", "readme.txt"]);
    assert_eq!(names(&zip.read_dir("images").unwrap()), ["sprite.png"]);
    assert_eq!(
        zip.read("images").unwrap_err().kind(),
        ErrorKind::IsADirectory
    );
    assert_eq!(
        zip.read_dir("readme.txt").unwrap_err().kind(),
        ErrorKind::NotADirectory
    );
    assert_eq!(
        zip.read_dir("nope").unwrap_err().kind(),
        ErrorKind::NotFound
    );
}

#[cfg(feature = "deflate")]
#[test]
fn symlinks_are_followed() {
    let zip = ZipArchive::new(SYMLINKS).unwrap();
    assert_eq!(zip.read_to_string("symlink_to_d.txt").unwrap(), "d");
    assert_eq!(zip.read_to_string("a/d.txt").unwrap(), "a");
    assert_eq!(zip.read_link("b/c/symlink_to_a.txt").unwrap(), "a/a.txt");
    assert_eq!(zip.read_link("/symlink_to_d.txt").unwrap(), "b/c/d.txt");
    assert_eq!(
        zip.symlink_metadata("a/b.txt").unwrap().kind,
        EntryKind::Symlink
    );
    assert_eq!(zip.metadata("a/b.txt").unwrap().kind, EntryKind::File);
    assert_eq!(
        names(&zip.read_dir("b").unwrap()),
        ["c", "symlink_to_d.txt"]
    );
    assert_eq!(
        names(&zip.read_dir("").unwrap()),
        ["a", "b", "symlink_to_d.txt"]
    );
}

#[cfg(not(feature = "deflate"))]
#[test]
fn deflate_archives_need_the_feature() {
    let err = ZipArchive::new(SYMLINKS).err().expect("must be rejected");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert!(err.to_string().contains("`deflate` feature"), "{err}");
}

#[test]
fn external_and_oversized_symlinks_are_rejected() {
    let zip = ZipArchive::new(EXTERNAL_SYMLINK).unwrap();
    assert!(zip.read_link("symlink_to_etc_passwd").is_err());
    assert!(zip.read("symlink_to_etc_passwd").is_err());

    let zip = ZipArchive::new(LONG_SYMLINK).unwrap();
    assert_eq!(
        zip.read_link("link").unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(zip.read("link").unwrap_err().kind(), ErrorKind::InvalidData);
    assert_eq!(zip.read_to_string("a").unwrap(), "target");
}

#[cfg(feature = "deflate")]
#[test]
fn output_is_capped_at_declared_size() {
    let zip = ZipArchive::new(SIZE_BOMB).unwrap();
    assert_eq!(
        zip.read("bomb.bin").unwrap_err().kind(),
        ErrorKind::InvalidData
    );

    let mut out = Vec::new();
    let err = zip
        .open("bomb.bin")
        .unwrap()
        .read_to_end(&mut out)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidData, "{err}");
    assert!(
        out.len() <= 16,
        "read {} bytes past the declared 16",
        out.len()
    );
}

#[test]
fn corrupt_local_header_is_an_error() {
    let zip = ZipArchive::new(CORRUPT_LOCAL_HEADER).unwrap();
    assert_eq!(
        zip.read("readme.txt").unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert!(zip.read("images/sprite.png").is_ok());
}

#[test]
fn iter_lists_entries_in_path_order() {
    let zip = ZipArchive::new(STORED).unwrap();
    let paths: Vec<_> = zip.iter().map(|(path, _)| path.to_string()).collect();
    assert_eq!(paths, ["images", "images/sprite.png", "readme.txt"]);
}

// ---------------------------------------------------------------------------------------------
// Robustness: corrupted input must produce errors, never panics or hangs.
// ---------------------------------------------------------------------------------------------

fn exercise(bytes: &[u8]) {
    if let Ok(slice) = ZipSlice::new(bytes) {
        for entry in slice.entries().flatten() {
            let _ = entry.name();
            let _ = entry.modified();
            let _ = slice.raw_data(&entry);
            if let Ok(name) = entry.name() {
                let _ = slice.get(&name);
                let _ = slice.read_link(&name);
            }
        }
    }
    if let Ok(zip) = ZipArchive::new(bytes) {
        let paths: Vec<String> = zip.iter().map(|(path, _)| path.to_string()).collect();
        for path in paths {
            let _ = zip.read(&path);
            let _ = zip.read_dir(&path);
            let _ = zip.metadata(&path);
            if let Ok(mut reader) = zip.open(&path) {
                let _ = io::copy(&mut reader, &mut io::sink());
            }
        }
        let _ = zip.read_dir("");
    }
}

#[test]
fn mutated_archives_do_not_panic() {
    let generated = write_zip(|w| {
        for method in supported_methods() {
            w.start_file(format!("dir/{method:?}.txt"), options(method))
                .unwrap();
            w.write_all(&b"some compressible text ".repeat(50)).unwrap();
        }
        w.add_symlink("link", "dir/Stored.txt", options(CompressionMethod::Stored))
            .unwrap();
    });
    let mut zip64 = Hand::file(b"z.bin", b"zip64 entry");
    zip64.zip64 = true;
    let handmade = build(&[zip64, Hand::file(b"a/b.txt", b"b")], b"stub");

    let mut rng = Rng(0xF00D);
    for base in [STORED, SYMLINKS, &generated[..], &handmade[..]] {
        for _ in 0..1500 {
            let mut bytes = base.to_vec();
            for _ in 0..1 + rng.next() % 4 {
                let at = (rng.next() as usize) % bytes.len();
                bytes[at] = rng.next() as u8;
            }
            exercise(&bytes);
        }
        for len in 0..base.len() {
            exercise(&base[..len]);
        }
    }
}
