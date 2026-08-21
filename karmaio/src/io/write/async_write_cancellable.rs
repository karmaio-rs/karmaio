use super::AsyncWrite;
use crate::{
    buf::{BufResult, IntoInner, IoBuf, IoVectoredBuf},
    io::cancel::CancelHandle,
};

/// Cancellable completion writes.
///
/// Dropping a Cancellable future without [`crate::io::Canceller::cancel`] still
/// detaches. Cancel then await to observe terminal cleanup.
#[allow(async_fn_in_trait)]
pub trait AsyncWriteCancellable: AsyncWrite {
    /// Write from `buf`, registering `cancellation` for eager cancel.
    async fn write_cancellable<B: IoBuf>(&mut self, buf: B, cancellation: &CancelHandle) -> BufResult<usize, B>;

    /// Vectored write, registering `cancellation` for eager cancel.
    ///
    /// The default implementation adapts the first non-empty component to a
    /// scalar Cancellable write.
    async fn write_vectored_cancellable<V: IoVectoredBuf>(
        &mut self,
        bufs: V,
        cancellation: &CancelHandle,
    ) -> BufResult<usize, V> {
        let mut iter = match bufs.owned_iter() {
            Ok(iter) => iter,
            Err(bufs) => return BufResult(Ok(0), bufs),
        };

        loop {
            if !iter.as_init().is_empty() {
                let (result, iter) = self.write_cancellable(iter, cancellation).await.into_parts();
                return BufResult(result, iter.into_inner());
            }
            iter = match iter.next() {
                Ok(iter) => iter,
                Err(bufs) => return BufResult(Ok(0), bufs),
            };
        }
    }

    /// Writes all initialized bytes of every buffer, registering `cancellation`
    /// for eager cancel on each submitted operation.
    ///
    /// Repeatedly calls [`AsyncWriteCancellable::write_vectored_cancellable`],
    /// advancing an owned view of the collection by the completed byte count,
    /// and returns the original collection without rebuilding or flattening it.
    ///
    /// A successful zero-byte write with input remaining is reported as
    /// [`std::io::ErrorKind::WriteZero`]. Ordinary `Interrupted` errors are
    /// retried; a user-requested cancellation surfaces as an error carrying
    /// [`crate::io::OperationCanceled`] and propagates immediately instead of
    /// being retried. A writer that reports more bytes than remain is a
    /// contract violation reported as [`std::io::ErrorKind::InvalidData`].
    async fn write_vectored_all_cancellable<V: IoVectoredBuf>(
        &mut self,
        mut bufs: V,
        cancellation: &CancelHandle,
    ) -> BufResult<usize, V> {
        let length_result = {
            bufs.iter_slice().try_fold(0usize, |total, buf| {
                total
                    .checked_add(buf.len())
                    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "buffer length overflow"))
            })
        };
        let length = match length_result {
            Ok(length) => length,
            Err(error) => return BufResult(Err(error), bufs),
        };
        let mut written = 0usize;

        while written < length {
            let view = IoVectoredBuf::slice(bufs, written);
            let (result, view) = self.write_vectored_cancellable(view, cancellation).await.into_parts();
            bufs = view.into_inner();

            match result {
                Ok(bytes) if bytes > length - written => {
                    return BufResult(
                        Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "writer reported more bytes written than remain",
                        )),
                        bufs,
                    );
                }
                Ok(0) => return BufResult(Err(std::io::Error::from(std::io::ErrorKind::WriteZero)), bufs),
                Ok(bytes) => written += bytes,
                Err(ref error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return BufResult(Err(error), bufs),
            }
        }

        BufResult(Ok(written), bufs)
    }
}

impl<T: ?Sized + AsyncWriteCancellable> AsyncWriteCancellable for &mut T {
    #[inline]
    async fn write_cancellable<B: IoBuf>(&mut self, buf: B, cancellation: &CancelHandle) -> BufResult<usize, B> {
        (**self).write_cancellable(buf, cancellation).await
    }

    #[inline]
    async fn write_vectored_cancellable<V: IoVectoredBuf>(
        &mut self,
        bufs: V,
        cancellation: &CancelHandle,
    ) -> BufResult<usize, V> {
        (**self).write_vectored_cancellable(bufs, cancellation).await
    }

    #[inline]
    async fn write_vectored_all_cancellable<V: IoVectoredBuf>(
        &mut self,
        bufs: V,
        cancellation: &CancelHandle,
    ) -> BufResult<usize, V> {
        (**self).write_vectored_all_cancellable(bufs, cancellation).await
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future, task::Poll};

    use super::*;
    use crate::io::{Canceller, is_operation_canceled, operation_canceled};

    fn run_ready<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("test future unexpectedly yielded"),
        }
    }

    // Completes at most `max_per_call` bytes per Cancellable scalar write.
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
        async fn write<B: IoBuf>(&mut self, _buf: B) -> BufResult<usize, B> {
            unreachable!("Cancellable helpers must not call plain write")
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl AsyncWriteCancellable for LimitedWriter {
        async fn write_cancellable<B: IoBuf>(&mut self, buf: B, _cancellation: &CancelHandle) -> BufResult<usize, B> {
            let len = self.max_per_call.min(buf.as_init().len());
            self.observed.extend_from_slice(&buf.as_init()[..len]);
            BufResult(Ok(len), buf)
        }
    }

    // Accepts the first call, then reports a user-requested cancellation.
    struct CancelingAfterProgressWriter {
        observed: Vec<u8>,
    }

    impl AsyncWrite for CancelingAfterProgressWriter {
        async fn write<B: IoBuf>(&mut self, _buf: B) -> BufResult<usize, B> {
            unreachable!("Cancellable helpers must not call plain write")
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl AsyncWriteCancellable for CancelingAfterProgressWriter {
        async fn write_cancellable<B: IoBuf>(&mut self, buf: B, _cancellation: &CancelHandle) -> BufResult<usize, B> {
            if self.observed.is_empty() {
                self.observed.extend_from_slice(buf.as_init());
                return BufResult(Ok(buf.as_init().len()), buf);
            }
            BufResult(Err(operation_canceled()), buf)
        }
    }

    #[test]
    fn Cancellable_vectored_all_advances_across_components_without_plain_writes() {
        let mut writer = LimitedWriter::new(1);
        let canceller = Canceller::new();
        let cancellation = canceller.handle();

        let bufs = [*b"ab", *b"cd", *b"ef"];
        let (result, returned) = run_ready(writer.write_vectored_all_cancellable(bufs, &cancellation)).into_parts();

        assert_eq!(result.unwrap(), 6);
        assert_eq!(returned, [*b"ab", *b"cd", *b"ef"]);
        assert_eq!(writer.observed, b"abcdef");
        assert!(!cancellation.is_cancel_requested());
    }

    #[test]
    fn Cancellable_vectored_all_propagates_cancellation_with_the_buffer() {
        let mut writer = CancelingAfterProgressWriter { observed: Vec::new() };
        let canceller = Canceller::new();
        let cancellation = canceller.handle();

        let bufs = [*b"ab", *b"cd"];
        let (result, returned) = run_ready(writer.write_vectored_all_cancellable(bufs, &cancellation)).into_parts();

        let error = result.unwrap_err();
        assert!(is_operation_canceled(&error));
        assert_eq!(returned, [*b"ab", *b"cd"]);
        assert_eq!(writer.observed, b"ab");
    }
}
