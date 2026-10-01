use alloc::borrow::Cow;
use core::cmp;

#[cfg(feature = "std")]
use std::io::{Read, Result};

#[cfg(not(feature = "std"))]
use crate::Result;

use super::EroFS;
use crate::backend::Image;
use crate::types::Inode;

#[cfg(not(feature = "std"))]
/// A trait for reading file contents in `no_std` mode.
pub trait Read {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize>;
}

/// A handle to a file within an EROFS filesystem.
///
/// `File` implements [`std::io::Read`], allowing you to read the file's contents
/// using standard I/O methods like `read`, `read_to_end`, or `read_to_string`.
///
/// # Example
///
/// ```no_run
/// # #[cfg(feature = "std")]
/// # {
/// use std::io::Read;
/// use erofs_rs::EroFS;
/// use erofs_rs::backend::MmapImage;
///
/// // SAFETY: assume the file remains immutable until the filesystem is dropped.
/// let image = unsafe { MmapImage::new_from_path("image.erofs").unwrap() };
/// let fs = EroFS::new(image).unwrap();
///
/// let mut file = fs.open("/etc/passwd").unwrap();
/// let mut content = Vec::new();
/// file.read_to_end(&mut content).unwrap();
/// # }
/// ```
#[derive(Debug)]
pub struct File<'a, I: Image> {
    inode: Inode,
    erofs: &'a EroFS<I>,
    offset: u64,
    buf: Cow<'a, [u8]>,
    buf_pos: usize,
}

impl<'a, I: Image> File<'a, I> {
    pub(crate) fn new(inode: Inode, erofs: &'a EroFS<I>) -> Self {
        Self {
            inode,
            erofs,
            offset: 0,
            buf: Cow::Borrowed(&[]),
            buf_pos: 0,
        }
    }

    /// Returns the size of the file in bytes.
    pub fn size(&self) -> u64 {
        self.inode.data_size()
    }
}

impl<'a, I: Image> Read for File<'a, I> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() || self.offset >= self.inode.data_size() {
            return Ok(0);
        }

        if self.buf.is_empty() {
            let block = self.erofs.get_inode_data(&self.inode, self.offset);
            #[cfg(feature = "std")]
            let block = block.map_err(std::io::Error::other);
            self.buf = block?;
        }

        let data = &self.buf[self.buf_pos..];
        let n = cmp::min(buf.len(), data.len());
        buf[..n].copy_from_slice(&data[..n]);
        self.buf_pos += n;
        self.offset += n as u64;
        if self.buf_pos == self.buf.len() {
            self.buf = Cow::Borrowed(&[]);
            self.buf_pos = 0;
        }
        Ok(n)
    }
}
