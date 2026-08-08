use crate::{
    buf::{BufResult, IntoInner, IoBufMut, IoBufMutExt, Slice},
    io::AsyncRead,
};

// Generates both big-endian and little-endian async reader methods for the given types.`
macro_rules! reader_trait_impl {
    // One line per type: Type => be_method_name, le_method_name
    ($($t:ty => $be:ident, $le:ident),* $(,)?) => {
        $(
            /// Reads a value encoded in big-endian byte order.
            async fn $be(&mut self) -> std::io::Result<$t> {
                let buf = Box::new([0; std::mem::size_of::<$t>()]);

                let (res, buf) = self.read_exact(buf).await.into_parts();
                res?;

                Ok(<$t>::from_be_bytes(*buf))
            }

            /// Reads a value encoded in little-endian byte order.
            async fn $le(&mut self) -> std::io::Result<$t> {
                let buf = Box::new([0; std::mem::size_of::<$t>()]);

                let (res, buf) = self.read_exact(buf).await.into_parts();
                res?;

                Ok(<$t>::from_le_bytes(*buf))
            }
        )*
    };
}

/// Convenience methods for all [`AsyncRead`] implementations.
///
/// Futures are `!Send` by design in this share-nothing runtime.
#[allow(async_fn_in_trait)]
pub trait AsyncReadExt: AsyncRead {
    /// Reads until the buffer's capacity is filled.
    async fn read_exact<B: IoBufMut + 'static>(&mut self, buf: B) -> BufResult<usize, B>;

    /// Reads into the uninitialized tail of `buf`.
    ///
    /// This is equivalent to reading into a slice spanning the initialized
    /// length through capacity and then restoring the full buffer.
    async fn append<B: IoBufMut + 'static>(&mut self, buf: B) -> BufResult<usize, B>;

    reader_trait_impl! {
        u8   => read_u8,   read_u8_le,
        u16  => read_u16,  read_u16_le,
        u32  => read_u32,  read_u32_le,
        u64  => read_u64,  read_u64_le,
        u128 => read_u128, read_u128_le,
        i8   => read_i8,   read_i8_le,
        i16  => read_i16,  read_i16_le,
        i32  => read_i32,  read_i32_le,
        i64  => read_i64,  read_i64_le,
        i128 => read_i128, read_i128_le,
        f32  => read_f32,  read_f32_le,
        f64  => read_f64,  read_f64_le,
    }
}

impl<T: AsyncRead + ?Sized> AsyncReadExt for T {
    async fn read_exact<B: IoBufMut + 'static>(&mut self, mut buf: B) -> BufResult<usize, B> {
        let buf_capacity = buf.as_uninit().len();
        let mut bytes_read = 0;

        while bytes_read < buf_capacity {
            let buf_slice = Slice::new(buf, bytes_read, buf_capacity);
            let (result, buf_slice) = self.read(buf_slice).await.into_parts();
            buf = buf_slice.into_inner();

            match result {
                Ok(0) => {
                    return BufResult(
                        Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "failed to fill whole buffer",
                        )),
                        buf,
                    );
                }
                Ok(n) => {
                    bytes_read += n;
                    // Safety: successful reads initialized every byte in the
                    // accumulated prefix and the loop never exceeds capacity.
                    unsafe { buf.set_len(bytes_read) };
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return BufResult(Err(e), buf),
            }
        }

        BufResult(Ok(bytes_read), buf)
    }

    async fn append<B: IoBufMut + 'static>(&mut self, mut buf: B) -> BufResult<usize, B> {
        let init = buf.as_init().len();
        let total = buf.as_uninit().len();
        if init >= total {
            return BufResult(Ok(0), buf);
        }
        let uninit = buf.uninit();
        let (res, uninit) = self.read(uninit).await.into_parts();
        BufResult(res, uninit.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future, task::Poll};

    use super::*;

    fn run_ready<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("test future unexpectedly yielded"),
        }
    }

    #[test]
    fn append_reads_into_the_spare_tail() {
        let mut reader = b"de".as_slice();
        let mut buffer = Vec::with_capacity(5);
        buffer.extend_from_slice(b"abc");

        let (result, buffer) = run_ready(reader.append(buffer)).into_parts();

        assert_eq!(result.unwrap(), 2);
        assert_eq!(buffer, b"abcde");
    }
}
