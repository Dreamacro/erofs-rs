use typed_path::{UnixPath, UnixPathBuf};

use super::EroFS;
use crate::backend::Image;
use crate::dirent::{DirEntry, DirentBlock};
use crate::{Result, types::Inode};

#[derive(Debug)]
pub struct ReadDir<'a, I: Image> {
    dir: UnixPathBuf,
    inode: Inode,
    erofs: &'a EroFS<I>,
    dirent_block: DirentBlock<&'a [u8]>,
    offset: usize,
}

impl<'a, I: Image> ReadDir<'a, I> {
    pub(crate) fn new<P: AsRef<UnixPath>>(
        erofs: &'a EroFS<I>,
        inode: Inode,
        dir: P,
    ) -> Result<Self> {
        let block = erofs.get_inode_block(&inode, 0)?;
        let dirent_block = DirentBlock::new(dir.as_ref().to_path_buf(), block)?;
        Ok(Self {
            dir: dir.as_ref().to_path_buf(),
            inode,
            erofs,
            dirent_block,
            offset: 0,
        })
    }

    fn next_entry(&mut self) -> Result<Option<DirEntry>> {
        if self.offset >= self.inode.data_size() {
            return Ok(None);
        }

        while self.offset < self.inode.data_size() {
            match self.dirent_block.next_entry()? {
                Some(entry) => return Ok(Some(entry)),
                None => {
                    self.offset += self.dirent_block.block_size();
                    if self.offset < self.inode.data_size() {
                        let block = self.erofs.get_inode_block(&self.inode, self.offset)?;
                        self.dirent_block = DirentBlock::new(self.dir.clone(), block)?;
                    }
                }
            }
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
