use std::io::{self, SeekFrom};
use typed_path::UnixPath;

use super::{
    Metadata, State, check_output_length,
    encode::{self, BLOCK_SIZE, Content, MetadataBlock, ZERO_BLOCK},
    symlink_target,
};
use crate::{
    Result,
    backend::{AsyncRead, AsyncSeek, AsyncWrite},
    types::SUPER_BLOCK_OFFSET,
};

/// Runtime-independent async counterpart of [`Builder`](super::Builder), available with `std`.
///
/// Uses the library's [`AsyncRead`], [`AsyncWrite`] and [`AsyncSeek`] backend traits,
/// following the same async-function model as `backend::AsyncImage`. Memory cursors,
/// byte-slice inputs and EROFS async file inputs work without a runtime dependency.
/// The optional `tokio` feature provides `backend::TokioIo` for Tokio types.
///
/// Path, metadata, hard-link and format rules are shared with `Builder`.
/// `append_dir` and `append_hard_link` only change metadata and remain synchronous;
/// operations performing I/O are async. The output must be empty and honor seeks,
/// not use append mode. Full data blocks are streamed; inline tails share one
/// pending metadata block rather than accumulating per-file buffers.
///
/// # Errors and cancellation
///
/// Append validation errors leave the builder usable. An I/O error or cancellation
/// of an append after I/O begins poisons it: discard the output and start again.
/// Dropping an unpolled append future has no effect. Canceling `new` or `finish`
/// may leave an incomplete output, which must also be discarded. Dropping never
/// finishes the image, and `finish` flushes but does not shut down the returned
/// writer. Futures are `Send` when their writer, reader and path arguments are.
///
/// ```
/// use std::io::Cursor;
/// use erofs_rs::build::{AsyncBuilder, Metadata};
/// # async fn example() -> erofs_rs::Result<()> {
/// let mut image = AsyncBuilder::new(Cursor::new(Vec::new())).await?;
/// image.append_dir("etc", Metadata { mode: 0o755, ..Metadata::default() })?;
/// image.append_file("etc/message", Metadata::default(), 5, &b"hello"[..]).await?;
/// image.append_symlink("message", Metadata { mode: 0o777, ..Metadata::default() }, "etc/message").await?;
/// image.append_hard_link("copy", "etc/message")?;
/// let bytes = image.finish().await?.into_inner();
/// # assert!(!bytes.is_empty());
/// # Ok(())
/// # }
/// ```
#[must_use = "await finish() to complete the image"]
pub struct AsyncBuilder<W: AsyncWrite + AsyncSeek> {
    output: W,
    state: State,
}

