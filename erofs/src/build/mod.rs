//! Incremental EROFS image generation with `std`.
//!
//! [`Builder`] accepts byte paths, explicit metadata and arbitrary readers without
//! consulting the host filesystem. [`AsyncBuilder`] shares the same image
//! semantics using runtime-independent async backend traits. [`from_directory`]
//! is a Unix convenience layer. Images use 4 KiB blocks, compact or extended inodes
//! and flat data, with optional compression for regular files.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    vec::Vec,
};
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
/// Compressed files use Compact indexes with byte-granular logical extents and
/// a sliding 64 KiB input window, independently of file size. Prefix fitting is
/// bounded; it does not guarantee the optimal compression ratio. [`Options::pcluster_size`] bounds
/// physical clusters (4 KiB by default). Files up to 4 KiB retain flat/inline
/// storage. Without opt-in tail packing, files up to 64 KiB also fall back to flat
/// storage when compressed data plus indexes would not save space. Larger
/// unprofitable regions stay PLAIN, with compression-index overhead.
/// Inline compressed tails and fragment packing are separately opt-in.
/// Working memory is bounded by the input window and codec, not by file size.
/// Encoder allocations are reused across files until the builder is dropped.
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
    /// Zstandard level 1 via zrip with a 128 KiB window limit and content checksum.
    /// Requires the `zstd` feature, which also enables `std`.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    uuid: [u8; 16],
    build_time: (i64, u32),
    compression: Compression,
    inode_format: InodeFormat,
    pcluster_size: u32,
    inline_compressed_tails: bool,
    fragments: bool,
    shared_xattrs: bool,
    xattr_prefixes: bool,
    skip_unsupported_xattrs: bool,
    workers: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            uuid: [0; 16],
            build_time: (0, 0),
            compression: Compression::None,
            inode_format: InodeFormat::Auto,
            pcluster_size: BLOCK_SIZE as u32,
            inline_compressed_tails: false,
            fragments: false,
            shared_xattrs: false,
            xattr_prefixes: false,
            skip_unsupported_xattrs: true,
            workers: 1,
        }
    }
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

    /// Maximum physical compression cluster, a multiple of 4096 up to 1 MiB.
    /// Defaults to 4096. Input remains bounded by the 64 KiB compression window.
    pub fn pcluster_size(mut self, bytes: u32) -> Self {
        self.pcluster_size = bytes;
        self
    }

    /// Store a final compressed extent (or short PLAIN tail) in index-page slack.
    /// Defaults to false; unsuccessful candidates keep external storage.
    pub fn inline_compressed_tails(mut self, enabled: bool) -> Self {
        self.inline_compressed_tails = enabled;
        self
    }

    /// Pack remaining tails of at most 4096 bytes into a hidden chunk-indexed file.
    /// Inline tails take precedence. Defaults to false. Tails beyond the 32-bit
    /// packed-offset limit retain external storage.
    pub fn fragments(mut self, enabled: bool) -> Self {
        self.fragments = enabled;
        self
    }

    /// Reuse identical xattr records with shared references. Defaults to false.
    /// An exact-match cache retains at most 256 KiB of record bytes / 4096 entries;
    /// misses stay inline.
    pub fn shared_xattrs(mut self, enabled: bool) -> Self {
        self.shared_xattrs = enabled;
        self
    }

    /// Deduplicate long xattr name prefixes, up to the format's 128-prefix limit.
    /// Defaults to false. With fragments, the table shares the packed inode;
    /// otherwise its primary-image address must start below 16 GiB.
    pub fn xattr_prefixes(mut self, enabled: bool) -> Self {
        self.xattr_prefixes = enabled;
        self
    }

    /// Skip host xattrs outside EROFS namespaces during directory import.
    /// Defaults to true; malformed or oversized attributes still fail.
    pub fn skip_unsupported_xattrs(mut self, enabled: bool) -> Self {
        self.skip_unsupported_xattrs = enabled;
        self
    }

    /// Threads compressing files during directory import. Defaults to 1.
    /// Images are identical for any count. Large files use fixed 16 MiB segments;
    /// up to 8 jobs and 32 MiB of input/output buffers per worker are prefetched,
    /// plus the segment being committed, indexes and encoder state.
    /// Inline compressed tails and fragments retain the serial streaming path.
    pub fn workers(mut self, workers: usize) -> Self {
        self.workers = workers;
        self
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    fn buffers_file(self, size: u64) -> bool {
        self.prefits_file(size) && size <= compress::INPUT_SIZE as u64
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    fn prefits_file(self, size: u64) -> bool {
        self.compression != Compression::None
            && size > BLOCK_SIZE as u64
            && !self.inline_compressed_tails
            && !self.fragments
    }

    fn config_size(self) -> usize {
        if self.compression == Compression::Lz4 && self.pcluster_size > BLOCK_SIZE as u32 {
            16
        } else {
            self.compression.config_size()
        }
    }

    fn root_offset(self) -> usize {
        (ROOT_OFFSET + self.config_size()).next_multiple_of(INODE_SLOT_SIZE)
    }

    fn validate(self) -> Result<()> {
        if !(4096..=1024 * 1024).contains(&self.pcluster_size)
            || !self.pcluster_size.is_multiple_of(4096)
            || (self.compression == Compression::None
                && (self.pcluster_size != 4096 || self.inline_compressed_tails || self.fragments))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid compression cluster size or tail options without compression",
            )
            .into());
        }
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
        if self.workers == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid worker count").into());
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
    /// Attributes with full byte names and uninterpreted values. Inline by default;
    /// [`Options::shared_xattrs`] and [`Options::xattr_prefixes`] enable deduplication.
    ///
    /// Supports `user.*`, `trusted.*`, `security.*`, `system.posix_acl_access`
    /// and `system.posix_acl_default`. Names must be nonempty, at most 255 bytes
    /// and contain no NUL; values are at most 65,535 bytes. The unoptimized inline
    /// body is limited to 262,148 bytes per inode. Unsupported namespaces are rejected.
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

