// SPDX-License-Identifier: MIT
// Compact-index decoding adapted from erofs-utils/lib/zmap.c (MIT option).
// Copyright (C) 2018-2019 HUAWEI, Inc.
// Authors: Gao Xiang <xiang@kernel.org>, Huang Jianan <huangjianan@oppo.com>.
// See LICENSE-MIT for permission and warranty terms.

//! EROFS compressed-extent mapping. I/O stays in the sync/async executors.

use alloc::{format, string::ToString, vec::Vec};
use binrw::{BinRead, BinReaderExt, io::Cursor};
use core::ops::Range;

use crate::{
    Error, Result,
    filesystem::{BlockPlan, EroFSCore},
    types::{Inode, InodeData, MapHeader, SB_EXTSLOT_SIZE, SUPER_BLOCK_OFFSET, SuperBlock},
};

const MAX_ENCODED_SIZE: u64 = 1024 * 1024;
const MAX_DECODED_SIZE: u64 = 12 * 1024 * 1024;
const CBLKCNT: u16 = 1 << 11;
const MAX_LZMA_DICT_SIZE: u32 = 8 * 1024 * 1024;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy)]
struct Head {
    start: u64,
    block: u32,
    kind: u8,
    partial: bool,
}

struct NonHead {
    back: u16,
    blocks: Option<u16>,
}

enum Entry {
    Head(Head),
    NonHead(NonHead),
}

#[derive(Clone, Copy)]
struct Index {
    start: u64,
    end: u64,
    count: u64,
    file_size: u64,
    block_bits: u8,
    compact: bool,
    initial: u64,
    middle: u64,
    algorithms: u8,
    interlaced: bool,
    big1: bool,
    big2: bool,
    inline_size: u16,
    fragment: Option<u32>,
}

impl Index {
    fn new(core: &EroFSCore, inode: &Inode, header_offset: u64, data: &[u8]) -> Result<Self> {
        let header = MapHeader::read(&mut Cursor::new(data))?;
        let compact = matches!(inode.data, InodeData::CompressedCompact);
        let allowed = if compact { 0x3f } else { 0x3e };
        let big1 = header.advise & 2 != 0;
        let big2 = header.advise & 4 != 0;
        if (big1 || big2) && core.super_block.feature_incompat & 2 == 0 {
            return Err(Error::CorruptedData(
                "block-count indexes without filesystem feature".to_string(),
            ));
        }
        if compact && big1 != big2 {
            return Err(Error::CorruptedData(
                "inconsistent compact block-count flags".to_string(),
            ));
        }
        if header.advise & !allowed != 0 {
            return Err(Error::NotSupported(format!(
                "compressed inode advice {:#06x}",
                header.advise
            )));
        }
        if header.clusterbits & !15 != 0 {
            return Err(Error::NotSupported("reserved cluster bits".to_string()));
        }
        let block_bits = core.super_block.blk_size_bits + (header.clusterbits & 15);
        if header.advise & 8 != 0
            && (core.super_block.feature_incompat & 0x10 == 0
                || header.advise & 0x20 != 0
                || header.data_size == 0)
        {
            return Err(Error::CorruptedData(
                "invalid inline compression flags or size".to_string(),
            ));
        }
        if compact && block_bits > 14 {
            return Err(Error::NotSupported(
                "compact compression indexes above 16 KiB".to_string(),
            ));
        }
        let start = header_offset
            .checked_add(if compact { 8 } else { 16 })
            .ok_or(Error::Overflow("compression index start"))?;
        let count = inode.data_size().div_ceil(1 << block_bits);
        // Compact pack regions are sized in filesystem blocks, even when
        // logical clusters are larger. Only `count` entries are addressable.
        let total = inode.data_size().div_ceil(core.block_size);
        let initial = if compact {
            (((32 - start % 32) / 4) & 7).min(total)
        } else {
            0
        };
        let middle = if compact && header.advise & 1 != 0 {
            (total - initial) / 16 * 16
        } else {
            0
        };
        if middle != 0 && block_bits > 12 {
            return Err(Error::NotSupported(
                "2-byte compression indexes above 4 KiB".to_string(),
            ));
        }
        let bytes = if compact {
            // A 4-byte index pack holds two entries plus its shared block address.
            (initial * 4 + middle * 2 + (total - initial - middle) * 4).next_multiple_of(8)
        } else {
            count * 8
        };
        let end = start
            .checked_add(bytes)
            .ok_or(Error::Overflow("compression index end"))?;
        let mut layout = Self {
            start,
            end,
            count,
            file_size: inode.data_size(),
            block_bits,
            compact,
            initial,
            middle,
            algorithms: header.algorithmtype,
            interlaced: header.advise & 0x10 != 0,
            big1,
            big2,
            inline_size: if header.advise & 8 != 0 {
                header.data_size
            } else {
                0
            },
            fragment: (header.advise & 0x20 != 0).then(|| header.fragmentoff()),
        };
        if compact {
            let (offset, _, size) = layout.pack(count - 1)?;
            layout.end = offset + size as u64;
        }
        Ok(layout)
    }

