use alloc::{format, string::ToString, sync::Arc};
use core::ops::Range;

use binrw::BinRead;
use binrw::io::Cursor;
use bytes::Buf;
use rustix::fs::FileType;

use crate::compression::{CompressedRead, EncodedExtent};
use crate::devices::{DeviceInfo, DeviceTable, SLOT_SIZE};
use crate::metadata::{NID_METABOX, ReadSource, primary_inode};
use crate::types::*;
use crate::{Error, Result};

// Reject unknown features rather than misinterpreting their address spaces.
const SUPPORTED_INCOMPAT_FEATURES: u32 = 0x01ff;
const FEATURE_INCOMPAT_DEVICE_TABLE: u32 = 0x0008;

/// Shared parsing and mapping logic, independent of synchronous/asynchronous I/O.
#[derive(Debug, Clone)]
pub struct EroFSCore {
    pub(crate) super_block: SuperBlock,
    pub(crate) block_size: u64,
    devices: Option<Arc<DeviceTable>>,
    pub(crate) metabox_nid: Option<u64>,
}

/// A data extent, or metadata needed to resolve its location.
pub enum BlockPlan {
    Direct {
        source: ReadSource,
        offset: u64,
        size: usize,
    },
    Hole {
        size: usize,
    },
    /// Read a 4- or 8-byte record, then call `resolve_chunk_read()`.
    Chunked {
        source: ReadSource,
        addr_offset: u64,
        format: u16,
        block_index: u32,
        offset_in_block: u64,
        size: usize,
    },
    CompressionMetadata {
        source: ReadSource,
        offset: u64,
        size: usize,
        reader: CompressedRead,
    },
    Encoded(EncodedExtent),
    /// A range in the special packed inode, never another fragment.
    Fragment {
        offset: u64,
        size: u64,
    },
}

impl BlockPlan {
    pub(crate) fn direct(source: ReadSource, offset: u64, size: usize) -> Result<Self> {
        offset
            .checked_add(size as u64)
            .ok_or(Error::Overflow("block read range"))?;
        Ok(Self::Direct {
            source,
            offset,
            size,
        })
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
        if super_block.fixed_nsec >= 1_000_000_000 {
            return Err(Error::InvalidSuperblock(
                "invalid build time nanoseconds".to_string(),
            ));
        }
        if super_block.feature_incompat & crate::metadata::FEATURE_METABOX != 0 {
            primary_inode(super_block.packed_nid)?;
        }
        let block_size = 1u64 << blk_size_bits;
        Ok(Self {
            super_block,
            block_size,
            devices: None,
            metabox_nid: None,
        })
    }

    pub(crate) fn device_table_range(&self, supplied: usize) -> Result<Range<u64>> {
        // The feature bit also means compression HEAD2. Ignore the count when absent.
        let expected = if self.super_block.feature_incompat & FEATURE_INCOMPAT_DEVICE_TABLE != 0 {
            usize::from(self.super_block.extra_devices)
        } else {
            0
        };
        if expected != supplied {
            return Err(Error::OutOfBounds(format!(
                "expected {expected} additional devices, got {supplied}"
            )));
        }
        let start = u64::from(self.super_block.devt_slot_off) * SLOT_SIZE as u64;
        Ok(start..start + (expected * SLOT_SIZE) as u64)
    }

    pub(crate) fn set_device_table(&mut self, data: &[u8]) -> Result<()> {
        self.devices = Some(Arc::new(DeviceTable::parse(
            data,
            self.block_size,
            self.super_block.block_count(),
            self.super_block.feature_incompat & 0x80 != 0,
        )?));
        Ok(())
    }

    pub(crate) fn devices(&self) -> &[DeviceInfo] {
        self.devices.as_ref().map_or(&[], |table| &table.entries)
    }

    pub(crate) fn resolve_device(&self, device: u16, offset: u64, size: u64) -> Result<(u16, u64)> {
        if let Some(table) = &self.devices {
            return table.resolve(device, offset, size);
        }
        if device != 0 {
            return Err(Error::CorruptedData(format!("invalid device ID {device}")));
        }
        offset
            .checked_add(size)
            .ok_or(Error::Overflow("device read range"))?;
        Ok((0, offset))
    }