/// A special inode without contents. A socket entry records metadata only;
/// it does not create or connect to a host socket.
/// Device numbers are portable major/minor values, not a host's encoded `dev_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialFile {
    Fifo,
    Socket,
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
    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    encoder: compress::Encoder,
    nodes: Vec<Node>,
    paths: BTreeMap<Vec<u8>, usize>,
    next_block: u32,
    pending_metadata: Option<MetadataBlock>,
    // Only suffix addresses are retained, not flushed page contents.
    metadata_free: BTreeSet<(u64, u32)>,
    fragments: Fragments,
    attributes: encode::Attributes,
    root_reservation: Option<core::ops::Range<u32>>,
    root_explicit: bool,
    poisoned: bool,
}

struct PendingEntry {
    path: Vec<u8>,
    node: Node,
    next_block: u32,
}

#[derive(Default)]
struct Fragments {
    data: Vec<u8>,
    block: u32,
    used: usize,
    size: u64,
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
impl Fragments {
    fn append(&mut self, entry: &mut PendingEntry, data: &[u8]) -> usize {
        entry.node.fragment_offset.get_or_insert(self.size);
        if self.used == 0 {
            self.block = entry.next_block;
            entry.next_block += 1; // At most one block per tail, checked before I/O.
            entry.node.fragment_block = Some(self.block);
            self.data.clear();
            self.data.resize(BLOCK_SIZE, 0);
        }
        let len = data.len().min(BLOCK_SIZE - self.used);
        self.data[self.used..self.used + len].copy_from_slice(&data[..len]);
        self.used += len;
        self.size += len as u64;
        len
    }
}

impl State {
    fn new(options: Options) -> Result<Self> {
        options.validate()?;
        Ok(Self {
            #[cfg(any(
                feature = "lz4",
                feature = "lzma",
                feature = "deflate",
                feature = "zstd"
            ))]
            encoder: compress::Encoder::new(&options),
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
            metadata_free: BTreeSet::new(),
            fragments: Fragments::default(),
            attributes: encode::Attributes::default(),
            root_reservation: None,
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
            // Reserve large roots before payloads can push their NID out of the
            // legacy range. Keep the reservation separate from the final record:
            // xattr deduplication at finish may shrink it.
            let size = InodeExtended::size() + node.xattr_size;
            if size > BLOCK_SIZE - self.options.root_offset() {
                let start = self.next_block;
                let end = start
                    .checked_add(size.div_ceil(BLOCK_SIZE) as u32)
                    .ok_or(Error::Overflow("image block count"))?;
                node.nid = u64::from(start) * (BLOCK_SIZE / INODE_SLOT_SIZE) as u64;
                self.root_reservation = Some(start..end);
                self.next_block = end;
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
        node.compression = if matches!(node.content, Content::File) && size > BLOCK_SIZE as u64 {
            self.options.compression
        } else {
            Compression::None
        };
        node.big_pcluster = self.options.pcluster_size > BLOCK_SIZE as u32;
        let prefix_count = self.attributes.prefixes.len();
        self.attributes.rewrite(&mut node, &self.options);
        let next_block = self.allocate_payload(&mut node).inspect_err(|_| {
            // Recoverable validation must not leave unused global prefixes behind.
            self.attributes.prefixes.truncate(prefix_count);
        })?;
        // Set before I/O, including the first await. Errors or cancellation leave
        // this set; only a complete payload and its padding can commit the entry.
        self.poisoned = true;
        Ok(PendingEntry {
            path,
            next_block,
            node,
        })
    }

