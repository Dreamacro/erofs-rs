use std::io::{self, Read, Seek, SeekFrom, Write};
use typed_path::UnixPath;

use super::{
    Metadata, State, check_output_length,
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
/// Uses 4 KiB blocks and extended inodes with flat or inline-tail data. UUID and
/// filesystem creation time are zero; xattrs, special files, compression,
/// checksums and additional devices are not generated.
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

impl<W: Write + Seek> Builder<W> {
    /// Starts an image at offset zero. Nonempty outputs are rejected, not truncated.
    pub fn new(mut output: W) -> Result<Self> {
        if output.seek(SeekFrom::End(0))? != 0 {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "image output must be empty").into(),
            );
        }
        output.seek(SeekFrom::Start(0))?;
        // A valid superblock is only written by finish().
        output.write_all(&ZERO_BLOCK)?;
        Ok(Self {
            output,
            state: State::new(),
        })
    }

    /// Adds a directory. An empty path or `/` sets the root metadata once.
    pub fn append_dir(&mut self, path: impl AsRef<UnixPath>, metadata: Metadata) -> Result<()> {
        self.state.append_dir(path.as_ref(), metadata)
    }

    /// Appends exactly `size` bytes from `data`, using bounded memory.
    ///
    /// A short source is an error and poisons the builder. Bytes beyond `size` are
    /// not read; pass `&mut reader` to retain the remaining input. A zero size does
    /// not read the source. File sizes and offsets remain `u64` on 32-bit hosts.
    pub fn append_file(
        &mut self,
        path: impl AsRef<UnixPath>,
        metadata: Metadata,
        size: u64,
        data: impl Read,
    ) -> Result<()> {
        self.append_payload(path.as_ref(), metadata, Content::File, size, data)
    }

    /// Adds a symbolic link without resolving its target. Target bytes are retained
    /// verbatim, including absolute paths and `..`; empty/NUL targets are rejected.
    pub fn append_symlink(
        &mut self,
        path: impl AsRef<UnixPath>,
        metadata: Metadata,
        target: impl AsRef<UnixPath>,
    ) -> Result<()> {
        self.state.check_ready()?;
        let target = symlink_target(target.as_ref())?;
        self.append_payload(
            path.as_ref(),
            metadata,
            Content::Symlink,
            target.len() as u64,
            target,
        )
    }

    /// Adds another name for an existing file or symlink, sharing its inode and
    /// metadata. The target must already have been appended; directory hard links
    /// are rejected, and symlink targets are not followed.
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
        for (offset, block) in encode::metadata_blocks(&self.state.nodes) {
            self.output.seek(SeekFrom::Start(offset))?;
            self.output.write_all(&block)?;
        }
        // Ordinary file headers were written with their payloads. Only hard-link
        // counts need patching; never rewrite a packed block over its inline data.
        for (node_index, node) in self.state.nodes.iter().enumerate() {
            if node.nlink > 1 && !matches!(node.content, Content::Directory { .. }) {
                self.output.seek(SeekFrom::Start(node.inode_offset()))?;
                self.output.write_all(&encode::inode(node, node_index))?;
            }
        }
        self.output.seek(SeekFrom::Start(SUPER_BLOCK_OFFSET))?;
        self.output
            .write_all(&encode::superblock(self.state.nodes.len(), blocks))?;
        check_output_length(
            self.output.seek(SeekFrom::End(0))?,
            u64::from(blocks) * BLOCK_SIZE as u64,
        )?;
        self.output.flush()?;
        Ok(self.output)
    }

    fn append_payload(
        &mut self,
        path: &UnixPath,
        metadata: Metadata,
        content: Content,
        size: u64,
        mut data: impl Read,
    ) -> Result<()> {
        let entry = self.state.prepare_payload(path, metadata, content, size)?;
        let block = entry.node.metadata_block();
        if self
            .state
            .pending_metadata
            .as_ref()
            .is_none_or(|page| page.block != block)
        {
            self.flush_metadata()?;
            self.state.pending_metadata = Some(MetadataBlock::new(block));
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

    fn flush_metadata(&mut self) -> Result<()> {
        if let Some(page) = self.state.pending_metadata.take() {
            self.output
                .seek(SeekFrom::Start(u64::from(page.block) * BLOCK_SIZE as u64))?;
            self.output.write_all(&page.data)?;
        }
        Ok(())
    }
}