    /// Location of the complete pack, entry within that pack, and pack size.
    fn pack(&self, index: u64) -> Result<(u64, usize, usize)> {
        if index >= self.count {
            return Err(Error::CorruptedData(
                "compression index outside file".to_string(),
            ));
        }
        if !self.compact {
            return Ok((self.start + index * 8, 0, 8));
        }
        let (position, stride, size) = if index < self.initial {
            (self.start + index * 4, 4, 8)
        } else if index < self.initial + self.middle {
            (
                self.start + self.initial * 4 + (index - self.initial) * 2,
                2,
                32,
            )
        } else {
            (
                self.start
                    + self.initial * 4
                    + self.middle * 2
                    + (index - self.initial - self.middle) * 4,
                4,
                8,
            )
        };
        let within = (position % size as u64) as usize;
        Ok((position - within as u64, within / stride, size))
    }

    fn entry(&self, index: u64, page_offset: u64, page: &[u8]) -> Result<Option<Entry>> {
        let (offset, slot, size) = self.pack(index)?;
        let Some(relative) = offset
            .checked_sub(page_offset)
            .and_then(|v| usize::try_from(v).ok())
        else {
            return Ok(None);
        };
        let Some(data) = relative
            .checked_add(size)
            .and_then(|end| page.get(relative..end))
        else {
            return Ok(None);
        };
        let (kind, cluster_offset, block, partial) = if !self.compact {
            let mut cursor = Cursor::new(data);
            let advice: u16 = cursor.read_le()?;
            let cluster_offset: u16 = cursor.read_le()?;
            let block: u32 = cursor.read_le()?;
            if advice & !0x8003 != 0 {
                return Err(Error::NotSupported("compression index advice".to_string()));
            }
            let kind = (advice & 3) as u8;
            if kind == 2 {
                return self.nonhead(block as u16).map(|n| Some(Entry::NonHead(n)));
            }
            (kind, cluster_offset, block, advice & 0x8000 != 0)
        } else {
            let slots = if size == 32 { 16 } else { 2 };
            let low_bits = self.block_bits.max(12);
            let entry_bits = (size - 4) * 8 / slots;
            let field = |slot: usize| -> Result<(u16, u8)> {
                let bit = slot * entry_bits;
                let word: u32 = Cursor::new(&data[bit / 8..]).read_le()?;
                let word = word >> (bit % 8);
                Ok((
                    (word & ((1 << low_bits) - 1)) as u16,
                    ((word >> low_bits) & 3) as u8,
                ))
            };
            let (low, kind) = field(slot)?;
            if kind == 2 {
                if low & CBLKCNT != 0 || slot + 1 != slots {
                    return self.nonhead(low).map(|n| Some(Entry::NonHead(n)));
                }
                // The final NONHEAD stores a forward distance, not a back distance.
                let (previous, kind) = field(slot - 1)?;
                let back = if kind == 2 {
                    self.nonhead(previous)?.back + 1
                } else {
                    1
                };
                return Ok(Some(Entry::NonHead(NonHead { back, blocks: None })));
            }
            let mut blocks = u32::from(!self.big1);
            let mut previous = slot;
            while previous != 0 {
                previous -= 1;
                let (distance, kind) = field(previous)?;
                if kind == 2 {
                    let nonhead = self.nonhead(distance)?;
                    if self.big1 {
                        if let Some(count) = nonhead.blocks {
                            previous = previous.saturating_sub(1);
                            blocks += u32::from(count);
                        } else {
                            let Some(head) = previous.checked_sub(usize::from(nonhead.back - 2))
                            else {
                                break;
                            };
                            previous = head;
                        }
                        continue;
                    }
                    let Some(head) = previous.checked_sub(usize::from(nonhead.back)) else {
                        break;
                    };
                    previous = head;
                }
                blocks += 1;
            }
            let base: u32 = Cursor::new(&data[size - 4..]).read_le()?;
            // Compact predictors wrap at u32; legacy -1 encodes block zero.
            (kind, low, base.wrapping_add(blocks), false)
        };
        if u64::from(cluster_offset) >= 1u64 << self.block_bits {
            return Err(Error::CorruptedData(
                "invalid compression cluster offset".to_string(),
            ));
        }
        let start = (index << self.block_bits)
            .checked_add(u64::from(cluster_offset))
            .filter(|start| *start <= self.file_size)
            .ok_or_else(|| {
                Error::CorruptedData("compressed extent starts outside file".to_string())
            })?;
        Ok(Some(Entry::Head(Head {
            start,
            block,
            kind,
            partial,
        })))
    }

    fn nonhead(&self, encoded: u16) -> Result<NonHead> {
        if encoded & CBLKCNT != 0 {
            if !(self.big1 || self.big2) || (encoded == CBLKCNT && self.fragment.is_none()) {
                return Err(Error::CorruptedData(
                    "invalid compressed block count".to_string(),
                ));
            }
            let blocks = encoded & !CBLKCNT;
            return Ok(NonHead {
                back: 1,
                blocks: Some(blocks),
            });
        }
        if encoded == 0 || (self.compact && self.big1 && encoded == 1) {
            return Err(Error::CorruptedData(
                "invalid compression lookback distance".to_string(),
            ));
        }
        Ok(NonHead {
            back: encoded,
            blocks: None,
        })
    }

    fn window(&self, index: u64) -> Result<(u64, usize)> {
        let (offset, _, _) = self.pack(index)?;
        // Read bounded windows rather than issuing one remote read per index.
        let start = (offset & !511).max(self.start);
        let end = (offset | 511).saturating_add(1).min(self.end);
        Ok((start, (end - start) as usize))
    }
}

#[derive(Clone, Copy)]
struct ExtentIndex {
    start: u64,
    count: u64,
    record_size: u64,
    cluster_bits: u8,
    advise: u16,
}