    // Plan without consuming a free slot; only commit_payload changes the free list.
    fn allocate_payload(&self, node: &mut Node) -> Result<u32> {
        node.inline_size = if node.compression == Compression::None {
            node.flat_inline_size()
        } else {
            0
        };
        let record_size = node.record_size();
        let mut data_block = u64::from(self.next_block);
        let slot = self.metadata_free.range((record_size, 0)..).next().copied();
        let inode_offset = if let Some((free, block)) = slot {
            (u64::from(block) + 1) * BLOCK_SIZE as u64 - free
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
        Ok(next_block)
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    fn buffered_file_size(&self, node: &Node) -> Option<usize> {
        (node.compression != Compression::None && self.options.buffers_file(node.size))
            .then_some(node.size as usize)
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    fn buffer_file(
        &mut self,
        entry: &mut PendingEntry,
        input: &[u8],
    ) -> Result<Option<compress::BufferedFile>> {
        let fitted = compress::FittedFile::fit(&self.options, &mut self.encoder, input)?;
        self.place_fitted(entry, fitted)
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    fn place_fitted(
        &self,
        entry: &mut PendingEntry,
        fitted: compress::FittedFile,
    ) -> Result<Option<compress::BufferedFile>> {
        let node = &entry.node;
        let inline = node.flat_inline_size();
        let flat_cost = (node.size - inline as u64).next_multiple_of(BLOCK_SIZE as u64)
            + (node.inode_size() + node.xattr_size + inline).next_multiple_of(INODE_SLOT_SIZE)
                as u64;
        let compressed_cost = fitted.data.len() as u64 + node.record_size();
        if compressed_cost < flat_cost {
            return Ok(Some(fitted.into_buffered(node, entry.next_block)));
        }
        // Neither layout owns a slot until commit, so fallback needs no rollback.
        entry.node.compression = Compression::None;
        entry.next_block = self.allocate_payload(&mut entry.node)?;
        Ok(None)
    }

    // Keep one page pending; the driver writes the displaced page before continuing.
    fn start_metadata_block(&mut self, offset: u64) -> Option<MetadataBlock> {
        let block = (offset / BLOCK_SIZE as u64) as u32;
        if self
            .pending_metadata
            .as_ref()
            .is_some_and(|page| page.block == block)
        {
            None
        } else {
            self.pending_metadata
                .replace(MetadataBlock::from_offset(offset))
        }
    }

    fn prepare_metadata(&mut self, node: &Node) -> impl Iterator<Item = MetadataBlock> {
        node.header_blocks().filter_map(move |block| {
            let offset = (u64::from(block) * BLOCK_SIZE as u64).max(node.inode_offset());
            let previous = self.start_metadata_block(offset);
            self.pending_metadata
                .as_mut()
                .unwrap()
                .write_inode(node, self.nodes.len());
            previous
        })
    }

    // Consume the iterator to write displaced pages. The final page stays pending
    // for more indexes or inline data.
    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    fn write_indexes(
        &mut self,
        node: &Node,
        offset: u64,
        bytes: &[u8],
    ) -> impl Iterator<Item = MetadataBlock> {
        let start = offset / BLOCK_SIZE as u64;
        let end = if bytes.is_empty() {
            start
        } else {
            (offset + bytes.len() as u64).div_ceil(BLOCK_SIZE as u64)
        };
        (start..end).filter_map(move |block| {
            let start = (block * BLOCK_SIZE as u64).max(node.inode_offset());
            let previous = self.start_metadata_block(start);
            let page = self.pending_metadata.as_mut().unwrap();
            page.write_inode(node, self.nodes.len());
            page.write_at(offset, bytes);
            previous
        })
    }

    fn commit_payload(&mut self, mut entry: PendingEntry) {
        if entry.node.metadata_block() < self.next_block {
            let slot = (
                BLOCK_SIZE as u64 - entry.node.inode_offset() % BLOCK_SIZE as u64,
                entry.node.metadata_block(),
            );
            let removed = self.metadata_free.remove(&slot);
            debug_assert!(
                removed,
                "committed metadata must occupy its planned free slot"
            );
        }
        if let Some(gap) = entry.node.metadata_gap() {
            self.metadata_free.insert(gap);
        }
        self.attributes.remember(&entry.node, &self.options);
        entry.node.xattrs = Vec::new();
        self.insert(entry.path, entry.node);
        self.next_block = entry.next_block;
        self.poisoned = false;
    }

    fn layout(&mut self) -> Result<u32> {
        self.check_ready()?;
        self.build_directory_entries()?;
        self.allocate_directories()?;
        self.attributes
            .allocate_prefixes(&mut self.next_block, self.fragments.size)?;
        if self.fragments.size != 0 {
            if self.nodes.len() >= u32::MAX as usize {
                return Err(Error::Overflow("inode count"));
            }
            let mut packed = Node::new(
                &Metadata {
                    modified: self.options.build_time,
                    ..Metadata::default()
                },
                Content::Packed,
                if self.attributes.table.is_empty() {
                    self.fragments.size
                } else {
                    self.fragments.size.next_multiple_of(BLOCK_SIZE as u64)
                        + self.attributes.table.len() as u64
                },
            )?;
            packed.select_format(&self.options, 1)?;
            packed.nid = u64::from(self.next_block) * (BLOCK_SIZE / INODE_SLOT_SIZE) as u64;
            self.next_block = u32::try_from(packed.metadata_end().div_ceil(BLOCK_SIZE as u64))
                .map_err(|_| Error::Overflow("packed inode metadata"))?;
            self.nodes.push(packed);
        }
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
        // Keep the root in block zero when possible; all other directories
        // share the same best-fit suffix allocator as streamed file records.
        for (node_index, node) in self.nodes.iter_mut().enumerate() {
            if !matches!(node.content, Content::Directory { .. }) {
                continue;
            }
            node.select_format(&self.options, node.nlink)?;
            self.attributes.rewrite(node, &self.options);
            let reservation = self.root_reservation.as_ref().filter(|_| node_index == 0);
            // ponytail: reserving early keeps the root NID small but can waste
            // a 32-byte prefix. Reclaim it only if later relocation is worthwhile.
            let prefix = if reservation.is_some() {
                InodeExtended::size() - node.inode_size()
            } else {
                0
            };
            let inode_size = node.inode_size() + node.xattr_size;
            let available = if node_index == 0 && reservation.is_none() {
                (BLOCK_SIZE - self.options.root_offset()).saturating_sub(inode_size)
            } else {
                (BLOCK_SIZE - (prefix + inode_size) % BLOCK_SIZE) % BLOCK_SIZE
            };
            let tail = (node.size % BLOCK_SIZE as u64) as usize;
            node.inline_size = if tail <= available { tail } else { 0 };
            let record_size = (inode_size + node.inline_size).next_multiple_of(INODE_SLOT_SIZE);
            let slot = if node_index == 0 {
                None
            } else {
                self.metadata_free
                    .range((record_size as u64, 0)..)
                    .next()
                    .copied()
            };
            let inode_offset = if let Some(reservation) = reservation {
                u64::from(reservation.start) * BLOCK_SIZE as u64 + prefix as u64
            } else if node_index == 0 && self.options.root_offset() + record_size <= BLOCK_SIZE {
                self.options.root_offset() as u64
            } else if let Some((free, block)) = slot {
                self.metadata_free.remove(&(free, block));
                (u64::from(block) + 1) * BLOCK_SIZE as u64 - free
            } else {
                let start = self.next_block;
                self.next_block = self
                    .next_block
                    .checked_add(record_size.div_ceil(BLOCK_SIZE) as u32)
                    .ok_or(Error::Overflow("image block count"))?;
                u64::from(start) * BLOCK_SIZE as u64
            };
            node.nid = inode_offset / INODE_SLOT_SIZE as u64;
            if let Some(reservation) = reservation
                && self.next_block == reservation.end
            {
                // Reclaim only an unused suffix at EOF. Earlier reservations may
                // have payloads after them, whose addresses must never move.
                let end = node.last_metadata_block() + 1;
                debug_assert!(end <= reservation.end);
                self.next_block = end;
            }
            self.attributes.remember(node, &self.options);
            if let Some(gap) = node.metadata_gap() {
                self.metadata_free.insert(gap);
            }
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
