//! Bounded compression and Compact indexes; both I/O drivers share decisions.

use alloc::{boxed::Box, vec::Vec};
use bytes::BufMut;
#[cfg(any(feature = "lz4", feature = "deflate", feature = "zstd"))]
use std::io;

use super::{
    Compression, Options,
    encode::{BLOCK_SIZE, Node},
};
use crate::{Error, Result};

pub(super) const INPUT_SIZE: usize = 64 * 1024;

impl Compression {
    // These encoders stop at the destination size and report the consumed prefix.
    fn fills_destination(self) -> bool {
        matches!(self, Self::Lz4 | Self::Lzma | Self::Deflate)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Placement {
    External,
    Inline,
    Fragment,
}

pub(super) struct BufferedFile {
    pub data: Vec<u8>,
    pub indexes: Vec<u8>,
}

pub(super) struct EncodedExtent<'a> {
    pub consumed: usize,
    pub data: &'a [u8],
    pub indexes: &'a [u8],
    pub placement: Placement,
}

#[derive(Clone, Copy)]
pub(super) struct FittedExtent {
    pub consumed: usize,
    pub compressed: bool,
    pub blocks: u32,
}

pub(super) struct FittedFile {
    pub data: Vec<u8>,
    extents: Vec<FittedExtent>,
}

impl FittedFile {
    // A complete short file can fall back to flat storage without rereading
    // input or output. Larger files retain the streaming extent path.
    pub fn fit(options: &Options, encoder: &mut Encoder, mut input: &[u8]) -> Result<Self> {
        let mut fitter = ExtentFitter::sized(input.len() as u64, options);
        debug_assert_eq!(fitter.position, 0);
        debug_assert_eq!(input.len() as u64, fitter.file_size);
        debug_assert!(
            input.len() <= INPUT_SIZE && fitter.inline_capacity == 0 && !fitter.fragments
        );
        debug_assert!(!options.inline_compressed_tails);
        let mut file = Self {
            data: Vec::with_capacity(input.len().next_multiple_of(BLOCK_SIZE)),
            extents: Vec::new(),
        };
        while !input.is_empty() {
            let extent = fitter.fit(encoder, input, 0)?;
            file.data.extend_from_slice(&fitter.data);
            file.extents.push(extent);
            input = &input[extent.consumed..];
        }
        Ok(file)
    }

    pub fn into_buffered(self, node: &Node, mut block: u32) -> BufferedFile {
        debug_assert_eq!(
            self.extents
                .iter()
                .map(|extent| extent.consumed as u64)
                .sum::<u64>(),
            node.size
        );
        let mut indexes = IndexWriter::new(node);
        for extent in &self.extents {
            indexes.push(*extent, block);
            block += extent.blocks;
        }
        BufferedFile {
            data: self.data,
            indexes: indexes.indexes,
        }
    }
}

pub(super) struct Compressor {
    fitter: ExtentFitter,
    indexes: IndexWriter,
}

impl Compressor {
    pub fn new(node: &Node, options: &Options) -> Self {
        Self {
            fitter: ExtentFitter::new(node, options),
            indexes: IndexWriter::new(node),
        }
    }

    /// Consumes a byte-granular prefix. Complete Compact packs stream out;
    /// a partial pack and a logical cluster shared by two extents survive calls.
    pub fn encode(
        &mut self,
        encoder: &mut Encoder,
        input: &[u8],
        block: u32,
        fragment_offset: u64,
    ) -> Result<EncodedExtent<'_>> {
        self.indexes.indexes.clear();
        let extent = self.fitter.fit(encoder, input, fragment_offset)?;
        self.indexes.push(extent, block);
        Ok(EncodedExtent {
            consumed: extent.consumed,
            data: &self.fitter.data,
            indexes: &self.indexes.indexes,
            placement: self.fitter.placement,
        })
    }

    pub fn inline_tail(&self) -> Option<&[u8]> {
        (self.fitter.placement == Placement::Inline).then_some(self.fitter.data.as_slice())
    }
}

struct ExtentFitter {
    data: Vec<u8>,
    placement: Placement,
    inline_capacity: usize,
    fragments: bool,
    position: u64,
    file_size: u64,
    hint: usize,
}

