use super::{AsyncRead, AsyncSeek, AsyncWrite};
use crate::Result;
use std::io::SeekFrom;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// Adapts owned or borrowed Tokio I/O to the library's async backend traits.
///
/// Available with the `tokio` feature. This is only a trait adapter: it neither
/// creates a runtime nor buffers data. No `tokio_util::compat` is needed.
/// The wrapped type must implement the corresponding Tokio trait, `Unpin` and
/// `Send`. A pinned pointer can be used to wrap an underlying `!Unpin` type.
///
/// ```
/// use std::io::Cursor;
/// use erofs_rs::{backend::TokioIo, build::{AsyncBuilder, Metadata}};
/// # async fn example() -> erofs_rs::Result<()> {
/// let mut builder = AsyncBuilder::new(TokioIo::new(Cursor::new(Vec::new()))).await?;
/// let mut input = Cursor::new(b"data");
/// builder.append_file("file", Metadata::default(), 4, TokioIo::new(&mut input)).await?;
/// let output = builder.finish().await?.into_inner();
/// # assert!(!output.into_inner().is_empty());
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct TokioIo<T> {
    inner: T,
}

impl<T> TokioIo<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Returns the wrapped I/O object, for example to call a file's `sync_all`.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: tokio::io::AsyncRead + Unpin + Send> AsyncRead for TokioIo<T> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        Ok(self.inner.read(buf).await?)
    }
}

impl<T: tokio::io::AsyncWrite + Unpin + Send> AsyncWrite for TokioIo<T> {
    async fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        Ok(self.inner.write_all(buf).await?)
    }

    async fn flush(&mut self) -> Result<()> {
        Ok(self.inner.flush().await?)
    }
}

impl<T: tokio::io::AsyncSeek + Unpin + Send> AsyncSeek for TokioIo<T> {
    async fn seek(&mut self, position: SeekFrom) -> Result<u64> {
        Ok(self.inner.seek(position).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Error,
        build::{AsyncBuilder, Builder, Metadata},
        tests::ready,
    };
    use alloc::vec::Vec;
    use core::{
        pin::Pin,
        task::{Context, Poll},
    };
    use std::io::{self, Cursor};
    use tokio::io::{BufReader, BufWriter, ReadBuf};

    struct NoIo;

    impl tokio::io::AsyncRead for NoIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            panic!("empty input must not perform I/O")
        }
    }

    impl tokio::io::AsyncWrite for NoIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            panic!("empty output must not perform I/O")
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            panic!("unexpected flush")
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            panic!("unexpected shutdown")
        }
    }

    #[test]
    fn tokio_adapter_preserves_io_contracts_and_builder_output() {
        let mut no_io = TokioIo::new(NoIo);
        assert_eq!(ready(AsyncRead::read(&mut no_io, &mut [])).unwrap(), 0);
        ready(AsyncWrite::write_all(&mut no_io, &[])).unwrap();
        let mut bytes = [0; 3];
        {
            let mut output = TokioIo::new(Cursor::new(&mut bytes[..]));
            assert!(
                matches!(ready(AsyncWrite::write_all(&mut output, b"abcd")), Err(Error::Io(error)) if error.kind() == io::ErrorKind::WriteZero)
            );
        }
        assert_eq!(&bytes, b"abc");

        let metadata = Metadata::default();
        let mut sync = Builder::new(Cursor::new(Vec::new())).unwrap();
        sync.append_file("file", metadata, 4, &b"data"[..]).unwrap();
        let mut output = BufWriter::with_capacity(17, Cursor::new(Vec::new()));
        let mut input = TokioIo::new(BufReader::new(Cursor::new(b"datarest")));
        {
            let mut builder = ready(AsyncBuilder::new(TokioIo::new(&mut output))).unwrap();
            ready(builder.append_file("file", metadata, 4, &mut input)).unwrap();
            ready(builder.finish()).unwrap();
        }
        let mut tail = [0; 8];
        let n = ready(AsyncRead::read(&mut input, &mut tail)).unwrap();
        assert_eq!(&tail[..n], b"rest");
        assert_eq!(
            output.into_inner().into_inner(),
            sync.finish().unwrap().into_inner()
        );
    }
}