#[derive(Clone, Copy)]
struct Extent {
    start: u64,
    offset: u64,
    plen: u32,
}

enum ExtentFormat {
    Plain { interlaced: bool },
    Compressed { algorithm: u8, partial: bool },
}

struct PhysicalExtent {
    offset: u64,
    size: u64,
    format: ExtentFormat,
}

#[derive(Clone, Copy)]
enum Stage {
    ConfigLength {
        remaining: u16,
    },
    Config {
        remaining: u16,
    },
    Header,
    FindHead {
        layout: Index,
        index: u64,
    },
    FindEnd {
        layout: Index,
        index: u64,
        head: Head,
        blocks: Option<u16>,
    },
    ExtentBase {
        layout: ExtentIndex,
    },
    Extents {
        layout: ExtentIndex,
        left: u64,
        right: u64,
        end: u64,
        head: Option<Extent>,
        physical: u64,
    },
}

/// A bounded, resumable metadata walk shared by both I/O implementations.
pub struct CompressedRead {
    inode: Inode,
    offset: u64,
    header_offset: u64,
    lzma_dict_size: u32,
    zstd_window_size: u32,
    stage: Stage,
}

impl CompressedRead {
    pub(crate) fn start(
        core: &EroFSCore,
        inode: &Inode,
        offset: u64,
        tail: u64,
    ) -> Result<BlockPlan> {
        if core.block_size > MAX_ENCODED_SIZE {
            return Err(Error::NotSupported(
                "compressed physical cluster above 1 MiB".to_string(),
            ));
        }
        let header_offset = tail
            .checked_add(7)
            .ok_or(Error::Overflow("compression header offset"))?
            & !7;
        let mut reader = Self {
            inode: *inode,
            offset,
            header_offset,
            lzma_dict_size: 0,
            zstd_window_size: 0,
            stage: Stage::Header,
        };
        let remaining = core.super_block.compr_algs;
        if core.super_block.feature_incompat & 2 != 0 && remaining != 0 {
            if remaining & !0x0f != 0 {
                return Err(Error::NotSupported(
                    "compression algorithm bitmap".to_string(),
                ));
            }
            reader.stage = Stage::ConfigLength { remaining };
            let config_offset = SUPER_BLOCK_OFFSET
                + SuperBlock::size() as u64
                + u64::from(core.super_block.ext_slots) * SB_EXTSLOT_SIZE as u64;
            reader.request(config_offset, 2)
        } else {
            reader.request(header_offset, MapHeader::size())
        }
    }

    fn request(self, offset: u64, size: usize) -> Result<BlockPlan> {
        offset
            .checked_add(size as u64)
            .ok_or(Error::Overflow("compression metadata range"))?;
        Ok(BlockPlan::CompressionMetadata {
            offset,
            size,
            reader: self,
        })
    }

