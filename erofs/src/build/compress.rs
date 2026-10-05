//! Bounded compression and Full indexes; both I/O drivers use the same decisions.

use alloc::vec::Vec;
use bytes::BufMut;
#[cfg(any(feature = "lz4", feature = "lzma", feature = "deflate"))]
use std::io;

use super::{
    Compression,
    encode::{BLOCK_SIZE, ZERO_BLOCK},
};
use crate::{Error, Result};

pub(super) const INPUT_SIZE: usize = 64 * 1024;

pub(super) struct Compressor {
    compression: Compression,
    scratch: Vec<u8>,
    pub data: [u8; BLOCK_SIZE],
    pub indexes: [u8; INPUT_SIZE / BLOCK_SIZE * 8],
}

impl Compressor {
    pub fn new(compression: Compression) -> Self {
        Self {
            compression,
            scratch: Vec::new(),
            data: ZERO_BLOCK,
            indexes: [0; INPUT_SIZE / BLOCK_SIZE * 8],
        }
    }

    /// Consumes a block-aligned prefix, or a final short PLAIN block. Compressed
    /// extents never end inside a logical cluster, so no EOF sentinel is needed.
    pub fn encode(&mut self, input: &[u8], block: u32) -> Result<usize> {
        debug_assert!(!input.is_empty() && input.len() <= INPUT_SIZE);
        let mut consumed = input.len() / BLOCK_SIZE * BLOCK_SIZE;
        let mut compressed = false;
        self.data.fill(0);
        // ponytail: halve aligned prefixes until they fit one physical block.
        // Use destination-size compression if measured ratios justify a new encoder.
        while consumed > BLOCK_SIZE {
            if let Some(len) = self.compress(&input[..consumed])?
                && len <= BLOCK_SIZE
                // EROFS skips leading zeros. A zero-leading DEFLATE stored block
                // cannot be distinguished from padding, so leave that region PLAIN.
                && self.scratch[0] != 0
            {
                self.data[BLOCK_SIZE - len..].copy_from_slice(&self.scratch[..len]);
                compressed = true;
                break;
            }
            consumed = consumed / 2 / BLOCK_SIZE * BLOCK_SIZE;
        }
        if !compressed {
            consumed = input.len().min(BLOCK_SIZE);
            self.data[..consumed].copy_from_slice(&input[..consumed]);
        }
        let count = consumed.div_ceil(BLOCK_SIZE);
        let mut fields = &mut self.indexes[..];
        for index in 0..count {
            fields.put_u16_le(if index != 0 { 2 } else { u16::from(compressed) });
            fields.put_u16_le(0); // Every extent starts at a logical-block boundary.
            if index == 0 {
                fields.put_u32_le(block);
            } else {
                fields.put_u16_le(index as u16);
                fields.put_u16_le((count - index) as u16);
            }
        }
        Ok(consumed)
    }

    // None means the encoder filled its bounded output before finishing.
    fn compress(&mut self, input: &[u8]) -> Result<Option<usize>> {
        let len = match self.compression {
            #[cfg(feature = "lz4")]
            Compression::Lz4 => {
                self.scratch
                    .resize(lz4_flex::block::get_maximum_output_size(INPUT_SIZE), 0);
                lz4_flex::block::compress_into(input, &mut self.scratch)
                    .map_err(io::Error::other)?
            }
            #[cfg(feature = "lzma")]
            Compression::Lzma => {
                use lzma_rust2::{LzmaOptions, LzmaWriter};
                use std::io::Write;

                self.scratch.resize(BLOCK_SIZE, 0);
                let mut output = io::Cursor::new(self.scratch.as_mut_slice());
                let mut options = LzmaOptions::with_preset(1);
                options.dict_size = super::encode::LZMA_DICT_SIZE;
                let mut encoder = LzmaWriter::new(
                    &mut output,
                    &options,
                    false,
                    false,
                    Some(input.len() as u64),
                )?;
                let props = encoder.props();
                match encoder
                    .write_all(input)
                    .and_then(|()| encoder.finish().map(|_| ()))
                {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::WriteZero => return Ok(None),
                    Err(error) => return Err(error.into()),
                }
                let len = output.position() as usize;
                // MicroLZMA is raw LZMA1 without an end marker, replacing its
                // initial zero byte with the complemented properties byte.
                debug_assert!(len >= 5 && self.scratch[0] == 0);
                self.scratch[0] = !props;
                len
            }
            #[cfg(feature = "deflate")]
            Compression::Deflate => {
                use miniz_oxide::deflate::core::{
                    CompressorOxide, TDEFLFlush, TDEFLStatus, compress,
                    create_comp_flags_from_zip_params,
                };

                self.scratch.resize(BLOCK_SIZE, 0);
                let mut encoder = CompressorOxide::new(create_comp_flags_from_zip_params(6, 0, 0));
                let (status, read, written) =
                    compress(&mut encoder, input, &mut self.scratch, TDEFLFlush::Finish);
                match status {
                    TDEFLStatus::Done if read == input.len() => written,
                    TDEFLStatus::Okay => return Ok(None),
                    _ => return Err(io::Error::other("DEFLATE encoding failed").into()),
                }
            }
            #[cfg(feature = "zstd")]
            Compression::Zstd => {
                // ruzstd's encoder unwraps I/O errors: only give it infallible
                // slice/Vec I/O, never the caller's source or image output.
                self.scratch = ruzstd::encoding::compress_to_vec(
                    input,
                    ruzstd::encoding::CompressionLevel::Fastest,
                );
                self.scratch.len()
            }
            _ => {
                return Err(Error::NotSupported(format!(
                    "compression encoder {:?} is disabled",
                    self.compression
                )));
            }
        };
        Ok(Some(len))
    }
}
