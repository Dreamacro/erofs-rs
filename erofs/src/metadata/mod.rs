//! Metadata locations and bounded reads in the primary image or a special inode.

use bytes::Buf;
use core::ops::Range;

use crate::{
    Error, Result,
    filesystem::EroFSCore,
    types::{Inode, SB_EXTSLOT_SIZE, SUPER_BLOCK_OFFSET, SuperBlock},
};

#[cfg(test)]
mod tests;

pub const NID_METABOX: u64 = 1 << 63;
pub const FEATURE_METABOX: u32 = 0x100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadSource {
    Device(u16),
    /// Only the primary-resident metabox and packed inodes are used as sources.
    Inode(u64),
}

impl EroFSCore {
    pub(crate) fn extension_range(&self) -> Result<Range<u64>> {
        if self.super_block.feature_incompat & FEATURE_METABOX != 0
            && self.super_block.ext_slots == 0
        {
            return Err(Error::CorruptedData(
                "missing metabox superblock extension".into(),
            ));
        }
        let start = SUPER_BLOCK_OFFSET + SuperBlock::size() as u64;
        Ok(start..start + u64::from(self.super_block.ext_slots) * SB_EXTSLOT_SIZE as u64)
    }

    pub(crate) fn set_extensions(&mut self, mut data: &[u8]) -> Result<()> {
        let range = self.extension_range()?;
        if data.len() as u64 != range.end - range.start {
            return Err(Error::CorruptedData(
                "truncated superblock extensions".into(),
            ));
        }
        if self.super_block.feature_incompat & FEATURE_METABOX != 0 {
            let nid = data.get_u64_le();
            primary_inode(nid)?;
            self.metabox_nid = Some(nid);
        }
        Ok(())
    }

    pub(crate) fn metadata_source(&self, nid: u64) -> Result<ReadSource> {
        if nid & NID_METABOX == 0 {
            Ok(ReadSource::Device(0))
        } else {
            self.metabox_nid.map(ReadSource::Inode).ok_or_else(|| {
                Error::CorruptedData("metabox inode without filesystem feature".into())
            })
        }
    }
}

/// Special inode metadata cannot itself live in the metabox. This bounds nested
/// reads to one special-inode layer; packed-inode fragment self-references are
/// rejected by the shared compression mapper.
pub fn primary_inode(nid: u64) -> Result<()> {
    if nid & NID_METABOX != 0 {
        return Err(Error::CorruptedData(
            "special inode resides in metabox".into(),
        ));
    }
    Ok(())
}

pub fn validate_range(inode: &Inode, offset: u64, size: usize) -> Result<()> {
    let end = offset
        .checked_add(size as u64)
        .ok_or(Error::Overflow("inode read range"))?;
    if end > inode.data_size() {
        return Err(Error::OutOfRange(end, inode.data_size()));
    }
    Ok(())
}
