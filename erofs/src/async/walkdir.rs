use alloc::vec::Vec;

use super::EroFS;
use super::dirent::ReadDir;
use crate::backend::AsyncImage;
use crate::dirent::DirEntry;
use crate::{Error, Result, types::Inode};
use typed_path::UnixPath;

/// An async iterator for recursively walking a directory tree.
pub struct WalkDir<'a, I: AsyncImage> {
    erofs: &'a EroFS<I>,
    dir_stack: Vec<(usize, ReadDir<'a, I>)>,
    max_depth: usize,
}

/// A single entry returned by [`WalkDir`].
pub struct WalkDirEntry {
    /// The depth of this entry relative to the starting directory (1-indexed).
    pub depth: usize,
    /// The directory entry containing file name and type.
    pub dir_entry: DirEntry,
    /// The inode containing file metadata.
    pub inode: Inode,
}

impl<'a, I: AsyncImage> WalkDir<'a, I> {
    pub(crate) async fn new(erofs: &'a EroFS<I>, root: impl AsRef<UnixPath>) -> Result<Self> {
        let read_dir = {
            let inode = erofs
                .get_path_inode(root.as_ref())
                .await?
                .ok_or_else(|| Error::PathNotFound(root.as_ref().to_string_lossy().into_owned()))?;

            if !inode.file_type().is_dir() {
                return Err(Error::NotADirectory(
                    root.as_ref().to_string_lossy().into_owned(),
                ));
            }

            ReadDir::new(erofs, inode, root).await?
        };
        Ok(WalkDir {
            erofs,
            dir_stack: vec![(1, read_dir)],
            max_depth: 0,
        })
    }

    /// Sets the maximum depth to descend into subdirectories.
    ///
    /// A depth of 1 means only immediate children are returned (like `read_dir`).
    /// A depth of 0 (the default) means unlimited depth.
    pub fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }

    async fn get_walk_dir_entry(
        &mut self,
        dir_entry: DirEntry,
        depth: usize,
    ) -> Result<WalkDirEntry> {
        let inode = self.erofs.get_inode(dir_entry.nid()).await?;

        if (depth < self.max_depth || self.max_depth == 0) && dir_entry.file_type().is_dir() {
            let child_dir = ReadDir::new(self.erofs, inode, dir_entry.path()).await?;
            self.dir_stack.push((depth + 1, child_dir));
        }

        Ok(WalkDirEntry {
            depth,
            dir_entry,
            inode,
        })
    }

    pub async fn next_entry(&mut self) -> Option<Result<WalkDirEntry>> {
        loop {
            let (depth, next_item) = {
                let (depth, dir) = self.dir_stack.last_mut()?;
                let next = dir.next_entry().await;
                (*depth, next)
            };

            match next_item {
                Ok(Some(entry)) => return Some(self.get_walk_dir_entry(entry, depth).await),
                Ok(None) => {
                    self.dir_stack.pop();
                }
                Err(e) => return Some(Err(e)),
            }
        }
    }
}
