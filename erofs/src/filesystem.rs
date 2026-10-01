use alloc::{format, string::ToString};

use binrw::BinRead;
use binrw::BinReaderExt;
use binrw::io::Cursor;

use crate::types::*;
use crate::{Error, Result};

/// Shared core data and pure computation logic for EROFS filesystem.
///
/// This struct is used by both sync and async `EroFS` implementations
/// to avoid duplicating parsing and calculation logic.
#[derive(Debug, Clone)]
pub struct EroFSCore {
    pub(crate) super_block: SuperBlock,
    pub(crate) block_size: usize,
}

/// Describes a planned block read operation.
///
/// Used by both sync and async implementations to share the layout
/// calculation logic, while keeping the actual I/O separate.
pub enum BlockPlan {
    /// A direct read: read `size` bytes at `offset`.
    Direct { offset: usize, size: usize },
    /// A two-phase read for chunk-based layout:
    /// 1. Read 4 bytes at `addr_offset` to get chunk address
    /// 2. Call `resolve_chunk_read()` with the chunk address
    Chunked {
        addr_offset: usize,
        chunk_fixed: usize,
        chunk_size: usize,
        data_size: usize,
        chunk_index: usize,
    },
}

impl BlockPlan {
    fn direct(offset: usize, size: usize) -> Result<Self> {
        offset
            .checked_add(size)
            .ok_or(Error::Overflow("block read range"))?;
        Ok(Self::Direct { offset, size })
    }
}

impl EroFSCore {
    /// Parse and validate a superblock from raw bytes.
    ///
    /// `data` should be the bytes starting at `SUPER_BLOCK_OFFSET`.
    pub(crate) fn new(data: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(data);
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

        let block_size = 1usize << blk_size_bits;
        Ok(Self {
            super_block,
            block_size,
        })
    }

    /// Parse an inode from raw bytes.
    pub(crate) fn parse_inode(&self, data: &[u8], nid: u64) -> Result<Inode> {
        let mut inode_buf = Cursor::new(data);
        let layout: u16 = inode_buf.read_le()?;
        inode_buf.set_position(0);
        let inode = if Inode::is_compact_format(layout) {
            Inode::Compact((nid, InodeCompact::read(&mut inode_buf)?))
        } else {
            Inode::Extended((nid, InodeExtended::read(&mut inode_buf)?))
        };
        inode.try_data_size()?;
        Ok(inode)
    }

