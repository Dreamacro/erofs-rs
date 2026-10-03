use core::future::Future;

use bytes::Buf;

use crate::Result;

/// Runtime-independent sequential async input, including EROFS file handles.
///
/// Like [`super::AsyncImage`], this uses `Send` futures rather than a runtime's
/// polling traits. Implementations must not perform blocking I/O on the executor.
pub trait AsyncRead: Send {
    /// Reads up to `buf.len()` bytes. Short reads are allowed; zero means EOF
    /// unless the buffer is empty. Never return a count larger than the buffer.
    /// Empty reads must succeed with zero without I/O. On error or cancellation,
    /// the buffer and stream position may have changed unless documented otherwise.
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send;
}

impl<R: AsyncRead + ?Sized> AsyncRead for &mut R {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        R::read(self, buf).await
    }
}

impl AsyncRead for &[u8] {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = self.len().min(buf.len());
        self.copy_to_slice(&mut buf[..n]);
        Ok(n)
    }
}

#[cfg(feature = "std")]
mod output {
    use super::*;
    use std::io::{Cursor, Read, Seek, SeekFrom, Write};

    /// Runtime-independent async output. A successful write must be complete,
    /// just as an `AsyncImage::read_exact_at` must fill its entire buffer.
    pub trait AsyncWrite: Send {
        /// Writes every byte, handling short writes internally. Empty writes must
        /// succeed without I/O. An error or cancellation may leave a partial write.
        fn write_all(&mut self, buf: &[u8]) -> impl Future<Output = Result<()>> + Send;

        /// Flushes buffered bytes without closing or shutting down the output.
        /// This does not promise durable storage; use the backend's sync API for that.
        fn flush(&mut self) -> impl Future<Output = Result<()>> + Send;
    }

    /// Runtime-independent async positioning, using `std::io::SeekFrom` offsets.
    pub trait AsyncSeek: Send {
        /// Sets and returns the position. `End(0)` returns the current logical
        /// length. Subsequent I/O must honor the position (not append mode).
        /// Buffered implementations must coordinate pending writes with seeking.
        /// On error or cancellation the position may have changed.
        fn seek(&mut self, position: SeekFrom) -> impl Future<Output = Result<u64>> + Send;
    }

    impl<W: AsyncWrite + ?Sized> AsyncWrite for &mut W {
        async fn write_all(&mut self, buf: &[u8]) -> Result<()> {
            W::write_all(self, buf).await
        }

        async fn flush(&mut self) -> Result<()> {
            W::flush(self).await
        }
    }

    impl<S: AsyncSeek + ?Sized> AsyncSeek for &mut S {
        async fn seek(&mut self, position: SeekFrom) -> Result<u64> {
            S::seek(self, position).await
        }
    }

    // Only memory-backed std I/O is adapted directly. Adapting arbitrary std
    // Read/Write types here would silently block the caller's async executor.
    impl<T: AsRef<[u8]> + Send> AsyncRead for Cursor<T> {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            Ok(Read::read(self, buf)?)
        }
    }

    impl<T> AsyncWrite for Cursor<T>
    where
        Self: Write + Send,
    {
        async fn write_all(&mut self, buf: &[u8]) -> Result<()> {
            Ok(Write::write_all(self, buf)?)
        }

        async fn flush(&mut self) -> Result<()> {
            Ok(Write::flush(self)?)
        }
    }

    impl<T: AsRef<[u8]> + Send> AsyncSeek for Cursor<T> {
        async fn seek(&mut self, position: SeekFrom) -> Result<u64> {
            Ok(Seek::seek(self, position)?)
        }
    }
}

#[cfg(feature = "std")]
pub use output::{AsyncSeek, AsyncWrite};

#[cfg(test)]
mod tests {
    use super::AsyncRead;
    use crate::tests::ready;

    #[test]
    fn byte_input_preserves_suffix_empty_reads_and_buffer_tails() {
        let mut input = &b"abc"[..];
        let mut buf = [7; 4];
        assert_eq!(
            ready(AsyncRead::read(&mut input, &mut buf[..2])).unwrap(),
            2
        );
        assert_eq!(&buf, &[b'a', b'b', 7, 7]);
        assert_eq!(input, b"c");
        let mut borrowed = &mut input;
        assert_eq!(ready(AsyncRead::read(&mut borrowed, &mut [])).unwrap(), 0);
        assert_eq!(ready(AsyncRead::read(&mut borrowed, &mut buf)).unwrap(), 1);
        assert_eq!(&buf, &[b'c', b'b', 7, 7]);
        assert_eq!(ready(AsyncRead::read(&mut input, &mut buf)).unwrap(), 0);
    }
}
