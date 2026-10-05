//! Incremental EROFS image generation with `std`.
//!
//! [`Builder`] accepts byte paths, explicit metadata and arbitrary readers without
//! consulting the host filesystem. [`AsyncBuilder`] shares the same image
//! semantics using runtime-independent async backend traits. [`from_directory`]
//! is a Unix convenience layer. Images use 4 KiB blocks, compact or extended inodes
//! and flat data, with optional compression for regular files.

use alloc::{collections::BTreeMap, vec::Vec};
use std::io;
use typed_path::UnixPath;

use crate::{
    Error, Result, Xattrs,
    types::{DirentFileType, InodeExtended},
};
use encode::{BLOCK_SIZE, Content, Entry, INODE_SLOT_SIZE, MetadataBlock, Node, ROOT_OFFSET};

mod r#async;
#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
mod compress;
#[cfg(unix)]
mod directory;
mod encode;
mod sync;
pub use r#async::AsyncBuilder;
#[cfg(unix)]
pub use directory::from_directory;
pub use sync::Builder;

/// Regular-file compression. Directories, symlinks and special entries stay flat.
///
/// Compressed files use Full indexes and at most 64 KiB of input per extent, stored
/// in one 4 KiB physical block. Files smaller than 8 KiB retain flat/inline storage.
/// Unprofitable regions and short tails stay PLAIN, with Full-index metadata overhead.
/// Working memory is bounded by the input window and codec, not by file size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Compression {
    /// Flat data with inline tails where possible.
    #[default]
    None,
    /// LZ4, requiring the `lz4` feature.
    Lz4,
    /// MicroLZMA preset 1 with a 64 KiB dictionary, requiring the `lzma` feature.
    Lzma,
    /// Raw DEFLATE level 6 with a 32 KiB window, requiring the `deflate` feature.
    Deflate,
    /// Zstandard (`ruzstd`'s Fastest level) with a 128 KiB window, requiring `zstd`.
    Zstd,
}

impl Compression {
    fn enabled(self) -> bool {
        match self {
            Self::None => true,
            Self::Lz4 => cfg!(feature = "lz4"),
            Self::Lzma => cfg!(feature = "lzma"),
            Self::Deflate => cfg!(feature = "deflate"),
            Self::Zstd => cfg!(feature = "zstd"),
        }
    }

    fn algorithm(self) -> Option<u8> {
        match self {
            Self::None => None,
            Self::Lz4 => Some(0),
            Self::Lzma => Some(1),
            Self::Deflate => Some(2),
            Self::Zstd => Some(3),
        }
    }

    fn config_size(self) -> usize {
        match self {
            Self::None | Self::Lz4 => 0,
            Self::Lzma => 16,
            Self::Deflate | Self::Zstd => 8,
        }
    }
}

/// Inode header format. Metadata is never truncated or timestamps normalized.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InodeFormat {
    /// Use 32-byte Compact headers when all fields fit, otherwise 64-byte Extended.
    /// Streamed Compact entries cannot grow beyond 65,535 links; select Extended
    /// up front if more links may be added. Directory import knows counts in advance.
    #[default]
    Auto,
    /// Require Compact headers. Unrepresentable metadata is rejected during append;
    /// final directory sizes and link counts (including the root) are checked at finish.
    Compact,
    /// Always use Extended headers, allowing 32-bit ownership and link counts,
    /// 64-bit sizes and independent modification times.
    Extended,
}

/// Image-wide settings shared by both builders and directory import.
///
/// Defaults to a zero UUID and Unix epoch time, without reading the clock or
/// generating random values. These settings do not change inode modification times.
/// Configure with chained methods, then call [`Self::build`] or [`Self::build_async`].
/// Validation happens before output I/O, not in the setters.
///
/// ```
/// use std::io::Cursor;
/// use erofs_rs::build::Options;
///
/// let builder = Options::default()
///     .uuid([0x11; 16])
///     .build_time((1_700_000_000, 123_456_789))
///     .build(Cursor::new(Vec::new()))?;
/// let image = builder.finish()?.into_inner();
/// # assert!(!image.is_empty());
/// # Ok::<(), erofs_rs::Error>(())
/// ```
#[must_use]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    uuid: [u8; 16],
    build_time: (i64, u32),
    compression: Compression,
    inode_format: InodeFormat,
}

impl Options {
    /// Sets UUID bytes in canonical (network) order, stored verbatim.
    pub fn uuid(mut self, uuid: [u8; 16]) -> Self {
        self.uuid = uuid;
        self
    }

    /// Sets filesystem creation time as signed Unix seconds and nanoseconds in
    /// `0..1_000_000_000`, like [`Metadata::modified`]. Inode times are unchanged.
    pub fn build_time(mut self, build_time: (i64, u32)) -> Self {
        self.build_time = build_time;
        self
    }

