// SPDX-License-Identifier: MIT
// Compact-index decoding adapted from erofs-utils/lib/zmap.c (MIT option).
// Copyright (C) 2018-2019 HUAWEI, Inc.
// Authors: Gao Xiang <xiang@kernel.org>, Huang Jianan <huangjianan@oppo.com>.
// See LICENSE-MIT for permission and warranty terms.

//! EROFS compressed-extent mapping. I/O stays in the sync/async executors.

use alloc::{format, string::ToString, vec::Vec};
use binrw::{BinRead, BinReaderExt, io::Cursor};

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
    blocks: u16,
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
}

impl Index {
    fn new(core: &EroFSCore, inode: &Inode, header_offset: u64, data: &[u8]) -> Result<Self> {
        let header = MapHeader::read(&mut Cursor::new(data))?;
        let compact = matches!(inode.data, InodeData::CompressedCompact);
        // Accept block-count indexes too: non-LZ4 images use them even for
        // single-block clusters. Full's bit 0 selects unsupported extent metadata.
        let allowed = if compact { 0x17 } else { 0x16 };
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
        if header.clusterbits & 0x80 != 0 {
            return Err(Error::NotSupported(
                "packed compressed fragments".to_string(),
            ));
        }
        if header.advise & !allowed != 0 {
            return Err(Error::NotSupported(format!(
                "compressed inode advice {:#06x}",
                header.advise
            )));
        }
        if header.clusterbits != 0 {
            return Err(Error::NotSupported(
                "non-default logical cluster size or reserved cluster bits".to_string(),
            ));
        }
        let block_bits = core.super_block.blk_size_bits;
        if compact && block_bits > 14 {
            return Err(Error::NotSupported(
                "compact compression indexes above 16 KiB".to_string(),
            ));
        }
        let start = header_offset
            .checked_add(if compact { 8 } else { 16 })
            .ok_or(Error::Overflow("compression index start"))?;
        let count = inode.data_size().div_ceil(core.block_size);
        let initial = if compact {
            (((32 - start % 32) / 4) & 7).min(count)
        } else {
            0
        };
        let middle = if compact && header.advise & 1 != 0 {
            (count - initial) / 16 * 16
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
            (initial * 4 + middle * 2 + (count - initial - middle) * 4).next_multiple_of(8)
        } else {
            count * 8
        };
        let end = start
            .checked_add(bytes)
            .ok_or(Error::Overflow("compression index end"))?;
        Ok(Self {
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
        })
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
                return Ok(Some(Entry::NonHead(NonHead { back, blocks: 0 })));
            }
            let mut blocks = u32::from(!self.big1);
            let mut previous = slot;
            while previous != 0 {
                previous -= 1;
                let (distance, kind) = field(previous)?;
                if kind == 2 {
                    let nonhead = self.nonhead(distance)?;
                    if self.big1 {
                        if nonhead.blocks != 0 {
                            previous = previous.saturating_sub(1);
                            blocks += u32::from(nonhead.blocks);
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
            if !(self.big1 || self.big2) || encoded == CBLKCNT {
                return Err(Error::CorruptedData(
                    "invalid compressed block count".to_string(),
                ));
            }
            let blocks = encoded & !CBLKCNT;
            if blocks != 1 {
                return Err(Error::NotSupported(
                    "multi-block compressed clusters".to_string(),
                ));
            }
            return Ok(NonHead { back: 1, blocks });
        }
        if encoded == 0 || (self.compact && self.big1 && encoded == 1) {
            return Err(Error::CorruptedData(
                "invalid compression lookback distance".to_string(),
            ));
        }
        Ok(NonHead {
            back: encoded,
            blocks: 0,
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
                let layout = Index::new(core, &self.inode, self.header_offset, data)?;
                self.stage = Stage::FindHead {
                    layout,
                    index: self.offset / core.block_size,
                };
            }
            _ => {}
        }
        loop {
            let (layout, index) = match self.stage {
                Stage::FindHead { layout, index } | Stage::FindEnd { layout, index, .. } => {
                    (layout, index)
                }
                _ => unreachable!("header and configuration handled above"),
            };
            if let Stage::FindEnd { head, .. } = self.stage {
                if index == layout.count {
                    return self.finish(core, layout, head, layout.file_size);
                }
                if (index << layout.block_bits).saturating_sub(head.start) > MAX_DECODED_SIZE {
                    return Err(Error::NotSupported(
                        "compressed extent above 12 MiB".to_string(),
                    ));
                }
            } else if self.offset.saturating_sub(index << layout.block_bits)
                > MAX_DECODED_SIZE + core.block_size
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
                    if head.partial {
                        return Err(Error::NotSupported(
                            "partial compressed references".to_string(),
                        ));
                    }
                    self.stage = Stage::FindEnd {
                        layout,
                        index: index + 1,
                        head,
                    };
                }
                (Stage::FindEnd { head, .. }, Entry::Head(next)) => {
                    return self.finish(core, layout, head, next.start);
                }
                (Stage::FindEnd { head, .. }, Entry::NonHead(nonhead)) => {
                    let distance = index - (head.start >> layout.block_bits);
                    let needs_count = distance == 1
                        && if head.kind == 1 {
                            layout.big1
                        } else {
                            layout.big2
                        };
                    if (nonhead.blocks != 0) != needs_count {
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
                    };
                }
                _ => unreachable!("only index stages reach the metadata walk"),
            }
        }
    }

