use std::{
    fs, io,
    ops::{Bound, RangeBounds},
    path,
};

use memmap2::Mmap;

use super::Image;

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
    pub fn new(mmap: Mmap) -> Self {
        Self(mmap)
    }

    pub fn new_from_file(file: &fs::File) -> io::Result<Self> {
        let mmap = unsafe { Mmap::map(file)? };
        Ok(Self(mmap))
    }

    pub fn new_from_path<P: AsRef<path::Path>>(path: P) -> io::Result<Self> {
        let file = fs::File::open(path)?;
        Self::new_from_file(&file)
    }
}
