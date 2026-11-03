//! `ZipFS<Source<S>>` behaves like the in-memory `ZipFS<D>` while fetching only what it needs.

#![cfg(all(feature = "lunchbox", feature = "deflate", feature = "zstd"))]

use std::io::{self, Cursor, SeekFrom, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lunchbox::ReadableFileSystem;
use lunchbox::types::ReadableFile;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;
use zipfs_ng::{AsyncSource, Source, ZipFS};

/// An archive in memory behind the async source interface, counting what is fetched. Every
/// read yields to the executor once before completing, so callers must handle pending reads.
struct MemorySource {
    data: Arc<[u8]>,
    fetched: Arc<AtomicU64>,
    /// Bytes to drop from the end of every read, to simulate a misbehaving source.
    short_by: usize,
}

impl MemorySource {
    fn new(data: &[u8]) -> Self {
        Self {
            data: Arc::from(data),
            fetched: Arc::default(),
            short_by: 0,
        }
    }
}

#[async_trait::async_trait]
impl AsyncSource for MemorySource {
    async fn size(&self) -> io::Result<u64> {
        Ok(self.data.len() as u64)
    }

    async fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        tokio::task::yield_now().await;
        let start = usize::try_from(offset).unwrap();
        let bytes = self
            .data
            .get(start..start + len)
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        self.fetched.fetch_add(len as u64, Ordering::Relaxed);
        Ok(bytes[..len.saturating_sub(self.short_by)].to_vec())
    }
}

static STORED: &[u8] = include_bytes!("../test_data/stored_only.zip");
static SYMLINKS: &[u8] = include_bytes!("../test_data/symlink_test.zip");
static EXTERNAL_SYMLINK: &[u8] = include_bytes!("../test_data/external_symlink.zip");
static CORRUPT_LOCAL_HEADER: &[u8] = include_bytes!("../test_data/corrupt_local_header.zip");

const BIG: usize = 300 * 1024;

/// Stored, Deflate and Zstd files, some larger than a fetch chunk, in nested directories.
fn mixed_archive() -> Vec<u8> {
    let text: Vec<u8> = (0..BIG as u32)
        .flat_map(|i| format!("{i:08}\n").into_bytes())
        .take(BIG)
        .collect();
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, method, data) in [
        ("big/stored.txt", CompressionMethod::Stored, &text[..]),
        ("big/deflate.txt", CompressionMethod::Deflated, &text[..]),
        ("big/zstd.txt", CompressionMethod::Zstd, &text[..]),
        ("small/a.txt", CompressionMethod::Stored, b"a"),
        ("small/empty.txt", CompressionMethod::Stored, b""),
        ("small/b.txt", CompressionMethod::Deflated, b"bbbb"),
    ] {
        let options = SimpleFileOptions::default().compression_method(method);
        writer.start_file(name, options).unwrap();
        writer.write_all(data).unwrap();
    }
    writer
        .add_directory("empty_dir", SimpleFileOptions::default())
        .unwrap();
    writer.finish().unwrap().into_inner()
}

async fn read_in_small_pieces<F: ReadableFile + Unpin>(file: &mut F) -> Vec<u8> {
    let mut contents = Vec::new();
    let mut piece = [0u8; 1000];
    loop {
        let read = file.read(&mut piece).await.unwrap();
        if read == 0 {
            return contents;
        }
        contents.extend_from_slice(&piece[..read]);
    }
}

async fn dir_names<B>(fs: &ZipFS<B>, path: &str) -> io::Result<Vec<String>>
where
    B: zipfs_ng::Backend,
    ZipFS<B>: ReadableFileSystem<ReadDirPollerType = zipfs_ng::ZipReadDirPoller>,
    <ZipFS<B> as lunchbox::types::HasFileType>::FileType: ReadableFile,
{
    let mut entries = fs.read_dir(path).await?;
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        names.push(entry.path().to_string());
    }
    Ok(names)
}

fn kind(result: io::Result<impl Sized>) -> Result<(), io::ErrorKind> {
    result.map(drop).map_err(|error| error.kind())
}

/// Every lookup and read gives the same answer from memory and from the source.
async fn assert_parity(bytes: &'static [u8]) {
    let memory = ZipFS::new(bytes).unwrap();
    let source: ZipFS<Source<MemorySource>> =
        ZipFS::from_source(MemorySource::new(bytes)).await.unwrap();

    let mut paths: Vec<String> = memory
        .archive()
        .iter()
        .map(|(path, _)| path.to_string())
        .collect();
    let source_paths: Vec<String> = source
        .archive()
        .iter()
        .map(|(path, _)| path.to_string())
        .collect();
    assert_eq!(paths, source_paths);
    paths.extend(["", "missing", "missing/file"].map(String::from));

    for path in &paths {
        let path = path.as_str();
        assert_eq!(
            memory.archive().metadata(path).ok(),
            source.archive().metadata(path).ok(),
            "{path}"
        );
        assert_eq!(
            memory.archive().symlink_metadata(path).ok(),
            source.archive().symlink_metadata(path).ok(),
            "{path}"
        );
        assert_eq!(
            memory.archive().read_link(path).ok(),
            source.archive().read_link(path).ok(),
            "{path}"
        );
        assert_eq!(
            memory.archive().canonicalize(path).ok(),
            source.archive().canonicalize(path).ok(),
            "{path}"
        );
        assert_eq!(
            dir_names(&memory, path).await.ok(),
            dir_names(&source, path).await.ok(),
            "{path}"
        );

        let expected = memory.read(path).await;
        let actual = source.read(path).await;
        match (&expected, &actual) {
            (Ok(expected), Ok(actual)) => assert_eq!(expected, actual, "{path}"),
            _ => assert_eq!(kind(expected), kind(actual), "{path}"),
        }

        match (memory.open(path).await, source.open(path).await) {
            (Ok(mut expected), Ok(mut actual)) => {
                let expected = read_in_small_pieces(&mut expected).await;
                let actual = read_in_small_pieces(&mut actual).await;
                assert_eq!(expected, actual, "{path}");
            }
            (expected, actual) => assert_eq!(kind(expected), kind(actual), "{path}"),
        }
    }
}