impl ExtentFitter {
    fn new(node: &Node, options: &Options) -> Self {
        Self {
            inline_capacity: if options.inline_compressed_tails {
                ((BLOCK_SIZE as u64 - node.index_end() % BLOCK_SIZE as u64) % BLOCK_SIZE as u64)
                    as usize
            } else {
                0
            },
            ..Self::sized(node.size, options)
        }
    }

    fn sized(file_size: u64, options: &Options) -> Self {
        Self {
            data: Vec::new(),
            placement: Placement::External,
            inline_capacity: 0,
            fragments: options.fragments,
            position: 0,
            file_size,
            hint: 4 * BLOCK_SIZE,
        }
    }

    fn fit(
        &mut self,
        encoder: &mut Encoder,
        input: &[u8],
        fragment_offset: u64,
    ) -> Result<FittedExtent> {
        debug_assert!(!input.is_empty() && input.len() <= INPUT_SIZE);
        let final_input = self.position + input.len() as u64 == self.file_size;
        let (consumed, compressed) =
            self.encode_data(encoder, input, final_input, fragment_offset)?;
        self.position += consumed as u64;
        let blocks = if self.placement == Placement::Fragment {
            0
        } else {
            self.data.len().div_ceil(BLOCK_SIZE) as u32
        };
        Ok(FittedExtent {
            consumed,
            compressed,
            blocks,
        })
    }

    fn encode_data(
        &mut self,
        encoder: &mut Encoder,
        input: &[u8],
        final_input: bool,
        fragment_offset: u64,
    ) -> Result<(usize, bool)> {
        self.data.clear();
        self.placement = Placement::External;
        let try_inline = final_input && self.inline_capacity != 0;
        // Other codecs emit complete frames, so search for a prefix that fits.
        let bounded = encoder.compression.fills_destination();
        let limit = encoder.limit;
        let mut trial_len = if bounded || try_inline || limit >= INPUT_SIZE / 4 {
            input.len()
        } else {
            input.len().min(self.hint)
        };
        let mut low = BLOCK_SIZE;
        let mut high = input.len() + 1;
        let mut best = 0;

        // Favor image size: stop within 8 bytes of the destination capacity.
        // shortcut: complete-frame codecs cap probes at 20; prefer destination-size APIs.
        // Prefix sizes need not be monotonic.
        for _ in 0..20 {
            let encoded = if trial_len > BLOCK_SIZE || try_inline {
                Some(encoder.compress(&input[..trial_len])?)
                    // EROFS skips leading zeros; a zero-leading DEFLATE stored
                    // block is indistinguishable from padding. Keep it PLAIN.
                    .filter(|(_, data)| data[0] != 0)
            } else {
                None
            };

            if try_inline && trial_len == input.len() {
                if let Some((consumed, data)) = encoded
                    && consumed == input.len()
                    && data.len() < input.len()
                    && data.len() <= self.inline_capacity
                {
                    self.data.extend_from_slice(data);
                    self.placement = Placement::Inline;
                    return Ok((input.len(), true));
                }
                if input.len() <= self.inline_capacity {
                    self.data.extend_from_slice(input);
                    self.placement = Placement::Inline;
                    return Ok((input.len(), false));
                }
            }

            if final_input
                && input.len() <= BLOCK_SIZE
                && self.fragments
                && fragment_offset + input.len() as u64 <= u64::from(u32::MAX)
            {
                self.data.extend_from_slice(input);
                self.placement = Placement::Fragment;
                return Ok((input.len(), true));
            }
            let Some((consumed, data)) = encoded else {
                break;
            };
            if consumed <= BLOCK_SIZE {
                break;
            }
            let len = data.len();
            let profitable = len <= limit && len.next_multiple_of(BLOCK_SIZE) < consumed;
            if profitable {
                self.data.clear();
                self.data.extend_from_slice(data);
                best = consumed;
            }
            if bounded {
                break;
            }

            let estimate =
                ((trial_len as u64 * limit as u64 / len as u64).min(INPUT_SIZE as u64)) as usize;
            if len <= limit {
                if profitable {
                    self.hint = estimate.max(BLOCK_SIZE + 1);
                }
                low = trial_len;
                if trial_len == input.len() || limit - len <= 8 {
                    break;
                }
            } else {
                high = trial_len;
            }
            if high <= low + 1 {
                break;
            }
            // Heterogeneous prefixes can predict outside the bracket. Bisect
            // instead of alternating one-byte steps at its opposite edges.
            trial_len = if best == 0 && estimate <= low {
                // Quickly reject incompressible input without twenty probes.
                low + 1
            } else if estimate > low && estimate < high {
                estimate
            } else {
                (low + high) / 2
            };
        }

        if best != 0 {
            let len = self.data.len();
            let padded = len.next_multiple_of(BLOCK_SIZE);
            self.data.resize(padded, 0);
            self.data.copy_within(..len, padded - len);
            self.data[..padded - len].fill(0);
            return Ok((best, true));
        }

        let consumed = input.len().min(BLOCK_SIZE);
        self.data.extend_from_slice(&input[..consumed]);
        self.data.resize(BLOCK_SIZE, 0);
        Ok((consumed, false))
    }
}