    pub(crate) fn resume(
        mut self,
        core: &EroFSCore,
        page_offset: u64,
        data: &[u8],
    ) -> Result<BlockPlan> {
        match self.stage {
            Stage::ConfigLength { remaining } => {
                let size: u16 = Cursor::new(data).read_le()?;
                let minimum = if remaining.trailing_zeros() <= 1 {
                    14
                } else {
                    6
                };
                if size < minimum {
                    return Err(Error::CorruptedData(
                        "truncated compression configuration".to_string(),
                    ));
                }
                self.stage = Stage::Config { remaining };
                return self.request(page_offset + 2, usize::from(size));
            }
            Stage::Config { remaining } => {
                match remaining.trailing_zeros() {
                    1 => {
                        let mut cursor = Cursor::new(data);
                        self.lzma_dict_size = cursor.read_le()?;
                        let format: u16 = cursor.read_le()?;
                        if !(4096..=MAX_LZMA_DICT_SIZE).contains(&self.lzma_dict_size) {
                            return Err(Error::CorruptedData(
                                "invalid MicroLZMA dictionary size".to_string(),
                            ));
                        }
                        if format != 0 {
                            return Err(Error::NotSupported(format!("LZMA format {format}")));
                        }
                    }
                    2 if !(8..=15).contains(&data[0]) => {
                        return Err(Error::CorruptedData(
                            "invalid DEFLATE window bits".to_string(),
                        ));
                    }
                    3 => {
                        if data[0] != 0 {
                            return Err(Error::NotSupported(format!("Zstd format {}", data[0])));
                        }
                        if data[1] > 10 {
                            return Err(Error::CorruptedData(
                                "Zstd window above 1 MiB".to_string(),
                            ));
                        }
                        self.zstd_window_size = 1 << (10 + data[1]);
                    }
                    _ => {}
                }
                let remaining = remaining & (remaining - 1);
                if remaining != 0 {
                    let offset = page_offset
                        .checked_add(data.len() as u64)
                        .and_then(|end| end.checked_add(3))
                        .ok_or(Error::Overflow("compression configuration offset"))?
                        & !3;
                    self.stage = Stage::ConfigLength { remaining };
                    return self.request(offset, 2);
                }
                self.stage = Stage::Header;
                let offset = self.header_offset;
                return self.request(offset, MapHeader::size());
            }
            Stage::Header => {
                if data[7] & 0x80 != 0 {
                    let offset = u64::from_le_bytes(data[..8].try_into().unwrap()) & !(1 << 63);
                    return self.fragment(core, offset, 0, self.inode.data_size());
                }
                if matches!(self.inode.data, InodeData::CompressedFull) && data[4] & 1 != 0 {
                    return self.start_extents(core, data);
                }
                let layout = Index::new(core, &self.inode, self.header_offset, data)?;
                self.stage = Stage::FindHead {
                    layout,
                    index: self.offset >> layout.block_bits,
                };
            }
            Stage::ExtentBase { layout } => {
                let physical = Cursor::new(data).read_le()?;
                self.stage = Stage::Extents {
                    layout,
                    left: 0,
                    right: (self.offset >> layout.cluster_bits) + 1,
                    end: self.inode.data_size(),
                    head: None,
                    physical,
                };
                return self.resume_extents(core, page_offset, data);
            }
            Stage::Extents { .. } => return self.resume_extents(core, page_offset, data),
            _ => {}
        }
        loop {
            let (layout, index) = match self.stage {
                Stage::FindHead { layout, index } | Stage::FindEnd { layout, index, .. } => {
                    (layout, index)
                }
                _ => unreachable!("header and configuration handled above"),
            };
            if let Stage::FindEnd { head, blocks, .. } = self.stage {
                if index == layout.count {
                    return self.finish(core, layout, head, layout.file_size, blocks);
                }
                if (index << layout.block_bits).saturating_sub(head.start) > MAX_DECODED_SIZE {
                    return Err(Error::NotSupported(
                        "compressed extent above 12 MiB".to_string(),
                    ));
                }
            } else if self.offset.saturating_sub(index << layout.block_bits)
                > MAX_DECODED_SIZE + (1 << layout.block_bits)
            {
                return Err(Error::NotSupported(
                    "compression lookback above 12 MiB".to_string(),
                ));
            }
            let Some(entry) = layout.entry(index, page_offset, data)? else {
                let (offset, size) = layout.window(index)?;
                return self.request(offset, size);
            };
            match (self.stage, entry) {
                (Stage::FindHead { .. }, Entry::NonHead(nonhead)) => {
                    let index = index.checked_sub(u64::from(nonhead.back)).ok_or_else(|| {
                        Error::CorruptedData("compression lookback before file start".to_string())
                    })?;
                    self.stage = Stage::FindHead { layout, index };
                }
                (Stage::FindHead { .. }, Entry::Head(head)) if head.start > self.offset => {
                    let index = index.checked_sub(1).ok_or_else(|| {
                        Error::CorruptedData("compressed file has no initial head".to_string())
                    })?;
                    self.stage = Stage::FindHead { layout, index };
                }
                (Stage::FindHead { .. }, Entry::Head(head)) => {
                    self.stage = Stage::FindEnd {
                        layout,
                        index: index + 1,
                        head,
                        blocks: None,
                    };
                }
                (Stage::FindEnd { head, blocks, .. }, Entry::Head(next)) => {
                    return self.finish(core, layout, head, next.start, blocks);
                }
                (Stage::FindEnd { head, blocks, .. }, Entry::NonHead(nonhead)) => {
                    let distance = index - (head.start >> layout.block_bits);
                    let needs_count = distance == 1
                        && if head.kind != 3 {
                            layout.big1
                        } else {
                            layout.big2
                        };
                    // A fragment has no physical blocks, even when its HEAD2
                    // slot does not advertise BIG_PCLUSTER_2.
                    let fragment_count =
                        layout.fragment.is_some() && distance == 1 && nonhead.blocks == Some(0);
                    if nonhead.blocks.is_some() != needs_count && !fragment_count {
                        return Err(Error::CorruptedData(
                            "misplaced or missing compressed block count".to_string(),
                        ));
                    }
                    if u64::from(nonhead.back) > distance {
                        return Err(Error::CorruptedData(
                            "compression lookback crosses extent head".to_string(),
                        ));
                    }
                    // Bounded linear scan; use validated forward skips
                    // only if profiling shows index decoding dominates.
                    self.stage = Stage::FindEnd {
                        layout,
                        index: index + 1,
                        head,
                        blocks: blocks.or(nonhead.blocks),
                    };
                }
                _ => unreachable!("only index stages reach the metadata walk"),
            }
        }
    }

    fn finish(
        self,
        core: &EroFSCore,
        layout: Index,
        head: Head,
        end: u64,
        blocks: Option<u16>,
    ) -> Result<BlockPlan> {
        if end == layout.file_size
            && let Some(low) = layout.fragment
        {
            let offset = u64::from(low)
                | if layout.compact {
                    0
                } else {
                    u64::from(head.block) << 32
                };
            return self.fragment(core, offset, head.start, end);
        }
        if blocks == Some(0) {
            return Err(Error::CorruptedData(
                "zero block count outside fragment tail".to_string(),
            ));
        }
        let mut offset = core.block_offset(head.block);
        let big = if head.kind == 3 {
            layout.big2
        } else {
            layout.big1
        };
        let mut size = if head.kind == 0 || !big {
            1u64 << layout.block_bits
        } else {
            u64::from(blocks.unwrap_or(1)) * core.block_size
        };
        if end == layout.file_size && layout.inline_size != 0 {
            offset = layout.end;
            size = u64::from(layout.inline_size);
            if size > core.block_size - offset % core.block_size {
                return Err(Error::CorruptedData(
                    "inline compressed data crosses metadata block boundary".to_string(),
                ));
            }
        }
        if head.kind == 3 && core.super_block.feature_incompat & 8 == 0 {
            return Err(Error::CorruptedData(
                "HEAD2 without filesystem feature".to_string(),
            ));
        }
        let format = if head.kind == 0 {
            ExtentFormat::Plain {
                interlaced: layout.interlaced,
            }
        } else {
            ExtentFormat::Compressed {
                algorithm: if head.kind == 3 {
                    layout.algorithms >> 4
                } else {
                    layout.algorithms & 15
                },
                partial: head.partial,
            }
        };
        self.map_extent(
            core,
            head.start..end,
            PhysicalExtent {
                offset,
                size,
                format,
            },
        )
    }

