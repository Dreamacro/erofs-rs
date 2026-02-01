use std::path::{Component, Path};
use std::sync::Arc;

use binrw::BinRead;
use binrw::BinReaderExt;
use bytes::Buf;
use memmap2::Mmap;

use crate::dirent;
use crate::file::File;
use crate::traits::ReadCursorExt;
use crate::types::*;
use crate::walkdir::WalkDir;
use crate::{Error, Result};

/// The main entry point for reading EROFS filesystem images.
///
/// `EroFS` provides methods to traverse directories, open files, and access
/// filesystem metadata from a memory-mapped EROFS image.
///
/// # Example
///
/// ```no_run
/// use std::fs::File;
/// use std::io::Read;
/// use memmap2::Mmap;
/// use erofs_rs::EroFS;
///
/// let file = File::open("image.erofs").unwrap();
/// let mmap = unsafe { Mmap::map(&file) }.unwrap();
/// let fs = EroFS::new(mmap).unwrap();
///
/// let mut file = fs.open("/etc/passwd").unwrap();
/// let mut content = String::new();
/// file.read_to_string(&mut content).unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct EroFS {
    mmap: Arc<Mmap>,
    super_block: SuperBlock,
    block_size: usize,
}

impl EroFS {
    /// Creates a new `EroFS` instance from a memory-mapped EROFS image.
    ///
    /// # Errors
    ///
    /// Returns an error if the superblock is invalid or the magic number doesn't match.
    pub fn new(mmap: Mmap) -> Result<Self> {
        let mut cursor = mmap.read_cursor(SUPER_BLOCK_OFFSET).ok_or_else(|| {
            Error::InvalidSuperblock("failed to read super block from mmap".to_string())
        })?;
        let super_block = SuperBlock::read(&mut cursor)?;

        let magic_number = super_block.magic;
        let blk_size_bits = super_block.blk_size_bits;

        if magic_number != MAGIC_NUMBER {
            return Err(Error::InvalidSuperblock(format!(
                "invalid magic number: 0x{:x}",
                magic_number
            )));
        }

        if !(9..=24).contains(&blk_size_bits) {
            return Err(Error::InvalidSuperblock(format!(
                "invalid block size bits: {}",
                blk_size_bits
            )));
        }

        let block_size = 1u64 << blk_size_bits;

        Ok(Self {
            mmap: mmap.into(),
            super_block,
            block_size: block_size as usize,
        })
    }