    /// Selects regular-file compression. Defaults to [`Compression::None`].
    /// A disabled encoder is rejected before accessing input or output.
    pub fn compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Selects inode headers for all entries, including directories and the root.
    /// Defaults to [`InodeFormat::Auto`]; this does not change entry metadata.
    pub fn inode_format(mut self, inode_format: InodeFormat) -> Self {
        self.inode_format = inode_format;
        self
    }

    fn root_offset(self) -> usize {
        (ROOT_OFFSET + self.compression.config_size()).next_multiple_of(INODE_SLOT_SIZE)
    }

    fn validate(self) -> Result<()> {
        if !self.compression.enabled() {
            return Err(Error::NotSupported(format!(
                "compression encoder {:?} is disabled",
                self.compression
            )));
        }
        if self.build_time.1 >= 1_000_000_000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid filesystem build time nanoseconds",
            )
            .into());
        }
        Ok(())
    }
}

/// Host-independent entry metadata.
///
/// Entry type, size and link count are supplied by the append operation or derived
/// by the builder. [`Options::inode_format`] controls header selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// Permission and special bits (`0o0000..=0o7777`), without file type bits.
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    /// Signed Unix seconds and nanoseconds in `0..1_000_000_000`.
    pub modified: (i64, u32),
    /// Inline attributes with full byte names and uninterpreted values.
    ///
    /// Supports `user.*`, `trusted.*`, `security.*`, `system.posix_acl_access`
    /// and `system.posix_acl_default`. Names must be nonempty, at most 255 bytes
    /// and contain no NUL; values are at most 65,535 bytes. The encoded body is
    /// limited to 262,148 bytes per inode. Unsupported namespaces are rejected.
    pub xattrs: Xattrs,
}

impl Default for Metadata {
    /// Mode `0o644`, UID/GID zero, Unix epoch modification time and no attributes.
    fn default() -> Self {
        Self {
            mode: 0o644,
            uid: 0,
            gid: 0,
            modified: (0, 0),
            xattrs: Xattrs::new(),
        }
    }
}

impl Metadata {
    fn validate(&self) -> Result<()> {
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

/// A FIFO or device node, without file contents. Socket entries are not supported.
/// Device numbers are portable major/minor values, not a host's encoded `dev_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialFile {
    Fifo,
    /// A character device with a 12-bit major and 20-bit minor number.
    CharacterDevice {
        major: u32,
        minor: u32,
    },
    /// A block device with a 12-bit major and 20-bit minor number.
    BlockDevice {
        major: u32,
        minor: u32,
    },
}

impl SpecialFile {
    fn validate(self) -> Result<()> {
        if let Self::CharacterDevice { major, minor } | Self::BlockDevice { major, minor } = self
            && (major > 0xfff || minor > 0xfffff)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "device number exceeds EROFS major/minor limits",
            )
            .into());
        }
        Ok(())
    }
}

// Shared namespace, layout and commit state; neither builder duplicates these rules.
struct State {
    options: Options,
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
    fn new(options: Options) -> Result<Self> {
        options.validate()?;
        Ok(Self {
            options,
            nodes: vec![Node::new(
                &Metadata {
                    mode: 0o755,
                    ..Metadata::default()
                },
                Content::Directory {
                    parent: 0,
                    entries: Vec::new(),
                },
                0,
            )?],
            paths: BTreeMap::from([(Vec::new(), 0)]),
            next_block: 1,
            pending_metadata: None,
            root_explicit: false,
            poisoned: false,
        })
    }

    fn append_dir(&mut self, path: &UnixPath, metadata: &Metadata) -> Result<()> {
        self.check_ready()?;
        let mut node = Node::new(
            metadata,
            Content::Directory {
                parent: 0,
                entries: Vec::new(),
            },
            0,
        )?;
        node.select_format(&self.options, node.nlink)?;
        let path = entry_path(path, true)?;
        if path.is_empty() && !self.root_explicit {
            let inode_size = InodeExtended::size() + node.xattr_size;
            if inode_size > BLOCK_SIZE - self.options.root_offset() {
                // Reserve a large root when supplied, so directory import keeps
                // its NID small instead of requiring 48-bit addressing after payloads.
                node.nid = u64::from(self.next_block) * (BLOCK_SIZE / INODE_SLOT_SIZE) as u64;
                self.next_block = self
                    .next_block
                    .checked_add(inode_size.div_ceil(BLOCK_SIZE) as u32)
                    .ok_or(Error::Overflow("image block count"))?;
            }
            self.nodes[0] = node;
            self.root_explicit = true;
            return Ok(());
        }
        self.check_path(&path, true)?;
        if self.nodes.len() >= u32::MAX as usize {
            return Err(Error::Overflow("inode count"));
        }
        self.insert(path, node);
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
        let nlink = node
            .nlink
            .checked_add(1)
            .ok_or(Error::Overflow("inode link count"))?;
        // ponytail: streamed headers cannot grow; choose Extended up front for more links.
        if node.compact && nlink > u32::from(u16::MAX) {
            return Err(Error::Overflow("compact inode link count"));
        }
        node.nlink = nlink;
        self.paths.insert(path, node_index);
        Ok(())
    }