    fn fragment(&self, core: &EroFSCore, offset: u64, start: u64, end: u64) -> Result<BlockPlan> {
        if core.super_block.feature_incompat & 0x20 == 0
            || core.super_block.packed_nid == 0
            || core.super_block.packed_nid == self.inode.id()
            || start > self.offset
            || end <= self.offset
            || end > self.inode.data_size()
        {
            return Err(Error::CorruptedData(
                "invalid packed fragment reference".to_string(),
            ));
        }
        let offset = offset
            .checked_add(self.offset - start)
            .ok_or(Error::Overflow("fragment offset"))?;
        let size = end - self.offset;
        offset
            .checked_add(size)
            .ok_or(Error::Overflow("fragment range"))?;
        Ok(BlockPlan::Fragment { offset, size })
    }

    fn start_extents(mut self, core: &EroFSCore, data: &[u8]) -> Result<BlockPlan> {
        let header = MapHeader::read(&mut Cursor::new(data))?;
        if header.advise & !0x37 != 0 {
            return Err(Error::NotSupported("extent header advice".to_string()));
        }
        let record_size = 4u64 << ((header.advise >> 1) & 3);
        let cluster_bits = core.super_block.blk_size_bits + (header.clusterbits & 15);
        let count = if record_size <= 8 {
            if header.clusterbits & !15 != 0 {
                return Err(Error::NotSupported(
                    "extent logical cluster size".to_string(),
                ));
            }
            self.inode.data_size().div_ceil(1 << cluster_bits)
        } else {
            u64::from(u32::from_le_bytes(data[..4].try_into().unwrap()))
                | (u64::from(u16::from_le_bytes(data[6..8].try_into().unwrap())) << 32)
        };
        let start = self
            .header_offset
            .checked_add(8 + record_size - 1)
            .ok_or(Error::Overflow("extent index start"))?
            & !(record_size - 1);
        let start = start
            .checked_add(u64::from(record_size == 4) * 8)
            .ok_or(Error::Overflow("extent index start"))?;
        if count == 0 || count > self.inode.data_size() {
            return Err(Error::CorruptedData("invalid extent count".to_string()));
        }
        start
            .checked_add(
                count
                    .checked_mul(record_size)
                    .ok_or(Error::Overflow("extent index size"))?,
            )
            .ok_or(Error::Overflow("extent index range"))?;
        let layout = ExtentIndex {
            start,
            count,
            record_size,
            cluster_bits,
            advise: header.advise,
        };
        if record_size == 4 {
            self.stage = Stage::ExtentBase { layout };
            return self.request(start - 8, 8);
        }
        let (left, right) = if record_size == 8 {
            let index = self.offset >> cluster_bits;
            (index, index + 1)
        } else {
            (0, count)
        };
        self.stage = Stage::Extents {
            layout,
            left,
            right,
            end: self.inode.data_size(),
            head: None,
            physical: 0,
        };
        let offset = self.header_offset;
        self.resume_extents(core, offset, data)
    }

    fn resume_extents(
        mut self,
        core: &EroFSCore,
        page_offset: u64,
        data: &[u8],
    ) -> Result<BlockPlan> {
        loop {
            let Stage::Extents {
                layout,
                mut left,
                mut right,
                mut end,
                mut head,
                mut physical,
            } = self.stage
            else {
                unreachable!()
            };
            if left == right {
                let head = head.ok_or_else(|| {
                    Error::CorruptedData("compressed file has no initial extent".to_string())
                })?;
                let fragment = end == self.inode.data_size() && layout.advise & 0x20 != 0;
                let interlaced = layout.advise & 0x10 != 0
                    && (head.offset | u64::from(head.plen & 0x1f_ffff))
                        .is_multiple_of(core.block_size);
                return self.finish_extent(core, head, end, fragment, interlaced);
            }
            let index = if layout.record_size <= 8 {
                left
            } else {
                left + (right - left) / 2
            };
            let at = layout.start + index * layout.record_size;
            let entry = at
                .checked_sub(page_offset)
                .and_then(|v| usize::try_from(v).ok())
                .and_then(|v| {
                    v.checked_add(layout.record_size as usize)
                        .and_then(|end| data.get(v..end))
                });
            let Some(entry) = entry else {
                let offset = (at & !511).max(layout.start);
                let end = (at | 511)
                    .saturating_add(1)
                    .min(layout.start + layout.count * layout.record_size);
                return self.request(offset, (end - offset) as usize);
            };
            let mut cursor = Cursor::new(entry);
            let plen: u32 = cursor.read_le()?;
            let mut offset = if layout.record_size == 4 {
                physical
            } else {
                u64::from(cursor.read_le::<u32>()?)
            };
            let start = if layout.record_size <= 8 {
                index << layout.cluster_bits
            } else {
                offset |= u64::from(cursor.read_le::<u32>()?) << 32;
                let low: u32 = cursor.read_le()?;
                u64::from(low)
                    | if layout.record_size == 32 {
                        u64::from(cursor.read_le::<u32>()?) << 32
                    } else {
                        0
                    }
            };
            if start >= end || head.is_some_and(|previous| start <= previous.start) {
                return Err(Error::CorruptedData(
                    "unordered compression extents".to_string(),
                ));
            }
            if layout.record_size <= 8 {
                if layout.record_size == 4 {
                    if index + 1 == layout.count && layout.advise & 0x20 != 0 {
                        offset = 0; // The last record contains a fragment offset, not a length.
                    } else {
                        if plen & 0x07e0_0000 != 0 || u64::from(plen & 0x1f_ffff) > MAX_ENCODED_SIZE
                        {
                            return Err(Error::CorruptedData(
                                "invalid extent physical length".to_string(),
                            ));
                        }
                        physical = offset
                            .checked_add(u64::from(plen & 0x1f_ffff))
                            .ok_or(Error::Overflow("extent physical offset"))?;
                    }
                }
                head = Some(Extent {
                    start,
                    offset,
                    plen,
                });
                left += 1;
                if left == right {
                    end = start
                        .saturating_add(1 << layout.cluster_bits)
                        .min(self.inode.data_size());
                }
            } else if start > self.offset {
                end = start;
                right = index;
            } else {
                head = Some(Extent {
                    start,
                    offset,
                    plen,
                });
                left = index + 1;
            }
            self.stage = Stage::Extents {
                layout,
                left,
                right,
                end,
                head,
                physical,
            };
        }
    }

