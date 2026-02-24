use alloc::format;
use alloc::vec::Vec;
use typed_path::Component;

use bytes::Buf;
use typed_path::{UnixComponent, UnixPath};

use super::file::File;
use super::walkdir::WalkDir;
use crate::backend::AsyncImage;
use crate::dirent;
use crate::filesystem::{BlockPlan, EroFSCore};
use crate::types::*;
use crate::{Error, Result};

/// The async entry point for reading EROFS filesystem images.
///
/// `EroFS` provides async methods to traverse directories, open files, and access
/// filesystem metadata from EROFS images.
#[derive(Debug, Clone)]
pub struct EroFS<I: AsyncImage> {
    image: I,
    core: EroFSCore,
}

impl<I: AsyncImage> EroFS<I> {
    /// Creates a new async `EroFS` instance from an async backend image source.
    pub async fn new(image: I) -> Result<Self> {
        let mut super_block = vec![0u8; SuperBlock::size()];
        image.read_at(&mut super_block, SUPER_BLOCK_OFFSET).await?;
        let core = EroFSCore::new(&super_block)?;
        Ok(Self { image, core })
    }

    /// Recursively walks a directory tree starting from the given path.
    pub async fn walk_dir<P: AsRef<UnixPath>>(&self, root: P) -> Result<WalkDir<'_, I>> {
        WalkDir::new(self, root).await
    }

    /// Lists the immediate contents of a directory.
    pub async fn read_dir<P: AsRef<UnixPath>>(&self, path: P) -> Result<WalkDir<'_, I>> {
        Ok(WalkDir::new(self, path).await?.max_depth(1))
    }

    /// Opens a file at the given path for reading.
    ///
    /// The returned [`File`] provides an async [`read`](File::read) method.
    ///
    /// # Errors
    ///
    /// Returns an error if the path doesn't exist or is not a regular file.
    pub async fn open<P: AsRef<UnixPath>>(&self, path: P) -> Result<File<'_, I>> {
        let inode = self
            .get_path_inode(&path)
            .await?
            .ok_or_else(|| Error::PathNotFound(path.as_ref().to_string_lossy().into_owned()))?;

        self.open_inode_file(inode)
    }

    /// Opens a file from an inode directly.
    ///
    /// This is useful when you already have an inode from directory traversal.
    pub fn open_inode_file(&self, inode: Inode) -> Result<File<'_, I>> {
        if !inode.is_file() {
            return Err(Error::NotAFile(format!(
                "inode {} is not a regular file",
                inode.id()
            )));
        }

        Ok(File::new(inode, self))
    }

    /// Returns a reference to the filesystem superblock.
    pub fn super_block(&self) -> &SuperBlock {
        &self.core.super_block
    }

    pub(crate) fn block_size(&self) -> usize {
        self.core.block_size
    }

    pub async fn get_inode(&self, nid: u64) -> Result<Inode> {
        let offset = self.core.get_inode_offset(nid) as usize;
        let mut buf = vec![0u8; InodeExtended::size()];
        self.image.read_at(&mut buf, offset).await?;
        self.core.parse_inode(&buf, nid)
    }

    pub(crate) async fn read_inode_block(&self, inode: &Inode, offset: usize) -> Result<Vec<u8>> {
        match self.core.plan_inode_block_read(inode, offset)? {
            BlockPlan::Direct { offset, size } => {
                let mut buf = vec![0u8; size];
                self.image.read_at(&mut buf, offset).await?;
                Ok(buf)
            }
            BlockPlan::Chunked {
                addr_offset,
                chunk_fixed,
                data_size,
                chunk_index,
                chunk_count,
            } => {
                let mut addr_buf = vec![0u8; 4];
                self.image.read_at(&mut addr_buf, addr_offset).await?;
                let chunk_addr = (&addr_buf[..]).get_i32_le();

                let (offset, size) = self.core.resolve_chunk_read(
                    chunk_addr,
                    chunk_fixed,
                    data_size,
                    chunk_index,
                    chunk_count,
                )?;
                let mut buf = vec![0u8; size];
                self.image.read_at(&mut buf, offset).await?;
                Ok(buf)
            }
        }
    }

    pub(crate) async fn get_path_inode<P: AsRef<UnixPath>>(
        &self,
        path: P,
    ) -> Result<Option<Inode>> {
        let mut nid = self.core.super_block.root_nid as u64;

        let path = path.as_ref().normalize();
        'outer: for part in path.components() {
            if part == UnixComponent::RootDir {
                continue;
            }

            let inode = self.get_inode(nid).await?;
            let block_count = inode.data_size().div_ceil(self.core.block_size);
            if block_count == 0 {
                return Ok(None);
            }

            for i in 0..block_count {
                let block = self.read_inode_block(&inode, i).await?;
                if let Some(found_nid) = dirent::find_nodeid_by_name(part.as_bytes(), &block)? {
                    nid = found_nid;
                    continue 'outer;
                }
            }
            return Ok(None);
        }

        let inode = self.get_inode(nid).await?;
        Ok(Some(inode))
    }
}
