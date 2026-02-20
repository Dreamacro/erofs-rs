//! Backend abstraction layer for EROFS image sources.
//!
//! This module provides a unified interface for accessing EROFS image data
//! from different sources:
//!
//! - [`MmapImage`]: Memory-mapped files (requires `std` feature)
//! - [`SliceImage`]: Raw byte slices (available in `no_std` mode)
//!
//! The [`Image`] trait defines the common interface that all backends must implement,
//! and the [`Backend`] enum uses `enum_dispatch` for efficient dynamic dispatch.
//!
//! # Examples
//!
//! ## Using mmap backend (std)
//!
//! ```no_run
//! use erofs_rs::backend::MmapImage;
//!
//! # fn main() -> std::io::Result<()> {
//! let image = MmapImage::new_from_path("image.erofs")?;
//! let backend = image.into();
//! # Ok(())
//! # }
//! ```
//!
//! ## Using slice backend (no_std)
//!
//! ```
//! use erofs_rs::backend::{Backend, SliceImage};
//!
//! let data: &[u8] = &[/* EROFS image data */];
//! let backend = Backend::Slice(SliceImage::new(data));
//! ```

use binrw::io::Cursor;
use enum_dispatch::*;

#[cfg(feature = "std")]
use std::ops;
#[cfg(feature = "std")]
mod mmap;
#[cfg(feature = "std")]
pub use mmap::MmapImage;

#[cfg(not(feature = "std"))]
use core::ops;

mod slice;
pub use slice::SliceImage;

/// A unified backend enum for different EROFS image sources.
///
/// This enum uses `enum_dispatch` to provide zero-cost abstraction over
/// different backend implementations.
#[enum_dispatch(Image)]
#[derive(Debug)]
pub enum Backend<'a> {
    /// Memory-mapped file backend (requires `std` feature)
    #[cfg(feature = "std")]
    Mmap(mmap::MmapImage),
    /// Byte slice backend (available in `no_std` mode)
    Slice(slice::SliceImage<'a>),
}

/// A trait for accessing EROFS image data from various sources.
///
/// This trait provides a common interface for reading data from different
/// backend types, enabling zero-copy access where possible.
#[enum_dispatch]
pub trait Image {
    /// Gets a slice of data at the specified range.
    ///
    /// Returns `None` if the range is out of bounds.
    ///
    /// # Examples
    ///
    /// ```
    /// use erofs_rs::backend::{Image, SliceImage};
    ///
    /// let data = b"Hello, world!";
    /// let image = SliceImage::new(data);
    /// assert_eq!(image.get(0..5), Some(&b"Hello"[..]));
    /// assert_eq!(image.get(100..), None);
    /// ```
    fn get<R: ops::RangeBounds<usize>>(&self, range: R) -> Option<&[u8]>;

    /// Gets a cursor for reading data starting at the specified offset.
    ///
    /// This is a convenience method for creating a `Cursor` that can be used
    /// with binary parsing libraries like `binrw`.
    fn get_cursor(&self, offset: usize) -> Option<Cursor<&[u8]>> {
        self.get(offset..).map(Cursor::new)
    }

    /// Returns the total length of the image in bytes.
    fn len(&self) -> u64;

    /// Returns `true` if the image is empty (length is 0).
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