    fn finish(self, core: &EroFSCore, layout: Index, head: Head, end: u64) -> Result<BlockPlan> {
        if head.start > self.offset || end <= self.offset || end > self.inode.data_size() {
            return Err(Error::CorruptedData(
                "invalid compressed extent range".to_string(),
            ));
        }
        let length = end - head.start;
        if length > MAX_DECODED_SIZE {
            return Err(Error::NotSupported(
                "compressed extent above 12 MiB".to_string(),
            ));
        }
        let skip = usize::try_from(self.offset - head.start)
            .map_err(|_| Error::Overflow("decoded offset"))?;
        let decoded_size = usize::try_from(length).map_err(|_| Error::Overflow("decoded size"))?;
        let offset = core.block_offset(head.block);
        let encoding = if head.kind == 0 {
            if length > core.block_size {
                return Err(Error::CorruptedData(
                    "plain compressed extent exceeds physical cluster".to_string(),
                ));
            }
            if !layout.interlaced {
                return BlockPlan::direct(offset + skip as u64, decoded_size - skip);
            }
            Encoding::Interlaced((head.start % core.block_size) as usize)
        } else {
            if head.kind == 3 && core.super_block.feature_incompat & 8 == 0 {
                return Err(Error::CorruptedData(
                    "HEAD2 without filesystem feature".to_string(),
                ));
            }
            let algorithm = if head.kind == 3 {
                layout.algorithms >> 4
            } else {
                layout.algorithms & 15
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
            match algorithm {
                #[cfg(feature = "lz4")]
                0 => {
                    if core.super_block.feature_incompat & 1 == 0 {
                        return Err(Error::NotSupported(
                            "LZ4 without leading zero padding".to_string(),
                        ));
                    }
                    Encoding::Lz4
                }
                #[cfg(feature = "lzma")]
                1 => Encoding::Lzma(self.lzma_dict_size),
                #[cfg(feature = "deflate")]
                2 => Encoding::Deflate,
                #[cfg(feature = "zstd")]
                3 => Encoding::Zstd(self.zstd_window_size),
                _ => {
                    return Err(Error::NotSupported(format!(
                        "compression algorithm {algorithm} (codec feature disabled or unsupported)"
                    )));
                }
            }
        };
        offset
            .checked_add(core.block_size)
            .ok_or(Error::Overflow("compressed data range"))?;
        Ok(BlockPlan::Encoded(EncodedExtent {
            offset,
            size: core.block_size as usize,
            decoded_size,
            skip,
            encoding,
        }))
    }
}

enum Encoding {
    Interlaced(usize),
    #[cfg(feature = "lz4")]
    Lz4,
    #[cfg(feature = "lzma")]
    Lzma(u32),
    #[cfg(feature = "deflate")]
    Deflate,
    #[cfg(feature = "zstd")]
    Zstd(u32),
}

pub struct EncodedExtent {
    pub(crate) offset: u64,
    pub(crate) size: usize,
    decoded_size: usize,
    skip: usize,
    encoding: Encoding,
}

impl EncodedExtent {
    pub(crate) fn decode(&self, input: &[u8]) -> Result<Vec<u8>> {
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
            let start = input
                .iter()
                .position(|&b| b != 0)
                .ok_or_else(|| Error::CorruptedData("empty compressed cluster".to_string()))?;
            let input = &input[start..];
            match self.encoding {
                #[cfg(feature = "lz4")]
                Encoding::Lz4 => {
                    let written = lz4_flex::block::decompress_into(input, &mut output)
                        .map_err(|err| Error::CorruptedData(format!("invalid LZ4 data: {err}")))?;
                    if written != self.decoded_size {
                        return Err(Error::CorruptedData(
                            "LZ4 decoded length mismatch".to_string(),
                        ));
                    }
                }
                #[cfg(feature = "lzma")]
                Encoding::Lzma(dict_size) => decode_microlzma(input, &mut output, dict_size)?,
                #[cfg(feature = "deflate")]
                Encoding::Deflate => {
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
                    if status != TINFLStatus::Done
                        || written != output.len()
                        || consumed != input.len()
                    {
                        return Err(Error::CorruptedData(
                            "DEFLATE length or stream end mismatch".to_string(),
                        ));
                    }
                }
                #[cfg(feature = "zstd")]
                Encoding::Zstd(window_size) => decode_zstd(input, &mut output, window_size)?,
                Encoding::Interlaced(_) => unreachable!("interlaced data handled above"),
            }
        }
        if self.skip != 0 {
            output.copy_within(self.skip.., 0);
            output.truncate(self.decoded_size - self.skip);
        }
        Ok(output)
    }
}

