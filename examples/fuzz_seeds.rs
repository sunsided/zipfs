//! Writes the seed corpus for the fuzz targets to `fuzz/seeds/`.
//!
//! ```sh
//! cargo run --example fuzz_seeds
//! ```
//!
//! The seeds are small archives covering the features the reader handles, written with the
//! `zip` crate (a dev-dependency), plus the archives in `test_data/`.

use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;

use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

fn options(method: CompressionMethod) -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(method)
        .unix_permissions(0o644)
        .last_modified_time(zip::DateTime::default())
}

fn write_zip(build: impl FnOnce(&mut zip::ZipWriter<Cursor<Vec<u8>>>)) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    build(&mut writer);
    writer.finish().expect("finish archive").into_inner()
}

fn main() -> std::io::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let seeds = root.join("fuzz/seeds");
    fs::create_dir_all(&seeds)?;

    let text = b"lorem ipsum dolor sit amet ".repeat(20);
    let mut seeds_written = Vec::new();
    let mut save = |name: &str, bytes: Vec<u8>| -> std::io::Result<()> {
        fs::write(seeds.join(name), bytes)?;
        seeds_written.push(name.to_string());
        Ok(())
    };

    for (name, method) in [
        ("stored.zip", CompressionMethod::Stored),
        ("deflate.zip", CompressionMethod::Deflated),
        ("zstd.zip", CompressionMethod::Zstd),
    ] {
        save(
            name,
            write_zip(|w| {
                w.add_directory("dir/", options(CompressionMethod::Stored))
                    .unwrap();
                w.start_file("dir/a.txt", options(method)).unwrap();
                w.write_all(&text).unwrap();
                w.start_file("empty.txt", options(method)).unwrap();
                w.add_symlink("link", "dir/a.txt", options(CompressionMethod::Stored))
                    .unwrap();
                w.add_symlink("dir/up", "../empty.txt", options(CompressionMethod::Stored))
                    .unwrap();
            }),
        )?;
    }

    save(
        "zip64.zip",
        write_zip(|w| {
            let options = options(CompressionMethod::Stored).large_file(true);
            w.start_file("big.bin", options).unwrap();
            w.write_all(b"zip64 extra fields").unwrap();
        }),
    )?;

    save(
        "unicode_comment.zip",
        write_zip(|w| {
            w.set_comment("archive comment");
            w.start_file("ünïcödé/✓.txt", options(CompressionMethod::Stored))
                .unwrap();
            w.write_all(b"unicode").unwrap();
            w.start_file(
                "implicit/dirs/only.txt",
                options(CompressionMethod::Deflated),
            )
            .unwrap();
            w.write_all(&text).unwrap();
        }),
    )?;

    let mut prepended = b"#!/bin/sh\nexit 0\n".to_vec();
    prepended.extend_from_slice(&write_zip(|w| {
        w.start_file("hello.txt", options(CompressionMethod::Stored))
            .unwrap();
        w.write_all(b"hello").unwrap();
    }));
    save("prepended.zip", prepended)?;

    for fixture in fs::read_dir(root.join("test_data"))? {
        let path = fixture?.path();
        if path.extension().is_some_and(|ext| ext == "zip") {
            let name = format!("fixture_{}", path.file_name().unwrap().to_string_lossy());
            save(&name, fs::read(&path)?)?;
        }
    }

    seeds_written.sort();
    println!("wrote {} seeds to {}", seeds_written.len(), seeds.display());
    for name in seeds_written {
        println!("  {name}");
    }
    Ok(())
}
