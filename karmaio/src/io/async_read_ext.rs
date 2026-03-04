use crate::{
    buf::{BufResult, IoBufMut, Slice},
    io::AsyncRead,
};

// Generates both big-endian and little-endian async reader methods for the given types.`
macro_rules! reader_trait_impl {
    // One line per type: Type => be_method_name, le_method_name
    ($($t:ty => $be:ident, $le:ident),* $(,)?) => {
        $(
            // Big Endian
            async fn $be(&mut self) -> std::io::Result<$t> {
                let buf = Box::new([0; std::mem::size_of::<$t>()]);

                let (res, buf) = self.read_exact(buf).await;
                res?;

                Ok(<$t>::from_be_bytes(*buf))
            }

            // Little Endian
            async fn $le(&mut self) -> std::io::Result<$t> {
                let buf = Box::new([0; std::mem::size_of::<$t>()]);

                let (res, buf) = self.read_exact(buf).await;
                res?;

                Ok(<$t>::from_le_bytes(*buf))
            }
        )*
    };
}

// Implemented as an extension trait, adding utility methods to all [`AsyncRead`] types.
// Callers will tend to import this trait instead of [`AsyncRead`].
pub trait AsyncReadExt: AsyncRead {
    // Read until buf capacity is fulfilled
    async fn read_exact<B: IoBufMut + 'static>(&mut self, buf: B) -> BufResult<usize, B>;

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
        let buf_capacity = buf.bytes_total();
        let mut bytes_read = 0;

        while bytes_read < buf_capacity {
            let buf_slice = Slice::new(buf, bytes_read, buf_capacity);
            let (result, buf_slice) = self.read(buf_slice).await;
            buf = buf_slice.into_inner();

            match result {
                Ok(0) => {
                    return (
                        Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "failed to fill whole buffer",
                        )),
                        buf,
                    );
                }
                Ok(n) => {
                    bytes_read += n;
                    buf.set_init(bytes_read);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return (Err(e), buf),
            }
        }

        (Ok(bytes_read), buf)
    }
}
