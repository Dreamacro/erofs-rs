use alloc::{format, string::String, vec::Vec};
use bytes::BufMut;
use crc32c::{crc32c, crc32c_append};
use std::io;

use super::{Compression, InodeFormat, Metadata, Options, SpecialFile};
use crate::{
    Error, Result, Xattrs,
    types::{
        Dirent, DirentFileType, FileMode, InodeCompact, InodeExtended, Layout, MAGIC_NUMBER,
        SUPER_BLOCK_OFFSET, SuperBlock, XattrEntry, XattrHeader,
    },
};

pub(super) const BLOCK_SIZE: usize = 4096;
pub(super) const INODE_SLOT_SIZE: usize = 32;
pub(super) const LZMA_DICT_SIZE: u32 = 64 * 1024;
pub(super) const ZSTD_WINDOW_LOG: u8 = 17;
pub(super) const ZERO_BLOCK: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];
pub(super) const ROOT_OFFSET: usize = SUPER_BLOCK_OFFSET as usize + SuperBlock::size();
pub(super) const MAX_XATTR_SIZE: usize = size_of::<XattrHeader>() + (u16::MAX as usize - 1) * 4;

fn xattr_name(name: &[u8]) -> Result<(u8, &[u8])> {
    if name.is_empty() || name.len() > 255 || name.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid xattr name").into());
    }
    for index in [1, 2, 3, 4, 6] {
        if let Some(suffix) = name.strip_prefix(crate::xattr::namespace(index)?) {
            if suffix.is_empty() != matches!(index, 2 | 3) {
                return Err(
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid xattr name").into(),
                );
            }
            return Ok((index, suffix));
        }
    }
    Err(Error::NotSupported(format!(
        "xattr namespace for {:?}",
        String::from_utf8_lossy(name),
    )))
}

pub(super) fn xattr_entry_size(name: &[u8], value_len: usize) -> Result<usize> {
    let (_, suffix) = xattr_name(name)?;
    if value_len > u16::MAX as usize {
        return Err(Error::Overflow("xattr value size"));
    }
    Ok((size_of::<XattrEntry>() + suffix.len() + value_len).next_multiple_of(4))
}

fn xattr_size(attrs: &Xattrs) -> Result<usize> {
    if attrs.is_empty() {
        return Ok(0);
    }
    let mut size = size_of::<XattrHeader>();
    for (name, value) in attrs {
        size += xattr_entry_size(name, value.len())?;
        if size > MAX_XATTR_SIZE {
            return Err(Error::Overflow("inode xattr size"));
        }
    }
    Ok(size)
}

fn xattrs(attrs: &Xattrs) -> Result<Vec<u8>> {
    let size = xattr_size(attrs)?;
    let mut data = Vec::new();
    data.try_reserve_exact(size)
        .map_err(|_| Error::OutOfBounds("cannot allocate inode xattrs".into()))?;
    data.resize(size.min(size_of::<XattrHeader>()), 0); // No filter, shared IDs or extensions.
    for (name, value) in attrs {
        let (index, suffix) = xattr_name(name)?;
        data.put_u8(suffix.len() as u8);
        data.put_u8(index);
        data.put_u16_le(value.len() as u16);
        data.put_slice(suffix);
        data.put_slice(value);
        data.resize(data.len().next_multiple_of(4), 0);
    }
    Ok(data)
}

pub(super) struct MetadataBlock {
    pub block: u32,
    pub used: usize,
    pub data: [u8; BLOCK_SIZE],
}

impl MetadataBlock {
    pub fn new(block: u32) -> Self {
        Self {
            block,
            used: 0,
            data: ZERO_BLOCK,
        }
    }

    pub fn inline_data_mut(&mut self, node: &Node) -> &mut [u8] {
        debug_assert_eq!(self.block, node.last_metadata_block());
        let start = (node.inline_offset() % BLOCK_SIZE as u64) as usize;
        &mut self.data[start..start + node.inline_size]
    }

