use std::{io::Cursor, rc::Rc, sync::Arc};

use crate::buf::{BufResult, IntoInner, IoBufMut, IoVectoredBufMut};

/// Asynchronously reads bytes into owned buffers.
///
/// Implementations manage completion-based read operations without blocking
/// the executor. Futures are `!Send` by design in this share-nothing runtime.
#[allow(async_fn_in_trait)]
pub trait AsyncRead {
    /// Reads bytes into the beginning of an owned mutable buffer.
    ///
    /// Ownership of `buf` is transferred to the operation and the same buffer
    /// is returned on completion. On success, implementations must set its
    /// initialized length to exactly the number of bytes read.
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B>;

    /// Reads bytes into the rented mutable buffers (vectored / scatter-gather).
    /// Ownership of the collection is transferred to the operation and the
    /// same collection is returned on completion.
    /// Implementors should fill buffers in order.
    ///
    /// The default implementation adapts the first component to a scalar read.
    /// Native scatter implementations should override this method.
    async fn read_vectored<V: IoVectoredBufMut>(&mut self, bufs: V) -> BufResult<usize, V> {
        let mut iter = match bufs.owned_iter() {
            Ok(iter) => iter,
            Err(bufs) => return BufResult(Ok(0), bufs),
        };

        loop {
            if !iter.as_uninit().is_empty() {
                let (result, iter) = self.read(iter).await.into_parts();
                return BufResult(result, iter.into_inner());
            }
            iter = match iter.next() {
                Ok(iter) => iter,
                Err(bufs) => return BufResult(Ok(0), bufs),
            };
        }
    }
}

/// Asynchronously reads bytes at explicit offsets into owned buffers.
///
/// Futures are `!Send` by design in this share-nothing runtime.
#[allow(async_fn_in_trait)]
pub trait AsyncReadAt {
    /// Like [`AsyncRead::read`], except that it reads at a specified position.
    async fn read_at<B: IoBufMut>(&self, buf: B, pos: u64) -> BufResult<usize, B>;

    /// Like [`AsyncRead::read_vectored`], except that it reads at a specified position (vectored).
    ///
    /// The default implementation adapts the first component to a scalar read.
    async fn read_vectored_at<V: IoVectoredBufMut>(&self, bufs: V, pos: u64) -> BufResult<usize, V> {
        let mut iter = match bufs.owned_iter() {
            Ok(iter) => iter,
            Err(bufs) => return BufResult(Ok(0), bufs),
        };

        loop {
            if !iter.as_uninit().is_empty() {
                let (result, iter) = self.read_at(iter, pos).await.into_parts();
                return BufResult(result, iter.into_inner());
            }
            iter = match iter.next() {
                Ok(iter) => iter,
                Err(bufs) => return BufResult(Ok(0), bufs),
            };
        }
    }
}

impl<T: ?Sized + AsyncRead> AsyncRead for &mut T {
    #[inline]
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        (**self).read(buf).await
    }

    #[inline]
    async fn read_vectored<V: IoVectoredBufMut>(&mut self, bufs: V) -> BufResult<usize, V> {
        (**self).read_vectored(bufs).await
    }
}

impl<T: ?Sized + AsyncReadAt> AsyncReadAt for &mut T {
    #[inline]
    async fn read_at<B: IoBufMut>(&self, buf: B, pos: u64) -> BufResult<usize, B> {
        (**self).read_at(buf, pos).await
    }

    #[inline]
    async fn read_vectored_at<V: IoVectoredBufMut>(&self, bufs: V, pos: u64) -> BufResult<usize, V> {
        (**self).read_vectored_at(bufs, pos).await
    }
}

macro_rules! delegate_read_at {
    ($($ty:ty),* $(,)?) => {
        $(
            impl<T: ?Sized + AsyncReadAt> AsyncReadAt for $ty {
                #[inline]
                async fn read_at<B: IoBufMut>(&self, buf: B, pos: u64) -> BufResult<usize, B> {
                    (**self).read_at(buf, pos).await
                }

                #[inline]
                async fn read_vectored_at<V: IoVectoredBufMut>(
                    &self,
                    bufs: V,
                    pos: u64,
                ) -> BufResult<usize, V> {
                    (**self).read_vectored_at(bufs, pos).await
                }
            }
        )*
    };
}

