//! A pure Rust library for reading EROFS (Enhanced Read-Only File System) images.
//!
//! EROFS is a read-only filesystem designed for performance and space efficiency,
//! commonly used in Android and other embedded systems.
//!
//! # Example
//!
//! ```no_run
//! use std::fs::File;
//! use std::io::Read;
//! use memmap2::Mmap;
//! use erofs_rs::EroFS;
//!
//! let file = File::open("image.erofs").unwrap();
//! let mmap = unsafe { Mmap::map(&file) }.unwrap();
//! let fs = EroFS::new(mmap).unwrap();
//!
//! // Read a file
//! let mut file = fs.open("/etc/passwd").unwrap();
//! let mut content = String::new();
//! file.read_to_string(&mut content).unwrap();
//! ```
#![no_std]

#[macro_use]
extern crate alloc;
extern crate std;

pub mod backend;
mod dirent;
mod error;
pub mod file;
pub mod filesystem;
pub mod types;
pub mod walkdir;

pub use dirent::{DirEntry, ReadDir};
pub use error::*;
pub use filesystem::EroFS;
pub use walkdir::{WalkDir, WalkDirEntry};
