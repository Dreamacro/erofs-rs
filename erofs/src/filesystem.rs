use alloc::{format, string::ToString, vec::Vec};

use binrw::BinRead;
use binrw::BinReaderExt;
use binrw::io::{Cursor, Seek, SeekFrom};

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
        if Inode::is_compact_format(layout) {
            let inode = InodeCompact::read(&mut inode_buf)?;
            Ok(Inode::Compact((nid, inode)))
        } else {
            let inode = InodeExtended::read(&mut inode_buf)?;
            Ok(Inode::Extended((nid, inode)))
        }
    }

    /// Plan a block read operation for the given inode and offset.
    ///
    /// Returns a `BlockPlan` describing what bytes to read.
    /// For `BlockPlan::Chunked`, the caller must perform an additional
    /// read and call `resolve_chunk_read()`.
    pub(crate) fn plan_inode_block_read(&self, inode: &Inode, offset: usize) -> Result<BlockPlan> {
        match inode.layout()? {
            Layout::FlatPlain => {
                let block_count = inode.data_size().div_ceil(self.block_size);
                let block_index = offset / self.block_size;
                if block_index >= block_count {
                    return Err(Error::OutOfRange(block_index, block_count));
                }

                let size = inode.data_size();
                let offset = self.block_offset(inode.raw_block_addr()) as usize
                    + (block_index * self.block_size);
                Ok(BlockPlan::Direct { offset, size })
            }
            Layout::FlatInline => {
                let block_count = inode.data_size().div_ceil(self.block_size);
                let block_index = offset / self.block_size;
                if block_index >= block_count {
                    return Err(Error::OutOfRange(block_index, block_count));
                }

                if block_count != 0 && block_index == block_count - 1 {
                    // tail block
                    let inode_offset = self.get_inode_offset(inode.id());
                    let buf_size = inode.data_size() % self.block_size;
                    let offset = inode_offset as usize + inode.size() + inode.xattr_size();
                    return Ok(BlockPlan::Direct {
                        offset,
                        size: buf_size,
                    });
                }

                let offset = self.block_offset(inode.raw_block_addr()) as usize
                    + (block_index * self.block_size);
                let len = self.block_size.min(inode.data_size());
                Ok(BlockPlan::Direct { offset, size: len })
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
                let chunk_size = 1usize << chunk_bits;
                let chunk_count = inode.data_size().div_ceil(chunk_size);
                let chunk_index = offset >> chunk_bits;
                let chunk_fixed = offset % chunk_size / self.block_size;
                if chunk_index >= chunk_count {
                    return Err(Error::OutOfRange(chunk_index, chunk_count));
                }

                let inode_offset = self.get_inode_offset(inode.id());
                let addr_offset =
                    inode_offset as usize + inode.size() + inode.xattr_size() + (chunk_index * 4);

                Ok(BlockPlan::Chunked {
                    addr_offset,
                    chunk_fixed,
                    chunk_size,
                    data_size: inode.data_size(),
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

        let file_byte_offset = chunk_index * chunk_size + chunk_fixed * self.block_size;
        let remaining = data_size.saturating_sub(file_byte_offset);
        let read_size = remaining.min(self.block_size);

        if read_size == 0 {
            return Err(Error::OutOfRange(file_byte_offset, data_size));
        }

        let offset = self.block_offset(chunk_addr as u32 + chunk_fixed as u32) as usize;
        Ok((offset, read_size))
    }

    pub(crate) fn get_inode_offset(&self, nid: u64) -> u64 {
        self.block_offset(self.super_block.meta_blk_addr) + (nid * InodeCompact::size() as u64)
    }

    pub(crate) fn block_offset(&self, block: u32) -> u64 {
        (block as u64) << self.super_block.blk_size_bits
    }

    // Xattr

    /// Returns true if the long xattr prefix table is stored directly in the
    /// main image (COMPAT_PLAIN_XATTR_PFX feature).
    ///
    /// When false, prefixes are stored in the packed inode's data instead.
    /// Corresponds to kernel `erofs_sb_has_plain_xattr_pfx(sbi)`.
    pub(crate) fn has_plain_xattr_prefix(&self) -> bool {
        self.super_block.feature_compat & FEATURE_COMPAT_PLAIN_XATTR_PFX != 0
    }

    /// Byte offset in the image where the long xattr prefix table starts.
    ///
    /// Corresponds to kernel: `pos = (erofs_off_t)sbi->xattr_prefix_start << 2`
    pub(crate) fn xattr_prefix_table_offset(&self) -> usize {
        (self.super_block.xattr_prefix_start as usize) << 2
    }

    /// Byte offset in the image for a shared xattr entry by its ID.
    ///
    /// Corresponds to kernel:
    /// `it->pos = erofs_pos(sb, sbi->xattr_blkaddr) + shared_id * sizeof(__le32)`
    pub(crate) fn shared_xattr_offset(&self, shared_id: u32) -> usize {
        self.block_offset(self.super_block.xattr_blk_addr) as usize
            + shared_id as usize * size_of::<u32>()
    }

    /// Byte offset of the xattr area for an inode.
    /// = inode start + inode header size
    /// Corresponds to kernel: `pos = erofs_iloc(inode) + vi->inode_isize`
    pub(crate) fn xattr_area_offset(&self, inode: &Inode) -> usize {
        self.get_inode_offset(inode.id()) as usize + inode.size()
    }

    /// Parse all inline xattr entries for an inode.
    /// Returns a list of (full name bytes, value bytes).
    ///
    /// `data` starts at `xattr_area_offset(inode)` and is at least
    /// `inode.xattr_size()` bytes long.
    ///
    /// Corresponds to kernel `erofs_xattr_iter_inline()` +
    /// `erofs_listxattr_foreach()`.
    pub(crate) fn parse_xattrs(&self, inode: &Inode, data: &[u8]) -> Result<Vec<Xattr>> {
        let total_size = inode.xattr_size();
        if total_size == 0 {
            return Ok(Vec::new());
        }

        let xattr_offset = self.xattr_area_offset(inode);
        let xattr_data = data
            .get(xattr_offset..xattr_offset + total_size)
            .ok_or_else(|| {
                Error::CorruptedData(format!(
                    "xattr area out of bounds: offset={} size={}",
                    xattr_offset, total_size
                ))
            })?;

        let mut cursor = Cursor::new(xattr_data);
        let header = XattrHeader::read(&mut cursor)?;

        // read shared_ids manually (variable-length tail of the header)
        let mut shared_ids = Vec::with_capacity(header.shared_count as usize);
        for _ in 0..header.shared_count {
            shared_ids.push(cursor.read_le::<u32>()?);
        }

        let mut result = Vec::new();

        // shared xattr: each shared_id points to a packed XattrEntry + body
        // elsewhere in the image
        for shared_id in &shared_ids {
            let offset = self.shared_xattr_offset(*shared_id);
            let shared_data = data.get(offset..).ok_or_else(|| {
                Error::CorruptedData(format!("shared xattr offset out of bounds: {}", offset))
            })?;
            let mut shared_cursor = Cursor::new(shared_data);
            if let Some(xattr) = Self::read_xattr(&mut shared_cursor)? {
                result.push(xattr);
            }
        }

        // inline xattr entries, each 4-byte aligned
        while cursor.position() < xattr_data.len() as u64 {
            if let Some(xattr) = Self::read_xattr(&mut cursor)? {
                result.push(xattr);
            }
        }

        Ok(result)
    }

    /// Read a single xattr entry (fixed header + variable name_suffix + value
    /// + 4-byte padding) from `cursor`, advancing past the padding.
    ///
    /// Returns `Ok(None)` for entries that use long prefixes or unknown
    /// short-prefix indexes (caller chooses to skip them).
    ///
    /// Corresponds to one iteration of kernel `erofs_listxattr_foreach()`.
    fn read_xattr<R: binrw::io::Read + Seek>(cursor: &mut R) -> Result<Option<Xattr>> {
        let entry = XattrEntry::read_le(cursor)?;

        let mut name_suffix = alloc::vec![0u8; entry.name_len as usize];
        cursor.read_exact(&mut name_suffix)?;

        let mut value = alloc::vec![0u8; entry.value_len as usize];
        cursor.read_exact(&mut value)?;

        let padding = XattrEntry::padding(name_suffix.len() + value.len());
        cursor.seek(SeekFrom::Current(padding as i64))?;

        // long prefix: bit 7 set — not handled in this PR
        if entry.name_index & XattrShortPrefixIndex::LONG_PREFIX != 0 {
            return Ok(None);
        }

        let Ok(name_index) = XattrShortPrefixIndex::try_from(entry.name_index) else {
            return Ok(None);
        };

        let prefix = name_index.prefix().as_bytes();
        let mut name = Vec::with_capacity(prefix.len() + name_suffix.len());
        name.extend_from_slice(prefix);
        name.extend_from_slice(&name_suffix);

        Ok(Some(Xattr { name, value }))
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

    fn make_compact_inode(layout: Layout, data_size: u32, xattr_count: u16, inode_data: u32) -> Inode {
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
        assert_eq!(inode.xattr_size(), size_of::<XattrHeader>() + size_of::<XattrEntry>());
    }

    #[test]
    fn chunk_addr_offset_calculated_correctly() {
        let core = make_core();
        let inode = make_compact_inode(Layout::ChunkBased, core.block_size as u32, 0, 0);
        let plan = core.plan_inode_block_read(&inode, 0).expect("chunk plan");

        match plan {
            BlockPlan::Chunked { addr_offset, .. } => {
                let inode_offset = core.get_inode_offset(inode.id()) as usize;
                let expected = inode_offset + inode.size() + inode.xattr_size();
                assert_eq!(addr_offset, expected);
            }
            _ => panic!("expected chunked plan"),
        }
    }
}
