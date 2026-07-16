use std::io::Cursor;

use crate::buf::{BoundedIoBufMut, BufResult};

// The AsyncRead trait defines asynchronous reading operations for objects that implement it.
//
// It provides a way to read bytes from a source into a buffer asynchronously.
//
// Types that implement this trait are expected to manage asynchronous read operations,
// allowing them to interact with other asynchronous tasks without blocking the executor.
// Share-nothing runtime: futures are `!Send` by design, so `async fn` in traits is intentional.
#[allow(async_fn_in_trait)]
pub trait AsyncRead {
    // Reads bytes into the rented mutable buffer.
    // Ownership of `buf` is transferred to the operation; the same buffer is returned on completion.
    // Implementors **must** update the buffer's initialized length via `BoundedIoBufMut::set_len
    async fn read<B: BoundedIoBufMut>(&mut self, buf: B) -> BufResult<usize, B>;

    /// Reads bytes into the rented mutable buffers (vectored / scatter-gather).
    /// Ownership of the `bufs` vector and its buffers is transferred to the operation;
    /// the same vector of buffers is returned on completion.
    /// Implementors should fill buffers in order.
    async fn read_vectored<B: BoundedIoBufMut>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>>;
}

// The AsyncReadAt trait defines asynchronous reading operations for objects that implement it.
//
// It provides a way to read bytes from a source into a buffer asynchronously from a specified offset.
//
// Types that implement this trait are expected to manage asynchronous read operations,
// allowing them to interact with other asynchronous tasks without blocking the executor.
// Share-nothing runtime: futures are `!Send` by design, so `async fn` in traits is intentional.
#[allow(async_fn_in_trait)]
pub trait AsyncReadAt {
    // Like [`AsyncRead::read`], except that it reads at a specified position.
    async fn read_at<B: BoundedIoBufMut>(&mut self, buf: B, pos: u64) -> BufResult<usize, B>;

    /// Like [`AsyncRead::readv`], except that it reads at a specified position (vectored).
    async fn read_vectored_at<B: BoundedIoBufMut>(&mut self, bufs: Vec<B>, pos: u64) -> BufResult<usize, Vec<B>>;
}

impl<T: ?Sized + AsyncRead> AsyncRead for &mut T {
    #[inline]
    async fn read<B: BoundedIoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        (**self).read(buf).await
    }

    #[inline]
    async fn read_vectored<B: BoundedIoBufMut>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>> {
        (**self).read_vectored(bufs).await
    }
}

impl<T: ?Sized + AsyncReadAt> AsyncReadAt for &mut T {
    #[inline]
    async fn read_at<B: BoundedIoBufMut>(&mut self, buf: B, pos: u64) -> BufResult<usize, B> {
        (**self).read_at(buf, pos).await
    }

    #[inline]
    async fn read_vectored_at<B: BoundedIoBufMut>(&mut self, bufs: Vec<B>, pos: u64) -> BufResult<usize, Vec<B>> {
        (**self).read_vectored_at(bufs, pos).await
    }
}

impl AsyncRead for &[u8] {
    async fn read<B: BoundedIoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        let bytes_to_read = self.len().min(buf.bytes_total());

        unsafe {
            let dst = buf.stable_write_ptr();
            dst.copy_from_nonoverlapping(self.as_ptr(), bytes_to_read);
            buf.set_init(bytes_to_read);
        }
        *self = &self[bytes_to_read..];

        (Ok(bytes_to_read), buf)
    }

    async fn read_vectored<B: BoundedIoBufMut>(&mut self, mut bufs: Vec<B>) -> BufResult<usize, Vec<B>> {
        let mut total = 0usize;
        for buf in bufs.iter_mut() {
            let remaining = self.len();
            if remaining == 0 {
                break;
            }
            let space = buf.bytes_total();
            let n = remaining.min(space);
            if n > 0 {
                unsafe {
                    let dst = buf.stable_write_ptr();
                    dst.copy_from_nonoverlapping(self.as_ptr(), n);
                    buf.set_init(n);
                }
                *self = &self[n..];
                total += n;
            }
        }
        (Ok(total), bufs)
    }
}

impl<T: AsRef<[u8]>> AsyncRead for Cursor<T> {
    async fn read<B: BoundedIoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        let pos = self.position();
        let slice: &[u8] = (*self).get_ref().as_ref();

        if pos > slice.len() as u64 {
            return (Ok(0), buf);
        }

        let (res, buf) = (&slice[pos as usize..]).read(buf).await;
        if let Ok(n) = res {
            self.set_position(pos + n as u64);
        }
        (res, buf)
    }

    async fn read_vectored<B: BoundedIoBufMut>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>> {
        let pos = self.position();
        let slice: &[u8] = (*self).get_ref().as_ref();

        if pos > slice.len() as u64 {
            return (Ok(0), bufs);
        }

        let (res, bufs) = (&slice[pos as usize..]).read_vectored(bufs).await;
        if let Ok(n) = res {
            self.set_position(pos + n as u64);
        }
        (res, bufs)
    }
}