struct IndexWriter {
    // A completed pending pack plus this extent's Compact entries.
    indexes: Vec<u8>,
    pack: [u8; 32],
    pack_len: usize,
    next_index: u64,
    index_count: u64,
    position: u64,
    file_size: u64,
    big_pcluster: bool,
    two_byte_indexes: core::ops::Range<u64>,
}

impl IndexWriter {
    fn new(node: &Node) -> Self {
        Self {
            indexes: Vec::with_capacity(32 + INPUT_SIZE / BLOCK_SIZE * 4),
            pack: [0; 32],
            pack_len: 0,
            next_index: 0,
            index_count: node.size.div_ceil(BLOCK_SIZE as u64),
            position: 0,
            file_size: node.size,
            big_pcluster: node.big_pcluster,
            two_byte_indexes: node.two_byte_indexes(),
        }
    }

    fn push(&mut self, extent: FittedExtent, block: u32) {
        let FittedExtent {
            consumed,
            compressed,
            blocks,
        } = extent;
        let offset = (self.position % BLOCK_SIZE as u64) as usize;
        let whole = (offset + consumed) / BLOCK_SIZE;
        let count = whole.max(1);
        for index in 0..count {
            // A pack starting in NONHEADs predicts the current extent, or the
            // following physical block after a big-pcluster count entry.
            let base = if self.big_pcluster {
                block + if index > 1 { blocks } else { 0 }
            } else {
                block.wrapping_sub(u32::from(index == 0))
            };
            let slots = if self.two_byte_indexes.contains(&self.next_index) {
                16
            } else {
                2
            };
            let value = if index == 0 {
                u32::from(compressed) << 12 | offset as u32
            } else if self.big_pcluster && index == 1 {
                2 << 12 | 1 << 11 | blocks
            } else {
                // The final NONHEAD in a pack stores a forward distance.
                2 << 12
                    | if self.pack_len + 1 == slots {
                        count - index
                    } else {
                        index
                    } as u32
            };
            self.write_index(value, base);
        }
        self.position += consumed as u64;
        // A final partial logical cluster belongs to the preceding extent.
        // A PLAIN sentinel at EOF terminates it; it never references file data.
        if whole != 0
            && self.position == self.file_size
            && !self.position.is_multiple_of(BLOCK_SIZE as u64)
        {
            self.write_index(
                (self.position % BLOCK_SIZE as u64) as u32,
                (block + blocks).wrapping_sub(u32::from(!self.big_pcluster)),
            );
        }
    }

    fn write_index(&mut self, value: u32, base: u32) {
        debug_assert!(self.next_index < self.index_count);
        let (slots, bits, bytes) = if self.two_byte_indexes.contains(&self.next_index) {
            (16, 14, 32)
        } else {
            (2, 16, 8)
        };
        if self.pack_len == 0 {
            self.pack.fill(0);
            (&mut self.pack[bytes - 4..bytes]).put_u32_le(base);
        }
        let bit = self.pack_len * bits;
        for (i, byte) in (value << (bit % 8)).to_le_bytes()[..3].iter().enumerate() {
            self.pack[bit / 8 + i] |= byte;
        }
        self.pack_len += 1;
        self.next_index += 1;
        if self.pack_len == slots || self.next_index == self.index_count {
            self.indexes.extend_from_slice(&self.pack[..bytes]);
            self.pack_len = 0;
        }
    }
}

