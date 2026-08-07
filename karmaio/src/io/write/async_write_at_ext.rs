use std::io;

use crate::{
    buf::{BufResult, IoBuf, Slice},
    io::AsyncWriteAt,
};

/// Convenience methods for positional writers.
#[allow(async_fn_in_trait)]
pub trait AsyncWriteAtExt: AsyncWriteAt {
    /// Writes the entire contents of `buf`, starting at `pos`.
    async fn write_all_at<B: IoBuf + 'static>(&mut self, mut buf: B, pos: u64) -> BufResult<(), B> {
        let length = buf.bytes_init();
        let mut written = 0usize;

        while written < length {
            let offset = match checked_offset(pos, written) {
                Ok(offset) => offset,
                Err(error) => return (Err(error), buf),
            };
            let slice = Slice::new(buf, written, length);
            let (result, slice) = self.write_at(slice, offset).await;
            buf = slice.into_inner();

            match result {
                Ok(0) => return (Err(io::Error::from(io::ErrorKind::WriteZero)), buf),
                Ok(bytes) => written += bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return (Err(error), buf),
            }
        }

        (Ok(()), buf)
    }

    /// Writes the entire contents of every buffer, starting at `pos`.
    async fn write_vectored_all_at<B: IoBuf + 'static>(&mut self, mut bufs: Vec<B>, pos: u64) -> BufResult<(), Vec<B>> {
        let length = match total_length(&bufs) {
            Ok(length) => length,
            Err(error) => return (Err(error), bufs),
        };
        let mut written = 0usize;

        while written < length {
            let offset = match checked_offset(pos, written) {
                Ok(offset) => offset,
                Err(error) => return (Err(error), bufs),
            };
            let mut skipped = written;
            let slices = bufs
                .into_iter()
                .map(|buf| {
                    let initialized = buf.bytes_init();
                    let start = skipped.min(initialized);
                    skipped -= start;
                    Slice::new(buf, start, initialized)
                })
                .collect();
            let (result, slices) = self.write_vectored_at(slices, offset).await;
            bufs = slices.into_iter().map(Slice::into_inner).collect();

            match result {
                Ok(0) => return (Err(io::Error::from(io::ErrorKind::WriteZero)), bufs),
                Ok(bytes) => written += bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return (Err(error), bufs),
            }
        }

        (Ok(()), bufs)
    }
}

impl<T: AsyncWriteAt + ?Sized> AsyncWriteAtExt for T {}

fn checked_offset(pos: u64, offset: usize) -> io::Result<u64> {
    let offset =
        u64::try_from(offset).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file offset exceeds u64"))?;
    pos.checked_add(offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file offset overflow"))
}

fn total_length<B: IoBuf>(bufs: &[B]) -> io::Result<usize> {
    bufs.iter().try_fold(0usize, |total, buf| {
        total
            .checked_add(buf.bytes_init())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer length overflow"))
    })
}
