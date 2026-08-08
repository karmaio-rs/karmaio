use crate::{
    buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut},
    io::{AsyncBufRead, AsyncRead, AsyncWrite},
};

/// Adds buffering to an [`AsyncRead`] implementation.
///
/// `BufReader` performs large, infrequent reads and serves smaller reads from
/// an in-memory buffer. It is most useful for repeated small reads from files
/// and network streams, and offers little benefit for large or in-memory reads.
///
/// Dropping a `BufReader` discards buffered contents. Creating multiple
/// readers over the same stream can therefore cause data loss.
pub struct BufReader<R> {
    reader: R,
    buf: Box<[u8]>,
    pos: usize,
    cap: usize,
}

impl<R: AsyncRead> BufReader<R> {
    /// Creates a new `BufReader` with a default buffer capacity.
    /// The default is currently 8 KB, but may change in the future.
    #[inline]
    pub fn new(inner: R) -> Self {
        // TODO: Make this configurable later
        Self::with_capacity(8 * 1024, inner)
    }

    /// Creates a new `BufReader` with the specified buffer capacity.
    #[inline]
    pub fn with_capacity(capacity: usize, reader: R) -> Self {
        Self {
            reader,
            buf: vec![0; capacity].into_boxed_slice(),
            pos: 0,
            cap: 0,
        }
    }

    /// Gets a reference to the underlying reader.
    ///
    /// It is inadvisable to directly read from the underlying reader.
    #[inline]
    pub const fn get_ref(&self) -> &R {
        &self.reader
    }

    /// Gets a mutable reference to the underlying reader.
    ///
    /// It is inadvisable to directly read from the underlying reader.
    #[inline]
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// Consumes this `BufReader`, returning the underlying reader.
    ///
    /// Note that any leftover data in the internal buffer is lost.
    #[inline]
    pub fn into_inner(self) -> R {
        self.reader
    }

    /// Returns a reference to the internally buffered data.
    ///
    /// Unlike [`AsyncBufRead::fill_buf`], this will not attempt to fill the buffer if it is empty.
    #[inline]
    pub fn buffer(&self) -> &[u8] {
        &self.buf[self.pos..self.cap]
    }

    /// Invalidates all data in the internal buffer.
    #[inline]
    fn discard_buffer(&mut self) {
        self.pos = 0;
        self.cap = 0;
    }
}

impl<R: AsyncRead> AsyncRead for BufReader<R> {
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        // If we don't have any buffered data and we're doing a massive read
        // (larger than our internal buffer), bypass our internal buffer
        // entirely.
        let owned_buf = self.buf.as_ref();

        if self.pos == self.cap && buf.as_uninit().len() >= owned_buf.len() {
            self.discard_buffer();
            return self.reader.read(buf).await;
        }

        let rem = match self.fill_buf().await {
            Ok(slice) => slice,
            Err(e) => {
                return BufResult(Err(e), buf);
            }
        };

        let amt = rem.len().min(buf.as_uninit().len());

        unsafe {
            buf.as_uninit()
                .as_mut_ptr()
                .cast::<u8>()
                .copy_from_nonoverlapping(rem.as_ptr(), amt);
            buf.set_len(amt);
        }

        self.consume(amt);

        BufResult(Ok(amt), buf)
    }

    async fn read_vectored<V: IoVectoredBufMut>(&mut self, bufs: V) -> BufResult<usize, V> {
        // For vectored reads, bypass the internal buffer to keep the implementation simple and correct.
        // The buffer is a performance hint for small reads; scattering a large vectored request goes direct.
        self.discard_buffer();
        self.reader.read_vectored(bufs).await
    }
}

impl<R: AsyncRead> AsyncBufRead for BufReader<R> {
    async fn fill_buf(&mut self) -> std::io::Result<&'_ [u8]> {
        if self.pos == self.cap {
            let buf = std::mem::take(&mut self.buf);

            let (res, buf) = self.reader.read(buf).await.into_parts();

            self.buf = buf;

            match res {
                Ok(bytes_read) => {
                    self.pos = 0;
                    self.cap = bytes_read;

                    return Ok(&self.buf.as_ref()[self.pos..self.cap]);
                }
                Err(e) => return Err(e),
            }
        }

        Ok(&self.buf)
    }

    fn consume(&mut self, amount: usize) {
        self.pos = self.cap.min(self.pos + amount);
    }

    fn buffer(&self) -> &[u8] {
        &self.buf[self.pos..self.cap]
    }
}

impl<R: AsyncRead + AsyncWrite> AsyncWrite for BufReader<R> {
    #[inline]
    async fn write<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        self.reader.write(buf).await
    }

    #[inline]
    async fn write_vectored<V: IoVectoredBuf>(&mut self, bufs: V) -> BufResult<usize, V> {
        self.reader.write_vectored(bufs).await
    }

    #[inline]
    async fn flush(&mut self) -> std::io::Result<()> {
        self.reader.flush().await
    }

    #[inline]
    async fn shutdown(&mut self) -> std::io::Result<()> {
        self.reader.shutdown().await
    }
}
