use super::Image;
use core::ops;

/// A byte slice backend for EROFS images.
///
/// This backend wraps a byte slice, making it suitable for `no_std` environments
/// or when the image data is already in memory. It provides zero-copy access
/// to the image data.
///
/// # Examples
///
/// ```
/// use erofs_rs::backend::SliceImage;
///
/// let data: &[u8] = &[/* EROFS image data */];
/// let image = SliceImage::new(data);
/// ```
///
/// ## With embedded data
///
/// ```ignore
/// use erofs_rs::backend::SliceImage;
///
/// // In real usage, this would be actual EROFS image data
/// static IMAGE_DATA: &[u8] = include_bytes!("path/to/image.erofs");
/// let image = SliceImage::new(IMAGE_DATA);
/// ```
#[derive(Debug)]
pub struct SliceImage<'a>(&'a [u8]);

impl<'a> SliceImage<'a> {
    /// Creates a new `SliceImage` from a byte slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use erofs_rs::backend::SliceImage;
    ///
    /// let data: &[u8] = &[0; 1024];
    /// let image = SliceImage::new(data);
    /// ```
    pub fn new(data: &'a [u8]) -> Self {
        Self(data)
    }
}

impl<'a> Image for SliceImage<'a> {
    fn get<R: ops::RangeBounds<u64>>(&self, range: R) -> Option<&[u8]> {
        let start = match range.start_bound() {
            ops::Bound::Included(&s) => usize::try_from(s).ok()?,
            ops::Bound::Excluded(&s) => usize::try_from(s).ok()?.checked_add(1)?,
            ops::Bound::Unbounded => 0,
        };

        let end = match range.end_bound() {
            ops::Bound::Included(&e) => usize::try_from(e).ok()?.checked_add(1)?,
            ops::Bound::Excluded(&e) => usize::try_from(e).ok()?,
            ops::Bound::Unbounded => self.0.len(),
        };

        self.0.get(start..end)
    }

    fn len(&self) -> u64 {
        self.0.len() as u64
    }
}
