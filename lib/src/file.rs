use std::{cmp, io::Read};

use bytes::Bytes;

use crate::{EroFS, types::Inode};

/// A handle to a file within an EROFS filesystem.
///
/// `File` implements [`std::io::Read`], allowing you to read the file's contents
/// using standard I/O methods like `read`, `read_to_end`, or `read_to_string`.
///
/// # Example
///
/// ```no_run
/// use std::io::Read;
/// use erofs_rs::EroFS;
/// # use memmap2::Mmap;
/// # use std::fs::File;
/// # let file = File::open("image.erofs").unwrap();
/// # let mmap = unsafe { Mmap::map(&file) }.unwrap();
/// # let fs = EroFS::new(mmap).unwrap();
///
/// let mut file = fs.open("/etc/passwd").unwrap();
/// let mut content = Vec::new();
/// file.read_to_end(&mut content).unwrap();
/// ```
#[derive(Debug)]
pub struct File {
    inode: Inode,
    erofs: EroFS,
    offset: usize,
    buf: Option<Bytes>,
}

impl File {
    pub(crate) fn new(inode: Inode, erofs: EroFS) -> Self {
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
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
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
        let block = self
            .erofs
            .get_inode_block(&self.inode, cur_offset)
            .map_err(|e| std::io::Error::other(format!("read block failed: {}", e)))?;
        if buf.len() >= block.len() {
            let n = block.len();
            buf[..n].copy_from_slice(block);
            self.offset += n;
            Ok(n)
        } else {
            let offset = cur_offset % block_size;
            let n = cmp::min(buf.len(), block.len().saturating_sub(offset));
            buf[..n].copy_from_slice(&block[offset..offset + n]);
            self.buf = Some(Bytes::copy_from_slice(block));
            self.offset += n;
            Ok(n)
        }
    }
}