    pub fn write_inode(&mut self, node: &Node, node_index: usize) {
        self.write_at(
            node.inode_offset(),
            &inode(node, node_index)[..node.inode_size()],
        );
        self.write_at(node.inode_offset() + node.inode_size() as u64, &node.xattrs);
        if let Some(algorithm) = node.compression.algorithm() {
            let mut header = [0; 16];
            let mut fields = &mut header[..];
            fields.put_bytes(0, 6); // No inline data or advice.
            fields.put_u8(algorithm); // HEAD1 codec; HEAD2 is unused.
            fields.put_u8(0); // 4 KiB logical clusters.
            self.write_at(node.index_offset() - 16, &header);
        }
        let end = node.metadata_end();
        self.used = ((end - u64::from(self.block) * BLOCK_SIZE as u64).min(BLOCK_SIZE as u64)
            as usize)
            .next_multiple_of(INODE_SLOT_SIZE);
    }

    pub fn write_at(&mut self, offset: u64, data: &[u8]) {
        let base = u64::from(self.block) * BLOCK_SIZE as u64;
        let skip = base.saturating_sub(offset).min(data.len() as u64) as usize;
        let start = offset.saturating_sub(base).min(BLOCK_SIZE as u64) as usize;
        let len = (BLOCK_SIZE - start).min(data.len() - skip);
        self.data[start..start + len].copy_from_slice(&data[skip..skip + len]);
    }
}

pub(super) struct Entry {
    pub name: Vec<u8>,
    pub node_index: usize,
    pub kind: DirentFileType,
}

pub(super) enum Content {
    File,
    Directory { parent: usize, entries: Vec<Entry> },
    Symlink,
    Special(SpecialFile),
}

impl Content {
    pub fn file_type(&self) -> DirentFileType {
        match self {
            Self::File => DirentFileType::RegularFile,
            Self::Directory { .. } => DirentFileType::Directory,
            Self::Symlink => DirentFileType::Symlink,
            Self::Special(SpecialFile::Fifo) => DirentFileType::Fifo,
            Self::Special(SpecialFile::CharacterDevice { .. }) => DirentFileType::CharacterDevice,
            Self::Special(SpecialFile::BlockDevice { .. }) => DirentFileType::BlockDevice,
        }
    }
}

// NIDs address 32-byte slots, including variable-sized inline records. No source
// handles or per-file tail buffers are retained; only one metadata block is pending.
// ponytail: multi-block records start fresh, leaving up to one block of slack;
// pack continuation pages only if large-xattr workloads warrant it.
pub(super) struct Node {
    pub metadata: Metadata,
    pub content: Content,
    pub size: u64,
    pub nlink: u32,
    // On-disk i_u: a flat data address, or the compressed physical block count.
    pub data_block: u32,
    pub nid: u64,
    pub inline_size: usize,
    pub compact: bool,
    pub compression: Compression,
    pub xattr_size: usize,
    // Only directory attributes survive an append; file attributes are streamed.
    pub xattrs: Vec<u8>,
}

impl Node {
    pub fn new(metadata: &Metadata, content: Content, size: u64) -> Result<Self> {
        metadata.validate()?;
        if let Content::Special(kind) = content {
            kind.validate()?;
        }
        let nlink = if matches!(content, Content::Directory { .. }) {
            2
        } else {
            1
        };
        let xattrs = xattrs(&metadata.xattrs)?;
        Ok(Self {
            metadata: Metadata {
                mode: metadata.mode,
                uid: metadata.uid,
                gid: metadata.gid,
                modified: metadata.modified,
                xattrs: Xattrs::new(),
            },
            nlink,
            content,
            size,
            data_block: 0,
            nid: 0,
            inline_size: 0,
            compact: false,
            compression: Compression::None,
            xattr_size: xattrs.len(),
            xattrs,
        })
    }

    pub fn can_compact(&self, options: &Options, nlink: u32) -> bool {
        self.size <= u64::from(u32::MAX)
            && self.metadata.uid <= u32::from(u16::MAX)
            && self.metadata.gid <= u32::from(u16::MAX)
            && nlink <= u32::from(u16::MAX)
            // Legacy readers inherit the whole timestamp. Nonzero compact
            // mtime deltas would require the newer 48-bit addressing feature.
            && self.metadata.modified == options.build_time
    }

