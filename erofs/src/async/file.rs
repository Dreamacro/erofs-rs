use bytes::{Buf, Bytes};

use super::EroFS;
use crate::Result;
use crate::backend::AsyncImage;
use crate::types::Inode;

/// An async handle to a file within an EROFS filesystem.
///
/// Use [`read`](File::read) to asynchronously read file contents.
#[derive(Debug)]
pub struct File<'a, I: AsyncImage> {
    inode: Inode,
    erofs: &'a EroFS<I>,
    offset: u64,
    buf: Bytes,
}

impl<'a, I: AsyncImage> File<'a, I> {
    pub(crate) fn new(inode: Inode, erofs: &'a EroFS<I>) -> Self {
        Self {
            inode,
            erofs,
            offset: 0,
            buf: Bytes::new(),
        }
    }

    /// Returns the size of the file in bytes.
    pub fn size(&self) -> u64 {
        self.inode.data_size()
    }

    /// Asynchronously reads file contents into `buf`.
    ///
    /// Returns the number of bytes read, or `0` at EOF or for an empty buffer.
    /// A successful read may fill only part of `buf`. Errors do not advance the position.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() || self.offset >= self.inode.data_size() {
            return Ok(0);
        }

        if self.buf.is_empty() {
            self.buf = self
                .erofs
                .read_inode_block(&self.inode, self.offset)
                .await?
                .into();
        }

        let n = buf.len().min(self.buf.len());
        self.buf.copy_to_slice(&mut buf[..n]);
        self.offset += n as u64;
        if self.buf.is_empty() {
            self.buf = Bytes::new();
        }
        Ok(n)
    }
}
