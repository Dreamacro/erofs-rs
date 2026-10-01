use alloc::vec::Vec;

use super::EroFS;
use super::dirent::ReadDir;
use crate::backend::Image;
use crate::dirent::DirEntry;
pub use crate::dirent::WalkDirEntry;
use crate::{Error, Result};
use typed_path::UnixPath;

/// An iterator for recursively walking a directory tree.
///
/// Created by [`EroFS::walk_dir`] or [`EroFS::read_dir`].
#[derive(Debug)]
pub struct WalkDir<'a, I: Image> {
    erofs: &'a EroFS<I>,
    dir_stack: Vec<ReadDir<'a, I>>,
    max_depth: usize,
}

impl<'a, I: Image> WalkDir<'a, I> {
    pub(crate) fn new<P: AsRef<UnixPath>>(erofs: &'a EroFS<I>, root: P) -> Result<Self> {
        let read_dir = {
            let inode = erofs
                .get_path_inode(&root)?
                .ok_or_else(|| Error::PathNotFound(root.as_ref().to_string_lossy().into_owned()))?;

            if !inode.file_type().is_dir() {
                return Err(Error::NotADirectory(
                    root.as_ref().to_string_lossy().into_owned(),
                ));
            }

            ReadDir::new(erofs, inode, root)?
        };
        Ok(WalkDir {
            erofs,
            dir_stack: vec![read_dir],
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

    fn get_walk_dir_entry(&mut self, dir_entry: DirEntry, depth: usize) -> Result<WalkDirEntry> {
        let inode = self.erofs.get_inode(dir_entry.nid())?;

        if inode.is_dir() {
            if self
                .dir_stack
                .iter()
                .any(|dir| dir.inode.id() == inode.id())
            {
                return Err(Error::CorruptedData("directory cycle".into()));
            }
            if depth < self.max_depth || self.max_depth == 0 {
                let child_dir = ReadDir::new(self.erofs, inode, dir_entry.path())?;
                self.dir_stack.push(child_dir);
            }
        }

        Ok(WalkDirEntry {
            depth,
            dir_entry,
            inode,
        })
    }

    fn next_entry(&mut self) -> Option<Result<WalkDirEntry>> {
        loop {
            let depth = self.dir_stack.len();
            let next_item = self.dir_stack.last_mut()?.next();

            match next_item {
                Some(Ok(entry)) => return Some(self.get_walk_dir_entry(entry, depth)),
                Some(Err(e)) => return Some(Err(e)),
                None => {
                    self.dir_stack.pop();
                }
            }
        }
    }
}

impl<'a, I: Image> Iterator for WalkDir<'a, I> {
    type Item = Result<WalkDirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_entry()
    }
}
