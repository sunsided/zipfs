// Copyright 2023 Vivek Panyam
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// The README's `ZipFS` example needs the `lunchbox` feature, so the README is the crate
// documentation (and its examples are tested) when that feature is enabled.
#![cfg_attr(feature = "lunchbox", doc = include_str!("../README.md"))]
#![cfg_attr(
    not(feature = "lunchbox"),
    doc = "Read zip archives held in memory, without copying stored files.\n\n\
           See [`ZipArchive`] for indexed access and [`ZipSlice`] for an allocation-free view. \
           Enable the `lunchbox` feature for `ZipFS`, an async [lunchbox] filesystem.\n\n\
           [lunchbox]: https://crates.io/crates/lunchbox"
)]

pub mod archive;
mod crc32;
pub mod format;
mod index;
mod paths;
mod reader;
#[cfg(feature = "lunchbox")]
mod vfs;

pub use archive::ZipArchive;
pub use format::{EntryKind, Method, ZipSlice};
pub use reader::EntryReader;
#[cfg(feature = "lunchbox")]
pub use vfs::{File, ZipFS, ZipReadDirPoller};
