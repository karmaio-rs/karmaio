use super::AsyncRead;
use crate::{
    buf::{BufResult, IntoInner, IoBufMut, IoVectoredBufMut},
    io::cancel::CancelHandle,
};

/// Cancellable completion reads.
///
/// [`read_cancellable`](Self::read_cancellable) is the eager-cancel counterpart
/// of [`AsyncRead::read`]. Dropping a Cancellable future without
/// [`crate::io::Canceller::cancel`] still detaches; cancel then await to
/// reclaim the buffer without waiting for a silent peer.
#[allow(async_fn_in_trait)]
pub trait AsyncReadCancellable: AsyncRead {
    /// Read into `buf`, registering `cancellation` for eager cancel.
    async fn read_cancellable<B: IoBufMut>(&mut self, buf: B, cancellation: &CancelHandle) -> BufResult<usize, B>;

    /// Vectored read, registering `cancellation` for eager cancel.
    ///
    /// The default implementation adapts the first component to a scalar
    /// Cancellable read.
    async fn read_vectored_cancellable<V: IoVectoredBufMut>(
        &mut self,
        bufs: V,
        cancellation: &CancelHandle,
    ) -> BufResult<usize, V> {
        let mut iter = match bufs.owned_iter() {
            Ok(iter) => iter,
            Err(bufs) => return BufResult(Ok(0), bufs),
        };

        loop {
            if !iter.as_uninit().is_empty() {
                let (result, iter) = self.read_cancellable(iter, cancellation).await.into_parts();
                return BufResult(result, iter.into_inner());
            }
            iter = match iter.next() {
                Ok(iter) => iter,
                Err(bufs) => return BufResult(Ok(0), bufs),
            };
        }
    }
}

impl<T: ?Sized + AsyncReadCancellable> AsyncReadCancellable for &mut T {
    #[inline]
    async fn read_cancellable<B: IoBufMut>(&mut self, buf: B, cancellation: &CancelHandle) -> BufResult<usize, B> {
        (**self).read_cancellable(buf, cancellation).await
    }

    #[inline]
    async fn read_vectored_cancellable<V: IoVectoredBufMut>(
        &mut self,
        bufs: V,
        cancellation: &CancelHandle,
    ) -> BufResult<usize, V> {
        (**self).read_vectored_cancellable(bufs, cancellation).await
    }
}
