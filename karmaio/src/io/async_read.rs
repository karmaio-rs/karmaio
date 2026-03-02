use std::io::Cursor;

use crate::buf::{BoundedIoBufMut, BufResult};

// The AsyncRead trait defines asynchronous reading operations for objects that implement it.
//
// It provides a way to read bytes from a source into a buffer asynchronously.
//
// Types that implement this trait are expected to manage asynchronous read operations,
// allowing them to interact with other asynchronous tasks without blocking the executor.
pub trait AsyncRead {
    // Reads bytes into the rented mutable buffer.
    // Ownership of `buf` is transferred to the operation; the same buffer is returned on completion.
    // Implementors **must** update the buffer's initialized length via `BoundedIoBufMut::set_len
    async fn read<B: BoundedIoBufMut>(&mut self, buf: B) -> BufResult<usize, B>;
}

// The AsyncReadAt trait defines asynchronous reading operations for objects that implement it.
//
// It provides a way to read bytes from a source into a buffer asynchronously from a specified offset.
//
// Types that implement this trait are expected to manage asynchronous read operations,
// allowing them to interact with other asynchronous tasks without blocking the executor.
pub trait AsyncReadAt {
    // Like [`AsyncRead::read`], except that it reads at a specified position.
    async fn read_at<B: BoundedIoBufMut>(&mut self, buf: B, pos: usize) -> BufResult<usize, B>;
}

impl<T: ?Sized + AsyncRead> AsyncRead for &mut T {
    #[inline]
    fn read<B: BoundedIoBufMut>(&mut self, buf: B) -> impl Future<Output = BufResult<usize, B>> {
        (**self).read(buf)
    }
}

impl<T: ?Sized + AsyncReadAt> AsyncReadAt for &mut T {
    #[inline]
    fn read_at<B: BoundedIoBufMut>(&mut self, buf: B, pos: usize) -> impl Future<Output = BufResult<usize, B>> {
        (**self).read_at(buf, pos)
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
}

impl<T: AsRef<[u8]>> AsyncRead for Cursor<T> {
    async fn read<B: BoundedIoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        let pos = self.position();
        let slice: &[u8] = (*self).get_ref().as_ref();

        if pos > slice.len() as u64 {
            return (Ok(0), buf);
        }

        (&slice[pos as usize..]).read(buf).await
    }
}
