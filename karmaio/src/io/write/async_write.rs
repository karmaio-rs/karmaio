use std::{
    io::{Cursor, Result},
    slice,
};

use crate::buf::{BoundedIoBuf, BufResult};

// The `AsyncWrite` trait provides asynchronous writing capabilities for structs that implement it.
//
// It abstracts over the concept of writing bytes asynchronously to an underlying I/O object.
// The trait also encompasses the ability to flush buffered data and to shut down the output stream cleanly.
//
// Types implementing this trait are required to manage asynchronous I/O operations, allowing for non-blocking writes.
// This is particularly useful in scenarios where the object might need to interact
// with other asynchronous tasks without blocking the executor.
// Share-nothing runtime: futures are `!Send` by design, so `async fn` in traits is intentional.
#[allow(async_fn_in_trait)]
pub trait AsyncWrite {
    // Writes the contents of a buffer into this writer, returning the number of bytes written.
    //
    // This function attempts to write the entire buffer `buf`, but the write may not fully
    // succeed, and it might also result in an error. A call to `write` represents *at most one*
    // attempt to write to the underlying object.
    async fn write<B: BoundedIoBuf>(&mut self, buf: B) -> BufResult<usize, B>;

    /// Writes the contents of multiple buffers into this writer (vectored / gather).
    /// Ownership of the `bufs` vector and its buffers is transferred; the same vector is returned on completion.
    ///
    /// The default implementation writes each buffer sequentially via [`AsyncWrite::write`].
    /// Types with native gather support (e.g. io_uring `writev`) should override this.
    async fn write_vectored<B: BoundedIoBuf>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>> {
        let mut total = 0usize;
        let mut returned: Vec<B> = Vec::with_capacity(bufs.len());
        let mut remaining = bufs.into_iter();
        while let Some(buf) = remaining.next() {
            let init = buf.bytes_init();
            let (res, buf) = self.write(buf).await;
            match res {
                Ok(n) => {
                    total += n;
                    returned.push(buf);
                    if n < init {
                        returned.extend(remaining);
                        break;
                    }
                }
                Err(e) => {
                    returned.push(buf);
                    returned.extend(remaining);
                    return (Err(e), returned);
                }
            }
        }
        (Ok(total), returned)
    }

    // Flushes this output stream, ensuring that all buffered content is successfully written to its destination.
    async fn flush(&mut self) -> Result<()>;

    // Shuts down the output stream, ensuring that the value can be cleanly dropped.
    //
    // Similar to [`flush`], all buffered data is written to the underlying stream.
    // After this operation completes, the caller should no longer attempt to write to the stream.
    async fn shutdown(&mut self) -> Result<()>;
}

// The `AsyncWriteAt` trait provides asynchronous writing capabilities for structs that implement it.
//
// It abstracts over the concept of writing bytes asynchronously to an underlying I/O object from a specified position.
//
// Types implementing this trait are required to manage asynchronous I/O operations, allowing for non-blocking writes.
// This is particularly useful in scenarios where the object might need to interact
// with other asynchronous tasks without blocking the executor.
// Share-nothing runtime: futures are `!Send` by design, so `async fn` in traits is intentional.
#[allow(async_fn_in_trait)]
pub trait AsyncWriteAt {
    // Like [`AsyncWrite::write`], except that it writes at a specified position.
    async fn write_at<B: BoundedIoBuf>(&mut self, buf: B, pos: u64) -> BufResult<usize, B>;

    /// Like [`AsyncWrite::write_vectored`], except that it writes at a specified position (vectored).
    ///
    /// The default implementation writes each buffer sequentially via [`AsyncWriteAt::write_at`].
    /// Types with native gather support (e.g. io_uring `writev`, `pwritev`) should override this.
    async fn write_vectored_at<B: BoundedIoBuf>(&mut self, bufs: Vec<B>, pos: u64) -> BufResult<usize, Vec<B>> {
        let mut total = 0usize;
        let mut offset = pos;
        let mut returned: Vec<B> = Vec::with_capacity(bufs.len());
        let mut remaining = bufs.into_iter();
        while let Some(buf) = remaining.next() {
            let init = buf.bytes_init();
            let (res, buf) = self.write_at(buf, offset).await;
            match res {
                Ok(n) => {
                    total += n;
                    offset += n as u64;
                    returned.push(buf);
                    if n < init {
                        returned.extend(remaining);
                        break;
                    }
                }
                Err(e) => {
                    returned.push(buf);
                    returned.extend(remaining);
                    return (Err(e), returned);
                }
            }
        }
        (Ok(total), returned)
    }
}

impl<T: ?Sized + AsyncWriteAt> AsyncWriteAt for &mut T {
    #[inline]
    fn write_at<B: BoundedIoBuf>(&mut self, buf: B, pos: u64) -> impl Future<Output = BufResult<usize, B>> {
        (**self).write_at(buf, pos)
    }

    #[inline]
    fn write_vectored_at<B: BoundedIoBuf>(
        &mut self,
        bufs: Vec<B>,
        pos: u64,
    ) -> impl Future<Output = BufResult<usize, Vec<B>>> {
        (**self).write_vectored_at(bufs, pos)
    }
}

impl<T: ?Sized + AsyncWriteAt> AsyncWriteAt for Box<T> {
    #[inline]
    fn write_at<B: BoundedIoBuf>(&mut self, buf: B, pos: u64) -> impl Future<Output = BufResult<usize, B>> {
        (**self).write_at(buf, pos)
    }

