#[cfg(feature = "std")]
use std::{
    fs::Permissions,
    os::unix::fs::PermissionsExt,
    time::{Duration, SystemTime},
};

use alloc::format;
use binrw::BinRead;
use rustix::fs::FileType;

use crate::Error;

pub const MAGIC_NUMBER: u32 = 0xe0f5e1e2;
pub const SUPER_BLOCK_OFFSET: u64 = 1024;

pub const LAYOUT_CHUNK_FORMAT_BITS: u16 = 0x001F;
pub const LAYOUT_CHUNK_FORMAT_INDEXES: u16 = 0x0020;

pub const SB_EXTSLOT_SIZE: usize = 16;

#[repr(C)]
#[derive(Debug, Clone, Copy, BinRead)]
#[br(little)]
pub struct SuperBlock {
    pub magic: u32,
    pub checksum: u32,
    pub feature_compat: u32,
    pub blk_size_bits: u8,
    pub ext_slots: u8,
    pub root_nid: u16,
    pub inos: u64,
    /// Raw two's-complement Unix seconds used by compact inodes.
    pub build_time: u64,
    pub build_time_ns: u32,
    pub blocks: u32,
    pub meta_blk_addr: u32,
    pub xattr_blk_addr: u32,
    pub uuid: [u8; 16],
    pub volume_name: [u8; 16],
    pub feature_incompat: u32,
    pub compr_algs: u16,
    pub extra_devices: u16,
    pub devt_slot_off: u16,
    pub dir_blk_bits: u8,
    pub xattr_prefix_count: u8,
    pub xattr_prefix_start: u32,
    pub packed_nid: u64,
    pub xattr_filter_res: u8,
    pub reserved: [u8; 23],
}

