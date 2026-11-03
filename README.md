[<img alt="github" src="https://img.shields.io/badge/github-vivekpanyam/zipfs-8da0cb?style=for-the-badge&logo=github" height="20">](https://github.com/vivekpanyam/zipfs)
[<img alt="crates.io" src="https://img.shields.io/crates/v/zipfs.svg?style=for-the-badge&logo=rust" height="20">](https://crates.io/crates/zipfs)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-zipfs-66c2a5?style=for-the-badge&logo=docs.rs" height="20">](https://docs.rs/zipfs)

An async filesystem on top of Zip files.

This library is built on top of [Lunchbox](https://github.com/vivekpanyam/lunchbox), an async virtual filesystem interface.

See the lunchbox docs for usage details.

# Getting started

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

# Compression features

| Feature   | Default | Enables                                                  |
|-----------|---------|----------------------------------------------------------|
| `deflate` | yes     | Reading Deflate entries (what most zip tools produce)    |
| `zstd`    | yes     | Reading Zstandard entries (builds the C `zstd` library)  |

To use zip files as plain, uncompressed containers, disable the default features:

```toml
[dependencies]
zipfs = { version = "0.0.2", default-features = false }
```

Only Stored (uncompressed) entries can then be read, and no decompression code is compiled in.
`ZipFS::new` returns an `std::io::ErrorKind::Unsupported` error for any archive that contains an
entry using a compression method whose feature is disabled. Create compatible archives with
`zip -0 -r assets.zip assets/`.
