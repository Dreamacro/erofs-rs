//! Incremental EROFS image generation with `std`.
//!
//! [`Builder`] accepts byte paths, explicit metadata and arbitrary readers without
//! consulting the host filesystem. [`AsyncBuilder`] shares the same image
//! semantics using runtime-independent async backend traits. [`from_directory`]
//! is a Unix convenience layer. Images use 4 KiB blocks, extended inodes and flat data.

use alloc::{collections::BTreeMap, vec::Vec};
use std::io;
use typed_path::UnixPath;

use crate::{
    Error, Result,
    types::{DirentFileType, InodeExtended},
};
use encode::{BLOCK_SIZE, Content, Entry, INODE_SLOT_SIZE, MetadataBlock, Node, ROOT_OFFSET};

mod r#async;
#[cfg(unix)]
mod directory;
mod encode;
mod sync;
pub use r#async::AsyncBuilder;
#[cfg(unix)]
pub use directory::from_directory;
pub use sync::Builder;

/// Host-independent entry metadata. Entry type, size and link count are supplied
/// by the append operation or derived by the builder, not encoded into this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    /// Permission and special bits (`0o0000..=0o7777`), without file type bits.
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    /// Signed Unix seconds and nanoseconds in `0..1_000_000_000`.
    pub modified: (i64, u32),
}

impl Default for Metadata {
    /// Mode `0o644`, UID/GID zero, modification time at the Unix epoch.
    fn default() -> Self {
        Self {
            mode: 0o644,
            uid: 0,
            gid: 0,
            modified: (0, 0),
        }
    }
}

impl Metadata {
    fn validate(self) -> Result<()> {
        if self.mode & !0o7777 != 0 || self.modified.1 >= 1_000_000_000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid entry permissions or timestamp",
            )
            .into());
        }
        Ok(())
    }
}

// Shared namespace, layout and commit state; neither builder duplicates these rules.
struct State {
    nodes: Vec<Node>,
    paths: BTreeMap<Vec<u8>, usize>,
    next_block: u32,
    pending_metadata: Option<MetadataBlock>,
    root_explicit: bool,
    poisoned: bool,
}

struct PendingEntry {
    path: Vec<u8>,
    node: Node,
    next_block: u32,
}

impl State {
    fn new() -> Self {
        Self {
            nodes: vec![Node {
                metadata: Metadata {
                    mode: 0o755,
                    ..Metadata::default()
                },
                content: Content::Directory {
                    parent: 0,
                    entries: Vec::new(),
                },
                size: 0,
                nlink: 2,
                data_block: 0,
                nid: (ROOT_OFFSET / INODE_SLOT_SIZE) as u64,
                inline_size: 0,
            }],
            paths: BTreeMap::from([(Vec::new(), 0)]),
            next_block: 1,
            pending_metadata: None,
            root_explicit: false,
            poisoned: false,
        }
    }

    fn append_dir(&mut self, path: &UnixPath, metadata: Metadata) -> Result<()> {
        self.check_ready()?;
        metadata.validate()?;
        let path = entry_path(path, true)?;
        if path.is_empty() && !self.root_explicit {
            self.nodes[0].metadata = metadata;
            self.root_explicit = true;
            return Ok(());
        }
        self.check_path(&path, true)?;
        if self.nodes.len() >= u32::MAX as usize {
            return Err(Error::Overflow("inode count"));
        }
        self.insert(
            path,
            Node {
                metadata,
                content: Content::Directory {
                    parent: 0,
                    entries: Vec::new(),
                },
                size: 0,
                nlink: 2,
                data_block: 0,
                nid: 0, // Directories are sized and placed by layout().
                inline_size: 0,
            },
        );
        Ok(())
    }

    fn append_hard_link(&mut self, path: &UnixPath, target: &UnixPath) -> Result<()> {
        self.check_ready()?;
        let path = entry_path(path, false)?;
        self.check_path(&path, false)?;
        let target = entry_path(target, false)?;
        let node_index = *self.paths.get(&target).ok_or_else(|| {
            Error::PathNotFound(UnixPath::new(&target).to_string_lossy().into_owned())
        })?;
        let node = &mut self.nodes[node_index];
        if matches!(node.content, Content::Directory { .. }) {
            return Err(Error::NotSupported("directory hard links".into()));
        }
        node.nlink = node
            .nlink
            .checked_add(1)
            .ok_or(Error::Overflow("inode link count"))?;
        self.paths.insert(path, node_index);
        Ok(())
    }

