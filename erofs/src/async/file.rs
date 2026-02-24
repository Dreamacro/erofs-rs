use core::cmp;

use bytes::Bytes;

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
    offset: usize,
    buf: Option<Bytes>,
}

impl<'a, I: AsyncImage> File<'a, I> {
    pub(crate) fn new(inode: Inode, erofs: &'a EroFS<I>) -> Self {
        Self {
            inode,
            erofs,
            offset: 0,
            buf: None,
        }
    }

    /// Returns the size of the file in bytes.
    pub fn size(&self) -> usize {
        self.inode.data_size()
    }

    /// Asynchronously reads file contents into `buf`.
    ///
    /// Returns the number of bytes read, or `0` if EOF has been reached.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if self.offset >= self.inode.data_size() {
            return Ok(0);
        }

        if let Some(ref data) = self.buf {
            let offset = self.offset % self.erofs.block_size();
            let n = cmp::min(buf.len(), data.len().saturating_sub(offset));
            buf[..n].copy_from_slice(&data[offset..offset + n]);
            self.offset += n;
            return Ok(n);
        }

        let block_size = self.erofs.block_size();
        let cur_offset = self.offset;
        let block = self.erofs.read_inode_block(&self.inode, cur_offset).await?;
        if buf.len() >= block.len() {
            let n = block.len();
            buf[..n].copy_from_slice(&block);
            self.offset += n;
            Ok(n)
        } else {
            let offset = cur_offset % block_size;
            let n = cmp::min(buf.len(), block.len().saturating_sub(offset));
            buf[..n].copy_from_slice(&block[offset..offset + n]);
            self.buf = Some(Bytes::from(block));
            self.offset += n;
            Ok(n)
        }
    }
}
