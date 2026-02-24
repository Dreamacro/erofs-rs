use alloc::vec::Vec;
use typed_path::{UnixPath, UnixPathBuf};

use super::EroFS;
use crate::backend::AsyncImage;
use crate::dirent::{DirEntry, DirentBlock};
use crate::{Result, types::Inode};

pub struct ReadDir<'a, I: AsyncImage> {
    dir: UnixPathBuf,
    inode: Inode,
    erofs: &'a EroFS<I>,
    dirent_block: DirentBlock<Vec<u8>>,
    offset: usize,
}

impl<'a, I: AsyncImage> ReadDir<'a, I> {
    pub(crate) async fn new<P: AsRef<UnixPath>>(
        erofs: &'a EroFS<I>,
        inode: Inode,
        dir: P,
    ) -> Result<Self> {
        let block_data = erofs.read_inode_block(&inode, 0).await?;
        let dirent_block = DirentBlock::new(dir.as_ref().to_path_buf(), block_data)?;
        Ok(Self {
            dir: dir.as_ref().to_path_buf(),
            inode,
            erofs,
            dirent_block,
            offset: 0,
        })
    }

    pub async fn next_entry(&mut self) -> Result<Option<DirEntry>> {
        if self.offset >= self.inode.data_size() {
            return Ok(None);
        }

        while self.offset < self.inode.data_size() {
            match self.dirent_block.next_entry()? {
                Some(entry) => return Ok(Some(entry)),
                None => {
                    self.offset += self.dirent_block.block_size();
                    if self.offset < self.inode.data_size() {
                        let block_data = self
                            .erofs
                            .read_inode_block(&self.inode, self.offset)
                            .await?;
                        self.dirent_block = DirentBlock::new(self.dir.clone(), block_data)?;
                    }
                }
            }
        }
        Ok(None)
    }
}
