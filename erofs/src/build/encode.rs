use alloc::vec::Vec;
use bytes::BufMut;

use super::Metadata;
use crate::{
    Error, Result,
    types::{
        Dirent, DirentFileType, FileMode, InodeExtended, Layout, MAGIC_NUMBER, SUPER_BLOCK_OFFSET,
        SuperBlock,
    },
};

pub(super) const BLOCK_SIZE: usize = 4096;
pub(super) const INODE_SLOT_SIZE: usize = 32;
pub(super) const ZERO_BLOCK: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];
pub(super) const ROOT_OFFSET: usize = SUPER_BLOCK_OFFSET as usize + SuperBlock::size();

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
        debug_assert_eq!(self.block, node.metadata_block());
        let start = (node.inode_offset() % BLOCK_SIZE as u64) as usize + InodeExtended::size();
        &mut self.data[start..start + node.inline_size]
    }

    pub fn write_inode(&mut self, node: &Node, node_index: usize) {
        debug_assert_eq!(self.block, node.metadata_block());
        let start = (node.inode_offset() % BLOCK_SIZE as u64) as usize;
        let end = start + InodeExtended::size();
        self.data[start..end].copy_from_slice(&inode(node, node_index));
        self.used = (end + node.inline_size).next_multiple_of(INODE_SLOT_SIZE);
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
}

impl Content {
    pub fn file_type(&self) -> DirentFileType {
        match self {
            Self::File => DirentFileType::RegularFile,
            Self::Directory { .. } => DirentFileType::Directory,
            Self::Symlink => DirentFileType::Symlink,
        }
    }
}

// NIDs address 32-byte slots, including variable-sized inline records. No source
// handles or per-file tail buffers are retained; only one metadata block is pending.
pub(super) struct Node {
    pub metadata: Metadata,
    pub content: Content,
    pub size: u64,
    pub nlink: u32,
    pub data_block: u32,
    pub nid: u64,
    pub inline_size: usize,
}

impl Node {
    pub fn inode_offset(&self) -> u64 {
        self.nid * INODE_SLOT_SIZE as u64
    }

    pub fn metadata_block(&self) -> u32 {
        (self.inode_offset() / BLOCK_SIZE as u64) as u32
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
    };
    let layout = if node.inline_size == 0 {
        Layout::FlatPlain
    } else {
        Layout::FlatInline
    };
    let mut inode = [0; InodeExtended::size()];
    let mut fields = &mut inode[..];
    fields.put_u16_le(1 | (layout as u16) << 1); // Extended inode version, then data layout.
    fields.put_u16_le(0); // No xattrs.
    fields.put_u16_le(node.metadata.mode | kind.bits());
    fields.put_u16_le(0);
    fields.put_u64_le(node.size);
    fields.put_u32_le(node.data_block);
    fields.put_u32_le(node_index as u32 + 1);
    fields.put_u32_le(node.metadata.uid);
    fields.put_u32_le(node.metadata.gid);
    fields.put_i64_le(node.metadata.modified.0);
    fields.put_u32_le(node.metadata.modified.1);
    fields.put_u32_le(node.nlink);
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
    // Directory inodes are allocated in node order, so each block forms one group.
    let mut inodes = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| matches!(node.content, Content::Directory { .. }))
        .peekable();
    core::iter::from_fn(move || {
        let block = inodes.peek()?.1.metadata_block();
        let mut page = MetadataBlock::new(block);
        while let Some((node_index, node)) =
            inodes.next_if(|(_, node)| node.metadata_block() == block)
        {
            page.write_inode(node, node_index);
            if let Content::Directory { entries, .. } = &node.content
                && node.inline_size != 0
            {
                let tail = directory_block(directory_blocks(entries).last().unwrap(), nodes);
                page.inline_data_mut(node)
                    .copy_from_slice(&tail[..node.inline_size]);
            }
        }
        Some((u64::from(block) * BLOCK_SIZE as u64, page.data))
    })
}

pub(super) fn padding(size: u64) -> &'static [u8] {
    let padding = (BLOCK_SIZE as u64 - size % BLOCK_SIZE as u64) % BLOCK_SIZE as u64;
    &ZERO_BLOCK[..padding as usize]
}

pub(super) fn superblock(inodes: usize, blocks: u32) -> [u8; SuperBlock::size()] {
    let mut superblock = [0; SuperBlock::size()];
    let mut fields = &mut superblock[..];
    fields.put_u32_le(MAGIC_NUMBER);
    fields.put_u32_le(0); // Checksum feature is not advertised.
    fields.put_u32_le(0);
    fields.put_u8(12);
    fields.put_u8(0);
    fields.put_u16_le((ROOT_OFFSET / INODE_SLOT_SIZE) as u16);
    fields.put_u64_le(inodes as u64);
    fields.put_i64_le(0);
    fields.put_u32_le(0);
    fields.put_u32_le(blocks);
    fields.put_u32_le(0); // NIDs are relative to the start of the image.
    superblock
}
