[<img alt="github" src="https://img.shields.io/badge/github-vivekpanyam/zipfs-8da0cb?style=for-the-badge&logo=github" height="20">](https://github.com/vivekpanyam/zipfs)
[<img alt="crates.io" src="https://img.shields.io/crates/v/zipfs.svg?style=for-the-badge&logo=rust" height="20">](https://crates.io/crates/zipfs)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-zipfs-66c2a5?style=for-the-badge&logo=docs.rs" height="20">](https://docs.rs/zipfs)

An async filesystem on top of Zip files.

This library is built on top of [Lunchbox](https://github.com/vivekpanyam/lunchbox), an async virtual filesystem interface.

See the lunchbox docs for usage details.

# Getting started

Requires Rust 1.87 or newer.

```toml
[dependencies]
lunchbox = "0.1"
zipfs = "0.0.2"
```

## Step 1: Load a filesystem

```rust
use std::path::PathBuf;
use zipfs::ZipFS;

async fn load() -> std::io::Result<()> {
    let zipfs = ZipFS::new(PathBuf::from("/some/path/to/a/file.zip")).await?;

    // Use it like any other lunchbox filesystem. See the lunchbox docs for more details.
    Ok(())
}
```

# Synchronous, in-memory access

For archives that are already in memory, such as assets embedded with `include_bytes!`, zipfs
also has a synchronous API that needs no async runtime:

- `ZipArchive<D>` indexes an archive held in any `AsRef<[u8]>` (`&'static [u8]`, `Vec<u8>`,
  `Arc<[u8]>`, a memory map). Stored (uncompressed) files are returned borrowed, never copied;
  compressed files are decompressed in pure Rust with size and CRC-32 checks.
- `ZipSlice<'a>` is a view over `&'a [u8]` that allocates nothing: it walks the central
  directory in place and borrows stored files for the lifetime of the bytes.

```rust
use zipfs::{ZipArchive, ZipSlice};

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

# Compression features

| Feature   | Default | Enables                                                  |
|-----------|---------|----------------------------------------------------------|
| `deflate` | yes     | Reading Deflate entries (what most zip tools produce)    |
| `zstd`    | yes     | Reading Zstandard entries (builds the C `zstd` library)  |

The `zstd` feature compiles C code. On `wasm32-wasip1` this needs a WASI C toolchain
([wasi-sdk](https://github.com/WebAssembly/wasi-sdk)); without one, build with
`default-features = false, features = ["deflate"]`.

To use zip files as plain, uncompressed containers, disable the default features:

```toml
[dependencies]
zipfs = { version = "0.0.2", default-features = false }
```

Only Stored (uncompressed) entries can then be read, and no decompression code is compiled in.
`ZipFS::new` returns an `std::io::ErrorKind::Unsupported` error for any archive that contains an
entry using a compression method whose feature is disabled. Create compatible archives with
`zip -0 -r assets.zip assets/`.
