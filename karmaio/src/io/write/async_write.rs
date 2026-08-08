use std::{
    io::{Cursor, Result},
    slice,
};

use crate::buf::{BufResult, IntoInner, IoBuf, IoVectoredBuf};

/// Asynchronously writes owned buffers to an I/O object.
///
/// The trait also supports flushing buffered data and shutting down the output
/// stream. Futures are `!Send` by design in this share-nothing runtime.
#[allow(async_fn_in_trait)]
pub trait AsyncWrite {
    /// Writes bytes from an owned buffer, returning the number written.
    ///
    /// A call represents at most one attempt and may write fewer than all
    /// initialized bytes. The same buffer is returned on completion.
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B>;

    /// Writes the contents of multiple buffers into this writer (vectored / gather).
    /// Ownership of the collection is transferred and the same collection is
    /// returned on completion.
    ///
    /// The default implementation adapts the first non-empty component to a
    /// scalar write. Native gather implementations should override it.
    async fn write_vectored<V: IoVectoredBuf>(&mut self, bufs: V) -> BufResult<usize, V> {
        let mut iter = match bufs.owned_iter() {
            Ok(iter) => iter,
            Err(bufs) => return BufResult(Ok(0), bufs),
        };

        loop {
            if !iter.as_init().is_empty() {
                let (result, iter) = self.write(iter).await.into_parts();
                return BufResult(result, iter.into_inner());
            }
            iter = match iter.next() {
                Ok(iter) => iter,
                Err(bufs) => return BufResult(Ok(0), bufs),
            };
        }
    }

    /// Flushes buffered output to its destination.
    async fn flush(&mut self) -> Result<()>;

    /// Flushes buffered data and shuts down this output stream.
    ///
    /// After completion, callers should no longer write to the stream.
    async fn shutdown(&mut self) -> Result<()>;
}

/// Asynchronously writes owned buffers at explicit offsets.
///
/// Futures are `!Send` by design in this share-nothing runtime.
#[allow(async_fn_in_trait)]
pub trait AsyncWriteAt {
    /// Like [`AsyncWrite::write`], except that it writes at a specified position.
    async fn write_at<B: IoBuf>(&mut self, buf: B, pos: u64) -> BufResult<usize, B>;

    /// Like [`AsyncWrite::write_vectored`], except that it writes at a specified position (vectored).
    ///
    /// The default implementation adapts the first non-empty component to a
    /// scalar positional write.
    async fn write_vectored_at<V: IoVectoredBuf>(&mut self, bufs: V, pos: u64) -> BufResult<usize, V> {
        let mut iter = match bufs.owned_iter() {
            Ok(iter) => iter,
            Err(bufs) => return BufResult(Ok(0), bufs),
        };

        loop {
            if !iter.as_init().is_empty() {
                let (result, iter) = self.write_at(iter, pos).await.into_parts();
                return BufResult(result, iter.into_inner());
            }
            iter = match iter.next() {
                Ok(iter) => iter,
                Err(bufs) => return BufResult(Ok(0), bufs),
            };
        }
    }
}

impl<T: ?Sized + AsyncWriteAt> AsyncWriteAt for &mut T {
    #[inline]
    fn write_at<B: IoBuf>(&mut self, buf: B, pos: u64) -> impl Future<Output = BufResult<usize, B>> {
        (**self).write_at(buf, pos)
    }

    #[inline]
    fn write_vectored_at<V: IoVectoredBuf>(&mut self, bufs: V, pos: u64) -> impl Future<Output = BufResult<usize, V>> {
        (**self).write_vectored_at(bufs, pos)
    }
}

impl<T: ?Sized + AsyncWriteAt> AsyncWriteAt for Box<T> {
    #[inline]
    fn write_at<B: IoBuf>(&mut self, buf: B, pos: u64) -> impl Future<Output = BufResult<usize, B>> {
        (**self).write_at(buf, pos)
    }

    #[inline]
    fn write_vectored_at<V: IoVectoredBuf>(&mut self, bufs: V, pos: u64) -> impl Future<Output = BufResult<usize, V>> {
        (**self).write_vectored_at(bufs, pos)
    }
}

impl<T: ?Sized + AsyncWrite> AsyncWrite for &mut T {
    #[inline]
    fn write<B: IoBuf>(&mut self, buf: B) -> impl Future<Output = BufResult<usize, B>> {
        (**self).write(buf)
    }

    #[inline]
    fn write_vectored<V: IoVectoredBuf>(&mut self, bufs: V) -> impl Future<Output = BufResult<usize, V>> {
        (**self).write_vectored(bufs)
    }

    #[inline]
    fn flush(&mut self) -> impl Future<Output = std::io::Result<()>> {
        (**self).flush()
    }

    #[inline]
    fn shutdown(&mut self) -> impl Future<Output = std::io::Result<()>> {
        (**self).shutdown()
    }
}