impl<W: AsyncWrite + AsyncSeek> AsyncBuilder<W> {
    /// Starts an image at offset zero. Nonempty outputs are rejected, not truncated.
    pub async fn new(mut output: W) -> Result<Self> {
        if output.seek(SeekFrom::End(0)).await? != 0 {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "image output must be empty").into(),
            );
        }
        output.seek(SeekFrom::Start(0)).await?;
        output.write_all(&ZERO_BLOCK).await?;
        Ok(Self {
            output,
            state: State::new(),
        })
    }

    /// Same as [`Builder::append_dir`](super::Builder::append_dir); no I/O or await is needed.
    pub fn append_dir(&mut self, path: impl AsRef<UnixPath>, metadata: Metadata) -> Result<()> {
        self.state.append_dir(path.as_ref(), metadata)
    }

    /// Copies exactly `size` bytes, retaining any suffix in a borrowed reader.
    /// A zero size does not read the source. Short input, read/write errors or
    /// cancellation after I/O begins poison the builder.
    pub async fn append_file(
        &mut self,
        path: impl AsRef<UnixPath>,
        metadata: Metadata,
        size: u64,
        data: impl AsyncRead,
    ) -> Result<()> {
        self.append_payload(path.as_ref(), metadata, Content::File, size, data)
            .await
    }

    /// Same target-byte and validation rules as [`Builder::append_symlink`](super::Builder::append_symlink).
    pub async fn append_symlink(
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
        .await
    }

    /// Same as [`Builder::append_hard_link`](super::Builder::append_hard_link); no I/O or await is needed.
    pub fn append_hard_link(
        &mut self,
        path: impl AsRef<UnixPath>,
        target: impl AsRef<UnixPath>,
    ) -> Result<()> {
        self.state.append_hard_link(path.as_ref(), target.as_ref())
    }

    /// Writes final metadata and the superblock, flushes, and returns the writer.
    /// On error or cancellation discard the output. Durability is backend-specific;
    /// for a Tokio file, unwrap `TokioIo` and await the file's `sync_all()`.
    pub async fn finish(mut self) -> Result<W> {
        let blocks = self.state.layout()?;
        self.flush_metadata().await?;
        for (offset, block) in encode::metadata_blocks(&self.state.nodes) {
            self.output.seek(SeekFrom::Start(offset)).await?;
            self.output.write_all(&block).await?;
        }
        for (node_index, node) in self.state.nodes.iter().enumerate() {
            if node.nlink > 1 && !matches!(node.content, Content::Directory { .. }) {
                self.output
                    .seek(SeekFrom::Start(node.inode_offset()))
                    .await?;
                self.output
                    .write_all(&encode::inode(node, node_index))
                    .await?;
            }
        }
        self.output
            .seek(SeekFrom::Start(SUPER_BLOCK_OFFSET))
            .await?;
        self.output
            .write_all(&encode::superblock(self.state.nodes.len(), blocks))
            .await?;
        check_output_length(
            self.output.seek(SeekFrom::End(0)).await?,
            u64::from(blocks) * BLOCK_SIZE as u64,
        )?;
        self.output.flush().await?;
        Ok(self.output)
    }

    async fn append_payload(
        &mut self,
        path: &UnixPath,
        metadata: Metadata,
        content: Content,
        size: u64,
        mut data: impl AsyncRead,
    ) -> Result<()> {
        let entry = self.state.prepare_payload(path, metadata, content, size)?;
        let block = entry.node.metadata_block();
        if self
            .state
            .pending_metadata
            .as_ref()
            .is_none_or(|page| page.block != block)
        {
            self.flush_metadata().await?;
            self.state.pending_metadata = Some(MetadataBlock::new(block));
        }
        let external_size = entry.node.external_size();
        if external_size != 0 {
            self.output
                .seek(SeekFrom::Start(
                    u64::from(entry.node.data_block) * BLOCK_SIZE as u64,
                ))
                .await?;
            let mut buf = [0; 8192];
            let mut remaining = external_size;
            while remaining != 0 {
                let len = remaining.min(buf.len() as u64) as usize;
                let n = read_chunk(&mut data, &mut buf[..len]).await?;
                self.output.write_all(&buf[..n]).await?;
                remaining -= n as u64;
            }
            self.output
                .write_all(encode::padding(external_size))
                .await?;
        }
        let page = self.state.pending_metadata.as_mut().unwrap();
        let mut inline_data = page.inline_data_mut(&entry.node);
        while !inline_data.is_empty() {
            let n = read_chunk(&mut data, inline_data).await?;
            inline_data = &mut inline_data[n..];
        }
        self.state.commit_payload(entry);
        Ok(())
    }

    async fn flush_metadata(&mut self) -> Result<()> {
        if let Some(page) = self.state.pending_metadata.take() {
            self.output
                .seek(SeekFrom::Start(u64::from(page.block) * BLOCK_SIZE as u64))
                .await?;
            self.output.write_all(&page.data).await?;
        }
        Ok(())
    }
}

// Both payload phases require progress and must reject invalid backend counts.
async fn read_chunk(data: &mut impl AsyncRead, buf: &mut [u8]) -> Result<usize> {
    let n = data.read(buf).await?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "file source shorter than declared size",
        )
        .into());
    }
    if n > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "async reader returned an oversized count",
        )
        .into());
    }
    Ok(n)
}
