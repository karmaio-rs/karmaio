use std::io;

use crate::{
    buf::{BufResult, IoBufMut, IoVectoredBufMut, Slice},
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
        let capacity = buf.as_uninit().len();
        let mut read = 0usize;

        while read < capacity {
            let offset = match checked_offset(pos, read) {
                Ok(offset) => offset,
                Err(error) => return BufResult(Err(error), buf),
            };
            let slice = Slice::new(buf, read, capacity);
            let (result, slice) = self.read_at(slice, offset).await.into_parts();
            buf = slice.into_inner();

            match result {
                Ok(0) => {
                    return BufResult(
                        Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "failed to fill whole buffer",
                        )),
                        buf,
                    );
                }
                Ok(bytes) => read += bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return BufResult(Err(error), buf),
            }
        }

        BufResult(Ok(()), buf)
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
                Err(error) => return BufResult(Err(error), buf),
            };
            let slice = Slice::new(buf, start, capacity);
            let (result, slice) = self.read_at(slice, offset).await.into_parts();
            buf = slice.into_inner();

            match result {
                Ok(0) => return BufResult(Ok(read), buf),
                Ok(bytes) => read += bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return BufResult(Err(error), buf),
            }
        }
    }

    /// Reads all UTF-8 bytes from `pos` to EOF, appending them to `buf`.
    async fn read_to_string_at(&self, buf: String, pos: u64) -> BufResult<usize, String> {
        let original_len = buf.len();
        let (result, bytes) = self.read_to_end_at(buf.into_bytes(), pos).await.into_parts();
        string_from_bytes(result, bytes, original_len)
    }

    /// Reads enough bytes to fill every buffer, starting at `pos`.
    ///
    /// An [`io::ErrorKind::UnexpectedEof`] error is returned if the source ends
    /// before all buffers are full.
    async fn read_vectored_exact_at<V: IoVectoredBufMut>(&self, mut bufs: V, pos: u64) -> BufResult<(), V> {
        let length_result = {
            bufs.iter_uninit_slice().try_fold(0usize, |total, buf| {
                total
                    .checked_add(buf.len())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer capacity overflow"))
            })
        };
        let length = match length_result {
            Ok(length) => length,
            Err(error) => return BufResult(Err(error), bufs),
        };
        let mut read = 0usize;

        while read < length {
            let offset = match checked_offset(pos, read) {
                Ok(offset) => offset,
                Err(error) => return BufResult(Err(error), bufs),
            };
            let view = IoVectoredBufMut::slice_mut(bufs, read);
            let (result, view) = self.read_vectored_at(view, offset).await.into_parts();
            bufs = view.into_inner();

            match result {
                Ok(0) => {
                    return BufResult(
                        Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "failed to fill all buffers",
                        )),
                        bufs,
                    );
                }
                Ok(bytes) => read += bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return BufResult(Err(error), bufs),
            }
        }

        BufResult(Ok(()), bufs)
    }
}

impl<T: AsyncReadAt + ?Sized> AsyncReadAtExt for T {}

fn checked_offset(pos: u64, offset: usize) -> io::Result<u64> {
    let offset =
        u64::try_from(offset).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file offset exceeds u64"))?;
    pos.checked_add(offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file offset overflow"))
}

fn string_from_bytes(result: io::Result<usize>, bytes: Vec<u8>, original_len: usize) -> BufResult<usize, String> {
    match String::from_utf8(bytes) {
        Ok(string) => BufResult(result, string),
        Err(error) => {
            let mut bytes = error.into_bytes();
            bytes.truncate(original_len);
            // SAFETY: these bytes came from the original valid `String`; only
            // bytes appended by the read were removed.
            let string = unsafe { String::from_utf8_unchecked(bytes) };
            let error = result
                .err()
                .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "stream did not contain valid UTF-8"));
            BufResult(Err(error), string)
        }
    }
}