#[tokio::test]
async fn matches_the_in_memory_filesystem() {
    assert_parity(STORED).await;
    assert_parity(SYMLINKS).await;
    assert_parity(EXTERNAL_SYMLINK).await;
    assert_parity(CORRUPT_LOCAL_HEADER).await;
    assert_parity(mixed_archive().leak()).await;

    // Data prepended to the archive, as in a self-extracting executable.
    let mut prefixed = b"#!/bin/sh\nexit 0\n".to_vec();
    prefixed.extend(mixed_archive());
    assert_parity(prefixed.leak()).await;
}

#[tokio::test]
async fn opening_fetches_only_the_directory() {
    let bytes = mixed_archive();
    let source = MemorySource::new(&bytes);
    let fetched = Arc::clone(&source.fetched);
    let fs = ZipFS::from_source(source).await.unwrap();
    let opened = fetched.load(Ordering::Relaxed);
    assert!(
        opened < bytes.len() as u64 / 4,
        "opening fetched {opened} of {} bytes",
        bytes.len()
    );

    // Reading a small file fetches its header and contents, not its neighbours.
    assert_eq!(fs.read("small/a.txt").await.unwrap(), b"a");
    assert!(fetched.load(Ordering::Relaxed) - opened < 1024);

    // Reading the start of a large stored file fetches one chunk.
    let before = fetched.load(Ordering::Relaxed);
    let mut file = fs.open("big/stored.txt").await.unwrap();
    let mut head = [0u8; 9];
    file.read_exact(&mut head).await.unwrap();
    assert_eq!(&head, b"00000000\n");
    let used = fetched.load(Ordering::Relaxed) - before;
    assert!(used <= 64 * 1024 + 1024, "fetched {used} bytes for 9");
}

#[tokio::test]
async fn files_seek_across_chunks_and_share_state() {
    let bytes: &'static [u8] = mixed_archive().leak();
    let memory = ZipFS::new(bytes).unwrap();
    let fs = ZipFS::from_source(MemorySource::new(bytes)).await.unwrap();

    for name in ["big/stored.txt", "big/deflate.txt", "big/zstd.txt"] {
        let expected = memory.archive().read(name).unwrap().into_owned();
        let mut file = fs.open(name).await.unwrap();
        let mut chunk = vec![0u8; 100];
        // Around the 64 KiB fetch boundaries, backwards and forwards.
        for offset in [65_500u64, 10, 131_050, 0, 200_000, 65_536] {
            file.seek(SeekFrom::Start(offset)).await.unwrap();
            file.read_exact(&mut chunk).await.unwrap();
            let start = offset as usize;
            assert_eq!(chunk, expected[start..start + 100], "{name} at {offset}");
        }

        let end = file.seek(SeekFrom::End(-50)).await.unwrap();
        let mut tail = Vec::new();
        file.read_to_end(&mut tail).await.unwrap();
        assert_eq!(tail, expected[end as usize..], "{name} tail");

        file.seek(SeekFrom::End(10)).await.unwrap();
        assert_eq!(file.read(&mut chunk).await.unwrap(), 0, "{name} past end");
        assert!(file.seek(SeekFrom::Current(-1_000_000)).await.is_err());

        // Clones share the position.
        file.seek(SeekFrom::Start(1000)).await.unwrap();
        let mut clone = file.try_clone().await.unwrap();
        clone.read_exact(&mut chunk).await.unwrap();
        assert_eq!(chunk, expected[1000..1100]);
        file.read_exact(&mut chunk).await.unwrap();
        assert_eq!(chunk, expected[1100..1200]);
        assert_eq!(file.metadata().await.unwrap().len(), expected.len() as u64);
    }
}

#[tokio::test]
async fn short_reads_from_the_source_are_errors() {
    let mut source = MemorySource::new(STORED);
    source.short_by = 1;
    let error = ZipFS::from_source(source).await.err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[tokio::test]
async fn invalid_archives_are_rejected() {
    let error = ZipFS::from_source(MemorySource::new(b"not a zip archive"))
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    let error = ZipFS::from_source(MemorySource::new(b""))
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn file_source_reads_from_disk() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data/symlink_test.zip");
    let fs = ZipFS::from_source(zipfs_ng::FileSource::open(path).await.unwrap())
        .await
        .unwrap();
    assert_eq!(fs.read_to_string("b/symlink_to_d.txt").await.unwrap(), "d");
    let mut contents = String::new();
    fs.open("a/b.txt")
        .await
        .unwrap()
        .read_to_string(&mut contents)
        .await
        .unwrap();
    assert_eq!(contents, "a");

    let missing = zipfs_ng::FileSource::open("/definitely/not/here.zip").await;
    assert_eq!(missing.err().unwrap().kind(), io::ErrorKind::NotFound);
}