    pub fn select_format(&mut self, options: &Options, nlink: u32) -> Result<()> {
        let fits = self.can_compact(options, nlink);
        if options.inode_format == InodeFormat::Compact && !fits {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "inode metadata does not fit Compact format",
            )
            .into());
        }
        self.compact = options.inode_format != InodeFormat::Extended && fits;
        Ok(())
    }

    pub fn inode_size(&self) -> usize {
        if self.compact {
            InodeCompact::size()
        } else {
            InodeExtended::size()
        }
    }

    pub fn inode_offset(&self) -> u64 {
        self.nid * INODE_SLOT_SIZE as u64
    }

    pub fn metadata_block(&self) -> u32 {
        (self.inode_offset() / BLOCK_SIZE as u64) as u32
    }

    pub fn inline_offset(&self) -> u64 {
        self.inode_offset() + self.inode_size() as u64 + self.xattr_size as u64
    }

    pub fn index_offset(&self) -> u64 {
        self.inline_offset().next_multiple_of(8) + 16
    }

    pub fn metadata_end(&self) -> u64 {
        if self.compression != Compression::None {
            self.index_offset() + self.size.div_ceil(BLOCK_SIZE as u64) * 8
        } else {
            self.inline_offset() + self.inline_size as u64
        }
    }

    pub fn last_metadata_block(&self) -> u32 {
        ((self.metadata_end() - 1) / BLOCK_SIZE as u64) as u32
    }

    pub fn first_index_block(&self) -> u32 {
        (self.index_offset() / BLOCK_SIZE as u64) as u32
    }

    pub fn external_size(&self) -> u64 {
        self.size - self.inline_size as u64
    }
}

fn directory_blocks(mut entries: &[Entry]) -> impl Iterator<Item = &[Entry]> {
    core::iter::from_fn(move || {
        if entries.is_empty() {
            return None;
        }
        let mut size = 0;
        let count = entries
            .iter()
            .take_while(|entry| {
                size += Dirent::size() + entry.name.len();
                size <= BLOCK_SIZE
            })
            .count();
        // Validated names are at most 255 bytes, so every block makes progress.
        let (block, rest) = entries.split_at(count);
        entries = rest;
        Some(block)
    })
}

pub(super) fn directory_size(entries: &[Entry]) -> Result<u64> {
    let mut size = 0u64;
    for block in directory_blocks(entries) {
        size = size
            .div_ceil(BLOCK_SIZE as u64)
            .checked_mul(BLOCK_SIZE as u64)
            .and_then(|size| {
                size.checked_add(
                    block
                        .iter()
                        .map(|entry| (Dirent::size() + entry.name.len()) as u64)
                        .sum(),
                )
            })
            .ok_or(Error::Overflow("directory size"))?;
    }
    Ok(size)
}

fn directory_block(entries: &[Entry], nodes: &[Node]) -> [u8; BLOCK_SIZE] {
    let mut block = ZERO_BLOCK;
    let mut name_at = entries.len() * Dirent::size();
    for (index, entry) in entries.iter().enumerate() {
        let mut fields = &mut block[index * Dirent::size()..];
        fields.put_u64_le(nodes[entry.node_index].nid);
        fields.put_u16_le(name_at as u16);
        fields.put_u8(entry.kind as u8);
        fields.put_u8(0);
        block[name_at..name_at + entry.name.len()].copy_from_slice(&entry.name);
        name_at += entry.name.len();
    }
    block
}