    fn prepare_payload(
        &mut self,
        path: &UnixPath,
        metadata: Metadata,
        content: Content,
        size: u64,
    ) -> Result<PendingEntry> {
        self.check_ready()?;
        metadata.validate()?;
        let path = entry_path(path, false)?;
        self.check_path(&path, false)?;
        if self.nodes.len() >= u32::MAX as usize {
            return Err(Error::Overflow("inode count"));
        }
        let tail = (size % BLOCK_SIZE as u64) as usize;
        let inline_size = if tail <= BLOCK_SIZE - InodeExtended::size() {
            tail
        } else {
            0
        };
        let record_size = (InodeExtended::size() + inline_size).next_multiple_of(INODE_SLOT_SIZE);
        let mut data_block = u64::from(self.next_block);
        let inode_offset = if let Some(page) = &self.pending_metadata
            && page.used + record_size <= BLOCK_SIZE
        {
            u64::from(page.block) * BLOCK_SIZE as u64 + page.used as u64
        } else {
            // Reserve the new metadata block before allocating external data.
            data_block += 1;
            u64::from(self.next_block) * BLOCK_SIZE as u64
        };
        let external_size = size - inline_size as u64;
        let next_block = u32::try_from(data_block + external_size.div_ceil(BLOCK_SIZE as u64))
            .map_err(|_| Error::Overflow("file block count"))?;
        let entry = PendingEntry {
            path,
            next_block,
            node: Node {
                metadata,
                content,
                size,
                nlink: 1,
                data_block: if external_size == 0 {
                    0
                } else {
                    data_block as u32
                },
                nid: inode_offset / INODE_SLOT_SIZE as u64,
                inline_size,
            },
        };
        // Set before I/O, including the first await. Errors or cancellation leave
        // this set; only a complete payload and its padding can commit the entry.
        self.poisoned = true;
        Ok(entry)
    }

    fn commit_payload(&mut self, entry: PendingEntry) {
        self.pending_metadata
            .as_mut()
            .unwrap()
            .write_inode(&entry.node, self.nodes.len());
        self.insert(entry.path, entry.node);
        self.next_block = entry.next_block;
        self.poisoned = false;
    }

    fn layout(&mut self) -> Result<u32> {
        self.check_ready()?;
        self.build_directory_entries()?;
        self.allocate_directories()?;
        Ok(self.next_block)
    }

    fn build_directory_entries(&mut self) -> Result<()> {
        for (path, &node_index) in &self.paths {
            if path.is_empty() {
                continue;
            }
            let (parent, name) = path
                .iter()
                .rposition(|&byte| byte == b'/')
                .map_or((&b""[..], path.as_slice()), |at| {
                    (&path[..at], &path[at + 1..])
                });
            let parent_index = *self.paths.get(parent).ok_or_else(|| {
                Error::PathNotFound(UnixPath::new(parent).to_string_lossy().into_owned())
            })?;
            let kind = self.nodes[node_index].content.file_type();
            if let Content::Directory { parent, .. } = &mut self.nodes[node_index].content {
                *parent = parent_index;
            }
            let node = &mut self.nodes[parent_index];
            let Content::Directory { entries, .. } = &mut node.content else {
                return Err(Error::NotADirectory(
                    UnixPath::new(parent).to_string_lossy().into_owned(),
                ));
            };
            entries.push(Entry {
                name: name.to_vec(),
                node_index,
                kind,
            });
            if kind == DirentFileType::Directory {
                node.nlink = node
                    .nlink
                    .checked_add(1)
                    .ok_or(Error::Overflow("directory link count"))?;
            }
        }
        for (node_index, node) in self.nodes.iter_mut().enumerate() {
            if let Content::Directory { parent, entries } = &mut node.content {
                entries.extend([
                    Entry {
                        name: b".".to_vec(),
                        node_index,
                        kind: DirentFileType::Directory,
                    },
                    Entry {
                        name: b"..".to_vec(),
                        node_index: *parent,
                        kind: DirentFileType::Directory,
                    },
                ]);
                entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
                node.size = encode::directory_size(entries)?;
            }
        }
        Ok(())
    }