// Builder-owned allocations; file positions and indexes stay in Compressor.
pub(super) struct Encoder {
    compression: Compression,
    limit: usize,
    scratch: Vec<u8>,
    #[cfg(feature = "lz4")]
    lz4_small: Option<Box<lz4_flex::block::CompressTable>>,
    #[cfg(feature = "lz4")]
    lz4_large: Option<Box<lz4_flex::block::CompressTable>>,
    #[cfg(feature = "lzma")]
    lzma: Option<Box<lzma_rust2::MicroLzmaEncoder>>,
    #[cfg(feature = "deflate")]
    deflate: Option<Box<miniz_oxide::deflate::core::CompressorOxide>>,
    #[cfg(feature = "zstd")]
    zstd: Option<Box<zrip::CompressContext>>,
}

impl Encoder {
    pub fn new(options: &Options) -> Self {
        Self {
            compression: options.compression,
            limit: options.pcluster_size as usize,
            scratch: Vec::new(),
            #[cfg(feature = "lz4")]
            lz4_small: None,
            #[cfg(feature = "lz4")]
            lz4_large: None,
            #[cfg(feature = "lzma")]
            lzma: None,
            #[cfg(feature = "deflate")]
            deflate: None,
            #[cfg(feature = "zstd")]
            zstd: None,
        }
    }