    #[inline]
    fn write_vectored_at<B: BoundedIoBuf>(
        &mut self,
        bufs: Vec<B>,
        pos: u64,
    ) -> impl Future<Output = BufResult<usize, Vec<B>>> {
        (**self).write_vectored_at(bufs, pos)
    }
}

impl<T: ?Sized + AsyncWrite> AsyncWrite for &mut T {
    #[inline]
    fn write<B: BoundedIoBuf>(&mut self, buf: B) -> impl Future<Output = BufResult<usize, B>> {
        (**self).write(buf)
    }

    #[inline]
    fn write_vectored<B: BoundedIoBuf>(&mut self, bufs: Vec<B>) -> impl Future<Output = BufResult<usize, Vec<B>>> {
        (**self).write_vectored(bufs)
    }

    #[inline]
    fn flush(&mut self) -> impl Future<Output = std::io::Result<()>> {
        (**self).flush()
    }

    #[inline]
    fn shutdown(&mut self) -> impl Future<Output = std::io::Result<()>> {
        (**self).shutdown()
    }
}

impl<T: ?Sized + AsyncWrite + Unpin> AsyncWrite for Box<T> {
    #[inline]
    fn write<B: BoundedIoBuf>(&mut self, buf: B) -> impl Future<Output = BufResult<usize, B>> {
        (**self).write(buf)
    }

    #[inline]
    fn write_vectored<B: BoundedIoBuf>(&mut self, bufs: Vec<B>) -> impl Future<Output = BufResult<usize, Vec<B>>> {
        (**self).write_vectored(bufs)
    }

    #[inline]
    fn flush(&mut self) -> impl Future<Output = std::io::Result<()>> {
        (**self).flush()
    }

    #[inline]
    fn shutdown(&mut self) -> impl Future<Output = std::io::Result<()>> {
        (**self).shutdown()
    }
}

impl AsyncWrite for Vec<u8> {
    async fn write<B: BoundedIoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let bytes_to_write = buf.bytes_init();

        if bytes_to_write > 0 {
            unsafe {
                let slice = slice::from_raw_parts(buf.stable_read_ptr(), bytes_to_write);
                self.extend_from_slice(slice);
            }
        }

        (Ok(bytes_to_write), buf)
    }

    async fn write_vectored<B: BoundedIoBuf>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>> {
        let mut total = 0usize;
        for buf in bufs.iter() {
            let n = buf.bytes_init();
            if n > 0 {
                unsafe {
                    let slice = slice::from_raw_parts(buf.stable_read_ptr(), n);
                    self.extend_from_slice(slice);
                }
            }
            total += n;
        }
        (Ok(total), bufs)
    }

    async fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}

impl AsyncWriteAt for [u8] {
    async fn write_at<B: BoundedIoBuf>(&mut self, buf: B, pos: u64) -> BufResult<usize, B> {
        let pos = usize::try_from(pos).unwrap_or(usize::MAX).min(self.len());
        let bytes_to_write = buf.bytes_init().min(self.len() - pos);

        if bytes_to_write > 0 {
            unsafe {
                self[pos..]
                    .as_mut_ptr()
                    .copy_from_nonoverlapping(buf.stable_read_ptr(), bytes_to_write);
            }
        }

        (Ok(bytes_to_write), buf)
    }
}

impl<const N: usize> AsyncWriteAt for [u8; N] {
    #[inline]
    async fn write_at<B: BoundedIoBuf>(&mut self, buf: B, pos: u64) -> BufResult<usize, B> {
        self.as_mut_slice().write_at(buf, pos).await
    }
}

impl AsyncWriteAt for Vec<u8> {
    async fn write_at<B: BoundedIoBuf>(&mut self, buf: B, pos: u64) -> BufResult<usize, B> {
        let pos = match usize::try_from(pos) {
            Ok(pos) => pos,
            Err(_) => {
                return (
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "file offset exceeds usize",
                    )),
                    buf,
                );
            }
        };
        let bytes_to_write = buf.bytes_init();
        if bytes_to_write == 0 {
            return (Ok(0), buf);
        }
        if pos.checked_add(bytes_to_write).is_none() {
            return (
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "file offset overflow",
                )),
                buf,
            );
        }

        let bytes = unsafe { slice::from_raw_parts(buf.stable_read_ptr(), bytes_to_write) };
        if pos > self.len() {
            self.resize(pos, 0);
        }
        let overwrite = bytes_to_write.min(self.len() - pos);
        self[pos..pos + overwrite].copy_from_slice(&bytes[..overwrite]);
        self.extend_from_slice(&bytes[overwrite..]);

        (Ok(bytes_to_write), buf)
    }
}

impl<W: AsyncWriteAt> AsyncWrite for Cursor<W> {
    async fn write<B: BoundedIoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let pos = self.position();
        let (result, buf) = self.get_mut().write_at(buf, pos).await;
        if let Ok(written) = result {
            self.set_position(pos + written as u64);
        }
        (result, buf)
    }

    async fn write_vectored<B: BoundedIoBuf>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>> {
        let pos = self.position();
        let (result, bufs) = self.get_mut().write_vectored_at(bufs, pos).await;
        if let Ok(written) = result {
            self.set_position(pos + written as u64);
        }
        (result, bufs)
    }

    async fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}
