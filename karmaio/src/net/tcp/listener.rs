use std::net::SocketAddr;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

use crate::{driver::helpers::socket::Socket, net::tcp::TcpStream};

#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, RawSocket};

#[cfg(target_os = "linux")]
use crate::driver::helpers::socket::Incoming;
#[cfg(target_os = "linux")]
use crate::io::Stream;

/// A TCP socket server listening for connections.
///
/// You can accept a new connection by using the [`accept`](`TcpListener::accept`) method.
///
/// On Linux, `TcpListener::incoming` provides a stream of accepts
/// backed by io_uring multishot accept.
///
/// # Closing
///
/// Prefer [`TcpListener::close`] so close errors are reported. Dropping the listener
/// still closes the OS socket synchronously when the last reference is dropped.
pub struct TcpListener {
    pub(super) inner: Socket,
}

/// A stream of incoming TCP connections (Linux only).
///
/// Produced by [`TcpListener::incoming`]. Thin public mapping over the shared
/// [`Socket`] accept stream: each item is a connected [`TcpStream`] and peer
/// [`SocketAddr`]. Dropping the stream cancels the underlying accept request.
///
/// # Implementation notes
///
/// On Linux this uses **io_uring multishot accept** (requires kernel **6.12+**).
/// karmaio does not probe the kernel version at runtime. The multishot SQE is
/// **not** automatically re-armed after it terminates (error, cancel, or kernel
/// disarm). Call [`TcpListener::incoming`] again to start a new request.
/// Pending accepted sockets are bounded by
/// [`RuntimeBuilder::multishot_accept_capacity`](crate::RuntimeBuilder::multishot_accept_capacity).
/// Kernel submission is deferred until the first [`Stream::next`] poll.
#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
pub struct TcpIncoming {
    inner: Incoming,
}

#[cfg(target_os = "linux")]
impl Stream for TcpIncoming {
    type Item = std::io::Result<(TcpStream, SocketAddr)>;

    async fn next(&mut self) -> Option<Self::Item> {
        let item = self.inner.next().await?;
        Some(match item {
            Ok((socket, addr)) => addr
                .ok_or_else(|| std::io::Error::other("Could not get socket IP address"))
                .map(|addr| (TcpStream { inner: socket }, addr)),
            Err(err) => Err(err),
        })
    }
}

impl TcpListener {
    /// Creates a new TcpListener, which will be bound to the specified address.
    ///
    /// The returned listener is ready for accepting connections.
    ///
    /// Binding with a port number of 0 will request that the OS to assign a port to this listener.
    pub fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        let socket = Socket::bind(addr, socket2::Type::STREAM)?;
        socket.listen(128)?;
        Ok(TcpListener { inner: socket })
    }

    /// Closes the listener after in-flight operations complete.
    ///
    /// Prefer this over dropping when close errors must be observed.
    pub async fn close(self) -> std::io::Result<()> {
        self.inner.close().await
    }

    /// Returns the local address that this listener is bound to.
    ///
    /// This can be useful, for example, when binding to port 0 to figure out which port was actually bound.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner
            .handle
            .local_addr()?
            .as_socket()
            .ok_or_else(|| std::io::Error::other("Could not get socket IP address"))
    }

    /// Accepts a new incoming connection from this listener.
    ///
    /// This function will yield once a new TCP connection is established.
    /// When established, the corresponding [`TcpStream`] and the remote peer's address will be returned.
    pub async fn accept(&self) -> std::io::Result<(TcpStream, SocketAddr)> {
        let (socket, socket_addr) = self.inner.accept().await?;
        let stream = TcpStream { inner: socket };
        let socket_addr = socket_addr.ok_or_else(|| std::io::Error::other("Could not get socket IP address"))?;
        Ok((stream, socket_addr))
    }

    /// Returns a stream of incoming connections to this listener (Linux only).
    ///
    /// Prefer this over a manual `loop { accept().await }` when accepting many
    /// connections on Linux. Submission lives on the shared [`Socket`] helper
    /// (same layer as oneshot [`accept`](Self::accept)); this method maps
    /// accepted sockets into [`TcpStream`].
    ///
    /// # Implementation notes
    ///
    /// Backed by **io_uring multishot accept** (Linux **6.12+**). The runtime
    /// does not probe the kernel version. Submission occurs on the first
    /// [`Stream::next`] poll so [`crate::io::StreamExt::with_cancellation`] can
    /// wrap the stream before it reaches the kernel. Wrap it before that first
    /// poll; wrapping after submission does not attach the existing request.
    /// Submission failures are yielded as the first error item. The multishot
    /// request is **not** re-armed after it ends; call this method again to
    /// start a new stream.
    /// Dropping the returned stream cancels the in-flight request.
    /// If its pending-connection capacity is reached, overflow sockets are
    /// closed and the stream terminates with a capacity error after yielding
    /// connections that were already queued.
    ///
    /// # Concurrent use
    ///
    /// Do not run more than one incoming stream at a time on the same listener
    /// on the same runtime thread.
    #[cfg(target_os = "linux")]
    #[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
    pub fn incoming(&self) -> std::io::Result<TcpIncoming> {
        Ok(TcpIncoming {
            inner: self.inner.incoming()?,
        })
    }
}

#[cfg(unix)]
impl FromRawFd for TcpListener {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        // Safety: caller guarantees `fd` is an open TCP listener socket.
        TcpListener::from(unsafe { Socket::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
impl AsRawFd for TcpListener {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(windows)]
impl FromRawSocket for TcpListener {
    unsafe fn from_raw_socket(socket: RawSocket) -> Self {
        // Safety: caller guarantees `socket` is an open TCP listener socket.
        TcpListener::from(unsafe { Socket::from_raw_socket(socket) })
    }
}

#[cfg(windows)]
impl AsRawSocket for TcpListener {
    fn as_raw_socket(&self) -> RawSocket {
        self.inner.as_raw_socket()
    }
}

impl From<Socket> for TcpListener {
    fn from(inner: Socket) -> Self {
        Self { inner }
    }
}

impl TcpListener {
    /// Creates a listener from a previously bound standard-library listener.
    ///
    /// This function is intended to wrap a TCP listener from the standard library.
    /// The conversion assumes nothing about the underlying socket.
    /// It is left up to the user to decide what socket options are appropriate for their use case.
    ///
    /// This can be used in conjunction with socket2's `Socket` interface to configure a socket before it's handed off,
    /// such as setting options like `reuse_address` or binding to multiple addresses.
    pub fn from_std(socket: std::net::TcpListener) -> std::io::Result<Self> {
        let inner = Socket::from_socket(socket2::Socket::from(socket))?;
        Ok(Self { inner })
    }
}
