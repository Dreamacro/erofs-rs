use alloc::{format, string::ToString};

use binrw::BinRead;
use binrw::BinReaderExt;
use binrw::io::Cursor;
use rustix::fs::FileType;

use crate::types::*;
use crate::{Error, Result};

// Compression and xattr flags may coexist with readable uncompressed files.
// The 48-bit and metabox layouts, and all unknown flags, must be rejected.
const SUPPORTED_INCOMPAT_FEATURES: u32 = 0x007f;
const FEATURE_INCOMPAT_DEVICE_TABLE: u32 = 0x0008;

/// Shared parsing and mapping logic, independent of synchronous/asynchronous I/O.
#[derive(Debug, Clone)]
pub struct EroFSCore {
    pub(crate) super_block: SuperBlock,
    pub(crate) block_size: u64,
}

/// A bounded block read, or a chunk reference that must first be resolved.
pub enum BlockPlan {
    Direct {
        offset: u64,
        size: usize,
    },
    Hole {
        size: usize,
    },
    /// Read a u32 at `addr_offset`, then call `resolve_chunk_read()`.
    Chunked {
        addr_offset: u64,
        block_index: u32,
        size: usize,
    },
}

impl BlockPlan {
    fn direct(offset: u64, size: usize) -> Result<Self> {
        offset
            .checked_add(size as u64)
            .ok_or(Error::Overflow("block read range"))?;
        Ok(Self::Direct { offset, size })
    }
}

