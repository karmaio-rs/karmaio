use std::net::SocketAddr;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

#[cfg(target_os = "linux")]
use crate::{
    buf::PooledBuf,
    io::{AsyncReadManaged, AsyncReadMulti, Stream},
};
use crate::{
    buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut},
    driver::helpers::socket::Socket,
    io::{AsyncRead, AsyncReadCancellable, AsyncWrite, AsyncWriteCancellable, CancelHandle},
    net::split::{
        IntoOwnedSplit, OwnedReadHalf, OwnedWriteHalf, ReadHalf, ReuniteError, ReuniteOwned, WriteHalf, split,
    },
};

#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, RawSocket};

/// A TCP stream between a local and a remote socket.
///
/// A TCP stream can either be created by connecting to an endpoint
/// via [`TcpStream::connect`], or by accepting a connection from a
/// [`TcpListener`](crate::net::tcp::TcpListener).
///
/// # Closing
///
/// Prefer [`TcpStream::close`] so close errors are reported. Dropping the stream
/// still closes the OS socket synchronously when the last reference is dropped.
pub struct TcpStream {
    pub(super) inner: Socket,
}

impl TcpStream {
    /// Opens a TCP connection to a remote host at the given `SocketAddr`
    ///
    /// On Windows, the socket is automatically bound to an unspecified address
    /// before connecting, as required by `ConnectEx`.
    pub async fn connect(addr: SocketAddr) -> std::io::Result<TcpStream> {
        let socket = Socket::new(addr, socket2::Type::STREAM)?;

        // ConnectEx on Windows requires the socket to be bound before calling it.
        // Bind to an ephemeral port on the appropriate unspecified address.
        #[cfg(windows)]
        {
            use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

            let bind_addr = if addr.is_ipv4() {
                socket2::SockAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
            } else {
                socket2::SockAddr::from(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0))
            };
            socket.handle.bind(&bind_addr)?;
        }

        socket.connect(socket2::SockAddr::from(addr)).await?;
        let tcp_stream = TcpStream { inner: socket };
        Ok(tcp_stream)
    }

    /// Closes the stream after in-flight operations complete.
    ///
    /// Prefer this over dropping when close errors must be observed.
    pub async fn close(self) -> std::io::Result<()> {
        self.inner.close().await
    }

    /// Shuts down the read, write, or both halves of this connection.
    ///
    /// This function will cause all pending and future I/O on the specified portions to return
    /// immediately with an appropriate value.
    pub fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
        self.inner.shutdown(how)
    }

    /// Sets the value of the TCP_NODELAY option on this socket.
    ///
    /// If set, this option disables the Nagle algorithm.
    /// This means that segments are always sent as soon as possible, even if there is only a small amount of data.
    /// When not set, data is buffered until there is a sufficient amount to send out,
    /// thereby avoiding the frequent sending of small packets.
    pub fn set_nodelay(&self, nodelay: bool) -> std::io::Result<()> {
        self.inner.set_nodelay(nodelay)
    }

    /// Returns the socket address of the local half of this TCP connection.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner
            .handle
            .local_addr()?
            .as_socket()
            .ok_or_else(|| std::io::Error::other("Could not get socket IP address"))
    }

    /// Returns the socket address of the remote half of this TCP connection.
    pub fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner
            .handle
            .peer_addr()?
            .as_socket()
            .ok_or_else(|| std::io::Error::other("Could not get peer IP address"))
    }

    /// Splits a [`TcpStream`] into a read half and a write half, which can be
    /// used to read and write the stream concurrently.
    ///
    /// The returned halves borrow the stream and cannot be moved into
    /// independently spawned tasks.
    pub fn split(&self) -> (ReadHalf<'_, Self>, WriteHalf<'_, Self>) {
        split(self)
    }

    /// Splits a [`TcpStream`] into an owned read half and an owned write half,
    /// which can be used to read and write the stream concurrently.
    ///
    /// Unlike [`split`](Self::split), each half owns the socket and can be
    /// moved into a separately spawned local task. Reunite matching halves
    /// with [`OwnedReadHalf::reunite`].
    pub fn into_split(self) -> (OwnedReadHalf<Self>, OwnedWriteHalf<Self>) {
        <Self as IntoOwnedSplit>::into_split(self)
    }

    /// Receive into a runtime-provided buffer (Linux only).
    ///
    /// Returns `Ok(None)` on EOF. `len == 0` uses the full pool buffer size.
    ///
    /// The returned [`PooledBuf`] is a **lease**: drop or
    /// [`PooledBuf::release`] it promptly. Holding many leases without
    /// recycling can exhaust the pool and fail later receives with `ENOBUFS`.
    ///
    /// Requires Linux 6.12+ (karmaio does not probe the kernel version).
    #[cfg(target_os = "linux")]
    #[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
    pub async fn recv_managed(&self, len: usize) -> std::io::Result<Option<PooledBuf>> {
        self.inner.recv_managed(len).await
    }

    /// Multishot receive stream of runtime-provided buffers (Linux only).
    ///
    /// One submission produces many completions until the kernel ends the
    /// request (final CQE without `MORE`), including on `ENOBUFS` when the
    /// pool is empty. There is **no auto-rearm**: call again after recycling
    /// outstanding [`PooledBuf`] leases if you need more data.
    ///
    /// Each item is a pool **lease**. Drop or [`PooledBuf::release`] promptly;
    /// holding every buffer without recycle is the usual cause of `ENOBUFS`.
    /// Each completion may use the full configured pool-buffer capacity.
    ///
    /// Requires Linux 6.12+ (karmaio does not probe the kernel version).
    #[cfg(target_os = "linux")]
    #[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
    pub fn recv_multi(&self) -> std::io::Result<impl Stream<Item = std::io::Result<PooledBuf>> + use<>> {
        self.inner.recv_multi()
    }
}

