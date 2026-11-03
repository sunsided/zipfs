[<img alt="github" src="https://img.shields.io/badge/github-sunsided/zipfs-8da0cb?style=for-the-badge&logo=github" height="20">](https://github.com/sunsided/zipfs)
[<img alt="crates.io" src="https://img.shields.io/crates/v/zipfs-ng.svg?style=for-the-badge&logo=rust" height="20">](https://crates.io/crates/zipfs-ng)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-zipfs--ng-66c2a5?style=for-the-badge&logo=docs.rs" height="20">](https://docs.rs/zipfs-ng)

Read zip archives held in memory, such as assets embedded with `include_bytes!`, without
copying stored (uncompressed) files.

zipfs-ng continues [zipfs](https://crates.io/crates/zipfs) 0.0.2 by Vivek Panyam, rewritten
around its own zip reader. Import it as `zipfs_ng`; see the [changelog](CHANGELOG.md) for what
changed.

- `ZipArchive<D>` indexes an archive held in any `AsRef<[u8]>` (`&'static [u8]`, `Vec<u8>`,
  `Arc<[u8]>`, a memory map). Stored files are returned borrowed, never copied; compressed files
  are decompressed in pure Rust with size and CRC-32 checks.
- `ZipSlice<'a>` is a view over `&'a [u8]` that allocates nothing: it walks the central
  directory in place and borrows stored files for the lifetime of the bytes.
- `ZipFS<D>` (feature `lunchbox`) is an async, read-only
  [Lunchbox](https://github.com/vivekpanyam/lunchbox) filesystem over the same archive.
  `ZipFS<Source<S>>` is the same filesystem over an archive that is not in memory: it reads
  from any `AsyncSource` (a file on disk, an HTTP range client, an object store) and fetches
  only the central directory up front.

# Getting started

Requires Rust 1.87 or newer.

```toml
[dependencies]
zipfs-ng = "0.1"
```

```rust
use zipfs_ng::{ZipArchive, ZipSlice};

fn load(assets: &'static [u8]) -> std::io::Result<()> {
    // Indexed lookups; `read` borrows stored files and decompresses the rest.
    let zip = ZipArchive::new(assets)?;
    let config = zip.read_to_string("config/game.json")?;
    let sprite: &'static [u8] = zip.read_borrowed("images/sprite.png")?;
    for entry in zip.read_dir("images")? {
        println!("{} ({:?})", entry.name, entry.kind);
    }

    // No allocation at all; stored files only.
    let slice = ZipSlice::new(assets)?;
    let music: &'static [u8] = slice.get("audio/theme.ogg")?;
    Ok(())
}
```

Both follow symlinks in the final path component. Pack assets with already-compressed formats
(PNG, OGG, KTX2) stored, e.g. `zip -r -n .png:.ogg:.ktx2 assets.zip assets/`, to get them without
copying.

## As a Lunchbox filesystem

```toml
[dependencies]
lunchbox = "0.1"
zipfs-ng = { version = "0.1", features = ["lunchbox"] }
```

```rust
use std::sync::Arc;

use lunchbox::ReadableFileSystem;
use zipfs_ng::ZipFS;

async fn load(assets: &'static [u8]) -> std::io::Result<()> {
    // Embedded bytes, or any cheaply clonable buffer such as `Arc<[u8]>`.
    let fs = ZipFS::new(assets)?;
    let config = fs.read_to_string("config/game.json").await?;

    // An archive file on disk, read into memory (needs a tokio runtime).
    let fs: ZipFS<Arc<[u8]>> = ZipFS::from_file("assets.zip").await?;

    // Use it like any other lunchbox filesystem. See the lunchbox docs for more details.
    Ok(())
}
```

To read an archive on demand instead of loading it, open it from an `AsyncSource`.
`FileSource` reads a local file with positional reads; implement `AsyncSource` (`size` and
`read_at`) for anything else that serves byte ranges:

```rust
use lunchbox::ReadableFileSystem;
use zipfs_ng::{FileSource, Source, ZipFS};

async fn load() -> std::io::Result<()> {
    // Fetches the end records and the central directory; files are fetched when read.
    let fs: ZipFS<Source<FileSource>> =
        ZipFS::from_source(FileSource::open("assets.zip").await?).await?;
    let sprite = fs.read("images/sprite.png").await?;
    Ok(())
}
```

Stored files are fetched in 64 KiB chunks as they are read. Compressed files are fetched whole
(compressed) on their first read and decompressed incrementally, so keep very large files
stored when serving from a remote source.

# Features

| Feature     | Default | Enables                                                              |
|-------------|---------|----------------------------------------------------------------------|
| `deflate`   | yes     | Reading Deflate entries (what most zip tools produce), via `miniz_oxide` |
| `zstd`      | yes     | Reading Zstandard entries, via the pure-Rust `ruzstd`                |
| `crc32fast` | yes     | Hardware-accelerated CRC-32 via `crc32fast`; otherwise a built-in implementation is used |
| `full-symlinks` | no  | Follow symlinks in every path component (`linked_dir/file.png`), resolving `..` after links as POSIX does (entries stored below a symlinked directory are then shadowed by the link); by default only the last component is followed |
| `lunchbox`  | no      | `ZipFS`, an async Lunchbox filesystem, and `SourceArchive` for archives read on demand from an `AsyncSource` (adds `lunchbox`, `async-trait` and `tokio`) |

Everything is pure Rust and builds for WebAssembly, including `wasm32-wasip1`.

To use zip files as plain, uncompressed containers, disable the default features:

```toml
[dependencies]
zipfs-ng = { version = "0.1", default-features = false }
```

Only stored (uncompressed) entries can then be read, no decompression code is compiled in, and
zipfs-ng has no dependencies. Opening an archive that contains an entry using a compression method
whose feature is disabled fails with `std::io::ErrorKind::Unsupported`. Create compatible
archives with `zip -0 -r assets.zip assets/`.