impl<T: ?Sized + AsyncWrite + Unpin> AsyncWrite for Box<T> {
    #[inline]
    fn write<B: IoBuf>(&mut self, buf: B) -> impl Future<Output = BufResult<usize, B>> {
        (**self).write(buf)
    }

    #[inline]
    fn write_vectored<V: IoVectoredBuf>(&mut self, bufs: V) -> impl Future<Output = BufResult<usize, V>> {
        (**self).write_vectored(bufs)
    }

    #[inline]
    fn flush(&mut self) -> impl Future<Output = std::io::Result<()>> {
        (**self).flush()
    }

    #[inline]
    fn shutdown(&mut self) -> impl Future<Output = std::io::Result<()>> {
        (**self).shutdown()
    }
}

impl AsyncWrite for Vec<u8> {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let bytes_to_write = buf.as_init().len();

        self.extend_from_slice(buf.as_init());

        BufResult(Ok(bytes_to_write), buf)
    }

    async fn write_vectored<V: IoVectoredBuf>(&mut self, bufs: V) -> BufResult<usize, V> {
        let mut total = 0usize;
        for buf in bufs.iter_slice() {
            self.extend_from_slice(buf);
            total += buf.len();
        }
        BufResult(Ok(total), bufs)
    }

    async fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}

impl AsyncWriteAt for [u8] {
    async fn write_at<B: IoBuf>(&mut self, buf: B, pos: u64) -> BufResult<usize, B> {
        let pos = usize::try_from(pos).unwrap_or(usize::MAX).min(self.len());
        let bytes_to_write = buf.as_init().len().min(self.len() - pos);

        if bytes_to_write > 0 {
            unsafe {
                self[pos..]
                    .as_mut_ptr()
                    .copy_from_nonoverlapping(buf.as_init().as_ptr(), bytes_to_write);
            }
        }

        BufResult(Ok(bytes_to_write), buf)
    }
}

impl<const N: usize> AsyncWriteAt for [u8; N] {
    #[inline]
    async fn write_at<B: IoBuf>(&mut self, buf: B, pos: u64) -> BufResult<usize, B> {
        self.as_mut_slice().write_at(buf, pos).await
    }
}

impl AsyncWriteAt for Vec<u8> {
    async fn write_at<B: IoBuf>(&mut self, buf: B, pos: u64) -> BufResult<usize, B> {
        let pos = match usize::try_from(pos) {
            Ok(pos) => pos,
            Err(_) => {
                return BufResult(
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "file offset exceeds usize",
                    )),
                    buf,
                );
            }
        };
        let bytes_to_write = buf.as_init().len();
        if bytes_to_write == 0 {
            return BufResult(Ok(0), buf);
        }
        if pos.checked_add(bytes_to_write).is_none() {
            return BufResult(
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "file offset overflow",
                )),
                buf,
            );
        }

        let bytes = unsafe { slice::from_raw_parts(buf.as_init().as_ptr(), bytes_to_write) };
        if pos > self.len() {
            self.resize(pos, 0);
        }
        let overwrite = bytes_to_write.min(self.len() - pos);
        self[pos..pos + overwrite].copy_from_slice(&bytes[..overwrite]);
        self.extend_from_slice(&bytes[overwrite..]);

        BufResult(Ok(bytes_to_write), buf)
    }
}

impl<W: AsyncWriteAt> AsyncWrite for Cursor<W> {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let pos = self.position();
        let (result, buf) = self.get_mut().write_at(buf, pos).await.into_parts();
        if let Ok(written) = result {
            self.set_position(pos + written as u64);
        }
        BufResult(result, buf)
    }

    async fn write_vectored<V: IoVectoredBuf>(&mut self, bufs: V) -> BufResult<usize, V> {
        let pos = self.position();
        let (result, bufs) = self.get_mut().write_vectored_at(bufs, pos).await.into_parts();
        if let Ok(written) = result {
            self.set_position(pos + written as u64);
        }
        BufResult(result, bufs)
    }

    async fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
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

    #[derive(Default)]
    struct ScalarWriter {
        observed: Vec<u8>,
    }

    impl AsyncWrite for ScalarWriter {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            self.observed.extend_from_slice(buf.as_init());
            BufResult(Ok(buf.as_init().len()), buf)
        }

        async fn flush(&mut self) -> Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn scalar_vectored_fallback_skips_empty_components_without_copying() {
        let mut writer = ScalarWriter::default();
        let bufs = [Vec::new(), b"abc".to_vec()];
        let (result, bufs) = run_ready(writer.write_vectored(bufs)).into_parts();

        assert_eq!(result.unwrap(), 3);
        assert_eq!(writer.observed, b"abc");
        assert!(bufs[0].is_empty());
        assert_eq!(bufs[1], b"abc");
    }
}
