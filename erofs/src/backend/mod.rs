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

#[enum_dispatch(Image)]
#[derive(Debug)]
pub enum Backend<'a> {
    #[cfg(feature = "std")]
    Mmap(mmap::MmapImage),
    Slice(slice::SliceImage<'a>),
}

#[enum_dispatch]
pub trait Image {
    fn get<R: ops::RangeBounds<usize>>(&self, range: R) -> Option<&[u8]>;

    fn get_cursor(&self, offset: usize) -> Option<Cursor<&[u8]>> {
        self.get(offset..).map(Cursor::new)
    }

    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
