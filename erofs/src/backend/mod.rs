//! Backend abstraction layer for EROFS image sources.
//!
//! This module provides a unified interface for accessing EROFS image data
//! from different sources:
//!
//! - [`MmapImage`]: Memory-mapped files (requires `std` feature)
//! - [`SliceImage`]: Raw byte slices (available in `no_std` mode)
//!
//! The [`Image`] trait defines the common interface that all backend implementations
//! must implement.
//!
//! # Examples
//!
//! ## Using mmap backend (std)
//!
//! ```no_run
//! use erofs_rs::{EroFS, backend::MmapImage};
//!
//! # fn main() -> std::io::Result<()> {
//! let image = MmapImage::new_from_path("image.erofs")?;
//! let fs = EroFS::new(image)?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Using slice backend (no_std)
//!
//! ```
//! use erofs_rs::{EroFS, backend::SliceImage};
//!
//! let data: &[u8] = &[/* EROFS image data */];
//! let fs = EroFS::new(SliceImage::new(data)).unwrap();
//! ```

use binrw::io::Cursor;
use core::ops;

#[cfg(feature = "std")]
mod mmap;
#[cfg(feature = "std")]
pub use mmap::MmapImage;

mod slice;
pub use slice::SliceImage;

/// A trait for accessing EROFS image data from various sources.
///
/// This trait provides a common interface for reading data from different
/// backend types, enabling zero-copy access where possible.
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