    fn finish_extent(
        self,
        core: &EroFSCore,
        head: Extent,
        end: u64,
        fragment: bool,
        interlaced: bool,
    ) -> Result<BlockPlan> {
        if fragment {
            if head.offset > u64::from(u32::MAX) {
                return Err(Error::CorruptedData(
                    "invalid fragment high offset".to_string(),
                ));
            }
            return self.fragment(
                core,
                u64::from(head.plen) | (head.offset << 32),
                head.start,
                end,
            );
        }
        if head.plen & 0x07e0_0000 != 0 {
            return Err(Error::CorruptedData(
                "reserved extent length bits".to_string(),
            ));
        }
        let format = match head.plen >> 28 {
            0 => ExtentFormat::Plain { interlaced },
            format => ExtentFormat::Compressed {
                algorithm: format as u8 - 1,
                partial: head.plen & (1 << 27) != 0,
            },
        };
        self.map_extent(
            core,
            head.start..end,
            PhysicalExtent {
                offset: head.offset,
                size: u64::from(head.plen & 0x1f_ffff),
                format,
            },
        )
    }

    fn map_extent(
        self,
        core: &EroFSCore,
        logical: Range<u64>,
        physical: PhysicalExtent,
    ) -> Result<BlockPlan> {
        if !logical.contains(&self.offset) || logical.end > self.inode.data_size() {
            return Err(Error::CorruptedData(
                "invalid compressed extent range".to_string(),
            ));
        }
        let PhysicalExtent {
            offset,
            size,
            format,
        } = physical;
        if size == 0 {
            return Ok(BlockPlan::Hole {
                size: (logical.end - self.offset).min(core.block_size) as usize,
            });
        }
        let length = logical.end - logical.start;
        if length > MAX_DECODED_SIZE {
            return Err(Error::NotSupported(
                "compressed extent above 12 MiB".to_string(),
            ));
        }
        let skip = usize::try_from(self.offset - logical.start)
            .map_err(|_| Error::Overflow("decoded offset"))?;
        let decoded_size = usize::try_from(length).map_err(|_| Error::Overflow("decoded size"))?;
        if size > MAX_ENCODED_SIZE {
            return Err(Error::NotSupported(
                "compressed physical cluster above 1 MiB".to_string(),
            ));
        }
        offset
            .checked_add(size)
            .ok_or(Error::Overflow("compressed data range"))?;
        let (encoding, partial, zero_padding) = match format {
            ExtentFormat::Plain { interlaced } => {
                if length > size {
                    return Err(Error::CorruptedData(
                        "plain compressed extent exceeds physical cluster".to_string(),
                    ));
                }
                if !interlaced {
                    return BlockPlan::direct(offset + skip as u64, decoded_size - skip);
                }
                let start = logical.start % core.block_size;
                if size > core.block_size
                    || start >= size
                    || (size < core.block_size && start + length > size)
                {
                    return Err(Error::CorruptedData(
                        "invalid interlaced physical range".to_string(),
                    ));
                }
                (Encoding::Interlaced(start as usize), false, false)
            }
            ExtentFormat::Compressed { algorithm, partial } => {
                let encoding = match algorithm {
                    0 => Encoding::Lz4,
                    1 => Encoding::Lzma(self.lzma_dict_size),
                    2 => Encoding::Deflate,
                    3 => Encoding::Zstd(self.zstd_window_size),
                    _ => {
                        return Err(Error::NotSupported(format!(
                            "compression algorithm {algorithm}"
                        )));
                    }
                };
                let available = if core.super_block.feature_incompat & 2 != 0 {
                    core.super_block.compr_algs
                } else {
                    1
                };
                if available & (1 << algorithm) == 0 {
                    return Err(Error::CorruptedData(
                        "algorithm absent from compression bitmap".to_string(),
                    ));
                }
                (
                    encoding,
                    partial,
                    algorithm != 0 || core.super_block.feature_incompat & 1 != 0,
                )
            }
        };
        Ok(BlockPlan::Encoded(EncodedExtent {
            offset,
            size: size as usize,
            decoded_size,
            skip,
            encoding,
            partial,
            zero_padding,
        }))
    }
}