impl EroFSCore {
    /// Parse and validate a superblock starting at `SUPER_BLOCK_OFFSET`.
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
        let unsupported = super_block.feature_incompat & !SUPPORTED_INCOMPAT_FEATURES;
        if unsupported != 0 {
            return Err(Error::NotSupported(format!(
                "incompatible filesystem features {:#010x}",
                unsupported
            )));
        }
        // This bit also advertises compression HEAD2, so check the device count.
        if super_block.feature_incompat & FEATURE_INCOMPAT_DEVICE_TABLE != 0
            && super_block.extra_devices != 0
        {
            return Err(Error::NotSupported("multiple devices".to_string()));
        }
        if super_block.build_time_ns >= 1_000_000_000 {
            return Err(Error::InvalidSuperblock(
                "invalid build time nanoseconds".to_string(),
            ));
        }
        let block_size = 1u64 << blk_size_bits;
        Ok(Self {
            super_block,
            block_size,
        })
    }

    /// Validate the format before choosing an on-disk inode structure.
    pub(crate) fn inode_header(data: &[u8]) -> Result<(Layout, usize)> {
        let format: u16 = Cursor::new(data).read_le()?;
        if format & !0x000f != 0 {
            return Err(Error::NotSupported(format!(
                "inode format bits {:#06x}",
                format & !0x000f
            )));
        }
        let layout = Layout::try_from(((format & 0x0e) >> 1) as u8)?;
        let size = if format & 1 == 0 {
            InodeCompact::size()
        } else {
            InodeExtended::size()
        };
        Ok((layout, size))
    }

    /// Parse raw disk fields into validated, version-independent metadata.
    pub(crate) fn parse_inode(&self, data: &[u8], nid: u64) -> Result<Inode> {
        let (layout, inode_size) = Self::inode_header(data)?;
        if data.len() < inode_size {
            return Err(Error::CorruptedData("truncated inode".to_string()));
        }
        let mut cursor = Cursor::new(data);
        let (mut inode, raw_data, xattr_count) = if inode_size == InodeCompact::size() {
            let raw = InodeCompact::read(&mut cursor)?;
            (
                Inode {
                    nid,
                    data_size: u64::from(raw.size),
                    file_type: FileType::from_raw_mode(raw.mode.into()),
                    mode: raw.mode & 0o7777,
                    uid: u32::from(raw.uid),
                    gid: u32::from(raw.gid),
                    nlink: u32::from(raw.nlink),
                    modified: (
                        self.super_block.build_time as i64,
                        self.super_block.build_time_ns,
                    ),
                    data: InodeData::None,
                    inode_size,
                    xattr_size: 0,
                },
                raw.inode_data,
                raw.xattr_count,
            )
        } else {
            let raw = InodeExtended::read(&mut cursor)?;
            (
                Inode {
                    nid,
                    data_size: raw.size,
                    file_type: FileType::from_raw_mode(raw.mode.into()),
                    mode: raw.mode & 0o7777,
                    uid: raw.uid,
                    gid: raw.gid,
                    nlink: raw.nlink,
                    modified: (raw.mtime as i64, raw.mtime_ns),
                    data: InodeData::None,
                    inode_size,
                    xattr_size: 0,
                },
                raw.inode_data,
                raw.xattr_count,
            )
        };
        if inode.modified.1 >= 1_000_000_000 {
            return Err(Error::CorruptedData(
                "invalid inode modification time".to_string(),
            ));
        }
        inode.xattr_size = if xattr_count == 0 {
            0
        } else {
            usize::from(xattr_count - 1) * size_of::<XattrEntry>() + size_of::<XattrHeader>()
        };
        inode.data = match inode.file_type {
            FileType::RegularFile | FileType::Directory | FileType::Symlink => match layout {
                Layout::FlatPlain if raw_data == u32::MAX => InodeData::Hole,
                Layout::FlatPlain => InodeData::FlatPlain {
                    start_block: raw_data,
                },
                Layout::FlatInline => InodeData::FlatInline {
                    start_block: raw_data,
                },
                Layout::CompressedFull => InodeData::CompressedFull,
                Layout::CompressedCompact => InodeData::CompressedCompact,
                Layout::ChunkBased => {
                    let format = raw_data as u16;
                    if format & !(LAYOUT_CHUNK_FORMAT_BITS | LAYOUT_CHUNK_FORMAT_INDEXES) != 0 {
                        return Err(Error::NotSupported(format!(
                            "chunk based format {format:#06x}"
                        )));
                    }
                    let bits =
                        (format & LAYOUT_CHUNK_FORMAT_BITS) as u8 + self.super_block.blk_size_bits;
                    InodeData::ChunkBased {
                        chunk_size: 1u64 << bits,
                        indexes: format & LAYOUT_CHUNK_FORMAT_INDEXES != 0,
                    }
                }
            },
            FileType::CharacterDevice | FileType::BlockDevice => InodeData::Device {
                major: (raw_data >> 8) & 0xfff,
                minor: (raw_data & 0xff) | ((raw_data >> 12) & 0xfff00),
            },
            FileType::Fifo | FileType::Socket => InodeData::None,
            _ => return Err(Error::CorruptedData("invalid inode file type".to_string())),
        };
        if matches!(inode.data, InodeData::FlatInline { .. }) && inode.data_size != 0 {
            let size = (inode.data_size - 1) % self.block_size + 1;
            let offset = self.inode_tail_offset(&inode)?;
            if size > self.block_size - offset % self.block_size {
                return Err(Error::CorruptedData(
                    "inline data crosses metadata block boundary".to_string(),
                ));
            }
        }
        Ok(inode)
    }

    /// Map a logical file offset to at most one block of data.
    /// For `Chunked`, the caller reads the address and calls `resolve_chunk_read()`.
    pub(crate) fn plan_inode_block_read(&self, inode: &Inode, offset: u64) -> Result<BlockPlan> {
        let data_size = inode.data_size();
        if offset >= data_size {
            return Err(Error::OutOfRange(offset, data_size));
        }
        let block_size = self.block_size;
        let block_start = offset / block_size * block_size;
        // Narrow only the bounded size of the buffer to be materialized.
        let size = usize::try_from((data_size - block_start).min(block_size))
            .map_err(|_| Error::Overflow("block read size"))?;
        match inode.data {
            InodeData::Hole => Ok(BlockPlan::Hole { size }),
            InodeData::FlatInline { .. } if data_size - block_start <= block_size => {
                BlockPlan::direct(self.inode_tail_offset(inode)?, size)
            }
            InodeData::FlatPlain { start_block } | InodeData::FlatInline { start_block } => {
                let offset = self
                    .block_offset(start_block)
                    .checked_add(block_start)
                    .ok_or(Error::Overflow("file block offset"))?;
                BlockPlan::direct(offset, size)
            }
            InodeData::CompressedFull | InodeData::CompressedCompact => {
                Err(Error::NotSupported("compressed data".to_string()))
            }
            InodeData::ChunkBased { indexes: true, .. } => Err(Error::NotSupported(
                "chunk based format with indexes".to_string(),
            )),
            InodeData::ChunkBased {
                chunk_size,
                indexes: false,
            } => {
                let chunk_index = block_start / chunk_size;
                let block_index = u32::try_from(block_start % chunk_size / block_size)
                    .map_err(|_| Error::Overflow("chunk block index"))?;
                let index_offset = chunk_index
                    .checked_mul(4)
                    .ok_or(Error::Overflow("chunk index offset"))?;
                let addr_offset = self
                    .inode_tail_offset(inode)?
                    .checked_add(index_offset)
                    .ok_or(Error::Overflow("chunk address offset"))?;
                addr_offset
                    .checked_add(4)
                    .ok_or(Error::Overflow("chunk address range"))?;
                Ok(BlockPlan::Chunked {
                    addr_offset,
                    block_index,
                    size,
                })
            }
            InodeData::Device { .. } | InodeData::None => Err(Error::NotAFile(format!(
                "inode {} has no file data",
                inode.id()
            ))),
        }
    }

    /// Decode a disk chunk address, resolving the null address to a hole.
    pub(crate) fn resolve_chunk_read(
        &self,
        chunk_addr: u32,
        block_index: u32,
        size: usize,
    ) -> Result<BlockPlan> {
        if chunk_addr == u32::MAX {
            return Ok(BlockPlan::Hole { size });
        }
        let block = chunk_addr
            .checked_add(block_index)
            .ok_or(Error::Overflow("chunk block address"))?;
        BlockPlan::direct(self.block_offset(block), size)
    }

    pub(crate) fn get_inode_offset(&self, nid: u64) -> Result<u64> {
        let base = self.block_offset(self.super_block.meta_blk_addr);
        nid.checked_mul(InodeCompact::size() as u64)
            .and_then(|offset| base.checked_add(offset))
            .ok_or(Error::Overflow("inode offset"))
    }

    fn inode_tail_offset(&self, inode: &Inode) -> Result<u64> {
        self.get_inode_offset(inode.id())?
            .checked_add((inode.inode_size + inode.xattr_size()) as u64)
            .ok_or(Error::Overflow("inode tail offset"))
    }

    pub(crate) fn block_offset(&self, block: u32) -> u64 {
        u64::from(block) << self.super_block.blk_size_bits
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
            block_size: 1u64 << super_block.blk_size_bits,
        }
    }

    fn make_compact_inode(
        layout: Layout,
        data_size: u32,
        xattr_count: u16,
        inode_data: u32,
    ) -> Inode {
        let mut raw = [0u8; InodeCompact::size()];
        raw[..2].copy_from_slice(&((layout as u16) << 1).to_le_bytes());
        raw[2..4].copy_from_slice(&xattr_count.to_le_bytes());
        raw[4..6].copy_from_slice(&0o100644u16.to_le_bytes());
        raw[8..12].copy_from_slice(&data_size.to_le_bytes());
        raw[16..20].copy_from_slice(&inode_data.to_le_bytes());
        make_core().parse_inode(&raw, 1).expect("compact inode")
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
                let expected = inode_offset + (InodeCompact::size() + inode.xattr_size()) as u64;
                assert_eq!(addr_offset, expected);
            }
            _ => panic!("expected chunked plan"),
        }
    }
}