pub(super) fn inode(node: &Node, node_index: usize) -> [u8; InodeExtended::size()] {
    let kind = match node.content {
        Content::File => FileMode::IRREGULAR,
        Content::Directory { .. } => FileMode::DIR,
        Content::Symlink => FileMode::SYMLINK,
        Content::Special(SpecialFile::Fifo) => FileMode::NAMED_PIPE,
        Content::Special(SpecialFile::CharacterDevice { .. }) => FileMode::CHAR_DEVICE,
        Content::Special(SpecialFile::BlockDevice { .. }) => FileMode::BLOCK_DEVICE,
    };
    let inode_data = match node.content {
        Content::Special(
            SpecialFile::CharacterDevice { major, minor }
            | SpecialFile::BlockDevice { major, minor },
        ) => (major << 8) | (minor & 0xff) | ((minor & 0xfff00) << 12),
        _ => node.data_block,
    };
    let layout = if node.compression != Compression::None {
        Layout::CompressedFull
    } else if node.inline_size == 0 {
        Layout::FlatPlain
    } else {
        Layout::FlatInline
    };
    let mut inode = [0; InodeExtended::size()];
    let mut fields = &mut inode[..];
    fields.put_u16_le(u16::from(!node.compact) | (layout as u16) << 1);
    fields.put_u16_le(if node.xattr_size == 0 {
        0
    } else {
        ((node.xattr_size - size_of::<XattrHeader>()) / 4 + 1) as u16
    });
    fields.put_u16_le(node.metadata.mode | kind.bits());
    if node.compact {
        fields.put_u16_le(node.nlink as u16);
        fields.put_u32_le(node.size as u32);
        fields.put_u32_le(0); // mtime delta; the complete timestamp equals the superblock's.
        fields.put_u32_le(inode_data);
        fields.put_u32_le(node_index as u32 + 1);
        fields.put_u16_le(node.metadata.uid as u16);
        fields.put_u16_le(node.metadata.gid as u16);
    } else {
        fields.put_u16_le(0);
        fields.put_u64_le(node.size);
        fields.put_u32_le(inode_data);
        fields.put_u32_le(node_index as u32 + 1);
        fields.put_u32_le(node.metadata.uid);
        fields.put_u32_le(node.metadata.gid);
        fields.put_i64_le(node.metadata.modified.0);
        fields.put_u32_le(node.metadata.modified.1);
        fields.put_u32_le(node.nlink);
    }
    inode
}

// Directory records are allocated after payloads, starting in the superblock's
// unused space. File records have already been streamed in packed metadata blocks.
pub(super) fn metadata_blocks(nodes: &[Node]) -> impl Iterator<Item = (u64, [u8; BLOCK_SIZE])> {
    directory_data_blocks(nodes).chain(directory_inode_blocks(nodes))
}

fn directory_data_blocks(nodes: &[Node]) -> impl Iterator<Item = (u64, [u8; BLOCK_SIZE])> {
    nodes
        .iter()
        .filter_map(|node| match &node.content {
            Content::Directory { entries, .. } => Some((node, entries.as_slice())),
            _ => None,
        })
        .flat_map(move |(node, entries)| {
            let blocks = node.external_size().div_ceil(BLOCK_SIZE as u64);
            directory_blocks(entries)
                .take(blocks as usize)
                .enumerate()
                .map(move |(index, entries)| {
                    (
                        (u64::from(node.data_block) + index as u64) * BLOCK_SIZE as u64,
                        directory_block(entries, nodes),
                    )
                })
        })
}

fn directory_inode_blocks(nodes: &[Node]) -> impl Iterator<Item = (u64, [u8; BLOCK_SIZE])> {
    // Directory records are ordered; an attribute body may span several blocks.
    let mut inodes = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| matches!(node.content, Content::Directory { .. }))
        .peekable();
    let mut next_block = 0;
    core::iter::from_fn(move || {
        let block = next_block.max(inodes.peek()?.1.metadata_block());
        let mut page = MetadataBlock::new(block);
        while let Some(&(node_index, node)) = inodes.peek() {
            if node.metadata_block() > block {
                break;
            }
            page.write_inode(node, node_index);
            if node.last_metadata_block() != block {
                break;
            }
            if let Content::Directory { entries, .. } = &node.content
                && node.inline_size != 0
            {
                let tail = directory_block(directory_blocks(entries).last().unwrap(), nodes);
                page.inline_data_mut(node)
                    .copy_from_slice(&tail[..node.inline_size]);
            }
            inodes.next();
        }
        next_block = block + 1;
        Some((u64::from(block) * BLOCK_SIZE as u64, page.data))
    })
}

