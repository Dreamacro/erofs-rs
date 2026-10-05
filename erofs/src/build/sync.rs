use std::{
    borrow::Borrow,
    io::{self, Read, Seek, SeekFrom, Write},
};
use typed_path::UnixPath;

use super::{
    Compression, Metadata, Options, SpecialFile, State, check_output_length,
    encode::{self, BLOCK_SIZE, Content, MetadataBlock, ZERO_BLOCK},
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
/// [`Metadata::xattrs`] are stored inline; pass `&metadata` to reuse metadata
/// without cloning attributes. Compact inodes require 16-bit UID/GID/link counts,
/// a 32-bit size and a modification time equal to the configured build time.
/// [`Options::inode_format`] defaults to automatic selection. Once a streamed
/// entry is Compact, adding its 65,536th link fails without changing the builder;
/// select [`super::InodeFormat::Extended`] up front if more links may be needed.
/// Directory formats are chosen at finish when sizes and counts are known.
/// Directory attributes remain in memory until finish;
/// other attributes are streamed during append. [`Options::compression`] enables
/// bounded Full-index writing for regular files of at least 8 KiB. Sockets,
/// shared attributes and additional devices are not generated.
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
        self.state.check_ready()?;
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

    /// Adds a FIFO, character device or block device, with no payload.
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
        let mut superblock = encode::superblock(
            self.state.nodes.len(),
            blocks,
            self.state.nodes[0].nid,
            &self.state.options,
        );
        for (offset, block) in encode::metadata_blocks(&self.state.nodes) {
            if offset == 0 {
                encode::set_superblock_checksum(&mut superblock, &block);
            }
            self.output.seek(SeekFrom::Start(offset))?;
            self.output.write_all(&block)?;
        }
        // Ordinary file headers were written with their payloads. Only hard-link
        // counts need patching; never rewrite a packed block over its inline data.
        for (node_index, node) in self.state.nodes.iter().enumerate() {
            if node.nlink > 1 && !matches!(node.content, Content::Directory { .. }) {
                self.output.seek(SeekFrom::Start(node.inode_offset()))?;
                self.output
                    .write_all(&encode::inode(node, node_index)[..node.inode_size()])?;
            }
        }
        self.output.seek(SeekFrom::Start(SUPER_BLOCK_OFFSET))?;
        self.output.write_all(&superblock)?;
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
        let last_block = if entry.node.compression != Compression::None {
            entry.node.first_index_block()
        } else {
            entry.node.last_metadata_block()
        };
        for block in entry.node.metadata_block()..=last_block {
            if self
                .state
                .pending_metadata
                .as_ref()
                .is_none_or(|page| page.block != block)
            {
                self.flush_metadata()?;
                self.state.pending_metadata = Some(MetadataBlock::new(block));
            }
            self.state
                .pending_metadata
                .as_mut()
                .unwrap()
                .write_inode(&entry.node, self.state.nodes.len());
        }
        #[cfg(any(
            feature = "lz4",
            feature = "lzma",
            feature = "deflate",
            feature = "zstd"
        ))]
        if entry.node.compression != Compression::None {
            return self.append_compressed(entry, &mut data);
        }
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
                if io::copy(&mut (&mut data).take(external_size), &mut self.output)?
                    != external_size
                {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "file source shorter than declared size",
                    )
                    .into());
                }
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
    fn append_compressed(
        &mut self,
        mut entry: super::PendingEntry,
        data: &mut impl Read,
    ) -> Result<()> {
        use super::compress::{Compressor, INPUT_SIZE};

        let mut input = vec![0; INPUT_SIZE];
        let mut compressor = Compressor::new(entry.node.compression);
        let mut remaining = entry.node.size;
        let mut index_offset = entry.node.index_offset();
        self.output.seek(SeekFrom::Start(
            u64::from(entry.next_block) * BLOCK_SIZE as u64,
        ))?;
        while remaining != 0 {
            let len = remaining.min(INPUT_SIZE as u64) as usize;
            data.read_exact(&mut input[..len])?;
            remaining -= len as u64;
            let mut input = &input[..len];
            while !input.is_empty() {
                let consumed = compressor.encode(input, entry.next_block)?;
                self.output.write_all(&compressor.data)?;
                entry.next_block += 1; // Worst-case bounds were checked before I/O.
                entry.node.data_block += 1; // i_u is the physical block count for compressed inodes.
                let mut indexes = &compressor.indexes[..consumed.div_ceil(BLOCK_SIZE) * 8];
                while !indexes.is_empty() {
                    let block = (index_offset / BLOCK_SIZE as u64) as u32;
                    if self.state.pending_metadata.as_ref().unwrap().block != block {
                        self.flush_metadata()?;
                        let mut page = MetadataBlock::new(block);
                        page.write_inode(&entry.node, self.state.nodes.len());
                        self.state.pending_metadata = Some(page);
                        self.output.seek(SeekFrom::Start(
                            u64::from(entry.next_block) * BLOCK_SIZE as u64,
                        ))?;
                    }
                    let len = indexes
                        .len()
                        .min(BLOCK_SIZE - (index_offset % BLOCK_SIZE as u64) as usize);
                    self.state
                        .pending_metadata
                        .as_mut()
                        .unwrap()
                        .write_at(index_offset, &indexes[..len]);
                    index_offset += len as u64;
                    indexes = &indexes[len..];
                }
                input = &input[consumed..];
            }
        }
        let header = encode::inode(&entry.node, self.state.nodes.len());
        let header = &header[..entry.node.inode_size()];
        let page = self.state.pending_metadata.as_mut().unwrap();
        if page.block == entry.node.metadata_block() {
            page.write_at(entry.node.inode_offset(), header);
        } else {
            self.output
                .seek(SeekFrom::Start(entry.node.inode_offset()))?;
            self.output.write_all(header)?;
        }
        self.state.commit_payload(entry);
        Ok(())
    }

    fn flush_metadata(&mut self) -> Result<()> {
        if let Some(page) = self.state.pending_metadata.take() {
            self.output
                .seek(SeekFrom::Start(u64::from(page.block) * BLOCK_SIZE as u64))?;
            self.output.write_all(&page.data)?;
        }
        Ok(())
    }
}
