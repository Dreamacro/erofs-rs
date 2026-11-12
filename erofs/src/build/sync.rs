use std::{
    borrow::Borrow,
    io::{self, Read, Seek, SeekFrom, Write},
};
use typed_path::UnixPath;

use super::{
    Metadata, Options, SpecialFile, State, check_output_length,
    encode::{self, BLOCK_SIZE, Content, Node, ZERO_BLOCK},
    symlink_target,
};
use crate::{Result, types::SUPER_BLOCK_OFFSET};

/// Streams an image to an empty `Write + Seek` output. Writes must honor seeks;
/// do not open the output file in append mode.
///
/// Inputs are consumed immediately. Full data blocks are streamed and inline tails
/// are packed into one pending metadata block, never retained per file. Directories
/// may be added before or after their children, but every parent must exist as a
/// directory by [`finish`](Self::finish). No implicit parent
/// directories are created. The root exists initially with mode `0o755`, UID/GID
/// zero and epoch time; `append_dir("/", metadata)` can set its metadata once.
///
/// Paths are raw Unix bytes. A single leading `/` is optional; empty components,
/// `.`, `..`, NUL and components longer than 255 bytes are rejected. Only directory
/// paths may have a trailing `/`. Paths are never resolved through symlinks.
///
/// Append validation errors leave the builder usable. A file read or output I/O error
/// poisons it: discard that output and start again. Dropping a builder does **not**
/// finish it. Do not access or modify the output through other handles while building.
/// Uses 4 KiB blocks, compact/extended inodes, flat or inline-tail data and a CRC32C
/// superblock checksum. UUID and filesystem creation time default to zero;
/// [`Options`] can set them without changing inode modification times.
/// [`Metadata::xattrs`] are stored inline by default; [`Options::shared_xattrs`]
/// and [`Options::xattr_prefixes`] enable deduplication. Pass `&metadata` to reuse
/// metadata without cloning attributes. Compact inodes require 16-bit UID/GID/link counts,
/// a 32-bit size and a losslessly representable modification time.
/// [`Options::inode_format`] defaults to automatic selection. Once a streamed
/// entry is Compact, adding its 65,536th link fails without changing the builder;
/// select [`super::InodeFormat::Extended`] up front if more links may be needed.
/// Directory formats are chosen at finish when sizes and counts are known.
/// Directory attributes remain in memory until finish;
/// other attributes are streamed during append. [`Options::compression`] enables
/// bounded Compact-index writing for regular files larger than 4 KiB, with opt-in
/// larger physical clusters, inline compressed tails and packed fragments. A hidden
/// chunk-indexed inode holds fragments; only one fragment data page is buffered.
/// Additional devices are not generated.
///
/// ```
/// use std::io::Cursor;
/// use erofs_rs::build::{Builder, Metadata};
///
/// let mut image = Builder::new(Cursor::new(Vec::new()))?;
/// image.append_dir("etc", Metadata { mode: 0o755, ..Metadata::default() })?;
/// image.append_file("etc/message", Metadata::default(), 5, &b"hello"[..])?;
/// image.append_symlink("message", Metadata { mode: 0o777, ..Metadata::default() }, "etc/message")?;
/// image.append_hard_link("copy", "etc/message")?;
/// let bytes = image.finish()?.into_inner();
/// # assert!(!bytes.is_empty());
/// # Ok::<(), erofs_rs::Error>(())
/// ```
#[must_use = "call finish() to complete the image"]
pub struct Builder<W: Write + Seek> {
    output: W,
    state: State,
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
pub(super) struct SegmentedFile {
    entry: super::PendingEntry,
    indexes: super::compress::IndexWriter,
    index_offset: u64,
}

impl Options {
    /// Creates a synchronous builder with these settings and an empty output.
    ///
    /// Invalid options are rejected before seeking or writing. Nonempty outputs
    /// are rejected, not truncated. Call [`Builder::finish`] to complete the image.
    pub fn build<W: Write + Seek>(self, mut output: W) -> Result<Builder<W>> {
        let state = State::new(self)?;
        if output.seek(SeekFrom::End(0))? != 0 {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "image output must be empty").into(),
            );
        }
        output.seek(SeekFrom::Start(0))?;
        // A valid superblock is only written by finish().
        output.write_all(&ZERO_BLOCK)?;
        Ok(Builder { output, state })
    }
}

impl<W: Write + Seek> Builder<W> {
    /// Starts an image at offset zero with default [`Options`].
    /// Nonempty outputs are rejected, not truncated.
    pub fn new(output: W) -> Result<Self> {
        Options::default().build(output)
    }

