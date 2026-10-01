use alloc::borrow::Cow;
use typed_path::{UnixPath, UnixPathBuf};

use super::EroFS;
use crate::backend::Image;
use crate::dirent::{DirEntry, DirentBlock};
use crate::{Result, types::Inode};

#[derive(Debug)]
pub struct ReadDir<'a, I: Image> {
    dir: UnixPathBuf,
    pub(super) inode: Inode,
    erofs: &'a EroFS<I>,
    dirent_block: DirentBlock<Cow<'a, [u8]>>,
    offset: u64,
}

impl<'a, I: Image> ReadDir<'a, I> {
    pub(crate) fn new<P: AsRef<UnixPath>>(
        erofs: &'a EroFS<I>,
        inode: Inode,
        dir: P,
    ) -> Result<Self> {
        let block = erofs.get_inode_block(&inode, 0)?;
        let dirent_block = DirentBlock::new(block)?;
        Ok(Self {
            dir: dir.as_ref().to_path_buf(),
            inode,
            erofs,
            dirent_block,
            offset: 0,
        })
    }

    fn next_entry(&mut self) -> Result<Option<DirEntry>> {
        while self.offset < self.inode.data_size() {
            if let Some(entry) = self.dirent_block.next_entry(&self.dir)? {
                return Ok(Some(entry));
            }
            let offset = self.offset + self.dirent_block.block_size() as u64;
            if offset < self.inode.data_size() {
                let block = self.erofs.get_inode_block(&self.inode, offset)?;
                self.dirent_block = DirentBlock::new(block)?;
            }
            self.offset = offset;
        }
        Ok(None)
    }
}

impl<'a, I: Image> Iterator for ReadDir<'a, I> {
    type Item = Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_entry().transpose()
    }
}