    /// Validate the format before choosing an on-disk inode structure.
    pub(crate) fn inode_header(mut data: &[u8]) -> Result<(Layout, usize)> {
        let format = data
            .try_get_u16_le()
            .map_err(|_| Error::CorruptedData("truncated inode format".into()))?;
        if format & !0x001f != 0 {
            return Err(Error::NotSupported(format!(
                "inode format bits {:#06x}",
                format & !0x001f
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
        let (mut inode, raw_data, xattr_count, high, wide) = if inode_size == InodeCompact::size() {
            let raw = InodeCompact::read(&mut cursor)?;
            let nlink_one =
                raw.format & 0x10 != 0 && !FileType::from_raw_mode(raw.mode as _).is_dir();
            (
                Inode {
                    nid,
                    data_size: u64::from(raw.size),
                    file_type: FileType::from_raw_mode(raw.mode as _),
                    mode: raw.mode & 0o7777,
                    uid: u32::from(raw.uid),
                    gid: u32::from(raw.gid),
                    nlink: if nlink_one { 1 } else { u32::from(raw.nlink) },
                    modified: (
                        self.super_block
                            .epoch
                            .checked_add(i64::from(raw.mtime))
                            .ok_or(Error::Overflow("compact inode modification time"))?,
                        self.super_block.fixed_nsec,
                    ),
                    data: InodeData::None,
                    inode_size,
                    xattr_size: 0,
                },
                raw.inode_data,
                raw.xattr_count,
                if nlink_one { raw.nlink } else { 0 },
                nlink_one,
            )
        } else {
            let raw = InodeExtended::read(&mut cursor)?;
            (
                Inode {
                    nid,
                    data_size: raw.size,
                    file_type: FileType::from_raw_mode(raw.mode as _),
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
                if self.super_block.feature_incompat & 0x80 != 0 {
                    raw.nb_blocks_hi
                } else {
                    0
                },
                self.super_block.feature_incompat & 0x80 != 0,
            )
        };
        let start_block = u64::from(raw_data) | (u64::from(high) << 32);
        let null_block = if wide {
            (1u64 << 48) - 1
        } else {
            u64::from(u32::MAX)
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
                Layout::FlatPlain if start_block == null_block => InodeData::Hole,
                Layout::FlatPlain => InodeData::FlatPlain { start_block },
                Layout::FlatInline => InodeData::FlatInline { start_block },
                Layout::CompressedFull => InodeData::CompressedFull,
                Layout::CompressedCompact => InodeData::CompressedCompact,
                Layout::ChunkBased => {
                    let format = raw_data as u16;
                    if format
                        & !(LAYOUT_CHUNK_FORMAT_BITS
                            | LAYOUT_CHUNK_FORMAT_INDEXES
                            | LAYOUT_CHUNK_FORMAT_48BIT)
                        != 0
                    {
                        return Err(Error::NotSupported(format!(
                            "chunk based format {format:#06x}"
                        )));
                    }
                    let bits =
                        (format & LAYOUT_CHUNK_FORMAT_BITS) as u8 + self.super_block.blk_size_bits;
                    InodeData::ChunkBased {
                        chunk_size: 1u64 << bits,
                        format,
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

    /// Size of a logical block requested by directory and symlink readers.
    pub(crate) fn block_read_size(&self, inode: &Inode, offset: u64) -> Result<usize> {
        if offset >= inode.data_size() {
            return Err(Error::OutOfRange(offset, inode.data_size()));
        }
        usize::try_from((inode.data_size() - offset).min(self.block_size))
            .map_err(|_| Error::Overflow("block read size"))
    }

    /// Plan data starting exactly at `offset`. Compressed extents can span blocks.
    pub(crate) fn plan_inode_read(&self, inode: &Inode, offset: u64) -> Result<BlockPlan> {
        let data_size = inode.data_size();
        if offset >= data_size {
            return Err(Error::OutOfRange(offset, data_size));
        }
        let block_size = self.block_size;
        let offset_in_block = offset % block_size;
        let size = usize::try_from((data_size - offset).min(block_size - offset_in_block))
            .map_err(|_| Error::Overflow("block read size"))?;
        match inode.data {
            InodeData::Hole => Ok(BlockPlan::Hole { size }),
            InodeData::FlatInline { .. } if offset / block_size == (data_size - 1) / block_size => {
                let offset = self
                    .inode_tail_offset(inode)?
                    .checked_add(offset_in_block)
                    .ok_or(Error::Overflow("inline data offset"))?;
                BlockPlan::direct(self.metadata_source(inode.id())?, offset, size)
            }
            InodeData::FlatPlain { start_block } | InodeData::FlatInline { start_block } => {
                let offset = start_block
                    .checked_mul(block_size)
                    .and_then(|start| start.checked_add(offset))
                    .ok_or(Error::Overflow("file block offset"))?;
                let (device, offset) = self.resolve_device(0, offset, size as u64)?;
                BlockPlan::direct(ReadSource::Device(device), offset, size)
            }
            InodeData::CompressedFull | InodeData::CompressedCompact => {
                CompressedRead::start(self, inode, offset, self.inode_tail_offset(inode)?)
            }
            InodeData::ChunkBased { chunk_size, format } => {
                let unit = Self::chunk_entry_size(format) as u64;
                let chunk_index = offset / chunk_size;
                let block_index = u32::try_from(offset % chunk_size / block_size)
                    .map_err(|_| Error::Overflow("chunk block index"))?;
                let index_offset = chunk_index
                    .checked_mul(unit)
                    .ok_or(Error::Overflow("chunk index offset"))?;
                let start = self
                    .inode_tail_offset(inode)?
                    .checked_add(unit - 1)
                    .ok_or(Error::Overflow("chunk index alignment"))?
                    & !(unit - 1);
                let addr_offset = start
                    .checked_add(index_offset)
                    .ok_or(Error::Overflow("chunk address offset"))?;
                addr_offset
                    .checked_add(unit)
                    .ok_or(Error::Overflow("chunk address range"))?;
                Ok(BlockPlan::Chunked {
                    source: self.metadata_source(inode.id())?,
                    addr_offset,
                    format,
                    block_index,
                    offset_in_block,
                    size,
                })
            }
            InodeData::Device { .. } | InodeData::None => Err(Error::NotAFile(format!(
                "inode {} has no file data",
                inode.id()
            ))),
        }
    }

    pub(crate) fn plan_fragment(
        &self,
        packed: &Inode,
        offset: u64,
        size: u64,
    ) -> Result<BlockPlan> {
        if !packed.is_file()
            || offset
                .checked_add(size)
                .is_none_or(|end| end > packed.data_size())
        {
            return Err(Error::CorruptedData(
                "fragment outside packed inode".to_string(),
            ));
        }
        self.plan_inode_read(packed, offset)
    }

    pub(crate) fn chunk_entry_size(format: u16) -> usize {
        if format & LAYOUT_CHUNK_FORMAT_INDEXES != 0 {
            8
        } else {
            4
        }
    }

    /// Decode a chunk record once for both executors. Holes never access a device.
    pub(crate) fn resolve_chunk_read(
        &self,
        data: &[u8],
        format: u16,
        block_index: u32,
        offset_in_block: u64,
        size: usize,
    ) -> Result<BlockPlan> {
        let mut data = data
            .get(..Self::chunk_entry_size(format))
            .ok_or_else(|| Error::CorruptedData("truncated chunk index".into()))?;
        let indexed = format & LAYOUT_CHUNK_FORMAT_INDEXES != 0;
        let (high, device) = if indexed {
            (data.get_u16_le(), data.get_u16_le())
        } else {
            (0, 0)
        };
        let low = data.get_u32_le();
        let mask = if indexed && format & LAYOUT_CHUNK_FORMAT_48BIT != 0 {
            (1u64 << 48) - 1
        } else {
            u64::from(u32::MAX)
        };
        let address = (u64::from(low) | (u64::from(high) << 32)) & mask;
        if address == mask {
            return Ok(BlockPlan::Hole { size });
        }
        let offset = address
            .checked_mul(self.block_size)
            .ok_or(Error::Overflow("chunk data offset"))?;
        let skip = u64::from(block_index)
            .checked_mul(self.block_size)
            .and_then(|skip| skip.checked_add(offset_in_block))
            .ok_or(Error::Overflow("chunk data offset"))?;
        let end = skip
            .checked_add(size as u64)
            .ok_or(Error::Overflow("chunk read range"))?;
        let device_mask = ((self.devices().len() as u32 + 1).next_power_of_two() - 1) as u16;
        // A chunk's base selects its device, not the requested block inside it.
        let (device, offset) = self.resolve_device(device & device_mask, offset, end)?;
        BlockPlan::direct(ReadSource::Device(device), offset + skip, size)
    }

    pub(crate) fn get_inode_offset(&self, nid: u64) -> Result<u64> {
        let base = match self.metadata_source(nid)? {
            ReadSource::Device(_) => self.block_offset(self.super_block.meta_blk_addr),
            ReadSource::Inode(_) => 0,
        };
        (nid & !NID_METABOX)
            .checked_mul(InodeCompact::size() as u64)
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
    use bytes::BufMut;
    use core::mem::size_of;

    fn make_core() -> EroFSCore {
        let mut data = [0; 128];
        (&mut data[..]).put_u32_le(MAGIC_NUMBER);
        data[12] = 12;
        EroFSCore::new(&data).unwrap()
    }

    fn make_compact_inode(
        layout: Layout,
        data_size: u32,
        xattr_count: u16,
        inode_data: u32,
    ) -> Inode {
        let mut raw = [0u8; InodeCompact::size()];
        (&mut raw[..]).put_u16_le((layout as u16) << 1);
        (&mut raw[2..]).put_u16_le(xattr_count);
        (&mut raw[4..]).put_u16_le(0o100644);
        (&mut raw[8..]).put_u32_le(data_size);
        (&mut raw[16..]).put_u32_le(inode_data);
        make_core().parse_inode(&raw, 1).expect("compact inode")
    }

    #[test]
    fn superblock_creation_time_is_distinct_from_compact_inode_epoch() {
        let mut raw = [0; 128];
        (&mut raw[..]).put_u32_le(MAGIC_NUMBER);
        raw[12] = 9;
        (&mut raw[14..]).put_u16_le(13);
        (&mut raw[24..]).put_i64_le(-2);
        (&mut raw[32..]).put_u32_le(123);
        (&mut raw[36..]).put_u32_le(25);
        raw[80] = 0x80;
        (&mut raw[108..]).put_u32_le(5);
        (&mut raw[112..]).put_u64_le(1 << 35);
        let mut core = EroFSCore::new(&raw).unwrap();
        assert_eq!(core.super_block.created_unix(), Some((3, 123)));
        assert_eq!(core.super_block.root_inode_id(), 1 << 35);
        assert_eq!(core.super_block.block_count(), (13 << 32) | 25);
        let mut inode = [0; 32];
        (&mut inode[4..]).put_u16_le(0o100644);
        inode[12] = 1;
        assert_eq!(
            core.parse_inode(&inode, 1).unwrap().modified_unix(),
            (-1, 123)
        );
        core.super_block.epoch = i64::MAX;
        assert!(core.super_block.created_unix().is_none());
        (&mut raw[32..]).put_u32_le(1_000_000_000);
        assert!(matches!(
            EroFSCore::new(&raw),
            Err(Error::InvalidSuperblock(_))
        ));
    }

    #[test]
    fn wide_addresses_root_ids_and_compact_timestamp_deltas() {
        let mut core = make_core();
        core.super_block.feature_incompat = 0x80;
        core.super_block.root_nid = 13;
        core.super_block.blocks = 25;
        core.super_block.root_nid_wide = 1 << 35;
        assert_eq!(core.super_block.root_inode_id(), 1 << 35);
        assert_eq!(core.super_block.block_count(), (13 << 32) | 25);
        let mut raw = [0; 64];
        raw[0] = 1;
        (&mut raw[4..]).put_u16_le(0o100644);
        (&mut raw[6..]).put_u16_le(0x1234);
        raw[8] = 1;
        (&mut raw[16..]).put_u32_le(u32::MAX);
        let inode = core.parse_inode(&raw, 1).unwrap();
        assert!(
            matches!(core.plan_inode_read(&inode, 0).unwrap(), BlockPlan::Direct { source: ReadSource::Device(0), offset, size: 1 } if offset == 0x1234_ffff_ffff << 12)
        );
        raw[6..8].fill(255);
        assert!(matches!(
            core.parse_inode(&raw, 1).unwrap().data,
            InodeData::Hole
        ));
        raw[..32].fill(0);
        raw[0] = 0x10; // Compact nlink=1; former nlink field holds address MSBs.
        (&mut raw[4..]).put_u16_le(0o100644);
        raw[6] = 2;
        raw[8] = 1;
        raw[12] = 3;
        raw[16] = 3;
        core.super_block.epoch = -2;
        let inode = core.parse_inode(&raw, 1).unwrap();
        assert_eq!((inode.nlink(), inode.modified_unix()), (1, (1, 0)));
        assert!(
            matches!(core.plan_inode_read(&inode, 0).unwrap(), BlockPlan::Direct { source: ReadSource::Device(0), offset, size: 1 } if offset == ((2u64 << 32) | 3) << 12)
        );
        core.super_block.epoch = i64::MAX;
        assert!(matches!(core.parse_inode(&raw, 1), Err(Error::Overflow(_))));
        core.super_block.feature_incompat = 0;
        assert_eq!(core.super_block.root_inode_id(), 13);
        assert_eq!(core.super_block.block_count(), 25);
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
    fn truncated_scalar_fields_return_errors() {
        let core = make_core();
        let data = [0; 8];
        for end in 0..2 {
            assert!(EroFSCore::inode_header(&data[..end]).is_err());
        }
        for format in [0, 0x20, 0x60] {
            for end in 0..EroFSCore::chunk_entry_size(format) {
                assert!(
                    core.resolve_chunk_read(&data[..end], format, 0, 0, 1)
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn chunk_addr_offset_calculated_correctly() {
        let core = make_core();
        let inode = make_compact_inode(Layout::ChunkBased, core.block_size as u32, 0, 0);
        let plan = core.plan_inode_read(&inode, 0).expect("chunk plan");

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