    /// Adds a directory. An empty path or `/` sets the root metadata once.
    ///
    /// Supply large root attributes before payloads to keep the root NID small.
    /// A late root beyond the legacy NID range requires EROFS's 48-bit feature.
    pub fn append_dir(
        &mut self,
        path: impl AsRef<UnixPath>,
        metadata: impl Borrow<Metadata>,
    ) -> Result<()> {
        self.state.append_dir(path.as_ref(), metadata.borrow())
    }

    /// Appends exactly `size` bytes from `data`, using bounded memory.
    ///
    /// A short source is an error and poisons the builder. Bytes beyond `size` are
    /// not read; pass `&mut reader` to retain the remaining input. A zero size does
    /// not read the source. File sizes and offsets remain `u64` on 32-bit hosts.
    pub fn append_file(
        &mut self,
        path: impl AsRef<UnixPath>,
        metadata: impl Borrow<Metadata>,
        size: u64,
        data: impl Read,
    ) -> Result<()> {
        self.append_payload(
            path.as_ref(),
            metadata.borrow(),
            Content::File,
            size,
            1,
            data,
        )
    }

    /// Adds a symbolic link without resolving its target. Target bytes are retained
    /// verbatim, including absolute paths and `..`; empty/NUL targets are rejected.
    pub fn append_symlink(
        &mut self,
        path: impl AsRef<UnixPath>,
        metadata: impl Borrow<Metadata>,
        target: impl AsRef<UnixPath>,
    ) -> Result<()> {
        let target = symlink_target(target.as_ref())?;
        self.append_payload(
            path.as_ref(),
            metadata.borrow(),
            Content::Symlink,
            target.len() as u64,
            1,
            target,
        )
    }

    /// Adds a FIFO, socket, character device or block device, with no payload.
    ///
    /// Device numbers must fit EROFS's 12-bit major and 20-bit minor fields.
    /// No host device or FIFO is created or opened, and no special privileges are
    /// needed. This can flush a pending metadata block, so output errors poison
    /// the builder just like other appends.
    pub fn append_special(
        &mut self,
        path: impl AsRef<UnixPath>,
        metadata: impl Borrow<Metadata>,
        kind: SpecialFile,
    ) -> Result<()> {
        self.append_payload(
            path.as_ref(),
            metadata.borrow(),
            Content::Special(kind),
            0,
            1,
            &[][..],
        )
    }

    /// Adds another name for an existing file, symlink or special inode, sharing its
    /// metadata. The target must already have been appended; directory hard links
    /// are rejected, and symlink targets are not followed. A Compact inode cannot
    /// exceed 65,535 links; that error leaves both the count and path unchanged.
    pub fn append_hard_link(
        &mut self,
        path: impl AsRef<UnixPath>,
        target: impl AsRef<UnixPath>,
    ) -> Result<()> {
        self.state.append_hard_link(path.as_ref(), target.as_ref())
    }

    /// Writes directories, inodes and the final superblock, flushes, and returns
    /// the output. This consumes the builder; no implicit finish is performed on
    /// drop. On failure discard the incomplete output. For durable file storage,
    /// the caller can call `File::sync_all` on the returned file.
    pub fn finish(mut self) -> Result<W> {
        let blocks = self.state.layout()?;
        self.flush_metadata()?;
        self.flush_fragments()?;
        let mut superblock = encode::superblock(&self.state, blocks);
        for page in encode::metadata_blocks(&self.state.nodes, &self.state.attributes) {
            if page.block == 0 {
                encode::set_superblock_checksum(&mut superblock, &page.data);
            }
            write_at(&mut self.output, page.offset(), page.bytes())?;
        }
        // Ordinary file headers were written with their payloads. Only hard-link
        // counts need patching; never rewrite a packed block over its inline data.
        for (node_index, node) in self.state.nodes.iter().enumerate() {
            if node.nlink > 1 && !matches!(node.content, Content::Directory { .. }) {
                write_at(
                    &mut self.output,
                    node.inode_offset(),
                    &encode::inode(node, node_index)[..node.inode_size()],
                )?;
            }
        }
        write_at(&mut self.output, SUPER_BLOCK_OFFSET, &superblock)?;
        check_output_length(
            self.output.seek(SeekFrom::End(0))?,
            u64::from(blocks) * BLOCK_SIZE as u64,
        )?;
        self.output.flush()?;
        Ok(self.output)
    }