    /// Plan a block read operation for the given inode and offset.
    ///
    /// Returns a `BlockPlan` describing what bytes to read.
    /// For `BlockPlan::Chunked`, the caller must perform an additional
    /// read and call `resolve_chunk_read()`.
    pub(crate) fn plan_inode_block_read(&self, inode: &Inode, offset: usize) -> Result<BlockPlan> {
        let data_size = inode.try_data_size()?;
        match inode.layout()? {
            Layout::FlatPlain => {
                let block_count = data_size.div_ceil(self.block_size);
                let block_index = offset / self.block_size;
                if block_index >= block_count {
                    return Err(Error::OutOfRange(block_index, block_count));
                }

                let block_start = block_index * self.block_size;
                let size = (data_size - block_start).min(self.block_size);
                let offset = self
                    .block_offset(inode.raw_block_addr())?
                    .checked_add(block_start)
                    .ok_or(Error::Overflow("file block offset"))?;
                BlockPlan::direct(offset, size)
            }
            Layout::FlatInline => {
                let block_count = data_size.div_ceil(self.block_size);
                let block_index = offset / self.block_size;
                if block_index >= block_count {
                    return Err(Error::OutOfRange(block_index, block_count));
                }

                if block_index == block_count - 1 {
                    let size = data_size % self.block_size;
                    if size == 0 {
                        return Err(Error::CorruptedData("empty inline tail".to_string()));
                    }
                    return BlockPlan::direct(self.inode_tail_offset(inode)?, size);
                }

                let offset = self
                    .block_offset(inode.raw_block_addr())?
                    .checked_add(block_index * self.block_size)
                    .ok_or(Error::Overflow("file block offset"))?;
                BlockPlan::direct(offset, self.block_size)
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
                    return Err(Error::NotSupported(
                        "chunk based format with indexes".to_string(),
                    ));
                }

                let chunk_bits = chunk_format.chunk_size_bits() + self.super_block.blk_size_bits;
                let chunk_size = 1usize
                    .checked_shl(u32::from(chunk_bits))
                    .ok_or(Error::Overflow("chunk size"))?;
                let chunk_count = data_size.div_ceil(chunk_size);
                let chunk_index = offset / chunk_size;
                let chunk_fixed = offset % chunk_size / self.block_size;
                if chunk_index >= chunk_count {
                    return Err(Error::OutOfRange(chunk_index, chunk_count));
                }

                let addr_offset = self
                    .inode_tail_offset(inode)?
                    .checked_add(chunk_index * 4)
                    .ok_or(Error::Overflow("chunk address offset"))?;
                addr_offset
                    .checked_add(4)
                    .ok_or(Error::Overflow("chunk address range"))?;

                Ok(BlockPlan::Chunked {
                    addr_offset,
                    chunk_fixed,
                    chunk_size,
                    data_size,
                    chunk_index,
                })
            }
        }
    }

    /// Resolve the final read offset and size for a chunk-based block read.
    ///
    /// `chunk_addr` is the i32 value read from `addr_offset` in the `Chunked` plan.
    /// `chunk_size` is the full chunk size in bytes (may span multiple blocks).
    pub(crate) fn resolve_chunk_read(
        &self,
        chunk_addr: i32,
        chunk_fixed: usize,
        chunk_size: usize,
        data_size: usize,
        chunk_index: usize,
    ) -> Result<(usize, usize)> {
        if chunk_addr <= 0 {
            return Err(Error::CorruptedData(
                "sparse chunks are not supported".to_string(),
            ));
        }

        let chunk_offset = chunk_fixed
            .checked_mul(self.block_size)
            .ok_or(Error::Overflow("chunk offset"))?;
        let file_byte_offset = chunk_index
            .checked_mul(chunk_size)
            .and_then(|offset| offset.checked_add(chunk_offset))
            .ok_or(Error::Overflow("file chunk offset"))?;
        let remaining = data_size.saturating_sub(file_byte_offset);
        let read_size = remaining.min(self.block_size);

        if read_size == 0 {
            return Err(Error::OutOfRange(file_byte_offset, data_size));
        }

        let chunk_fixed =
            u32::try_from(chunk_fixed).map_err(|_| Error::Overflow("chunk block index"))?;
        let block = (chunk_addr as u32)
            .checked_add(chunk_fixed)
            .ok_or(Error::Overflow("chunk block address"))?;
        let offset = self.block_offset(block)?;
        offset
            .checked_add(read_size)
            .ok_or(Error::Overflow("chunk read range"))?;
        Ok((offset, read_size))
    }

    pub(crate) fn get_inode_offset(&self, nid: u64) -> Result<usize> {
        let base = self.block_offset(self.super_block.meta_blk_addr)?;
        usize::try_from(nid)
            .ok()
            .and_then(|nid| nid.checked_mul(InodeCompact::size()))
            .and_then(|offset| base.checked_add(offset))
            .ok_or(Error::Overflow("inode offset"))
    }

    fn inode_tail_offset(&self, inode: &Inode) -> Result<usize> {
        self.get_inode_offset(inode.id())?
            .checked_add(inode.size() + inode.xattr_size())
            .ok_or(Error::Overflow("inode tail offset"))
    }

    pub(crate) fn block_offset(&self, block: u32) -> Result<usize> {
        let offset = u64::from(block) << self.super_block.blk_size_bits;
        usize::try_from(offset).map_err(|_| Error::Overflow("block offset"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;

    fn make_core() -> EroFSCore {
        let super_block = SuperBlock {
            magic: MAGIC_NUMBER,
            checksum: 0,
            feature_compat: 0,
            blk_size_bits: 12,
            ext_slots: 0,
            root_nid: 0,
            inos: 0,
            build_time: 0,
            build_time_ns: 0,
            blocks: 0,
            meta_blk_addr: 0,
            xattr_blk_addr: 0,
            uuid: [0; 16],
            volume_name: [0; 16],
            feature_incompat: 0,
            compr_algs: 0,
            extra_devices: 0,
            devt_slot_off: 0,
            dir_blk_bits: 0,
            xattr_prefix_count: 0,
            xattr_prefix_start: 0,
            packed_nid: 0,
            xattr_filter_res: 0,
            reserved: [0; 23],
        };

        EroFSCore {
            super_block,
            block_size: 1usize << super_block.blk_size_bits,
        }
    }

    fn make_compact_inode(
        layout: Layout,
        data_size: u32,
        xattr_count: u16,
        inode_data: u32,
    ) -> Inode {
        let format = (layout as u16) << 1;
        let inode = InodeCompact {
            format,
            xattr_count,
            mode: 0,
            nlink: 0,
            size: data_size,
            reserved: 0,
            inode_data,
            inode: 0,
            uid: 0,
            gid: 0,
            reserved2: 0,
        };
        Inode::Compact((1, inode))
    }

    #[test]
    fn xattr_size_matches_icount_formula() {
        let inode = make_compact_inode(Layout::FlatInline, 0, 0, 0);
        assert_eq!(inode.xattr_size(), 0);

        let inode = make_compact_inode(Layout::FlatInline, 0, 1, 0);
        assert_eq!(inode.xattr_size(), size_of::<XattrHeader>());

        let inode = make_compact_inode(Layout::FlatInline, 0, 2, 0);
        assert_eq!(
            inode.xattr_size(),
            size_of::<XattrHeader>() + size_of::<XattrEntry>()
        );
    }

    #[test]
    fn chunk_addr_offset_calculated_correctly() {
        let core = make_core();
        let inode = make_compact_inode(Layout::ChunkBased, core.block_size as u32, 0, 0);
        let plan = core.plan_inode_block_read(&inode, 0).expect("chunk plan");

        match plan {
            BlockPlan::Chunked { addr_offset, .. } => {
                let inode_offset = core.get_inode_offset(inode.id()).expect("inode offset");
                let expected = inode_offset + inode.size() + inode.xattr_size();
                assert_eq!(addr_offset, expected);
            }
            _ => panic!("expected chunked plan"),
        }
    }
}
