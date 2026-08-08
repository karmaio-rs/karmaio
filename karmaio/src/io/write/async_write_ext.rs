use crate::{
    buf::{BufResult, IoBuf, Slice},
    io::AsyncWrite,
};

// Generates both big-endian and little-endian async reader methods for the given types.`
macro_rules! writer_trait_impl {
    // One line per type: Type => be_method_name, le_method_name
    ($($t:ty => $be:ident, $le:ident),* $(,)?) => {
        $(
            /// Writes a value encoded in big-endian byte order.
            async fn $be(&mut self) -> std::io::Result<usize> {
                let buf = Box::new([0; std::mem::size_of::<$t>()]);

                let (res, _) = self.write_all(buf).await.into_parts();
                res
            }

            /// Writes a value encoded in little-endian byte order.
            async fn $le(&mut self) -> std::io::Result<usize> {
                let buf = Box::new([0; std::mem::size_of::<$t>()]);

                let (res, _) = self.write_all(buf).await.into_parts();
                res
            }
        )*
    };
}

/// Convenience methods for all [`AsyncWrite`] implementations.
///
/// Futures are `!Send` by design in this share-nothing runtime.
#[allow(async_fn_in_trait)]
pub trait AsyncWriteExt: AsyncWrite {
    /// Writes all initialized bytes in the buffer.
    async fn write_all<B: IoBuf + 'static>(&mut self, buf: B) -> BufResult<usize, B>;

    writer_trait_impl! {
        u8   => write_u8,   write_u8_le,
        u16  => write_u16,  write_u16_le,
        u32  => write_u32,  write_u32_le,
        u64  => write_u64,  write_u64_le,
        u128 => write_u128, write_u128_le,
        i8   => write_i8,   write_i8_le,
        i16  => write_i16,  write_i16_le,
        i32  => write_i32,  write_i32_le,
        i64  => write_i64,  write_i64_le,
        i128 => write_i128, write_i128_le,
        f32  => write_f32,  write_f32_le,
        f64  => write_f64,  write_f64_le,
    }
}

impl<T: AsyncWrite + ?Sized> AsyncWriteExt for T {
    async fn write_all<B: IoBuf + 'static>(&mut self, mut buf: B) -> BufResult<usize, B> {
        let bytes_to_write = buf.as_init().len();
        let mut bytes_written = 0;

        while bytes_written < bytes_to_write {
            let buf_slice = Slice::new(buf, bytes_written, bytes_to_write);
            let (result, buf_slice) = self.write(buf_slice).await.into_parts();
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
                Ok(n) => bytes_written += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return BufResult(Err(e), buf),
            }
        }

        BufResult(Ok(bytes_written), buf)
    }
}