    // Directory import supplies its known in-tree count for header selection.
    // Public streaming appends start with one link; actual counts are tracked by State.
    pub(super) fn append_payload(
        &mut self,
        path: &UnixPath,
        metadata: &Metadata,
        content: Content,
        size: u64,
        nlink: u32,
        mut data: impl Read,
    ) -> Result<()> {
        let entry = self
            .state
            .prepare_payload(path, metadata, content, size, nlink)?;
        #[cfg(any(
            feature = "lz4",
            feature = "lzma",
            feature = "deflate",
            feature = "zstd"
        ))]
        if let Some(size) = self.state.buffered_file_size(&entry.node) {
            let mut entry = entry;
            let mut input = vec![0; size];
            data.read_exact(&mut input)?;
            let encoded = self.state.buffer_file(&mut entry, &input)?;
            return self.append_encoded(entry, encoded, &input);
        }
        self.prepare_metadata(&entry.node)?;
        #[cfg(any(
            feature = "lz4",
            feature = "lzma",
            feature = "deflate",
            feature = "zstd"
        ))]
        if entry.node.compression != super::Compression::None {
            return self.append_compressed(entry, &mut data);
        }
        self.append_flat(entry, &mut data)
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    pub(super) fn append_fitted(
        &mut self,
        path: &UnixPath,
        metadata: &Metadata,
        nlink: u32,
        input: &[u8],
        fitted: super::compress::FittedFile,
    ) -> Result<()> {
        let mut entry =
            self.state
                .prepare_payload(path, metadata, Content::File, input.len() as u64, nlink)?;
        debug_assert_eq!(
            self.state.buffered_file_size(&entry.node),
            Some(input.len())
        );
        let encoded = self.state.place_fitted(&mut entry, fitted)?;
        self.append_encoded(entry, encoded, input)
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    pub(super) fn begin_segments(
        &mut self,
        path: &UnixPath,
        metadata: &Metadata,
        size: u64,
        nlink: u32,
    ) -> Result<SegmentedFile> {
        let entry = self
            .state
            .prepare_payload(path, metadata, Content::File, size, nlink)?;
        self.prepare_metadata(&entry.node)?;
        Ok(SegmentedFile {
            indexes: super::compress::IndexWriter::new(&entry.node),
            index_offset: entry.node.index_offset(),
            entry,
        })
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    pub(super) fn append_segment(
        &mut self,
        file: &mut SegmentedFile,
        fitted: super::compress::FittedFile,
    ) -> Result<()> {
        let entry = &mut file.entry;
        let bytes = file.indexes.append(&fitted, entry.next_block);
        self.output.seek(SeekFrom::Start(
            u64::from(entry.next_block) * BLOCK_SIZE as u64,
        ))?;
        self.output.write_all(&fitted.data)?;
        let blocks = (fitted.data.len() / BLOCK_SIZE) as u32;
        entry.next_block += blocks;
        entry.node.data_block += blocks;
        for page in self
            .state
            .write_indexes(&entry.node, file.index_offset, bytes)
        {
            write_at(&mut self.output, page.offset(), page.bytes())?;
        }
        file.index_offset += bytes.len() as u64;
        Ok(())
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    pub(super) fn finish_segments(&mut self, file: SegmentedFile) -> Result<()> {
        self.finish_compressed(file.entry, file.index_offset, None)
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    fn append_encoded(
        &mut self,
        entry: super::PendingEntry,
        encoded: Option<super::compress::BufferedFile>,
        input: &[u8],
    ) -> Result<()> {
        self.prepare_metadata(&entry.node)?;
        if let Some(encoded) = encoded {
            return self.append_buffered(entry, encoded);
        }
        self.append_flat(entry, &mut &input[..])
    }

    fn append_flat(&mut self, entry: super::PendingEntry, data: &mut impl Read) -> Result<()> {
        let external_size = entry.node.external_size();
        if external_size != 0 {
            self.output.seek(SeekFrom::Start(
                u64::from(entry.node.data_block) * BLOCK_SIZE as u64,
            ))?;
            if external_size <= BLOCK_SIZE as u64 {
                // Avoid io::copy's descriptor probing for a single block, and
                // combine a non-inline tail with its zero padding in one write.
                let mut buf = ZERO_BLOCK;
                data.read_exact(&mut buf[..external_size as usize])?;
                self.output.write_all(&buf)?;
            } else {
                copy_exact(data, &mut self.output, external_size)?;
                self.output.write_all(encode::padding(external_size))?;
            }
        }
        if entry.node.inline_size != 0 {
            let page = self.state.pending_metadata.as_mut().unwrap();
            data.read_exact(page.inline_data_mut(&entry.node))?;
        }
        self.state.commit_payload(entry);
        Ok(())
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    fn append_buffered(
        &mut self,
        mut entry: super::PendingEntry,
        encoded: super::compress::BufferedFile,
    ) -> Result<()> {
        self.output.seek(SeekFrom::Start(
            u64::from(entry.next_block) * BLOCK_SIZE as u64,
        ))?;
        self.output.write_all(&encoded.data)?;
        let blocks = (encoded.data.len() / BLOCK_SIZE) as u32;
        entry.next_block += blocks;
        entry.node.data_block = blocks;
        for page in
            self.state
                .write_indexes(&entry.node, entry.node.index_offset(), &encoded.indexes)
        {
            write_at(&mut self.output, page.offset(), page.bytes())?;
        }
        if entry.node.metadata_block() != self.state.pending_metadata.as_ref().unwrap().block {
            write_at(
                &mut self.output,
                entry.node.inode_offset(),
                &encode::inode(&entry.node, self.state.nodes.len())[..entry.node.inode_size()],
            )?;
        }
        self.state.commit_payload(entry);
        Ok(())
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    fn append_compressed(
        &mut self,
        mut entry: super::PendingEntry,
        data: &mut impl Read,
    ) -> Result<()> {
        use super::compress::{Compressor, INPUT_SIZE, Placement};

        let mut input = vec![0; INPUT_SIZE];
        let mut compressor = Compressor::new(&entry.node, &self.state.options);
        let mut remaining = entry.node.size;
        let mut index_offset = entry.node.index_offset();
        self.output.seek(SeekFrom::Start(
            u64::from(entry.next_block) * BLOCK_SIZE as u64,
        ))?;
        let mut buffered = 0;
        while remaining != 0 || buffered != 0 {
            let len = remaining.min((input.len() - buffered) as u64) as usize;
            data.read_exact(&mut input[buffered..buffered + len])?;
            remaining -= len as u64;
            buffered += len;
            let extent = compressor.encode(
                &mut self.state.encoder,
                &input[..buffered],
                entry.next_block,
                self.state.fragments.size,
            )?;
            match extent.placement {
                Placement::Inline => entry.node.inline_size = extent.data.len(),
                Placement::Fragment => {
                    let mut tail = extent.data;
                    while !tail.is_empty() {
                        let n = self.state.fragments.append(&mut entry, tail);
                        tail = &tail[n..];
                        if self.state.fragments.used == BLOCK_SIZE {
                            self.flush_fragments()?;
                        }
                    }
                }
                Placement::External => {
                    self.output.write_all(extent.data)?;
                    let blocks = (extent.data.len() / BLOCK_SIZE) as u32;
                    entry.next_block += blocks; // Worst-case bounds checked before I/O.
                    entry.node.data_block += blocks;
                }
            }
            for page in self
                .state
                .write_indexes(&entry.node, index_offset, extent.indexes)
            {
                write_at(&mut self.output, page.offset(), page.bytes())?;
                self.output.seek(SeekFrom::Start(
                    u64::from(entry.next_block) * BLOCK_SIZE as u64,
                ))?;
            }
            index_offset += extent.indexes.len() as u64;
            input.copy_within(extent.consumed..buffered, 0);
            buffered -= extent.consumed;
        }
        self.finish_compressed(entry, index_offset, compressor.inline_tail())
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    fn finish_compressed(
        &mut self,
        entry: super::PendingEntry,
        index_offset: u64,
        inline_tail: Option<&[u8]>,
    ) -> Result<()> {
        debug_assert_eq!(index_offset, entry.node.index_end());
        let page = self.state.pending_metadata.as_mut().unwrap();
        if let Some(tail) = inline_tail {
            page.write_at(index_offset, tail);
        }
        page.write_inode(&entry.node, self.state.nodes.len());
        let header = encode::inode(&entry.node, self.state.nodes.len());
        let map = entry.node.map_header();
        for (offset, bytes) in [
            (
                entry.node.inode_offset(),
                &header[..entry.node.inode_size()],
            ),
            (entry.node.map_offset(), &map[..]),
        ] {
            // write_inode already updated the pending page; patch only headers
            // in pages that were flushed while streaming indexes.
            if offset / BLOCK_SIZE as u64 != u64::from(page.block) {
                write_at(&mut self.output, offset, bytes)?;
            }
        }
        self.state.commit_payload(entry);
        Ok(())
    }

    fn prepare_metadata(&mut self, node: &Node) -> Result<()> {
        for page in self.state.prepare_metadata(node) {
            write_at(&mut self.output, page.offset(), page.bytes())?;
        }
        Ok(())
    }

    fn flush_fragments(&mut self) -> Result<()> {
        let fragments = &mut self.state.fragments;
        if fragments.used != 0 {
            write_at(
                &mut self.output,
                u64::from(fragments.block) * BLOCK_SIZE as u64,
                &fragments.data,
            )?;
            fragments.used = 0;
        }
        Ok(())
    }

    fn flush_metadata(&mut self) -> Result<()> {
        if let Some(page) = self.state.pending_metadata.take() {
            write_at(&mut self.output, page.offset(), page.bytes())?;
        }
        Ok(())
    }
}

fn write_at(output: &mut (impl Write + Seek), offset: u64, bytes: &[u8]) -> Result<()> {
    output.seek(SeekFrom::Start(offset))?;
    Ok(output.write_all(bytes)?)
}

fn copy_exact(data: &mut impl Read, output: &mut impl Write, size: u64) -> Result<()> {
    if io::copy(&mut data.take(size), output)? != size {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "file source shorter than declared size",
        )
        .into());
    }
    Ok(())
}