    // Returns the consumed prefix length and its complete independent stream.
    fn compress(&mut self, input: &[u8]) -> Result<(usize, &[u8])> {
        debug_assert!(input.len() <= INPUT_SIZE);
        let (consumed, written) = match self.compression {
            #[cfg(feature = "lz4")]
            Compression::Lz4 => {
                use lz4_flex::block::{CompressTable, compress_prefix_into_with_table};

                // Keep both widths so a previous large input cannot change small-input output.
                let small = input.len() < usize::from(u16::MAX);
                let table = if small {
                    &mut self.lz4_small
                } else {
                    &mut self.lz4_large
                };
                let table = table.get_or_insert_with(|| {
                    Box::new(if small {
                        CompressTable::small()
                    } else {
                        CompressTable::large()
                    })
                });
                self.scratch.resize(self.limit, 0);
                // compress_prefix_into_with_table clears history on every call.
                compress_prefix_into_with_table(input, &mut self.scratch, table)
                    .map_err(io::Error::other)?
            }
            #[cfg(feature = "lzma")]
            Compression::Lzma => {
                use lzma_rust2::{LzmaOptions, MicroLzmaEncoder};

                let encoder = match &mut self.lzma {
                    Some(encoder) => encoder,
                    slot @ None => {
                        let mut options = LzmaOptions::with_preset(1);
                        options.dict_size = super::encode::LZMA_DICT_SIZE;
                        slot.insert(Box::new(MicroLzmaEncoder::new(&options)?))
                    }
                };
                self.scratch.resize(self.limit, 0);
                // Each call resets history and emits a complete MicroLZMA prefix.
                encoder.compress_prefix(input, &mut self.scratch)?
            }
            #[cfg(feature = "deflate")]
            Compression::Deflate => {
                use miniz_oxide::deflate::core::{
                    CompressorOxide, TDEFLStatus, compress_prefix,
                    create_comp_flags_from_zip_params,
                };

                self.scratch.resize(self.limit, 0);
                let encoder = self.deflate.get_or_insert_with(|| {
                    Box::new(CompressorOxide::new(create_comp_flags_from_zip_params(
                        6, 0, 0,
                    )))
                });
                // compress_prefix resets history while retaining encoder allocations.
                let (status, consumed, written) =
                    compress_prefix(encoder, input, &mut self.scratch);
                match status {
                    TDEFLStatus::Done => (consumed, written),
                    _ => return Err(io::Error::other("DEFLATE encoding failed").into()),
                }
            }
            #[cfg(feature = "zstd")]
            Compression::Zstd => {
                // Single-segment frames advertise the input length as their window.
                const { assert!(INPUT_SIZE <= 1 << super::encode::ZSTD_WINDOW_LOG) };
                let encoder = match &mut self.zstd {
                    Some(encoder) => encoder,
                    slot @ None => slot.insert(Box::new(
                        zrip::CompressContext::new(1).map_err(io::Error::other)?,
                    )),
                };
                self.scratch.resize(zrip::compress_bound(input.len()), 0);
                // Each call starts an independent frame, retaining only allocations.
                let len = encoder
                    .compress_into(input, &mut self.scratch)
                    .map_err(io::Error::other)?;
                (input.len(), len)
            }
            _ => {
                return Err(Error::NotSupported(format!(
                    "compression encoder {:?} is disabled",
                    self.compression
                )));
            }
        };
        debug_assert!(consumed <= input.len());
        debug_assert!(written != 0);
        Ok((consumed, &self.scratch[..written]))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn reused_encoders_reset_between_trials() {
        let mut seed = 7u32;
        let random: Vec<_> = (0..INPUT_SIZE)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        let zeros = vec![0; INPUT_SIZE];
        let moderate: Vec<_> = random
            .as_chunks::<512>()
            .0
            .iter()
            .flat_map(|chunk| chunk.iter().chain(chunk))
            .copied()
            .take(INPUT_SIZE)
            .collect();
        let repeated: Vec<_> = random[..1024]
            .iter()
            .copied()
            .cycle()
            .take(INPUT_SIZE)
            .collect();
        for compression in [
            Compression::Lz4,
            Compression::Lzma,
            Compression::Deflate,
            Compression::Zstd,
        ]
        .into_iter()
        .filter(|compression| compression.enabled())
        {
            let options = Options::default().compression(compression);
            let mut reused = Encoder::new(&options);
            let mut partial = false;
            for input in [
                random.as_slice(),
                moderate.as_slice(),
                &repeated[..8192],
                &repeated[..65535],
                &random[..32768],
                &zeros[..4097],
                &repeated[..65534],
                &zeros[..65536],
                &repeated[..16384],
                &random[..4097],
                &repeated[..16384],
            ] {
                let mut fresh = Encoder::new(&options);
                let (used, encoded) = reused.compress(input).unwrap();
                assert_eq!((used, encoded), fresh.compress(input).unwrap());
                let len = encoded.len();
                assert!(used > 0 && used <= input.len());
                partial |= used < input.len() && len.next_multiple_of(BLOCK_SIZE) < used;
                match compression {
                    #[cfg(feature = "lz4")]
                    Compression::Lz4 => {
                        let mut decoded = vec![0; used];
                        assert_eq!(
                            lz4_flex::block::decompress_into(encoded, &mut decoded).unwrap(),
                            used
                        );
                        assert_eq!(decoded, input[..used]);
                    }
                    #[cfg(feature = "lzma")]
                    Compression::Lzma => {
                        use std::io::Read;

                        // MicroLZMA is LZMA1 without an end marker; its always-zero
                        // first range-coder byte stores the complemented properties.
                        let mut decoder = lzma_rust2::LzmaReader::new_with_props(
                            [0].as_slice().chain(&encoded[1..]),
                            used as u64,
                            !encoded[0],
                            super::super::encode::LZMA_DICT_SIZE,
                            None,
                        )
                        .unwrap();
                        let mut decoded = Vec::new();
                        decoder.read_to_end(&mut decoded).unwrap();
                        assert_eq!(decoded, input[..used]);
                        // Decoding `used` bytes must consume the whole stream.
                        let (reader, unused) = decoder.into_parts();
                        assert!(unused.is_empty() && reader.get_ref().1.is_empty());
                    }
                    #[cfg(feature = "deflate")]
                    Compression::Deflate => {
                        assert_eq!(
                            miniz_oxide::inflate::decompress_to_vec(encoded).unwrap(),
                            input[..used]
                        );
                    }
                    #[cfg(feature = "zstd")]
                    Compression::Zstd => {
                        assert_eq!(encoded, zrip::compress(input, 1).unwrap());
                        assert_eq!(
                            zrip::decompress_with_limit(encoded, input.len()).unwrap(),
                            input
                        );
                    }
                    _ => unreachable!(),
                }
            }
            assert_eq!(partial, compression.fills_destination());
        }
    }

    #[test]
    fn incomplete_prefixes_are_not_inlined() {
        let input = vec![42; INPUT_SIZE];
        for (compression, minimum) in [
            (Compression::Lz4, 1),
            (Compression::Lzma, 5),
            (Compression::Deflate, 2),
        ]
        .into_iter()
        .filter(|(compression, _)| compression.enabled())
        {
            let options = Options::default()
                .compression(compression)
                .inline_compressed_tails(true);
            let mut encoder = Encoder::new(&options);
            let mut node = Node::new(
                &super::super::Metadata::default(),
                super::super::encode::Content::File,
                input.len() as u64,
            )
            .unwrap();
            node.compression = compression;
            for limit in [minimum, 64] {
                // Force empty and nonempty partial prefixes that would fit inline.
                encoder.limit = limit;
                let (prefix_len, _) = encoder.compress(&input).unwrap();
                assert_eq!(prefix_len == 0, limit == minimum);
                assert!(prefix_len < input.len());
                let mut fitter = ExtentFitter::new(&node, &options);
                assert!(fitter.inline_capacity > limit);
                let (consumed, compressed) =
                    fitter.encode_data(&mut encoder, &input, true, 0).unwrap();
                assert_eq!(consumed, prefix_len.max(BLOCK_SIZE));
                assert_eq!(compressed, prefix_len > BLOCK_SIZE);
                assert!(matches!(fitter.placement, Placement::External));
                if !compressed {
                    assert_eq!(fitter.data, input[..BLOCK_SIZE]);
                }
            }
        }
    }

    #[test]
    fn fitted_files_match_streaming_extents_at_any_block() {
        use super::super::{Metadata, encode::Content};

        let mut seed = 11u32;
        let data: Vec<_> = (0..INPUT_SIZE)
            .map(|i| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                if i % 5000 < 2500 {
                    seed as u8
                } else {
                    (i / 7) as u8
                }
            })
            .collect();
        let mut multiple_extents = false;
        for compression in [
            Compression::Lz4,
            Compression::Lzma,
            Compression::Deflate,
            Compression::Zstd,
        ]
        .into_iter()
        .filter(|compression| compression.enabled())
        {
            for cluster in [4096, 16384] {
                let options = Options::default()
                    .compression(compression)
                    .pcluster_size(cluster);
                let mut reused = Encoder::new(&options);
                for size in [4097, 12_345, 40_000, INPUT_SIZE] {
                    let input = &data[INPUT_SIZE - size..];
                    let mut node =
                        Node::new(&Metadata::default(), Content::File, size as u64).unwrap();
                    node.select_format(&options, 1).unwrap();
                    node.compression = compression;
                    node.big_pcluster = cluster > 4096;
                    for block in [1, 77, 4096] {
                        let mut compressor = Compressor::new(&node, &options);
                        let mut fresh = Encoder::new(&options);
                        let (mut data, mut indexes) = (Vec::new(), Vec::new());
                        let (mut rest, mut at, mut extents) = (input, block, 0);
                        while !rest.is_empty() {
                            let extent = compressor.encode(&mut fresh, rest, at, 0).unwrap();
                            data.extend_from_slice(extent.data);
                            indexes.extend_from_slice(extent.indexes);
                            at += (extent.data.len() / BLOCK_SIZE) as u32;
                            rest = &rest[extent.consumed..];
                            extents += 1;
                        }
                        multiple_extents |= extents > 1;

                        let fitted = FittedFile::fit(&options, &mut reused, input).unwrap();
                        assert_eq!(fitted.extents.len(), extents);
                        let actual = fitted.into_buffered(&node, block);
                        assert_eq!(actual.data, data);
                        assert_eq!(actual.indexes, indexes);
                    }
                }
            }
        }
        assert!(multiple_extents);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_frames_reuse_context_without_carrying_history() {
        fn assert_send<T: Send>() {}
        assert_send::<zrip::CompressContext>();
        assert_send::<super::super::State>();

        let data: Vec<_> = (0..INPUT_SIZE)
            .map(|i| ((i * 31) ^ (i / 97)) as u8)
            .collect();
        let options = Options::default().compression(Compression::Zstd);
        let mut compressor = Encoder::new(&options);
        let mut other = Encoder::new(&options);
        assert!(compressor.zstd.is_none());
        for size in [16384, INPUT_SIZE, 4097, 0, 16384] {
            // Interleave files on the same thread, including different input bytes.
            other.compress(&vec![42; INPUT_SIZE - size]).unwrap();
            let input = &data[..size];
            let (used, frame) = compressor.compress(input).unwrap();
            assert_eq!(used, input.len());
            assert_eq!(frame, zrip::compress(input, 1).unwrap());
            let header = zrip::frame::header::parse_frame_header(frame).unwrap();
            assert!(header.content_checksum);
            assert_eq!(header.frame_content_size, Some(size as u64));
            assert!(header.window_size <= 1 << super::super::encode::ZSTD_WINDOW_LOG);
            assert_eq!(zrip::decompress_with_limit(frame, size).unwrap(), input);
            let mut corrupt = frame.to_vec();
            assert!(compressor.zstd.is_some());
            *corrupt.last_mut().unwrap() ^= 1;
            assert!(zrip::decompress_with_limit(&corrupt, size).is_err());
        }
    }
}
