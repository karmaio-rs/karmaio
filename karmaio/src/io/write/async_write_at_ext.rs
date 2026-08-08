use std::io;

use crate::{
    buf::{BufResult, IoBuf, IoVectoredBuf, Slice},
    io::AsyncWriteAt,
};

/// Convenience methods for positional writers.
#[allow(async_fn_in_trait)]
pub trait AsyncWriteAtExt: AsyncWriteAt {
    /// Writes the entire contents of `buf`, starting at `pos`.
    async fn write_all_at<B: IoBuf + 'static>(&mut self, mut buf: B, pos: u64) -> BufResult<(), B> {
        let length = buf.as_init().len();
        let mut written = 0usize;

        while written < length {
            let offset = match checked_offset(pos, written) {
                Ok(offset) => offset,
                Err(error) => return BufResult(Err(error), buf),
            };
            let slice = Slice::new(buf, written, length);
            let (result, slice) = self.write_at(slice, offset).await.into_parts();
            buf = slice.into_inner();

            match result {
                Ok(0) => return BufResult(Err(io::Error::from(io::ErrorKind::WriteZero)), buf),
                Ok(bytes) => written += bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return BufResult(Err(error), buf),
            }
        }

        BufResult(Ok(()), buf)
    }

    /// Writes the entire contents of every buffer, starting at `pos`.
    async fn write_vectored_all_at<V: IoVectoredBuf>(&mut self, mut bufs: V, pos: u64) -> BufResult<(), V> {
        let length_result = {
            bufs.iter_slice().try_fold(0usize, |total, buf| {
                total
                    .checked_add(buf.len())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer length overflow"))
            })
        };
        let length = match length_result {
            Ok(length) => length,
            Err(error) => return BufResult(Err(error), bufs),
        };
        let mut written = 0usize;

        while written < length {
            let offset = match checked_offset(pos, written) {
                Ok(offset) => offset,
                Err(error) => return BufResult(Err(error), bufs),
            };
            let view = IoVectoredBuf::slice(bufs, written);
            let (result, view) = self.write_vectored_at(view, offset).await.into_parts();
            bufs = view.into_inner();

            match result {
                Ok(0) => return BufResult(Err(io::Error::from(io::ErrorKind::WriteZero)), bufs),
                Ok(bytes) => written += bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return BufResult(Err(error), bufs),
            }
        }

        BufResult(Ok(()), bufs)
    }
}

impl<T: AsyncWriteAt + ?Sized> AsyncWriteAtExt for T {}

fn checked_offset(pos: u64, offset: usize) -> io::Result<u64> {
    let offset =
        u64::try_from(offset).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file offset exceeds u64"))?;
    pos.checked_add(offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file offset overflow"))
}
