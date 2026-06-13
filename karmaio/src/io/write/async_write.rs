use std::{
    io::{Cursor, Result, Write},
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
pub trait AsyncWrite {
    // Writes the contents of a buffer into this writer, returning the number of bytes written.
    //
    // This function attempts to write the entire buffer `buf`, but the write may not fully
    // succeed, and it might also result in an error. A call to `write` represents *at most one*
    // attempt to write to the underlying object.
    async fn write<B: BoundedIoBuf>(&mut self, buf: B) -> BufResult<usize, B>;

    /// Writes the contents of multiple buffers into this writer (vectored / gather).
    /// Ownership of the `bufs` vector and its buffers is transferred; the same vector is returned on completion.
    async fn write_vectored<B: BoundedIoBuf>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>>;

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
pub trait AsyncWriteAt {
    // Like [`AsyncWrite::write`], except that it writes at a specified position.
    async fn write_at<B: BoundedIoBuf>(&mut self, buf: B, pos: u64) -> BufResult<usize, B>;

    /// Like [`AsyncWrite::writev`], except that it writes at a specified position (vectored).
    async fn write_vectored_at<B: BoundedIoBuf>(&mut self, bufs: Vec<B>, pos: u64) -> BufResult<usize, Vec<B>>;
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

impl<W> AsyncWrite for Cursor<W>
where
    Cursor<W>: Write + Unpin,
{
    async fn write<B: BoundedIoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let bytes_to_write = buf.bytes_init();

        if bytes_to_write == 0 {
            return (Ok(0), buf);
        }

        let res = unsafe {
            let slice = slice::from_raw_parts(buf.stable_read_ptr(), bytes_to_write);
            Write::write(self, slice)
        };
        match res {
            Ok(n) => (Ok(n), buf),
            Err(e) => (Err(e), buf),
        }
    }

    async fn write_vectored<B: BoundedIoBuf>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>> {
        let mut total = 0usize;
        for b in bufs.iter() {
            let n = b.bytes_init();
            if n > 0 {
                let res = unsafe {
                    let slice = slice::from_raw_parts(b.stable_read_ptr(), n);
                    Write::write(self, slice)
                };
                match res {
                    Ok(w) => total += w,
                    Err(e) => return (Err(e), bufs),
                }
            }
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
