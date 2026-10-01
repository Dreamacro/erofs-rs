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
//! # #[cfg(feature = "std")]
//! # {
//! use erofs_rs::{EroFS, backend::MmapImage};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // SAFETY: assume the file remains immutable until the filesystem is dropped.
//! let image = unsafe { MmapImage::new_from_path("image.erofs")? };
//! let fs = EroFS::new(image)?;
//! # Ok(())
//! # }
//! # }
//! ```
//!
//! ## Using slice backend (no_std)
//!
//! ```no_run
//! use erofs_rs::{EroFS, backend::SliceImage};
//!
//! let data: &[u8] = &[/* EROFS image data */];
//! let fs = EroFS::new(SliceImage::new(data)).unwrap();
//! ```

use binrw::io::Cursor;
use core::{future::Future, ops};

use super::Result;

#[cfg(feature = "std")]
mod mmap;
#[cfg(feature = "std")]
pub use mmap::MmapImage;

#[cfg(all(feature = "std", feature = "opendal"))]
mod opendal;
#[cfg(all(feature = "std", feature = "opendal"))]
pub use opendal::OpendalImage;

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

/// A trait for asynchronously accessing EROFS image data from various sources.
///
/// This trait provides an async interface for reading data from different
/// backend types. Implementations should offload blocking I/O to appropriate
/// thread pools to avoid blocking async runtimes.
///
/// # Examples
///
/// ```no_run
/// use erofs_rs::backend::AsyncImage;
/// use erofs_rs::{Error, Result};
///
/// struct MyAsyncImage(Vec<u8>);
///
/// impl AsyncImage for MyAsyncImage {
///     async fn read_exact_at(&self, buf: &mut [u8], offset: usize) -> Result<()> {
///         let data = self.0.get(offset..)
///             .and_then(|data| data.get(..buf.len()))
///             .ok_or_else(|| Error::OutOfBounds("read exceeds image".into()))?;
///         buf.copy_from_slice(data);
///         Ok(())
///     }
/// }
/// ```
pub trait AsyncImage: Send + Sync {
    /// Asynchronously fills `buf` from the image at a specific offset.
    ///
    /// # Arguments
    ///
    /// * `buf` - The buffer to read data into
    /// * `offset` - The byte offset in the image to start reading from
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` only after all `buf.len()` bytes have been read.
    /// Implementations must not report a short read as success.
    ///
    /// # Errors
    ///
    /// Returns an error if the read operation fails or the image ends before
    /// `buf` is filled. On error, `buf` may have been partially modified.
    fn read_exact_at(
        &self,
        buf: &mut [u8],
        offset: usize,
    ) -> impl Future<Output = Result<()>> + Send;
}
