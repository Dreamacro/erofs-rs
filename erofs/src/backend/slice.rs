use super::Image;
use core::ops;

#[derive(Debug)]
pub struct SliceImage<'a>(&'a [u8]);

impl<'a> SliceImage<'a> {
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