impl AsyncRead for &TcpStream {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.inner.recv(buf).await
    }

    async fn read_vectored<V: IoVectoredBufMut>(&mut self, bufs: V) -> BufResult<usize, V> {
        let (result, bufs) = self.inner.recvmsg(bufs).await.into_parts();
        BufResult(result.map(|(n, _)| n), bufs)
    }
}

impl AsyncRead for TcpStream {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.inner.recv(buf).await
    }

    async fn read_vectored<V: IoVectoredBufMut>(&mut self, bufs: V) -> BufResult<usize, V> {
        let (result, bufs) = self.inner.recvmsg(bufs).await.into_parts();
        BufResult(result.map(|(n, _)| n), bufs)
    }
}

#[cfg(target_os = "linux")]
impl AsyncReadManaged for TcpStream {
    type Buffer = PooledBuf;

    async fn read_managed(&mut self, len: usize) -> std::io::Result<Option<Self::Buffer>> {
        self.recv_managed(len).await
    }
}

#[cfg(target_os = "linux")]
impl AsyncReadManaged for &TcpStream {
    type Buffer = PooledBuf;

    async fn read_managed(&mut self, len: usize) -> std::io::Result<Option<Self::Buffer>> {
        self.recv_managed(len).await
    }
}

#[cfg(target_os = "linux")]
impl AsyncReadMulti for TcpStream {
    fn read_multi(&mut self) -> std::io::Result<impl Stream<Item = std::io::Result<Self::Buffer>>> {
        self.recv_multi()
    }
}

#[cfg(target_os = "linux")]
impl AsyncReadMulti for &TcpStream {
    fn read_multi(&mut self) -> std::io::Result<impl Stream<Item = std::io::Result<Self::Buffer>>> {
        self.recv_multi()
    }
}

impl AsyncWrite for &TcpStream {
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
        self.inner.shutdown(std::net::Shutdown::Write)
    }
}

impl AsyncWrite for TcpStream {
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
        self.inner.shutdown(std::net::Shutdown::Write)
    }
}

impl AsyncReadCancellable for TcpStream {
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

impl AsyncReadCancellable for &TcpStream {
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

impl AsyncWriteCancellable for TcpStream {
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

impl AsyncWriteCancellable for &TcpStream {
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

#[cfg(unix)]
impl FromRawFd for TcpStream {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        // Safety: caller guarantees `fd` is an open TCP stream socket.
        TcpStream::from(unsafe { Socket::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(windows)]
impl FromRawSocket for TcpStream {
    unsafe fn from_raw_socket(socket: RawSocket) -> Self {
        // Safety: caller guarantees `socket` is an open TCP stream socket.
        TcpStream::from(unsafe { Socket::from_raw_socket(socket) })
    }
}

#[cfg(windows)]
impl AsRawSocket for TcpStream {
    fn as_raw_socket(&self) -> RawSocket {
        self.inner.as_raw_socket()
    }
}

impl From<Socket> for TcpStream {
    fn from(inner: Socket) -> Self {
        Self { inner }
    }
}

impl IntoOwnedSplit for TcpStream {
    type ReadHalf = OwnedReadHalf<Self>;
    type WriteHalf = OwnedWriteHalf<Self>;

    fn into_split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        let read = OwnedReadHalf {
            inner: self.inner.clone(),
            _stream: std::marker::PhantomData,
        };
        let write = OwnedWriteHalf {
            inner: self.inner,
            shutdown_on_drop: true,
            _stream: std::marker::PhantomData,
        };
        (read, write)
    }
}

impl ReuniteOwned for TcpStream {
    type ReuniteError = ReuniteError<OwnedReadHalf<Self>, OwnedWriteHalf<Self>>;

    fn reunite(read: Self::ReadHalf, write: Self::WriteHalf) -> Result<Self, Self::ReuniteError> {
        read.reunite(write)
    }
}

impl TcpStream {
    /// Creates a stream from a standard-library TCP stream.
    ///
    /// This function is intended to wrap a TCP stream from the standard library.
    /// The conversion assumes nothing about the underlying socket.
    /// It is left up to the user to decide what socket options are appropriate for their use case.
    ///
    /// This can be used in conjunction with socket2's `Socket` interface to configure a socket before it's handed off,
    /// such as setting options like `reuse_address` or binding to multiple addresses.
    pub fn from_std(socket: std::net::TcpStream) -> std::io::Result<Self> {
        let inner = Socket::from_socket(socket2::Socket::from(socket))?;
        Ok(Self { inner })
    }
}