#[derive(Debug, Clone, Copy)]
enum Encoding {
    Interlaced(usize),
    Lz4,
    #[cfg_attr(
        not(feature = "lzma"),
        expect(dead_code, reason = "mapping retains decoder configuration")
    )]
    Lzma(u32),
    Deflate,
    #[cfg_attr(
        not(feature = "zstd"),
        expect(dead_code, reason = "mapping retains decoder configuration")
    )]
    Zstd(u32),
}

impl Encoding {
    fn require_enabled(self) -> Result<()> {
        let enabled = match self {
            Self::Interlaced(_) => true,
            Self::Lz4 => cfg!(feature = "lz4"),
            Self::Lzma(_) => cfg!(feature = "lzma"),
            Self::Deflate => cfg!(feature = "deflate"),
            Self::Zstd(_) => cfg!(feature = "zstd"),
        };
        if enabled {
            Ok(())
        } else {
            Err(Error::NotSupported(format!(
                "compression codec {self:?} is disabled"
            )))
        }
    }
}

pub struct EncodedExtent {
    pub(crate) offset: u64,
    pub(crate) size: usize,
    decoded_size: usize,
    skip: usize,
    encoding: Encoding,
    partial: bool,
    zero_padding: bool,
}

impl EncodedExtent {
    pub(crate) fn decode(&self, input: &[u8], limit: usize) -> Result<Vec<u8>> {
        self.encoding.require_enabled()?;
        if input.len() != self.size {
            return Err(Error::CorruptedData(
                "truncated compressed cluster".to_string(),
            ));
        }
        let mut output = Vec::new();
        output
            .try_reserve_exact(self.decoded_size)
            .map_err(|_| Error::OutOfBounds("cannot allocate decoded extent".to_string()))?;
        output.resize(self.decoded_size, 0);
        if let Encoding::Interlaced(start) = self.encoding {
            let right = (input.len() - start).min(output.len());
            output[..right].copy_from_slice(&input[start..start + right]);
            let left = output.len() - right;
            output[right..].copy_from_slice(&input[..left]);
        } else {
            let start = if self.zero_padding {
                input
                    .iter()
                    .position(|&b| b != 0)
                    .ok_or_else(|| Error::CorruptedData("empty compressed cluster".to_string()))?
            } else {
                0
            };
            match (self.encoding, self.partial, &input[start..]) {
                #[cfg(feature = "lz4")]
                (Encoding::Lz4, partial, input) if partial || !self.zero_padding => {
                    decode_lz4_prefix(input, &mut output)?
                }
                #[cfg(feature = "lz4")]
                (Encoding::Lz4, _, input) => {
                    let written = lz4_flex::block::decompress_into(input, &mut output)
                        .map_err(|err| Error::CorruptedData(format!("invalid LZ4 data: {err}")))?;
                    if written != self.decoded_size {
                        return Err(Error::CorruptedData(
                            "LZ4 decoded length mismatch".to_string(),
                        ));
                    }
                }
                #[cfg(feature = "lzma")]
                (Encoding::Lzma(dict_size), partial, input) => {
                    decode_microlzma(input, &mut output, dict_size, partial)?
                }
                #[cfg(feature = "deflate")]
                (Encoding::Deflate, partial, input) => {
                    use miniz_oxide::inflate::{
                        TINFLStatus,
                        core::{
                            DecompressorOxide, decompress,
                            inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
                        },
                    };

                    let (status, consumed, written) = decompress(
                        &mut DecompressorOxide::new(),
                        input,
                        &mut output,
                        0,
                        TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
                    );
                    if written != output.len()
                        || if partial {
                            !matches!(status, TINFLStatus::Done | TINFLStatus::HasMoreOutput)
                        } else {
                            status != TINFLStatus::Done || consumed != input.len()
                        }
                    {
                        return Err(Error::CorruptedData(
                            "DEFLATE length or stream end mismatch".to_string(),
                        ));
                    }
                }
                #[cfg(feature = "zstd")]
                (Encoding::Zstd(window_size), partial, input) => {
                    decode_zstd(input, &mut output, window_size, partial)?
                }
                (Encoding::Interlaced(_), ..) => unreachable!("interlaced data handled above"),
                #[cfg(not(all(
                    feature = "lz4",
                    feature = "lzma",
                    feature = "deflate",
                    feature = "zstd"
                )))]
                _ => unreachable!("codec availability checked before decoding"),
            }
        }
        // Decode and validate the extent, but only move the requested fragment.
        let length = (output.len() - self.skip).min(limit);
        if self.skip != 0 {
            output.copy_within(self.skip..self.skip + length, 0);
        }
        output.truncate(length);
        Ok(output)
    }
}

