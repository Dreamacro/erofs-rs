//! A pure Rust library for reading EROFS (Enhanced Read-Only File System) images.
//!
//! EROFS is a read-only filesystem designed for performance and space efficiency,
//! commonly used in Android and other embedded systems.
//!
//! # Features
//!
//! - **no_std support**: Can be used in embedded systems with `alloc`
//! - **Zero-copy parsing**: Via mmap (std) or byte slices (no_std)
//! - **Multiple backends**: Memory-mapped files (std) or raw byte slices (no_std)
//! - **Multiple layouts**: Flat plain, flat inline, and chunk-based data layouts
//!
//! # Examples
//!
//! ## Standard usage (with std)
//!
//! ```no_run
//! use std::io::Read;
//! use erofs_rs::{EroFS, backend::MmapImage};
//!
//! let image = MmapImage::new_from_path("image.erofs").unwrap();
//! let fs = EroFS::new(image).unwrap();
//!
//! // Read a file
//! let mut file = fs.open("/etc/passwd").unwrap();
//! let mut content = String::new();
//! file.read_to_string(&mut content).unwrap();
//! ```
//!
//! ## no_std usage (with alloc)
//!
//! ```no_run
//! # extern crate alloc;
//! use erofs_rs::{EroFS, backend::SliceImage};
//!
//! // Assuming you have the EROFS image data in memory
//! let image_data: &'static [u8] = &[/* ... */];
//! let fs = EroFS::new(SliceImage::new(image_data)).unwrap();
//!
//! // List directory entries
//! for entry in fs.read_dir("/etc").unwrap() {
//!     let entry = entry.unwrap();
//!     // Process directory entry...
//! }
//! ```
#![no_std]

#[macro_use]
extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

pub(crate) mod dirent;
pub(crate) mod filesystem;
pub(crate) mod decomp;

pub mod r#async;
pub mod backend;
mod error;
pub mod sync;
pub mod types;

pub use dirent::DirEntry;
pub use error::*;
pub use sync::{EroFS, ReadDir, WalkDir, WalkDirEntry};
