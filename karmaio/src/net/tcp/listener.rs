use std::net::SocketAddr;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

use crate::{driver::helpers::socket::Socket, net::tcp::TcpStream};

#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, RawSocket};

/// A TCP socket server listening for connections.
///
/// You can accept a new connection by using the [`accept`](`TcpListener::accept`) method.
///
/// # Closing
///
/// Prefer [`TcpListener::close`] so close errors are reported. Dropping the listener
/// still closes the OS socket synchronously when the last reference is dropped.
pub struct TcpListener {
    pub(super) inner: Socket,
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

impl From<std::net::TcpListener> for TcpListener {
    /// Creates new `TcpListener` from a previously bound `std::net::TcpListener`.
    ///
    /// This function is intended to be used to wrap a TCP listener from the standard library.
    /// The conversion assumes nothing about the underlying socket.
    /// It is left up to the user to decide what socket options are appropriate for their use case.
    ///
    /// This can be used in conjunction with socket2's `Socket` interface to configure a socket before it's handed off,
    /// such as setting options like `reuse_address` or binding to multiple addresses.
    fn from(socket: std::net::TcpListener) -> Self {
        let inner = Socket::from(socket);
        Self { inner }
    }
}
