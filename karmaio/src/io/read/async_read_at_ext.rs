use std::io;

use crate::{
    buf::{BufResult, IoBufMut, Slice},
    io::AsyncReadAt,
};

const READ_TO_END_CHUNK_SIZE: usize = 8192;

/// Convenience methods for positional readers.
#[allow(async_fn_in_trait)]
pub trait AsyncReadAtExt: AsyncReadAt {
    /// Reads the exact number of bytes required to fill `buf`, starting at
    /// `pos`.
    ///
    /// An [`io::ErrorKind::UnexpectedEof`] error is returned if the source ends
    /// before the buffer is full.
    async fn read_exact_at<B: IoBufMut + 'static>(&self, mut buf: B, pos: u64) -> BufResult<(), B> {
        let capacity = buf.bytes_total();
        let mut read = 0usize;

        while read < capacity {
            let offset = match checked_offset(pos, read) {
                Ok(offset) => offset,
                Err(error) => return (Err(error), buf),
            };
            let slice = Slice::new(buf, read, capacity);
            let (result, slice) = self.read_at(slice, offset).await;
            buf = slice.into_inner();

            match result {
                Ok(0) => {
                    return (
                        Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "failed to fill whole buffer",
                        )),
                        buf,
                    );
                }
                Ok(bytes) => read += bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return (Err(error), buf),
            }
        }

        (Ok(()), buf)
    }

    /// Reads all bytes from `pos` to EOF, appending them to `buf`.
    ///
    /// On success, the returned count is the number of bytes appended.
    async fn read_to_end_at(&self, mut buf: Vec<u8>, pos: u64) -> BufResult<usize, Vec<u8>> {
        let mut read = 0usize;

        loop {
            if buf.len() == buf.capacity() {
                buf.reserve(READ_TO_END_CHUNK_SIZE);
            }

            let start = buf.len();
            let capacity = buf.capacity();
            let offset = match checked_offset(pos, read) {
                Ok(offset) => offset,
                Err(error) => return (Err(error), buf),
            };
            let slice = Slice::new(buf, start, capacity);
            let (result, slice) = self.read_at(slice, offset).await;
            buf = slice.into_inner();

            match result {
                Ok(0) => return (Ok(read), buf),
                Ok(bytes) => read += bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return (Err(error), buf),
            }
        }
    }

    /// Reads all UTF-8 bytes from `pos` to EOF, appending them to `buf`.
    async fn read_to_string_at(&self, buf: String, pos: u64) -> BufResult<usize, String> {
        let original_len = buf.len();
        let (result, bytes) = self.read_to_end_at(buf.into_bytes(), pos).await;
        string_from_bytes(result, bytes, original_len)
    }

    /// Reads enough bytes to fill every buffer, starting at `pos`.
    ///
    /// An [`io::ErrorKind::UnexpectedEof`] error is returned if the source ends
    /// before all buffers are full.
    async fn read_vectored_exact_at<B: IoBufMut + 'static>(&self, mut bufs: Vec<B>, pos: u64) -> BufResult<(), Vec<B>> {
        let length = match total_capacity(&bufs) {
            Ok(length) => length,
            Err(error) => return (Err(error), bufs),
        };
        let mut read = 0usize;

        while read < length {
            let offset = match checked_offset(pos, read) {
                Ok(offset) => offset,
                Err(error) => return (Err(error), bufs),
            };
            let mut skipped = read;
            let slices = bufs
                .into_iter()
                .map(|buf| {
                    let capacity = buf.bytes_total();
                    let start = skipped.min(capacity);
                    skipped -= start;
                    Slice::new(buf, start, capacity)
                })
                .collect();
            let (result, slices) = self.read_vectored_at(slices, offset).await;
            bufs = slices.into_iter().map(Slice::into_inner).collect();

            match result {
                Ok(0) => {
                    return (
                        Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "failed to fill all buffers",
                        )),
                        bufs,
                    );
                }
                Ok(bytes) => read += bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return (Err(error), bufs),
            }
        }

        (Ok(()), bufs)
    }
}

impl<T: AsyncReadAt + ?Sized> AsyncReadAtExt for T {}

fn checked_offset(pos: u64, offset: usize) -> io::Result<u64> {
    let offset =
        u64::try_from(offset).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file offset exceeds u64"))?;
    pos.checked_add(offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file offset overflow"))
}

fn total_capacity<B: IoBufMut>(bufs: &[B]) -> io::Result<usize> {
    bufs.iter().try_fold(0usize, |total, buf| {
        total
            .checked_add(buf.bytes_total())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer capacity overflow"))
    })
}

fn string_from_bytes(result: io::Result<usize>, bytes: Vec<u8>, original_len: usize) -> BufResult<usize, String> {
    match String::from_utf8(bytes) {
        Ok(string) => (result, string),
        Err(error) => {
            let mut bytes = error.into_bytes();
            bytes.truncate(original_len);
            // SAFETY: these bytes came from the original valid `String`; only
            // bytes appended by the read were removed.
            let string = unsafe { String::from_utf8_unchecked(bytes) };
            let error = result
                .err()
                .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "stream did not contain valid UTF-8"));
            (Err(error), string)
        }
    }
}