delegate_read_at!(&T, Box<T>, Rc<T>, Arc<T>);

impl AsyncRead for &[u8] {
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        let bytes_to_read = self.len().min(buf.as_uninit().len());

        unsafe {
            let dst = buf.as_uninit().as_mut_ptr().cast::<u8>();
            dst.copy_from_nonoverlapping(self.as_ptr(), bytes_to_read);
            buf.set_len(bytes_to_read);
        }
        *self = &self[bytes_to_read..];

        BufResult(Ok(bytes_to_read), buf)
    }

    async fn read_vectored<V: IoVectoredBufMut>(&mut self, mut bufs: V) -> BufResult<usize, V> {
        let mut total = 0usize;
        for buf in bufs.iter_uninit_slice() {
            let remaining = self.len();
            if remaining == 0 {
                break;
            }
            let space = buf.len();
            let n = remaining.min(space);
            if n > 0 {
                unsafe {
                    let dst = buf.as_mut_ptr().cast::<u8>();
                    dst.copy_from_nonoverlapping(self.as_ptr(), n);
                }
                *self = &self[n..];
                total += n;
            }
        }
        // Safety: the loop initialized exactly the aggregate prefix `total`.
        unsafe { bufs.set_len(total) };
        BufResult(Ok(total), bufs)
    }
}

impl AsyncReadAt for [u8] {
    async fn read_at<B: IoBufMut>(&self, mut buf: B, pos: u64) -> BufResult<usize, B> {
        let pos = pos.min(self.len() as u64) as usize;
        let bytes_to_read = (self.len() - pos).min(buf.as_uninit().len());

        unsafe {
            buf.as_uninit()
                .as_mut_ptr()
                .cast::<u8>()
                .copy_from_nonoverlapping(self.as_ptr().add(pos), bytes_to_read);
            buf.set_len(bytes_to_read);
        }

        BufResult(Ok(bytes_to_read), buf)
    }
}

impl<const N: usize> AsyncReadAt for [u8; N] {
    #[inline]
    async fn read_at<B: IoBufMut>(&self, buf: B, pos: u64) -> BufResult<usize, B> {
        self.as_slice().read_at(buf, pos).await
    }
}

impl AsyncReadAt for Vec<u8> {
    #[inline]
    async fn read_at<B: IoBufMut>(&self, buf: B, pos: u64) -> BufResult<usize, B> {
        self.as_slice().read_at(buf, pos).await
    }
}

impl<T: AsyncReadAt> AsyncRead for Cursor<T> {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        let pos = self.position();
        let (res, buf) = self.get_ref().read_at(buf, pos).await.into_parts();
        if let Ok(n) = res {
            self.set_position(pos + n as u64);
        }
        BufResult(res, buf)
    }

    async fn read_vectored<V: IoVectoredBufMut>(&mut self, bufs: V) -> BufResult<usize, V> {
        let pos = self.position();
        let (res, bufs) = self.get_ref().read_vectored_at(bufs, pos).await.into_parts();
        if let Ok(n) = res {
            self.set_position(pos + n as u64);
        }
        BufResult(res, bufs)
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future, task::Poll};

    use super::*;

    fn run_ready<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(&waker);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("test future unexpectedly yielded"),
        }
    }

    struct ScalarReader;

    impl AsyncRead for ScalarReader {
        async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
            let target = buf.as_uninit();
            let n = usize::from(!target.is_empty());
            if n != 0 {
                target[0] = std::mem::MaybeUninit::new(9);
            }
            // Safety: the only byte included in `n` was initialized above.
            unsafe { buf.set_len(n) };
            BufResult(Ok(n), buf)
        }
    }

    #[test]
    fn scalar_vectored_fallback_skips_empty_components_without_copying() {
        let mut reader = ScalarReader;
        let bufs = [Vec::with_capacity(0), Vec::with_capacity(4)];
        let (result, bufs) = run_ready(reader.read_vectored(bufs)).into_parts();

        assert_eq!(result.unwrap(), 1);
        assert!(bufs[0].is_empty());
        assert_eq!(bufs[1], [9]);
    }
}
