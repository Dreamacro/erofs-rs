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
/// ```
/// use erofs_rs::backend::SliceImage;
///
/// // In real usage, this would be actual EROFS image data
/// static IMAGE_DATA: &[u8] = include_bytes!("../../../test_data/test.erofs");
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
    fn get<R: ops::RangeBounds<usize>>(&self, range: R) -> Option<&[u8]> {
        let start = match range.start_bound() {
            core::ops::Bound::Included(&s) => s,
            core::ops::Bound::Excluded(&s) => s + 1,
            core::ops::Bound::Unbounded => 0,
        };

        let end = match range.end_bound() {
            core::ops::Bound::Included(&e) => e + 1,
            core::ops::Bound::Excluded(&e) => e,
            core::ops::Bound::Unbounded => self.0.len(),
        };

        self.0.get(start..end)
    }

    fn len(&self) -> u64 {
        self.0.len() as u64
    }
}
