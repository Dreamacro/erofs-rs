use alloc::vec::Vec;
use typed_path::{UnixPath, UnixPathBuf};

use super::EroFS;
use crate::backend::AsyncImage;
use crate::dirent::{DirEntry, DirentBlock};
use crate::{Result, types::Inode};

pub struct ReadDir<'a, I: AsyncImage> {
    dir: UnixPathBuf,
    pub(super) inode: Inode,
    erofs: &'a EroFS<I>,
    dirent_block: DirentBlock<Vec<u8>>,
    offset: u64,
}

impl<'a, I: AsyncImage> ReadDir<'a, I> {
    pub(crate) async fn new<P: AsRef<UnixPath>>(
        erofs: &'a EroFS<I>,
        inode: Inode,
        dir: P,
    ) -> Result<Self> {
        let block_data = erofs.read_inode_block(&inode, 0).await?;
        let dirent_block = DirentBlock::new(block_data)?;
        Ok(Self {
            dir: dir.as_ref().to_path_buf(),
            inode,
            erofs,
            dirent_block,
            offset: 0,
        })
    }

    pub async fn next_entry(&mut self) -> Result<Option<DirEntry>> {
        while self.offset < self.inode.data_size() {
            if let Some(entry) = self.dirent_block.next_entry(&self.dir)? {
                return Ok(Some(entry));
            }
            let offset = self.offset + self.dirent_block.block_size() as u64;
            if offset < self.inode.data_size() {
                let block = self.erofs.read_inode_block(&self.inode, offset).await?;
                self.dirent_block = DirentBlock::new(block)?;
            }
            self.offset = offset;
        }
        Ok(None)
    }
}