// lz4_flex only exposes complete-block decoding. Partial references and legacy
// trailing padding require stopping inside a literal or match, without an EOS.
#[cfg(feature = "lz4")]
fn decode_lz4_prefix(mut input: &[u8], output: &mut [u8]) -> Result<()> {
    fn invalid() -> Error {
        Error::CorruptedData("invalid LZ4 prefix".to_string())
    }
    fn length(input: &mut &[u8], initial: usize) -> Result<usize> {
        let mut length = initial;
        if initial == 15 {
            loop {
                let (&byte, rest) = input.split_first().ok_or_else(invalid)?;
                *input = rest;
                length = length.checked_add(usize::from(byte)).ok_or_else(invalid)?;
                if byte != 255 {
                    break;
                }
            }
        }
        Ok(length)
    }
    let mut written = 0;
    while written < output.len() {
        let (&token, rest) = input.split_first().ok_or_else(invalid)?;
        input = rest;
        let literals = length(&mut input, usize::from(token >> 4))?.min(output.len() - written);
        let bytes = input.get(..literals).ok_or_else(invalid)?;
        output[written..written + literals].copy_from_slice(bytes);
        input = &input[literals..];
        written += literals;
        if written == output.len() {
            break;
        }
        let bytes = input.get(..2).ok_or_else(invalid)?;
        let distance = usize::from(u16::from_le_bytes([bytes[0], bytes[1]]));
        input = &input[2..];
        if distance == 0 || distance > written {
            return Err(invalid());
        }
        let count = length(&mut input, usize::from(token & 15))?
            .checked_add(4)
            .ok_or_else(invalid)?
            .min(output.len() - written);
        for _ in 0..count {
            output[written] = output[written - distance];
            written += 1;
        }
    }
    Ok(())
}

#[cfg(feature = "zstd")]
fn decode_zstd(input: &[u8], output: &mut [u8], window_size: u32, partial: bool) -> Result<()> {
    use ruzstd::{decoding::StreamingDecoder, io::Read};

    let mut decoder = StreamingDecoder::new_with_max_window_size(input, u64::from(window_size))
        .map_err(|err| Error::CorruptedData(format!("invalid Zstd frame: {err}")))?;
    // ruzstd exposes the content size but does not validate it or the reserved bit.
    let descriptor = input[4]; // Successful frame initialization validated this byte.
    if descriptor & 8 != 0
        || (descriptor & 0xe0 != 0
            && if partial {
                decoder.decoder.content_size() < output.len() as u64
            } else {
                decoder.decoder.content_size() != output.len() as u64
            })
    {
        return Err(Error::CorruptedData(
            "invalid Zstd frame descriptor or content size".to_string(),
        ));
    }
    decoder
        .read_exact(output)
        .map_err(|err| Error::CorruptedData(format!("invalid Zstd data: {err}")))?;
    if !partial
        && (decoder
            .read(&mut [0])
            .map_err(|err| Error::CorruptedData(format!("invalid Zstd end: {err}")))?
            != 0
            || !decoder.get_ref().is_empty()
            || !decoder.decoder.is_finished())
    {
        return Err(Error::CorruptedData(
            "Zstd length or stream end mismatch".to_string(),
        ));
    }
    // The hash feature computes the checksum; callers must compare it themselves.
    // ruzstd hashes bytes as they are collected, not when a block is decoded.
    if decoder.decoder.can_collect() == 0
        && let Some(checksum) = decoder.decoder.get_checksum_from_data()
        && Some(checksum) != decoder.decoder.get_calculated_checksum()
    {
        return Err(Error::CorruptedData("Zstd checksum mismatch".to_string()));
    }
    Ok(())
}

#[cfg(feature = "lzma")]
fn decode_microlzma(input: &[u8], output: &mut [u8], dict_size: u32, partial: bool) -> Result<()> {
    use lzma_rs::decompress::raw::{LzmaDecoder, LzmaParams, LzmaProperties};
    use std::io::Read;

    // MicroLZMA replaces the raw range coder's initial zero with !properties.
    // Restore it through a chained slice, without copying the compressed data.
    let props = u32::from(!input[0]);
    let properties = LzmaProperties {
        lc: props % 9,
        lp: props / 9 % 5,
        pb: props / 45,
    };
    if props >= 225 || properties.lc + properties.lp > 4 {
        return Err(Error::CorruptedData(
            "invalid MicroLZMA properties".to_string(),
        ));
    }
    // The raw API cannot stop in the middle of a match. A prefix-sized history
    // flushes exactly at the requested byte, where the sink stops the decoder.
    // This retains at most MAX_DECODED_SIZE bytes of history, including prefixes
    // larger than the on-disk dictionary; no unreferenced suffix is decoded.
    let dict_size = if partial {
        output.len() as u32
    } else {
        dict_size
    };
    let params = LzmaParams::new(properties, dict_size, Some(output.len() as u64));
    let mut decoder = LzmaDecoder::new(params, Some(dict_size as usize))
        .map_err(|err| Error::CorruptedData(format!("invalid MicroLZMA parameters: {err}")))?;
    let prefix = [0u8];
    let mut input = prefix.as_slice().chain(&input[1..]);
    if partial {
        struct Prefix<'a>(&'a mut [u8]);
        impl std::io::Write for Prefix<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let n = bytes.len().min(self.0.len());
                let remaining = core::mem::take(&mut self.0);
                remaining[..n].copy_from_slice(&bytes[..n]);
                self.0 = &mut remaining[n..];
                if self.0.is_empty() {
                    return Err(std::io::ErrorKind::WriteZero.into());
                }
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut prefix = Prefix(output);
        let result = decoder.decompress(&mut input, &mut prefix);
        return if prefix.0.is_empty() {
            Ok(())
        } else {
            Err(Error::CorruptedData(format!(
                "incomplete MicroLZMA prefix: {result:?}"
            )))
        };
    }
    let mut remaining = output;
    decoder
        .decompress(&mut input, &mut remaining)
        .map_err(|err| Error::CorruptedData(format!("invalid MicroLZMA data: {err}")))?;
    if !remaining.is_empty() || !input.get_ref().1.is_empty() {
        return Err(Error::CorruptedData(
            "MicroLZMA length or stream end mismatch".to_string(),
        ));
    }
    Ok(())
}
