use std::{fmt, marker::PhantomData, net::Shutdown, ops::Deref};

use crate::{
    buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut},
    driver::helpers::socket::Socket,
    io::{AsyncRead, AsyncWrite, ReuniteError},
};

#[cfg(target_os = "linux")]
use crate::{buf::PooledBuf, io::AsyncReadManaged};

pub(super) fn split<'a, T>(stream: &'a T) -> (ReadHalf<'a, T>, WriteHalf<'a, T>)
where
    &'a T: AsyncRead + AsyncWrite,
{
    (ReadHalf(stream), WriteHalf(stream))
}

/// Borrowed read half.
#[derive(Debug)]
pub struct ReadHalf<'a, T>(&'a T);

impl<'a, T> AsyncRead for ReadHalf<'a, T>
where
    &'a T: AsyncRead,
{
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.0.read(buf).await
    }

    async fn read_vectored<V: IoVectoredBufMut>(&mut self, bufs: V) -> BufResult<usize, V> {
        self.0.read_vectored(bufs).await
    }
}

impl<T> Deref for ReadHalf<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

/// Borrowed read half.
#[derive(Debug)]
pub struct WriteHalf<'a, T>(&'a T);

impl<'a, T> AsyncWrite for WriteHalf<'a, T>
where
    &'a T: AsyncWrite,
{
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        self.0.write(buf).await
    }

    async fn write_vectored<V: IoVectoredBuf>(&mut self, bufs: V) -> BufResult<usize, V> {
        self.0.write_vectored(bufs).await
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush().await
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        self.0.shutdown().await
    }
}

impl<T> Deref for WriteHalf<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Owned splitting
// ---------------------------------------------------------------------------

/// Owned read half of a stream, created by [`IntoOwnedSplit::into_split`].
///
/// The half owns the socket and can be moved into its own local task.
/// Reunite it with a matching write half via [`OwnedReadHalf::reunite`].
pub struct OwnedReadHalf<T> {
    pub(crate) inner: Socket,
    // Identifies the concrete stream type recovered by `reunite`.
    pub(crate) _stream: PhantomData<fn() -> T>,
}

impl<T> fmt::Debug for OwnedReadHalf<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedReadHalf").finish_non_exhaustive()
    }
}

impl<T> AsyncRead for OwnedReadHalf<T> {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.inner.recv(buf).await
    }

    async fn read_vectored<V: IoVectoredBufMut>(&mut self, bufs: V) -> BufResult<usize, V> {
        let (result, bufs) = self.inner.recvmsg(bufs).await.into_parts();
        BufResult(result.map(|(n, _)| n), bufs)
    }
}

#[cfg(target_os = "linux")]
impl<T> AsyncReadManaged for OwnedReadHalf<T> {
    type Buffer = PooledBuf;

    async fn read_managed(&mut self, len: usize) -> std::io::Result<Option<Self::Buffer>> {
        self.inner.recv_managed(len).await
    }
}

/// Owned write half of a stream, created by [`IntoOwnedSplit::into_split`].
///
/// The half owns the socket and can be moved into its own local task.
/// Dropping the half shuts down the write direction of the connection
/// (half-close) without invalidating input that is still being received.
/// Reunite it with a matching read half via [`OwnedWriteHalf::reunite`].
pub struct OwnedWriteHalf<T> {
    pub(crate) inner: Socket,
    pub(crate) shutdown_on_drop: bool,
    // Identifies the concrete stream type recovered by `reunite`.
    pub(crate) _stream: PhantomData<fn() -> T>,
}

impl<T> fmt::Debug for OwnedWriteHalf<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedWriteHalf").finish_non_exhaustive()
    }
}
impl<T> AsyncWrite for OwnedWriteHalf<T> {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        self.inner.send(buf).await
    }

    async fn write_vectored<V: IoVectoredBuf>(&mut self, bufs: V) -> BufResult<usize, V> {
        let (res, bufs) = self.inner.sendmsg(bufs, None, None::<Vec<u8>>).await.into_parts();
        BufResult(res.map(|(n, _)| n), bufs)
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        self.inner.shutdown(Shutdown::Write)
    }
}

impl<T> Drop for OwnedWriteHalf<T> {
    fn drop(&mut self) {
        // Half-close the write direction when the write half goes away.
        // Errors are ignored: the socket may already be shut down or closed.
        if self.shutdown_on_drop {
            let _ = self.inner.shutdown(Shutdown::Write);
        }
    }
}

// `Socket` is crate-internal, so this convenience method is available only
// for Karmaio stream types. Generic transports use
// [`ReuniteOwned::reunite`](crate::io::ReuniteOwned::reunite) instead.
#[allow(private_bounds)]
impl<T: From<Socket>> OwnedReadHalf<T> {
    /// Attempts to put the two halves of a stream back together, recovering
    /// the original stream value.
    ///
    /// Succeeds only if both halves originated from the same
    /// [`IntoOwnedSplit::into_split`](crate::io::IntoOwnedSplit::into_split)
    /// call and no detached in-flight
    /// operation still owns the socket. On failure, both halves are returned
    /// unchanged and remain usable.
    pub fn reunite(self, other: OwnedWriteHalf<T>) -> Result<T, ReuniteError<OwnedReadHalf<T>, OwnedWriteHalf<T>>> {
        if !self.inner.handle.ptr_eq(&other.inner.handle) {
            return Err(ReuniteError::mismatched(self, other));
        }

        // Reunification requires no incompatible extra ownership: the only
        // strong references to the socket must be the two halves. Detached
        // in-flight operations hold clones and must complete first.
        if self.inner.handle.strong_count() != 2 {
            return Err(ReuniteError::not_quiescent(self, other));
        }

        // Drop the write half without running its shutdown-on-drop: the
        // connection continues as a single stream.
        let mut other = other;
        other.shutdown_on_drop = false;
        drop(other);

        // Recover the unique remaining handle for the reconstructed stream.
        let socket = self.inner.clone();
        drop(self);
        Ok(T::from(socket))
    }
}

#[allow(private_bounds)]
impl<T: From<Socket>> OwnedWriteHalf<T> {
    /// Attempts to put the two halves of a stream back together, recovering
    /// the original stream value.
    ///
    /// See [`OwnedReadHalf::reunite`].
    pub fn reunite(self, other: OwnedReadHalf<T>) -> Result<T, ReuniteError<OwnedReadHalf<T>, OwnedWriteHalf<T>>> {
        other.reunite(self)
    }
}
