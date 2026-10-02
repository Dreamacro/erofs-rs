use alloc::{format, vec::Vec};
use bytes::Buf;
use core::ops::Range;

use crate::{Error, Result};

#[cfg(test)]
mod tests;

pub const SLOT_SIZE: usize = 128;

/// An additional backing device, in on-disk device-table order.
/// Device IDs are one-based; device zero is the primary image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Opaque on-disk identifier, not a filename or an implicitly verified digest.
    pub tag: [u8; 64],
    pub blocks: u64,
    /// Start in the unified block address space. Zero disables implicit mapping.
    pub unified_start_block: u64,
}

#[derive(Debug)]
pub struct DeviceTable {
    pub entries: Vec<DeviceInfo>,
    ranges: Vec<(Range<u64>, u16)>,
    block_size: u64,
    primary_size: u64,
}

impl DeviceTable {
    pub fn parse(data: &[u8], block_size: u64, primary_blocks: u64, wide: bool) -> Result<Self> {
        let (slots, remainder) = data.as_chunks::<SLOT_SIZE>();
        if !remainder.is_empty() || slots.len() > usize::from(u16::MAX) {
            return Err(Error::CorruptedData("invalid device table size".into()));
        }
        let mut entries = Vec::new();
        let mut ranges = Vec::new();
        entries
            .try_reserve_exact(slots.len())
            .map_err(|_| Error::OutOfBounds("cannot allocate device table".into()))?;
        ranges
            .try_reserve_exact(slots.len())
            .map_err(|_| Error::OutOfBounds("cannot allocate device ranges".into()))?;
        for (index, slot) in slots.iter().enumerate() {
            let mut fields = &slot[64..];
            let mut blocks = u64::from(fields.get_u32_le());
            let mut unified_start_block = u64::from(fields.get_u32_le());
            if wide {
                blocks |= u64::from(fields.get_u16_le()) << 32;
                unified_start_block |= u64::from(fields.get_u16_le()) << 32;
            }
            let size = blocks
                .checked_mul(block_size)
                .ok_or(Error::Overflow("device size"))?;
            let start = unified_start_block
                .checked_mul(block_size)
                .ok_or(Error::Overflow("device mapping start"))?;
            let end = start
                .checked_add(size)
                .ok_or(Error::Overflow("device mapping end"))?;
            entries.push(DeviceInfo {
                tag: slot[..64].try_into().unwrap(),
                blocks,
                unified_start_block,
            });
            if start != 0 && size != 0 {
                ranges.push((start..end, (index + 1) as u16));
            }
        }
        // Preserve device IDs in table order; only the lookup index is sorted.
        ranges.sort_unstable_by_key(|(range, _)| range.start);
        if ranges
            .windows(2)
            .any(|pair| pair[0].0.end > pair[1].0.start)
        {
            return Err(Error::CorruptedData("overlapping device mappings".into()));
        }
        Ok(Self {
            entries,
            ranges,
            block_size,
            primary_size: primary_blocks
                .checked_mul(block_size)
                .ok_or(Error::Overflow("primary device size"))?,
        })
    }

    /// Resolve data addresses, not primary-image metadata addresses.
    pub fn resolve(&self, mut device: u16, mut offset: u64, size: u64) -> Result<(u16, u64)> {
        let end = offset
            .checked_add(size)
            .ok_or(Error::Overflow("device read range"))?;
        if device == 0 {
            let next = self
                .ranges
                .partition_point(|(range, _)| range.start <= offset);
            if let Some((range, id)) = next.checked_sub(1).map(|index| &self.ranges[index])
                && offset < range.end
            {
                if end > range.end {
                    return Err(Error::CorruptedData(
                        "data crosses device mapping boundary".into(),
                    ));
                }
                device = *id;
                offset -= range.start;
            } else if self
                .ranges
                .get(next)
                .is_some_and(|(range, _)| range.start < end)
            {
                return Err(Error::CorruptedData(
                    "data crosses device mapping boundary".into(),
                ));
            }
        }
        let capacity = if device == 0 {
            self.primary_size
        } else {
            self.entries
                .get(usize::from(device) - 1)
                .ok_or_else(|| Error::CorruptedData(format!("invalid device ID {device}")))?
                .blocks
                * self.block_size
        };
        if offset.checked_add(size).is_none_or(|end| end > capacity) {
            return Err(Error::OutOfBounds(format!("read exceeds device {device}")));
        }
        Ok((device, offset))
    }
}
