use std::{io::Cursor, rc::Rc, sync::Arc};

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
    ///
    /// The default implementation reads into each buffer sequentially via [`AsyncRead::read`].
    /// Types with native scatter-gather support (e.g. io_uring `readv`) should override this.
    async fn read_vectored<B: BoundedIoBufMut>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>> {
        let mut total = 0usize;
        let mut returned: Vec<B> = Vec::with_capacity(bufs.len());
        let mut remaining = bufs.into_iter();
        while let Some(buf) = remaining.next() {
            let cap = buf.bytes_total();
            let (res, buf) = self.read(buf).await;
            match res {
                Ok(n) => {
                    total += n;
                    returned.push(buf);
                    if n < cap {
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
    async fn read_at<B: BoundedIoBufMut>(&self, buf: B, pos: u64) -> BufResult<usize, B>;

    /// Like [`AsyncRead::read_vectored`], except that it reads at a specified position (vectored).
    ///
    /// The default implementation reads into each buffer sequentially via [`AsyncReadAt::read_at`].
    /// Types with native scatter-gather support (e.g. io_uring `readv`, `preadv`) should override this.
    async fn read_vectored_at<B: BoundedIoBufMut>(&self, bufs: Vec<B>, pos: u64) -> BufResult<usize, Vec<B>> {
        let mut total = 0usize;
        let mut offset = pos;
        let mut returned: Vec<B> = Vec::with_capacity(bufs.len());
        let mut remaining = bufs.into_iter();
        while let Some(buf) = remaining.next() {
            let cap = buf.bytes_total();
            let (res, buf) = self.read_at(buf, offset).await;
            match res {
                Ok(n) => {
                    total += n;
                    offset += n as u64;
                    returned.push(buf);
                    if n < cap {
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
    async fn read_at<B: BoundedIoBufMut>(&self, buf: B, pos: u64) -> BufResult<usize, B> {
        (**self).read_at(buf, pos).await
    }

    #[inline]
    async fn read_vectored_at<B: BoundedIoBufMut>(&self, bufs: Vec<B>, pos: u64) -> BufResult<usize, Vec<B>> {
        (**self).read_vectored_at(bufs, pos).await
    }
}

macro_rules! delegate_read_at {
    ($($ty:ty),* $(,)?) => {
        $(
            impl<T: ?Sized + AsyncReadAt> AsyncReadAt for $ty {
                #[inline]
                async fn read_at<B: BoundedIoBufMut>(&self, buf: B, pos: u64) -> BufResult<usize, B> {
                    (**self).read_at(buf, pos).await
                }

                #[inline]
                async fn read_vectored_at<B: BoundedIoBufMut>(
                    &self,
                    bufs: Vec<B>,
                    pos: u64,
                ) -> BufResult<usize, Vec<B>> {
                    (**self).read_vectored_at(bufs, pos).await
                }
            }
        )*
    };
}

delegate_read_at!(&T, Box<T>, Rc<T>, Arc<T>);

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

impl AsyncReadAt for [u8] {
    async fn read_at<B: BoundedIoBufMut>(&self, mut buf: B, pos: u64) -> BufResult<usize, B> {
        let pos = pos.min(self.len() as u64) as usize;
        let bytes_to_read = (self.len() - pos).min(buf.bytes_total());

        unsafe {
            buf.stable_write_ptr()
                .copy_from_nonoverlapping(self.as_ptr().add(pos), bytes_to_read);
            buf.set_init(bytes_to_read);
        }

        (Ok(bytes_to_read), buf)
    }
}

impl<const N: usize> AsyncReadAt for [u8; N] {
    #[inline]
    async fn read_at<B: BoundedIoBufMut>(&self, buf: B, pos: u64) -> BufResult<usize, B> {
        self.as_slice().read_at(buf, pos).await
    }
}

impl AsyncReadAt for Vec<u8> {
    #[inline]
    async fn read_at<B: BoundedIoBufMut>(&self, buf: B, pos: u64) -> BufResult<usize, B> {
        self.as_slice().read_at(buf, pos).await
    }
}

impl<T: AsyncReadAt> AsyncRead for Cursor<T> {
    async fn read<B: BoundedIoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        let pos = self.position();
        let (res, buf) = self.get_ref().read_at(buf, pos).await;
        if let Ok(n) = res {
            self.set_position(pos + n as u64);
        }
        (res, buf)
    }

    async fn read_vectored<B: BoundedIoBufMut>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>> {
        let pos = self.position();
        let (res, bufs) = self.get_ref().read_vectored_at(bufs, pos).await;
        if let Ok(n) = res {
            self.set_position(pos + n as u64);
        }
        (res, bufs)
    }
}
