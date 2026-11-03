# Changelog

All notable changes to this project are documented in this file. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/).

## 0.1.0 - Unreleased

A rewrite around an in-house zip reader. `ZipFS` is still the crate's Lunchbox filesystem, but
it is now optional and built on synchronous, zero-copy archive types that work without any
async runtime.

### Added

- `ZipArchive<D>`: indexed, synchronous access to an archive held in any `AsRef<[u8]>`
  (`&'static [u8]` from `include_bytes!`, `Vec<u8>`, `Arc<[u8]>`, a memory map). Stored files
  are returned borrowed; `read_borrowed` on `ZipArchive<&'a [u8]>` returns them with the
  lifetime of the bytes. Also `open` (streaming `EntryReader`), `metadata`,
  `symlink_metadata`, `read_link`, `canonicalize`, `read_dir`, `iter` and `verify_all`.
- `ZipSlice<'a>`: an allocation-free view over `&'a [u8]` that walks the central directory in
  place, for stored files.
- `ZipFS<Source<S>>`, `SourceArchive<S>` and the `AsyncSource` trait: read an archive on
  demand from a file, an HTTP range client or an object store, fetching only the central
  directory up front. `FileSource` reads local files with positional reads.
- Opt-in CRC-32 checks for stored files on every read
  (`ZipArchive::with_verified_stored_reads`).
- `full-symlinks` feature: follow symlinks in every path component, with POSIX `..`
  semantics; by default only the final component is followed.
- `deflate`, `zstd` and `crc32fast` features (all default). Without them, only stored entries
  can be read, archives with other methods are rejected with `ErrorKind::Unsupported`, and the
  crate has no dependencies.
- ZIP64 archives, archives with prepended data (such as self-extracting executables), UTF-8
  and CP437 names, the Info-ZIP Unicode path field, and Unix modes and timestamps.
- `ZipArchive::with_one_shot_limit` to bound how large a file `read` decompresses in a single
  allocation.
- Fuzz targets for the reader and a CI workflow that runs them.

### Changed

- **Breaking:** `ZipFS` and the Lunchbox integration are behind the `lunchbox` feature, which
  is not enabled by default.
- **Breaking:** `ZipFS<B>` is generic over a sealed `Backend`: `ZipFS<D>` for archives in
  memory (as before) and `ZipFS<Source<S>>` for async sources. `ZipFS::new` takes the archive
  bytes directly and indexes them synchronously; `ZipFS::from_file` reads a file into an
  `Arc<[u8]>`.
- **Breaking:** requires Rust 1.87 and uses edition 2024.
- `ZipFS` files decompress incrementally and support seeking, instead of decompressing the
  whole file on open. Clones made with `try_clone` share the read position.
- Decompression is pure Rust (`miniz_oxide`, `ruzstd`); `async_zip` and its dependencies are
  gone, and `tokio` is only used by the `lunchbox` feature.

### Security

- Entry names must be portable relative paths: names with NUL bytes, drive letters or
  components that escape the archive root are rejected, and backslashes are treated as
  separators.
- Symlink targets are limited to 4 KiB and may not leave the archive; symlink chains are
  limited to 40 hops and path walks to a bounded number of components.
- Decompressed output is capped at the declared size and checked against the CRC-32;
  declared sizes beyond what the compressed data can produce are not allocated up front.
  Truncated or trailing-garbage Deflate streams are rejected.
- Encrypted entries and multi-disk archives are rejected with `ErrorKind::Unsupported`.

## [0.0.2]

Last release of the original crate, an async filesystem over `async_zip`.

[0.0.2]: https://crates.io/crates/zipfs/0.0.2
