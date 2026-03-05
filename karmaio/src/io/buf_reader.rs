use crate::{
    buf::{BoundedIoBuf, BoundedIoBufMut, BufResult},
    io::{AsyncBufRead, AsyncRead, AsyncWrite},
};

// The `BufReader` struct adds buffering to any reader.
//
// It can be excessively inefficient to work directly with a [`AsyncRead`]
// instance. A `BufReader` performs large, infrequent reads on the underlying
// [`AsyncRead`] and maintains an in-memory buffer of the results.
//
// `BufReader` can improve the speed of programs that make *small* and
// *repeated* read calls to the same file or network socket. It does not
// help when reading very large amounts at once, or reading just one or a few
// times. It also provides no advantage when reading from a source that is
// already in memory, like a `Vec<u8>`.
//
// When the `BufReader` is dropped, the contents of its buffer will be
// discarded. Creating multiple instances of a `BufReader` on the same
// stream can cause data loss.
pub struct BufReader<R> {
    reader: R,
    buf: Box<[u8]>,
    pos: usize,
    cap: usize,
}

impl<R: AsyncRead> BufReader<R> {
    // Creates a new `BufReader` with a default buffer capacity. The default is currently 8 KB,
    // but may change in the future.
    #[inline]
    pub fn new(inner: R) -> Self {
        // TODO: Make this configurable later
        Self::with_capacity(8 * 1024, inner)
    }

    // Creates a new `BufReader` with the specified buffer capacity.
    #[inline]
    pub fn with_capacity(capacity: usize, reader: R) -> Self {
        Self {
            reader,
            buf: vec![0; capacity].into_boxed_slice(),
            pos: 0,
            cap: 0,
        }
    }

    // Gets a reference to the underlying reader.
    //
    // It is inadvisable to directly read from the underlying reader.
    #[inline]
    pub const fn get_ref(&self) -> &R {
        &self.reader
    }

    // Gets a mutable reference to the underlying reader.
    #[inline]
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    // Consumes this `BufReader`, returning the underlying reader.
    //
    // Note that any leftover data in the internal buffer is lost.
    #[inline]
    pub fn into_inner(self) -> R {
        self.reader
    }

    // Returns a reference to the internally buffered data.
    //
    // Unlike `fill_buf`, this will not attempt to fill the buffer if it is empty.
    #[inline]
    pub fn buffer(&self) -> &[u8] {
        &self.buf[self.pos..self.cap]
    }

    // Invalidates all data in the internal buffer.
    #[inline]
    fn discard_buffer(&mut self) {
        self.pos = 0;
        self.cap = 0;
    }
}

impl<R: AsyncRead> AsyncRead for BufReader<R> {
    async fn read<B: BoundedIoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        // If we don't have any buffered data and we're doing a massive read
        // (larger than our internal buffer), bypass our internal buffer
        // entirely.
        let owned_buf = self.buf.as_ref();

        if self.pos == self.cap && buf.bytes_total() >= owned_buf.len() {
            self.discard_buffer();
            return self.reader.read(buf).await;
        }

        let rem = match self.fill_buf().await {
            Ok(slice) => slice,
            Err(e) => {
                return (Err(e), buf);
            }
        };

        let amt = rem.len().min(buf.bytes_total());

        unsafe {
            buf.stable_write_ptr().copy_from_nonoverlapping(rem.as_ptr(), amt);
            buf.set_init(amt);
        }

        self.consume(amt);

        (Ok(amt), buf)
    }
}

impl<R: AsyncRead> AsyncBufRead for BufReader<R> {
    async fn fill_buf(&mut self) -> std::io::Result<&'_ [u8]> {
        if self.pos == self.cap {
            let buf = std::mem::take(&mut self.buf);

            let (res, buf) = self.reader.read(buf).await;

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
    async fn write<T: BoundedIoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        self.reader.write(buf).await
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
