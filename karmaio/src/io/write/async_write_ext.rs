use std::io;

use crate::{
    buf::{BufResult, IoBuf, IoVectoredBuf, Slice},
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

    /// Writes all initialized bytes of every buffer.
    ///
    /// Repeatedly calls [`AsyncWrite::write_vectored`], advancing an owned
    /// view of the collection by the completed byte count, and returns the
    /// original collection without rebuilding or flattening it. Partial
    /// progress across component boundaries is handled transparently.
    ///
    /// A successful zero-byte write with input remaining is reported as
    /// [`io::ErrorKind::WriteZero`]. Ordinary `Interrupted` errors are retried;
    /// explicit cancellation (see [`crate::runtime::is_operation_canceled`]) never surfaces as
    /// `Interrupted`, so a canceled operation propagates immediately instead
    /// of being retried. A writer that reports more bytes than remain is a
    /// contract violation reported as [`io::ErrorKind::InvalidData`].
    async fn write_vectored_all<V: IoVectoredBuf>(&mut self, mut bufs: V) -> BufResult<usize, V> {
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
            let view = IoVectoredBuf::slice(bufs, written);
            let (result, view) = self.write_vectored(view).await.into_parts();
            bufs = view.into_inner();

            match result {
                Ok(bytes) if bytes > length - written => {
                    return BufResult(
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "writer reported more bytes written than remain",
                        )),
                        bufs,
                    );
                }
                Ok(0) => return BufResult(Err(io::Error::from(io::ErrorKind::WriteZero)), bufs),
                Ok(bytes) => written += bytes,
                Err(ref error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return BufResult(Err(error), bufs),
            }
        }

        BufResult(Ok(written), bufs)
    }

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

    // Records every byte observed through `write` and completes each call fully.
    // Simulates a native vectored writer by consuming every component per call.
    #[derive(Default)]
    struct CollectingWriter {
        observed: Vec<u8>,
    }

    impl AsyncWrite for CollectingWriter {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            self.observed.extend_from_slice(buf.as_init());
            BufResult(Ok(buf.as_init().len()), buf)
        }

        async fn write_vectored<V: IoVectoredBuf>(&mut self, bufs: V) -> BufResult<usize, V> {
            let mut total = 0;
            for slice in bufs.iter_slice() {
                self.observed.extend_from_slice(slice);
                total += slice.len();
            }
            BufResult(Ok(total), bufs)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // Completes at most `max_per_call` bytes per `write` call.
    struct LimitedWriter {
        observed: Vec<u8>,
        max_per_call: usize,
    }

    impl LimitedWriter {
        fn new(max_per_call: usize) -> Self {
            Self {
                observed: Vec::new(),
                max_per_call,
            }
        }
    }

    impl AsyncWrite for LimitedWriter {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            let len = self.max_per_call.min(buf.as_init().len());
            self.observed.extend_from_slice(&buf.as_init()[..len]);
            BufResult(Ok(len), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // Never makes progress.
    #[derive(Default)]
    struct ZeroWriter;

    impl AsyncWrite for ZeroWriter {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(0), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // Reports more bytes written than were provided.
    struct OverReportingWriter;

    impl AsyncWrite for OverReportingWriter {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(buf.as_init().len() + 1), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn vectored_all_completes_and_returns_the_collection() {
        let mut writer = CollectingWriter::default();
        let bufs = vec![b"abc".to_vec(), b"def".to_vec()];
        let (result, returned) = run_ready(writer.write_vectored_all(bufs)).into_parts();

        assert_eq!(result.unwrap(), 6);
        assert_eq!(returned, [b"abc".to_vec(), b"def".to_vec()]);
        assert_eq!(writer.observed, b"abcdef");
    }

    #[test]
    fn vectored_all_advances_across_components_one_byte_at_a_time() {
        let mut writer = LimitedWriter::new(1);
        let bufs = [*b"ab", *b"cd", *b"ef"];
        let (result, returned) = run_ready(writer.write_vectored_all(bufs)).into_parts();

        assert_eq!(result.unwrap(), 6);
        assert_eq!(returned, [*b"ab", *b"cd", *b"ef"]);
        assert_eq!(writer.observed, b"abcdef");
    }

    #[test]
    fn vectored_all_reports_write_zero_when_no_progress_is_possible() {
        let mut writer = ZeroWriter;
        let bufs = [*b"ab", *b"cd"];
        let (result, returned) = run_ready(writer.write_vectored_all(bufs)).into_parts();

        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WriteZero);
        assert_eq!(returned, [*b"ab", *b"cd"]);
    }

    #[test]
    fn vectored_all_rejects_writers_reporting_more_bytes_than_remain() {
        let mut writer = OverReportingWriter;
        let bufs = [*b"ab", *b"cd"];
        let (result, returned) = run_ready(writer.write_vectored_all(bufs)).into_parts();

        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(returned, [*b"ab", *b"cd"]);
    }
}