    /// Recursively walks a directory tree starting from the given path.
    ///
    /// Returns an iterator that yields all entries (files and directories)
    /// under the specified root path.
    pub fn walk_dir<P: AsRef<Path>>(&self, root: P) -> Result<WalkDir<'_>> {
        WalkDir::new(self, root)
    }

    /// Lists the immediate contents of a directory.
    ///
    /// This is equivalent to `walk_dir` with `max_depth(1)`.
    pub fn read_dir<P: AsRef<Path>>(&self, path: P) -> Result<WalkDir<'_>> {
        Ok(WalkDir::new(self, path)?.max_depth(1))
    }

    /// Opens a file at the given path for reading.
    ///
    /// The returned [`File`] implements [`std::io::Read`].
    ///
    /// # Errors
    ///
    /// Returns an error if the path doesn't exist or is not a regular file.
    pub fn open<P: AsRef<Path>>(&self, path: P) -> Result<File> {
        let inode = self
            .get_path_inode(&path)?
            .ok_or_else(|| Error::PathNotFound(path.as_ref().to_string_lossy().into_owned()))?;

        self.open_inode_file(inode)
    }

    /// Opens a file from an inode directly.
    ///
    /// This is useful when you already have an inode from directory traversal.
    pub fn open_inode_file(&self, inode: Inode) -> Result<File> {
        if !inode.is_file() {
            return Err(Error::NotAFile(format!(
                "inode {} is not a regular file",
                inode.id()
            )));
        }

        Ok(File::new(inode, self.clone()))
    }

    /// Returns a reference to the filesystem superblock.
    pub fn super_block(&self) -> &SuperBlock {
        &self.super_block
    }

    pub(crate) fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn get_inode(&self, nid: u64) -> Result<Inode> {
        let offset = self.get_inode_offset(nid) as usize;

        let mut inode_buf = self
            .mmap
            .read_cursor(offset)
            .ok_or_else(|| Error::OutOfBounds("failed to read inode format".to_string()))?;

        let layout = inode_buf.read_le()?;
        inode_buf.set_position(0);
        if Inode::is_compact_format(layout) {
            let inode = InodeCompact::read(&mut inode_buf)?;
            Ok(Inode::Compact((nid, inode)))
        } else {
            let inode = InodeExtended::read(&mut inode_buf)?;
            Ok(Inode::Extended((nid, inode)))
        }
    }

    pub(crate) fn get_inode_block(&self, inode: &Inode, offset: usize) -> Result<&[u8]> {
        match inode.layout()? {
            Layout::FlatPlain => {
                let block_count = inode.data_size().div_ceil(self.block_size);
                let block_index = offset / self.block_size;
                if block_index >= block_count {
                    return Err(Error::OutOfRange(block_index, block_count));
                }

                let size = inode.data_size();
                let offset = self.block_offset(inode.raw_block_addr())
                    + (block_index as u64 * self.block_size as u64);
                let data = self
                    .mmap
                    .get_at(offset as usize, size)
                    .ok_or_else(|| Error::OutOfBounds("failed to get inode data".to_string()))?;
                Ok(data)
            }
            Layout::FlatInline => {
                let block_count = inode.data_size().div_ceil(self.block_size);
                let block_index = offset / self.block_size;
                if block_index >= block_count {
                    return Err(Error::OutOfRange(block_index, block_count));
                }

                if block_count != 0 && block_index == block_count - 1 {
                    // tail block
                    let offset = self.get_inode_offset(inode.id());
                    let buf_size = inode.data_size() % self.block_size;
                    let offset = offset as usize + inode.size() + inode.xattr_size();
                    let data = self.mmap.get_at(offset, buf_size).ok_or_else(|| {
                        Error::OutOfBounds("failed to get inode tail data".to_string())
                    })?;
                    return Ok(data);
                }

                let offset = self.block_offset(inode.raw_block_addr()) as usize
                    + (block_index * self.block_size);
                let len = self.block_size.min(inode.data_size());
                let buf = self
                    .mmap
                    .get_at(offset, len)
                    .ok_or_else(|| Error::OutOfBounds("failed to get inode data".to_string()))?;
                Ok(buf)
            }
            Layout::CompressedFull | Layout::CompressedCompact => {
                Err(Error::NotSupported("compressed compact layout".to_string()))
            }
            Layout::ChunkBased => {
                let chunk_format = ChunkBasedFormat::new(inode.raw_block_addr());
                if !chunk_format.is_valid() {
                    return Err(Error::CorruptedData(format!(
                        "invalid chunk based format {}",
                        inode.raw_block_addr()
                    )));
                } else if chunk_format.is_indexes() {
                    // don't support chunk indexes yet
                    return Err(Error::NotSupported(
                        "chunk based format with indexes".to_string(),
                    ));
                }

                let chunk_bits = chunk_format.chunk_size_bits() + self.super_block.blk_size_bits;
                let chunk_size = 1usize << chunk_bits;
                let chunk_count = inode.data_size().div_ceil(chunk_size);
                let chunk_index = offset >> chunk_bits;
                let chunk_fixed = offset % chunk_size / self.block_size;
                if chunk_index >= chunk_count {
                    return Err(Error::OutOfRange(chunk_index, chunk_count));
                }

                let offset = self.get_inode_offset(inode.id());
                let offset =
                    offset as usize + inode.size() + inode.xattr_size() + (chunk_index * 4);
                let chunk_addr = self
                    .mmap
                    .get_at(offset, 4)
                    .ok_or_else(|| Error::OutOfBounds("failed to get chunk address".to_string()))?
                    .get_i32_le();

                let chunk_size = if chunk_index == chunk_count - 1 {
                    inode.data_size() % self.block_size
                } else {
                    self.block_size
                };

                if chunk_addr <= 0 {
                    return Err(Error::CorruptedData(
                        "sparse chunks are not supported".to_string(),
                    ));
                }

                let offset = self.block_offset(chunk_addr as u32 + chunk_fixed as u32);
                let data = self
                    .mmap
                    .get_at(offset as usize, chunk_size)
                    .ok_or_else(|| Error::OutOfBounds("failed to get inode data".to_string()))?;

                Ok(data)
            }
        }
    }

    pub(crate) fn get_path_inode<P: AsRef<Path>>(&self, path: P) -> Result<Option<Inode>> {
        let mut nid = self.super_block.root_nid as u64;

        'outer: for part in path.as_ref().components() {
            if part == Component::RootDir {
                continue;
            }

            let inode = self.get_inode(nid)?;
            let block_count = inode.data_size().div_ceil(self.block_size);
            if block_count == 0 {
                return Ok(None);
            }

            for i in 0..block_count {
                let block = self.get_inode_block(&inode, i)?;
                if let Some(found_nid) = dirent::find_nodeid_by_name(part.as_os_str(), block)? {
                    nid = found_nid;
                    continue 'outer;
                }
            }
            return Ok(None);
        }

        let inode = self.get_inode(nid)?;
        Ok(Some(inode))
    }

    fn get_inode_offset(&self, nid: u64) -> u64 {
        self.block_offset(self.super_block.meta_blk_addr) + (nid * InodeCompact::size() as u64)
    }

    fn block_offset(&self, block: u32) -> u64 {
        (block as u64) << self.super_block.blk_size_bits
    }
}
