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
}