#[cfg(feature = "zstd")]
fn decode_zstd(input: &[u8], output: &mut [u8], window_size: u32) -> Result<()> {
    use ruzstd::{decoding::StreamingDecoder, io::Read};

    let mut decoder = StreamingDecoder::new_with_max_window_size(input, u64::from(window_size))
        .map_err(|err| Error::CorruptedData(format!("invalid Zstd frame: {err}")))?;
    // ruzstd exposes the content size but does not validate it or the reserved bit.
    let descriptor = input[4]; // Successful frame initialization validated this byte.
    if descriptor & 8 != 0
        || (descriptor & 0xe0 != 0 && decoder.decoder.content_size() != output.len() as u64)
    {
        return Err(Error::CorruptedData(
            "invalid Zstd frame descriptor or content size".to_string(),
        ));
    }
    decoder
        .read_exact(output)
        .map_err(|err| Error::CorruptedData(format!("invalid Zstd data: {err}")))?;
    if decoder
        .read(&mut [0])
        .map_err(|err| Error::CorruptedData(format!("invalid Zstd end: {err}")))?
        != 0
        || !decoder.get_ref().is_empty()
        || !decoder.decoder.is_finished()
    {
        return Err(Error::CorruptedData(
            "Zstd length or stream end mismatch".to_string(),
        ));
    }
    // The hash feature computes the checksum; callers must compare it themselves.
    if let Some(checksum) = decoder.decoder.get_checksum_from_data()
        && Some(checksum) != decoder.decoder.get_calculated_checksum()
    {
        return Err(Error::CorruptedData("Zstd checksum mismatch".to_string()));
    }
    Ok(())
}

#[cfg(feature = "lzma")]
fn decode_microlzma(input: &[u8], output: &mut [u8], dict_size: u32) -> Result<()> {
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
    let params = LzmaParams::new(properties, dict_size, Some(output.len() as u64));
    let mut decoder = LzmaDecoder::new(params, Some(dict_size as usize))
        .map_err(|err| Error::CorruptedData(format!("invalid MicroLZMA parameters: {err}")))?;
    let prefix = [0u8];
    let mut input = prefix.as_slice().chain(&input[1..]);
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
