//! `ZipFS` files decompress incrementally: reading part of a large compressed file must not
//! materialize the whole file, and seeking must work anywhere in it.
//!
//! This test binary installs a counting global allocator to measure peak heap usage, so it
//! holds a single test function that runs its checks in sequence.

#![cfg(all(feature = "lunchbox", feature = "deflate", feature = "zstd"))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Cursor, SeekFrom, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

use lunchbox::ReadableFileSystem;
use lunchbox::types::ReadableFile;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;
use zipfs_ng::ZipFS;

struct CountingAllocator;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Peak heap growth while running `f`.
fn peak_growth<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let baseline = CURRENT.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let result = f();
    (result, PEAK.load(Ordering::Relaxed) - baseline)
}

const BIG: usize = 32 * 1024 * 1024;

fn archive() -> Vec<u8> {
    // Compressible but not trivially so, so offsets are distinguishable.
    let text: Vec<u8> = (0..1024 * 1024u32)
        .flat_map(|i| format!("{i:08}\n").into_bytes())
        .take(1024 * 1024)
        .collect();
    let zeros = vec![0u8; BIG];
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, method, data) in [
        ("text.deflate", CompressionMethod::Deflated, &text),
        ("text.zstd", CompressionMethod::Zstd, &text),
        ("zeros.deflate", CompressionMethod::Deflated, &zeros),
        ("zeros.zstd", CompressionMethod::Zstd, &zeros),
    ] {
        let options = SimpleFileOptions::default().compression_method(method);
        writer.start_file(name, options).unwrap();
        writer.write_all(data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn compressed_files_stream_and_seek() {
    let bytes: &'static [u8] = archive().leak();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let fs = ZipFS::new(bytes).unwrap();

    runtime.block_on(async {
        // Seeking anywhere returns the right bytes, for both codecs.
        for name in ["text.deflate", "text.zstd"] {
            let expected = fs.archive().read(name).unwrap().into_owned();
            let mut file = fs.open(name).await.unwrap();
            let mut chunk = vec![0u8; 100];

            for offset in [500_000u64, 10, 999_000, 0, 123_456] {
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
            assert_eq!(
                file.read(&mut chunk).await.unwrap(),
                0,
                "{name} past the end"
            );

            // Clones share the position and the decoder.
            file.seek(SeekFrom::Start(1000)).await.unwrap();
            let mut clone = file.try_clone().await.unwrap();
            clone.read_exact(&mut chunk).await.unwrap();
            assert_eq!(chunk, expected[1000..1100]);
            file.read_exact(&mut chunk).await.unwrap();
            assert_eq!(chunk, expected[1100..1200]);
        }

        // Reading the start of a 32 MiB compressed file stays far below its size.
        for name in ["zeros.deflate", "zeros.zstd"] {
            let (head, growth) = peak_growth(|| {
                futures_lite_block_on(async {
                    let mut file = fs.open(name).await.unwrap();
                    let mut head = [1u8; 16];
                    file.read_exact(&mut head).await.unwrap();
                    head
                })
            });
            assert_eq!(head, [0u8; 16]);
            assert!(
                growth < BIG / 8,
                "{name}: reading 16 bytes grew the heap by {growth} bytes"
            );
        }
    });
}

/// Polls a future to completion on the current thread; the reads never wait, so a no-op waker
/// is enough and the measurement does not include an executor's allocations.
fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
    }
}