    fn prepare_payload(
        &mut self,
        path: &UnixPath,
        metadata: &Metadata,
        content: Content,
        size: u64,
        nlink: u32,
    ) -> Result<PendingEntry> {
        self.check_ready()?;
        let mut node = Node::new(metadata, content, size)?;
        node.select_format(&self.options, nlink)?;
        let path = entry_path(path, false)?;
        self.check_path(&path, false)?;
        if self.nodes.len() >= u32::MAX as usize {
            return Err(Error::Overflow("inode count"));
        }
        node.compression = if matches!(node.content, Content::File) && size >= 2 * BLOCK_SIZE as u64
        {
            self.options.compression
        } else {
            Compression::None
        };
        let inode_size = node.inode_size() + node.xattr_size;
        let tail = (size % BLOCK_SIZE as u64) as usize;
        let available = (BLOCK_SIZE - inode_size % BLOCK_SIZE) % BLOCK_SIZE;
        node.inline_size = if node.compression == Compression::None && tail <= available {
            tail
        } else {
            0
        };
        let record_size = node.metadata_end().next_multiple_of(INODE_SLOT_SIZE as u64);
        let mut data_block = u64::from(self.next_block);
        let inode_offset = if let Some(page) = &self.pending_metadata
            && page.used as u64 + record_size <= BLOCK_SIZE as u64
        {
            u64::from(page.block) * BLOCK_SIZE as u64 + page.used as u64
        } else {
            // Reserve the contiguous inode/xattr blocks before external data.
            data_block += record_size.div_ceil(BLOCK_SIZE as u64);
            u64::from(self.next_block) * BLOCK_SIZE as u64
        };
        let external_size = node.external_size();
        // Validate even the incompressible case before consuming input. Compressed
        // data starts after reserved indexes; actual blocks are counted as written.
        let plain_end = u32::try_from(data_block + external_size.div_ceil(BLOCK_SIZE as u64))
            .map_err(|_| Error::Overflow("file block count"))?;
        let next_block = if node.compression != Compression::None {
            data_block as u32
        } else {
            plain_end
        };
        node.data_block = if node.compression != Compression::None || external_size == 0 {
            0
        } else {
            data_block as u32
        };
        node.nid = inode_offset / INODE_SLOT_SIZE as u64;
        let entry = PendingEntry {
            path,
            next_block,
            node,
        };
        // Set before I/O, including the first await. Errors or cancellation leave
        // this set; only a complete payload and its padding can commit the entry.
        self.poisoned = true;
        Ok(entry)
    }

    fn commit_payload(&mut self, mut entry: PendingEntry) {
        entry.node.xattrs = Vec::new();
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
        // Use block-zero slack unless a large root has already reserved its space.
        // Other directory records are allocated after the payloads.
        let mut metadata_block = 0;
        let mut used = self.options.root_offset();
        for (node_index, node) in self.nodes.iter_mut().enumerate() {
            if !matches!(node.content, Content::Directory { .. }) {
                continue;
            }
            node.select_format(&self.options, node.nlink)?;
            let reserved_root = node_index == 0 && node.nid != 0;
            // ponytail: reserving early keeps the root NID small but can waste
            // a 32-byte prefix. Reclaim it only if later relocation is worthwhile.
            let prefix = if reserved_root {
                InodeExtended::size() - node.inode_size()
            } else {
                0
            };
            let inode_size = node.inode_size() + node.xattr_size;
            let available = if node_index == 0 && !reserved_root {
                BLOCK_SIZE - self.options.root_offset() - inode_size
            } else {
                (BLOCK_SIZE - (prefix + inode_size) % BLOCK_SIZE) % BLOCK_SIZE
            };
            let tail = (node.size % BLOCK_SIZE as u64) as usize;
            node.inline_size = if tail <= available { tail } else { 0 };
            let record_size = (inode_size + node.inline_size).next_multiple_of(INODE_SLOT_SIZE);
            if reserved_root {
                metadata_block = node.metadata_block();
                used = prefix;
            } else if used + record_size > BLOCK_SIZE {
                metadata_block = self.next_block;
                self.next_block = self
                    .next_block
                    .checked_add(record_size.div_ceil(BLOCK_SIZE) as u32)
                    .ok_or(Error::Overflow("image block count"))?;
                used = 0;
            }
            let inode_offset = u64::from(metadata_block) * BLOCK_SIZE as u64 + used as u64;
            node.nid = inode_offset / INODE_SLOT_SIZE as u64;
            metadata_block = node.last_metadata_block();
            used = ((inode_offset + record_size as u64 - 1) % BLOCK_SIZE as u64 + 1) as usize;
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
