use std::{fmt, marker::PhantomData, net::Shutdown, ops::Deref};

use crate::{
    buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut},
    driver::helpers::socket::Socket,
    io::{AsyncRead, AsyncReadCancellable, AsyncWrite, AsyncWriteCancellable, CancelHandle},
};

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

impl<'a, T> AsyncReadCancellable for ReadHalf<'a, T>
where
    &'a T: AsyncReadCancellable,
{
    async fn read_cancellable<B: IoBufMut>(&mut self, buf: B, cancellation: &CancelHandle) -> BufResult<usize, B> {
        self.0.read_cancellable(buf, cancellation).await
    }

    async fn read_vectored_cancellable<V: IoVectoredBufMut>(
        &mut self,
        bufs: V,
        cancellation: &CancelHandle,
    ) -> BufResult<usize, V> {
        self.0.read_vectored_cancellable(bufs, cancellation).await
    }
}

impl<'a, T> AsyncWriteCancellable for WriteHalf<'a, T>
where
    &'a T: AsyncWriteCancellable,
{
    async fn write_cancellable<B: IoBuf>(&mut self, buf: B, cancellation: &CancelHandle) -> BufResult<usize, B> {
        self.0.write_cancellable(buf, cancellation).await
    }

    async fn write_vectored_cancellable<V: IoVectoredBuf>(
        &mut self,
        bufs: V,
        cancellation: &CancelHandle,
    ) -> BufResult<usize, V> {
        self.0.write_vectored_cancellable(bufs, cancellation).await
    }
}

// ---------------------------------------------------------------------------
// Owned splitting
// ---------------------------------------------------------------------------

/// Consumes a duplex value and produces independently owned read and write halves.
///
/// Each half owns the underlying resource strongly enough to outlive the
/// original stream value, so halves can be moved into separately supervised
/// `'static` local tasks and used concurrently.
///
/// Neither half requires `Send` or `Sync` merely to satisfy this trait;
/// the halves work on the local, share-nothing runtime.
///
/// Implementations must guarantee:
///
/// - Each half owns the underlying resource strongly enough to outlive the original stream value.
/// - Dropping one half does not close the underlying resource while the other
///   half or an in-flight operation still owns it.
pub trait IntoOwnedSplit: Sized {
    /// The type of the read half, which implements [`AsyncRead`].
    type ReadHalf: AsyncRead + 'static;

    /// The type of the write half, which implements [`AsyncWrite`].
    type WriteHalf: AsyncWrite + 'static;

    /// Consumes `self` and returns a read half and a write half that can be
    /// used independently and concurrently.
    fn into_split(self) -> (Self::ReadHalf, Self::WriteHalf);
}

/// Extends [`IntoOwnedSplit`] for transports that can reconstruct the original
/// value from matching owned halves.
///
/// Reunification succeeds only for halves from the same original value and
/// only when no incompatible ownership remains. Implementations should return
/// an error that preserves both halves when reunification fails.
pub trait ReuniteOwned: IntoOwnedSplit {
    /// The error returned when the halves cannot be reunited.
    type ReuniteError;

    /// Attempts to reunite matching owned halves into the original value.
    ///
    fn reunite(read: Self::ReadHalf, write: Self::WriteHalf) -> Result<Self, Self::ReuniteError>;
}

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

impl<T> AsyncReadCancellable for OwnedReadHalf<T> {
    async fn read_cancellable<B: IoBufMut>(&mut self, buf: B, cancellation: &CancelHandle) -> BufResult<usize, B> {
        self.inner.recv_cancellable(buf, cancellation).await
    }

    async fn read_vectored_cancellable<V: IoVectoredBufMut>(
        &mut self,
        bufs: V,
        cancellation: &CancelHandle,
    ) -> BufResult<usize, V> {
        let (result, bufs) = self.inner.recvmsg_cancellable(bufs, cancellation).await.into_parts();
        BufResult(result.map(|(n, _)| n), bufs)
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

impl<T> AsyncWriteCancellable for OwnedWriteHalf<T> {
    async fn write_cancellable<B: IoBuf>(&mut self, buf: B, cancellation: &CancelHandle) -> BufResult<usize, B> {
        self.inner.send_cancellable(buf, cancellation).await
    }

    async fn write_vectored_cancellable<V: IoVectoredBuf>(
        &mut self,
        bufs: V,
        cancellation: &CancelHandle,
    ) -> BufResult<usize, V> {
        let (res, bufs) = self
            .inner
            .sendmsg_cancellable(bufs, None, None::<Vec<u8>>, cancellation)
            .await
            .into_parts();
        BufResult(res.map(|(n, _)| n), bufs)
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

/// The semantic reason a reunification attempt failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReuniteErrorKind {
    /// The halves originated from different split operations.
    Mismatched,
    /// Matching halves cannot yet be reunited because another owner remains.
    NotQuiescent,
}

impl fmt::Display for ReuniteErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mismatched => write!(f, "halves originated from different split operations"),
            Self::NotQuiescent => write!(f, "matching halves are not yet quiescent"),
        }
    }
}

/// A failed reunification that preserves both owned halves.
///
/// Use [`Self::kind`] to choose whether to correct the pairing or wait for
/// outstanding ownership to end, then recover the halves with
/// [`Self::into_halves`].
#[derive(Debug)]
pub struct ReuniteError<R, W> {
    kind: ReuniteErrorKind,
    read: R,
    write: W,
}

impl<R, W> ReuniteError<R, W> {
    /// Creates an error for halves from different split operations.
    pub fn mismatched(read: R, write: W) -> Self {
        Self {
            kind: ReuniteErrorKind::Mismatched,
            read,
            write,
        }
    }

    /// Creates an error for matching halves that cannot yet be reunited.
    pub fn not_quiescent(read: R, write: W) -> Self {
        Self {
            kind: ReuniteErrorKind::NotQuiescent,
            read,
            write,
        }
    }

    /// Returns the reason the reunification attempt failed.
    pub fn kind(&self) -> ReuniteErrorKind {
        self.kind
    }

    /// Borrows the preserved read and write halves.
    pub fn halves(&self) -> (&R, &W) {
        (&self.read, &self.write)
    }

    /// Consumes the error and returns the preserved read and write halves.
    pub fn into_halves(self) -> (R, W) {
        (self.read, self.write)
    }
}

impl<R, W> fmt::Display for ReuniteError<R, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot reunite owned halves: {}", self.kind)
    }
}

impl<R: fmt::Debug, W: fmt::Debug> std::error::Error for ReuniteError<R, W> {}

// `Socket` is crate-internal, so this convenience method is available only
// for Karmaio stream types. Generic transports use [`ReuniteOwned::reunite`]
// instead.
#[allow(private_bounds)]
impl<T: From<Socket>> OwnedReadHalf<T> {
    /// Attempts to put the two halves of a stream back together, recovering
    /// the original stream value.
    ///
    /// Succeeds only if both halves originated from the same
    /// [`IntoOwnedSplit::into_split`] call and no detached in-flight
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
