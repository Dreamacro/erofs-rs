//! Basic Z_EROFS decompression support.
//!
//! This roughly follows parts of the Linux kernel implementation
//! (fs/erofs/zmap.c, zdata.c), but is not a 1:1 port.
//!
//! The idea is:
//! 1. VLE index  maps logical clusters
//! 2. pclusters - contain compressed data
//! 3. decompression - handled via lz4/lzma/deflate (not implemented here yet)
//!
//! Some details are still incomplete.

#[derive(Debug, Clone, Copy)]
pub struct VLEIndex {
    pub advise: u16,           // compression hints (di_advise)
    pub cluster_offset: u16,   // offset within the decompressed cluster
    pub block_addr: u32,       // physical block (only meaningful for HEAD entries)
}

impl VLEIndex {
    /// Read a VLE index from raw bytes.
    /// Expects at least 8 bytes (little-endian layout).
    pub fn from_bytes(data: &[u8]) -> crate::Result<Self> {
        if data.len() < 8 {
            return Err(crate::Error::NotSupported(
                "VLE index too short".into()
            ));
        }

        Ok(Self {
            advise: u16::from_le_bytes([data[0], data[1]]),
            cluster_offset: u16::from_le_bytes([data[2], data[3]]),
            block_addr: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
        })
    }
}