    fn allocate_directories(&mut self) -> Result<()> {
        // The root stays after the superblock; other directory records may start
        // a fresh metadata block. File records have already been allocated.
        let mut metadata_block = 0;
        let mut used = ROOT_OFFSET;
        for (node_index, node) in self.nodes.iter_mut().enumerate() {
            if !matches!(node.content, Content::Directory { .. }) {
                continue;
            }
            let available = if node_index == 0 {
                BLOCK_SIZE - ROOT_OFFSET
            } else {
                BLOCK_SIZE
            };
            let tail = (node.size % BLOCK_SIZE as u64) as usize;
            node.inline_size = if InodeExtended::size() + tail <= available {
                tail
            } else {
                0
            };
            let record_size =
                (InodeExtended::size() + node.inline_size).next_multiple_of(INODE_SLOT_SIZE);
            if used + record_size > BLOCK_SIZE {
                metadata_block = self.next_block;
                self.next_block = self
                    .next_block
                    .checked_add(1)
                    .ok_or(Error::Overflow("image block count"))?;
                used = 0;
            }
            let inode_offset = u64::from(metadata_block) * BLOCK_SIZE as u64 + used as u64;
            node.nid = inode_offset / INODE_SLOT_SIZE as u64;
            used += record_size;
            let external_size = node.external_size();
            node.data_block = if external_size == 0 {
                0
            } else {
                self.next_block
            };
            self.next_block = u32::try_from(
                u64::from(self.next_block) + external_size.div_ceil(BLOCK_SIZE as u64),
            )
            .map_err(|_| Error::Overflow("directory block count"))?;
        }
        Ok(())
    }

    fn check_ready(&self) -> Result<()> {
        if self.poisoned {
            return Err(io::Error::other(
                "image builder is unusable after an I/O error or cancellation",
            )
            .into());
        }
        Ok(())
    }

    fn check_path(&self, path: &[u8], directory: bool) -> Result<()> {
        if self.paths.contains_key(path) {
            return Err(
                io::Error::new(io::ErrorKind::AlreadyExists, "duplicate image entry").into(),
            );
        }
        for (at, &byte) in path.iter().enumerate() {
            if byte == b'/'
                && let Some(&node_index) = self.paths.get(&path[..at])
                && !matches!(self.nodes[node_index].content, Content::Directory { .. })
            {
                return Err(Error::NotADirectory(
                    UnixPath::new(&path[..at]).to_string_lossy().into_owned(),
                ));
            }
        }
        if !directory {
            let mut prefix = path.to_vec();
            prefix.push(b'/');
            if self
                .paths
                .range(prefix.clone()..)
                .next()
                .is_some_and(|(key, _)| key.starts_with(&prefix))
            {
                return Err(Error::NotADirectory(
                    UnixPath::new(path).to_string_lossy().into_owned(),
                ));
            }
        }
        Ok(())
    }

    fn insert(&mut self, path: Vec<u8>, node: Node) {
        self.paths.insert(path, self.nodes.len());
        self.nodes.push(node);
    }
}

fn entry_path(path: &UnixPath, directory: bool) -> Result<Vec<u8>> {
    let mut bytes = path.as_bytes();
    if directory && (bytes.is_empty() || bytes == b"/") {
        return Ok(Vec::new());
    }
    if directory {
        bytes = bytes.strip_suffix(b"/").unwrap_or(bytes);
    }
    bytes = bytes.strip_prefix(b"/").unwrap_or(bytes);
    if bytes.contains(&0)
        || bytes
            .split(|&byte| byte == b'/')
            .any(|part| part.is_empty() || part == b"." || part == b".." || part.len() > 255)
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid image entry path").into());
    }
    Ok(bytes.to_vec())
}

fn symlink_target(target: &UnixPath) -> Result<&[u8]> {
    let target = target.as_bytes();
    if target.is_empty() || target.contains(&0) {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "invalid symbolic link target").into(),
        );
    }
    Ok(target)
}

fn check_output_length(actual: u64, expected: u64) -> Result<()> {
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected image output length; output must be empty and must not use append mode",
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
