use crate::{
    buf::{BoundedIoBufMut, BufResult},
    io::{AsyncBufRead, AsyncRead, AsyncWrite, AsyncWriteExt},
};

// Wraps a writer and buffers its output.
//
// It can be excessively inefficient to work directly with something that implements [`AsyncWrite`].
// A `BufWriter` keeps an in-memory buffer of data
// and writes it to an underlying writer in large, infrequent batches.
//
// `BufWriter` can improve the speed of programs that make *small* and
// *repeated* write calls to the same file or network socket.
// It does not help when writing very large amounts at once, or writing just one or a few times.
// It also provides no advantage when writing to a destination that is in memory, like a `Vec<u8>`.
//
// When the `BufWriter` is dropped, the contents of its buffer will be discarded.
// Creating multiple instances of a `BufWriter` on the same stream can cause data loss.
// If you need to write out the contents of its buffer,
// you must manually call flush before the writer is dropped.
pub struct BufWriter<W> {
    writer: W,
    buf: Box<[u8]>,
    written: usize,
}

impl<W> BufWriter<W> {
    // Creates a new `BufReader` with a default buffer capacity. The default is currently 8 KB,
    // but may change in the future.
    #[inline]
    pub fn new(inner: W) -> Self {
        // TODO: Make this configurable later
        Self::with_capacity(8 * 1024, inner)
    }

    // Create BufWriter with given buffer size
    #[inline]
    pub fn with_capacity(capacity: usize, writer: W) -> Self {
        let buffer = vec![0; capacity];
        Self {
            writer,
            buf: buffer.into_boxed_slice(),
            written: 0,
        }
    }

    // Gets a reference to the underlying writer.
    #[inline]
    pub fn get_ref(&self) -> &W {
        &self.writer
    }

    // Gets a mutable reference to the underlying writer.
    #[inline]
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    // Consumes this `BufWriter`, returning the underlying writer.
    //
    // Note that any leftover data in the internal buffer is lost.
    #[inline]
    pub fn into_inner(self) -> W {
        self.writer
    }

    // Returns a reference to the internally buffered data.
    #[inline]
    pub fn buffer(&self) -> &[u8] {
        &self.buf.as_ref()[..self.written]
    }

    // Invalidates all data in the internal buffer.
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

            let (ret, buf) = self.writer.write_all(buf).await;

            // move it back and return
            self.buf = buf;
            ret?;

            self.discard_buffer();
        }
        Ok(())
    }
}

impl<W: AsyncWrite> AsyncWrite for BufWriter<W> {
    async fn write<B: crate::buf::BoundedIoBuf>(&mut self, buf: B) -> crate::buf::BufResult<usize, B> {
        let writer_buf = self.buf.as_ref();
        let writer_buf_written = writer_buf.len();
        let bytes_to_write = buf.bytes_init();

        // Buf can not be copied directly into OwnedBuf,
        // we must flush OwnedBuf first.
        if self.written + bytes_to_write > writer_buf_written {
            match self.flush_buf().await {
                Ok(_) => (),
                Err(e) => {
                    return (Err(e), buf);
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
                    .copy_from_nonoverlapping(buf.stable_read_ptr(), bytes_to_write);
            }
            self.written += bytes_to_write;
            return (Ok(bytes_to_write), buf);
        }
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
    async fn read<B: BoundedIoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.writer.read(buf).await
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
