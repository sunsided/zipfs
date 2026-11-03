# Fuzzing

Fuzz targets for the synchronous reader (`ZipArchive`, `ZipSlice`), using
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) and libFuzzer. They need a nightly
toolchain.

| Target       | Input                                                     | Checks |
|--------------|-----------------------------------------------------------|--------|
| `archive`    | Raw bytes into `ZipArchive`                               | `read` matches `metadata` and streaming `open`; borrowed slices point into the input; listed paths are normalized and portable; `read_dir` children exist |
| `slice`      | Raw bytes into `ZipSlice`                                 | Borrowed slices point into the input; `resolve` never returns a symlink; `ZipSlice` and `ZipArchive` agree where both succeed |
| `decompress` | Method, sizes, CRC-32 and data, wrapped in a valid archive | Output never exceeds the declared size, even on error; successful reads have the declared size and CRC-32 |

## Running

From the repository root:

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run archive fuzz/corpus/archive fuzz/seeds -- -dict=fuzz/zip.dict
cargo +nightly fuzz run slice fuzz/corpus/slice fuzz/seeds -- -dict=fuzz/zip.dict
cargo +nightly fuzz run decompress fuzz/corpus/decompress
```

Add `-max_total_time=<seconds>` to stop after a while. `fuzz/corpus/` collects the inputs
libFuzzer finds interesting and is not committed; `fuzz/seeds/` is the committed starting
point. CI fuzzes each target for 60 seconds on every pull request and push to `main`, for
30 minutes weekly, and for a chosen duration when the Fuzz workflow is started manually.

## Seeds

`fuzz/seeds/` holds small archives covering stored, Deflate and Zstandard entries, symlinks,
ZIP64 fields, Unicode names, comments and prepended data, plus the archives in `test_data/`.
Regenerate them (reproducibly) with:

```sh
cargo run --example fuzz_seeds
```

## Crashes

A failing input is written to `fuzz/artifacts/<target>/`; in CI it is uploaded as the
`fuzz-artifacts-<target>` artifact. To reproduce and inspect one:

```sh
cargo +nightly fuzz run <target> fuzz/artifacts/<target>/crash-<hash>
cargo +nightly fuzz fmt <target> fuzz/artifacts/<target>/crash-<hash>   # structured input
```

Turn a confirmed bug into a regression test in `tests/zip_archive.rs` before fixing it.