pub(super) fn padding(size: u64) -> &'static [u8] {
    let padding = (BLOCK_SIZE as u64 - size % BLOCK_SIZE as u64) % BLOCK_SIZE as u64;
    &ZERO_BLOCK[..padding as usize]
}

pub(super) fn superblock(inodes: usize, blocks: u32, root_nid: u64, options: &Options) -> Vec<u8> {
    let mut superblock = vec![0; SuperBlock::size()];
    let mut fields = &mut superblock[..];
    fields.put_u32_le(MAGIC_NUMBER);
    fields.put_u32_le(0); // Checksum is filled after the first metadata block is encoded.
    fields.put_u32_le(1); // EROFS_FEATURE_COMPAT_SB_CHKSUM.
    fields.put_u8(12);
    fields.put_u8(0);
    let wide_root = root_nid > u64::from(u16::MAX);
    fields.put_u16_le(if wide_root { 0 } else { root_nid as u16 });
    fields.put_u64_le(inodes as u64);
    // Creation time is epoch + build_time delta. Keep the delta zero; compact
    // inodes inherit this timestamp, while extended inode mtimes are independent.
    fields.put_i64_le(options.build_time.0);
    fields.put_u32_le(options.build_time.1);
    fields.put_u32_le(blocks);
    fields.put_u32_le(0); // NIDs are relative to the start of the image.
    fields.put_u32_le(0); // No shared xattrs.
    fields.put_slice(&options.uuid);
    fields.put_bytes(0, 16); // volume_name.
    let compression = options.compression;
    let flags = u32::from(compression != Compression::None)
        | if compression.config_size() != 0 { 2 } else { 0 };
    fields.put_u32_le(flags | if wide_root { 0x80 } else { 0 }); // 0PADDING / COMPR_CFGS / 48-bit.
    fields.put_u16_le(match compression {
        Compression::None => 0,
        Compression::Lz4 => u16::MAX, // Legacy LZ4 maximum match distance; no config record.
        _ => 1 << compression.algorithm().unwrap(),
    });
    fields.put_bytes(0, 26); // Unused layout fields and the zero build-time delta.
    fields.put_u64_le(if wide_root { root_nid } else { 0 }); // root_nid_wide.
    // Global codec configuration follows the base superblock. It is written with
    // the final header and included in its CRC, even when all root metadata moved.
    match compression {
        Compression::None | Compression::Lz4 => {}
        Compression::Lzma => {
            superblock.put_u16_le(14);
            superblock.put_u32_le(LZMA_DICT_SIZE);
            superblock.put_bytes(0, 10); // MicroLZMA format 0 and reserved fields.
        }
        Compression::Deflate => {
            superblock.put_u16_le(6);
            superblock.put_u8(15); // Raw DEFLATE, 32 KiB window.
            superblock.put_bytes(0, 5);
        }
        Compression::Zstd => {
            superblock.put_u16_le(6);
            superblock.put_u8(0); // Standard Zstd frame format.
            superblock.put_u8(ZSTD_WINDOW_LOG - 10);
            superblock.put_bytes(0, 4);
        }
    }
    // A relocated root leaves the suffix after the header/configuration zeroed.
    if root_nid >= (BLOCK_SIZE / INODE_SLOT_SIZE) as u64 {
        set_superblock_checksum(&mut superblock, &ZERO_BLOCK);
    }
    superblock
}

pub(super) fn set_superblock_checksum(superblock: &mut [u8], first_block: &[u8; BLOCK_SIZE]) {
    // For 4 KiB blocks, EROFS covers image bytes 1024..4096, including packed
    // directory metadata and padding, with the checksum field treated as zero.
    // Later hard-link patches only affect non-directory inodes outside block 0.
    superblock[4..8].fill(0);
    // The crate applies a final XOR; EROFS stores the uncomplemented CRC state.
    let suffix = SUPER_BLOCK_OFFSET as usize + superblock.len();
    let crc = !crc32c_append(crc32c(superblock), &first_block[suffix..]);
    (&mut superblock[4..]).put_u32_le(crc);
}
