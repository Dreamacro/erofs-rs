use std::{
    fs, io,
    ops::{Bound, RangeBounds},
    path,
};

use memmap2::Mmap;

use super::Image;

/// A memory-mapped file backend for EROFS images.
///
/// This backend uses `memmap2` to create a memory-mapped view of a file,
/// providing efficient zero-copy access to the image data. Available only
/// when the `std` feature is enabled.
///
/// # Safety
///
/// Memory mapping is inherently unsafe because the file can be modified by
/// external processes. Ensure that the underlying file is not modified while
/// the mapping is active.
///
/// # Examples
///
/// ```no_run
/// use std::fs::File;
/// use memmap2::Mmap;
/// use erofs_rs::backend::MmapImage;
///
/// # fn main() -> std::io::Result<()> {
/// let file = File::open("image.erofs")?;
/// let mmap = unsafe { Mmap::map(&file)? };
/// let image = MmapImage::new(mmap);
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct MmapImage(Mmap);

impl Image for MmapImage {
    fn get<R: RangeBounds<usize>>(&self, range: R) -> Option<&[u8]> {
        let start = match range.start_bound() {
            Bound::Included(&s) => s,
            Bound::Excluded(&s) => s + 1,
            Bound::Unbounded => 0,
        };

        let end = match range.end_bound() {
            Bound::Included(&e) => e + 1,
            Bound::Excluded(&e) => e,
            Bound::Unbounded => self.0.len(),
        };
        self.0.get(start..end)
    }

    fn len(&self) -> u64 {
        self.0.len() as u64
    }
}

impl MmapImage {
    /// Creates a new `MmapImage` from an existing memory map.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::fs::File;
    /// use memmap2::Mmap;
    /// use erofs_rs::backend::MmapImage;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let file = File::open("image.erofs")?;
    /// let mmap = unsafe { Mmap::map(&file)? };
    /// let image = MmapImage::new(mmap);
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(mmap: Mmap) -> Self {
        Self(mmap)
    }

    /// Creates a new `MmapImage` by memory-mapping the given file.
    ///
    /// # Safety
    ///
    /// This function creates a memory-mapped view of the file. The caller must
    /// ensure that the file is not modified while the mapping is active.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::fs::File;
    /// use erofs_rs::backend::MmapImage;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let file = File::open("image.erofs")?;
    /// let image = MmapImage::new_from_file(&file)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new_from_file(file: &fs::File) -> io::Result<Self> {
        let mmap = unsafe { Mmap::map(file)? };
        Ok(Self(mmap))
    }

    /// Creates a new `MmapImage` by opening and memory-mapping a file at the given path.
    ///
    /// This is a convenience method that combines file opening and memory mapping.
    ///
    /// # Safety
    ///
    /// This function creates a memory-mapped view of the file. The caller must
    /// ensure that the file is not modified while the mapping is active.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use erofs_rs::backend::MmapImage;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let image = MmapImage::new_from_path("image.erofs")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new_from_path<P: AsRef<path::Path>>(path: P) -> io::Result<Self> {
        let file = fs::File::open(path)?;
        Self::new_from_file(&file)
    }
}
