use crate::{
    buf::{BufResult, IoBufMut, IoVectoredBuf, IoVectoredBufMut},
    io::{AsyncBufRead, AsyncRead, AsyncWrite, AsyncWriteExt},
};

/// Buffers output before writing it to an [`AsyncWrite`] implementation.
///
/// `BufWriter` combines repeated small writes into larger, less frequent
/// writes. It offers little benefit for large writes or in-memory destinations.
///
/// Dropping a `BufWriter` discards buffered output. Call
/// [`AsyncWrite::flush`] before dropping it when that data must be preserved.
pub struct BufWriter<W> {
    writer: W,
    buf: Box<[u8]>,
    written: usize,
}

impl<W> BufWriter<W> {
    /// Creates a new `BufWriter` with a default buffer capacity.
    /// The default is currently 8 KB, but may change in the future.
    #[inline]
    pub fn new(inner: W) -> Self {
        // TODO: Make this configurable later
        Self::with_capacity(8 * 1024, inner)
    }

    /// Creates a new `BufWriter` with the specified buffer capacity.
    #[inline]
    pub fn with_capacity(capacity: usize, writer: W) -> Self {
        let buffer = vec![0; capacity];
        Self {
            writer,
            buf: buffer.into_boxed_slice(),
            written: 0,
        }
    }

    /// Gets a reference to the underlying writer.
    #[inline]
    pub fn get_ref(&self) -> &W {
        &self.writer
    }

    /// Gets a mutable reference to the underlying writer.
    #[inline]
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Consumes this `BufWriter`, returning the underlying writer.
    ///
    /// Note that any leftover data in the internal buffer is lost.
    #[inline]
    pub fn into_inner(self) -> W {
        self.writer
    }

    /// Returns a reference to the internally buffered data.
    #[inline]
    pub fn buffer(&self) -> &[u8] {
        &self.buf.as_ref()[..self.written]
    }

    /// Invalidates all data in the internal buffer.
    #[inline]
    fn discard_buffer(&mut self) {
        self.written = 0;
    }
}

impl<W: AsyncWrite> BufWriter<W> {
    async fn flush_buf(&mut self) -> std::io::Result<()> {
        if self.written != 0 {
            // there is some data left inside internal buf
            let buf = std::mem::take(&mut self.buf);

            let (ret, buf) = self.writer.write_all(buf).await.into_parts();

            // move it back and return
            self.buf = buf;
            ret?;

            self.discard_buffer();
        }
        Ok(())
    }
}

impl<W: AsyncWrite> AsyncWrite for BufWriter<W> {
    async fn write<B: crate::buf::IoBuf>(&mut self, buf: B) -> crate::buf::BufResult<usize, B> {
        let writer_buf = self.buf.as_ref();
        let writer_buf_written = writer_buf.len();
        let bytes_to_write = buf.as_init().len();

        // Buf can not be copied directly into OwnedBuf,
        // we must flush OwnedBuf first.
        if self.written + bytes_to_write > writer_buf_written {
            match self.flush_buf().await {
                Ok(_) => (),
                Err(e) => {
                    return BufResult(Err(e), buf);
                }
            }
        }

        // Now there are two situations here:
        // 1. The writer has data not yet flushed, and self.written + bytes_to_write <= writer_buf_written,
        // This means the data can be copied into the writer.
        // 2. Writer is empty. If we can copy buf into writer, we will copy it.
        // Otherwise we will send it directly(in this situation, the writer must be already empty).
        if bytes_to_write > writer_buf_written {
            self.writer.write(buf).await
        } else {
            unsafe {
                let writer_buf = self.buf.as_mut();
                writer_buf
                    .as_mut_ptr()
                    .add(self.written)
                    .copy_from_nonoverlapping(buf.as_init().as_ptr(), bytes_to_write);
            }
            self.written += bytes_to_write;
            return BufResult(Ok(bytes_to_write), buf);
        }
    }

    async fn write_vectored<V: IoVectoredBuf>(&mut self, bufs: V) -> crate::buf::BufResult<usize, V> {
        // To keep buffering logic simple for the gather case, flush any pending data first,
        // then delegate the entire vectored write to the underlying writer.
        if let Err(e) = self.flush_buf().await {
            // On flush error we have to return the bufs; since we don't know which, return them as-is (empty write effectively failed before).
            // A better approach would track, but for now surface the error with the original vec.
            return BufResult(Err(e), bufs);
        }
        self.writer.write_vectored(bufs).await
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        self.flush_buf().await?;
        self.writer.flush().await
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        self.flush_buf().await?;
        self.writer.shutdown().await
    }
}

impl<W: AsyncRead + AsyncWrite> AsyncRead for BufWriter<W> {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.writer.read(buf).await
    }

    async fn read_vectored<V: IoVectoredBufMut>(&mut self, bufs: V) -> BufResult<usize, V> {
        self.writer.read_vectored(bufs).await
    }
}

impl<W: AsyncBufRead + AsyncWrite> AsyncBufRead for BufWriter<W> {
    async fn fill_buf(&mut self) -> std::io::Result<&'_ [u8]> {
        self.writer.fill_buf().await
    }

    fn consume(&mut self, amount: usize) {
        self.writer.consume(amount)
    }

    fn buffer(&self) -> &[u8] {
        self.writer.buffer()
    }
}