impl SuperBlock {
    #[inline]
    pub const fn size() -> usize {
        size_of::<Self>()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Layout {
    FlatPlain = 0,
    CompressedFull = 1,
    FlatInline = 2,
    CompressedCompact = 3,
    ChunkBased = 4,
}

impl TryFrom<u8> for Layout {
    type Error = Error;
    fn try_from(x: u8) -> Result<Self, Error> {
        use Layout::*;
        match x {
            0 => Ok(FlatPlain),
            1 => Ok(CompressedFull),
            2 => Ok(FlatInline),
            3 => Ok(CompressedCompact),
            4 => Ok(ChunkBased),
            x => Err(Error::NotSupported(format!("inode data layout {x}"))),
        }
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FileMode: u16 {
        const READ = 0o400;
        const WRITE = 0o200;
        const EXEC = 0o100;
        const READ_GROUP = 0o040;
        const WRITE_GROUP = 0o020;
        const EXEC_GROUP = 0o010;
        const READ_OTHER = 0o004;
        const WRITE_OTHER = 0o002;
        const EXEC_OTHER = 0o001;
        const DIR = 0o040000;
        const CHAR_DEVICE = 0o020000;
        const BLOCK_DEVICE = 0o060000;
        const NAMED_PIPE = 0o010000;
        const SOCKET = 0o140000;
        const SYMLINK = 0o120000;
        const IRREGULAR = 0o100000;
        const SETUID = 0o004000;
        const SETGID = 0o002000;
        const STICKY = 0o001000;
    }
}

impl FileMode {
    pub fn is_dir(&self) -> bool {
        FileType::from_raw_mode(self.bits() as _).is_dir()
    }

    pub fn is_file(&self) -> bool {
        FileType::from_raw_mode(self.bits() as _).is_file()
    }
}

/// Validated inode metadata, independent of the on-disk inode version.
///
/// Obtained from a filesystem or directory entry. Raw disk structures remain
/// available as [`InodeCompact`] and [`InodeExtended`], but cannot be used as
/// file handles without parsing and validation.
#[derive(Debug, Clone, Copy)]
pub struct Inode {
    pub(crate) nid: u64,
    pub(crate) data_size: u64,
    pub(crate) file_type: FileType,
    pub(crate) mode: u16,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) nlink: u32,
    pub(crate) modified: (i64, u32),
    pub(crate) data: InodeData,
    pub(crate) inode_size: usize,
    pub(crate) xattr_size: usize,
}

/// Decoded interpretation of the inode's data union.
#[derive(Debug, Clone, Copy)]
pub(crate) enum InodeData {
    FlatPlain { start_block: u32 },
    FlatInline { start_block: u32 },
    Hole,
    ChunkBased { chunk_size: u64, indexes: bool },
    CompressedFull,
    CompressedCompact,
    Device { major: u32, minor: u32 },
    None,
}

impl Inode {
    pub fn id(&self) -> u64 {
        self.nid
    }

    /// Returns the data layout, or `None` for device nodes, FIFOs and sockets.
    pub fn layout(&self) -> Option<Layout> {
        match self.data {
            InodeData::FlatPlain { .. } | InodeData::Hole => Some(Layout::FlatPlain),
            InodeData::FlatInline { .. } => Some(Layout::FlatInline),
            InodeData::ChunkBased { .. } => Some(Layout::ChunkBased),
            InodeData::CompressedFull => Some(Layout::CompressedFull),
            InodeData::CompressedCompact => Some(Layout::CompressedCompact),
            InodeData::Device { .. } | InodeData::None => None,
        }
    }

    /// Returns the logical file size, without narrowing to the host pointer width.
    pub fn data_size(&self) -> u64 {
        self.data_size
    }

    /// Returns the size of the inode's on-disk xattr body in bytes.
    pub fn xattr_size(&self) -> usize {
        self.xattr_size
    }

    pub fn file_type(&self) -> FileType {
        self.file_type
    }

    pub fn is_dir(&self) -> bool {
        self.file_type.is_dir()
    }

    pub fn is_file(&self) -> bool {
        self.file_type.is_file()
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type.is_symlink()
    }

    /// Returns permission and special mode bits, without the file type bits.
    #[cfg(feature = "std")]
    pub fn permissions(&self) -> Permissions {
        Permissions::from_mode(self.mode.into())
    }

    /// Returns permission and special mode bits, without the file type bits.
    #[cfg(not(feature = "std"))]
    pub fn permissions(&self) -> u16 {
        self.mode
    }

    /// Returns signed Unix seconds plus nanoseconds in `0..1_000_000_000`.
    /// Compact inode timestamps are resolved from the superblock during parsing.
    pub fn modified_unix(&self) -> (i64, u32) {
        self.modified
    }

    /// Returns the timestamp, or `None` if the platform cannot represent it.
    #[cfg(feature = "std")]
    pub fn modified(&self) -> Option<SystemTime> {
        let (secs, nanos) = self.modified;
        let duration = Duration::from_secs(secs.unsigned_abs());
        let time = if secs < 0 {
            SystemTime::UNIX_EPOCH.checked_sub(duration)?
        } else {
            SystemTime::UNIX_EPOCH.checked_add(duration)?
        };
        time.checked_add(Duration::from_nanos(u64::from(nanos)))
    }

    pub fn gid(&self) -> u32 {
        self.gid
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// Returns the inode's hard link count.
    pub fn nlink(&self) -> u32 {
        self.nlink
    }

    /// Returns `(major, minor)` for character and block devices, otherwise `None`.
    pub fn device(&self) -> Option<(u32, u32)> {
        match self.data {
            InodeData::Device { major, minor } => Some((major, minor)),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, BinRead)]
#[br(little)]
pub struct InodeCompact {
    pub format: u16,
    pub xattr_count: u16,
    pub mode: u16,
    pub nlink: u16,
    pub size: u32,
    pub reserved: u32,
    pub inode_data: u32,
    pub inode: u32,
    pub uid: u16,
    pub gid: u16,
    pub reserved2: u32,
}

impl InodeCompact {
    #[inline]
    pub const fn size() -> usize {
        size_of::<Self>()
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, BinRead)]
#[br(little)]
pub struct InodeExtended {
    pub format: u16,
    pub xattr_count: u16,
    pub mode: u16,
    pub reserved: u16,
    pub size: u64,
    pub inode_data: u32,
    pub inode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Raw two's-complement Unix seconds; see [`Inode::modified_unix`].
    pub mtime: u64,
    pub mtime_ns: u32,
    pub nlink: u32,
    pub reserved2: [u8; 16],
}

impl InodeExtended {
    #[inline]
    pub const fn size() -> usize {
        size_of::<Self>()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DirentFileType {
    Unknown = 0,
    RegularFile = 1,
    Directory = 2,
    CharacterDevice = 3,
    BlockDevice = 4,
    Fifo = 5,
    Socket = 6,
    Symlink = 7,
}

impl DirentFileType {
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Directory)
    }

    pub fn is_file(&self) -> bool {
        matches!(self, Self::RegularFile)
    }

    pub fn is_symlink(&self) -> bool {
        matches!(self, Self::Symlink)
    }
}

impl TryFrom<u8> for DirentFileType {
    type Error = Error;
    fn try_from(x: u8) -> Result<Self, Error> {
        use DirentFileType::*;
        match x {
            0 => Ok(Unknown),
            1 => Ok(RegularFile),
            2 => Ok(Directory),
            3 => Ok(CharacterDevice),
            4 => Ok(BlockDevice),
            5 => Ok(Fifo),
            6 => Ok(Socket),
            7 => Ok(Symlink),
            _ => Err(Error::InvalidDirentFileType(x)),
        }
    }
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default, BinRead)]
#[br(little)]
pub struct Dirent {
    pub nid: u64,
    pub name_off: u16,
    pub file_type: u8,
    pub reserved: u8,
}

impl Dirent {
    #[inline]
    pub const fn size() -> usize {
        size_of::<Self>()
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, BinRead)]
#[br(little)]
pub struct XattrHeader {
    pub name_filter: u32,
    pub shared_count: u8,
    pub reserved: [u8; 7],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, BinRead)]
#[br(little)]
pub struct XattrEntry {
    pub name_len: u8,
    pub name_index: u8,
    pub value_len: u16,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, BinRead)]
#[br(little)]
pub struct XattrLongPrefixItem {
    pub prefix_addr: u32,
    pub prefix_len: u8,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, BinRead)]
#[br(little)]
pub struct XattrLongPrefix {
    pub base_index: u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, BinRead)]
#[br(little)]
pub struct MapHeader {
    pub _reserved: u16,
    pub data_size: u16,
    pub advise: u16,
    // algorithm type (bit 0-3: HEAD1; bit 4-7: HEAD2)
    pub algorithmtype: u8,
    /*
     * bit 0-3 : logical cluster bits - blkszbits
     * bit 4-6 : reserved
     * bit 7   : pack the whole file into packed inode
     */
    pub clusterbits: u8,
}

impl MapHeader {
    #[inline]
    pub const fn size() -> usize {
        size_of::<Self>()
    }

    pub fn fragmentoff(&self) -> u32 {
        u32::from_le((self._reserved as u32) << 16 | u32::from(self.data_size))
    }
}
