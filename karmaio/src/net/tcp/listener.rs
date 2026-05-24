use std::net::SocketAddr;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

#[cfg(unix)]
use crate::driver::helpers::io_handle::SharedIoHandle;
use crate::{driver::helpers::socket::Socket, net::tcp::TcpStream};

#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, RawSocket};

/// A TCP socket server listening for connections.
///
/// You can accept a new connection by using the [`accept`](`TcpListener::accept`) method.
pub struct TcpListener {
    inner: Socket,
}

impl TcpListener {
    /// Creates a new TcpListener, which will be bound to the specified address.
    ///
    /// The returned listener is ready for accepting connections.
    ///
    /// Binding with a port number of 0 will request that the OS to assign a port to this listener.
    pub fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        let socket = Socket::bind(addr, libc::SOCK_STREAM)?;
        // TODO: Make this configurable?
        socket.listen(1024)?;
        Ok(TcpListener { inner: socket })
    }

    /// Returns the local address that this listener is bound to.
    ///
    /// This can be useful, for example, when binding to port 0 to figure out which port was actually bound.
    #[cfg(unix)]
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        use std::os::fd::{AsRawFd, FromRawFd};

        let fd = self.inner.as_raw_fd();
        // SAFETY: Our fd is the handle the kernel has given us for a TcpListener.
        // Create a std::net::TcpListener long enough to call its local_addr method
        // and then forget it so the socket is not closed here.
        let l = unsafe { std::net::TcpListener::from_raw_fd(fd) };
        let local_addr = l.local_addr();
        std::mem::forget(l);
        local_addr
    }

    /// Returns the local address to which this UDP socket is bound.
    ///
    /// This can be useful, for example, when binding to port 0 to figure out which port was actually bound.
    #[cfg(windows)]
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        use std::os::windows::io::{AsRawSocket, FromRawSocket};

        let handle = self.inner.as_raw_socket();
        // SAFETY: Our handle is the handle the kernel has given us for a TcpListener.
        // Create a std::net::TcpListener long enough to call its local_addr method
        // and then forget it so the socket is not closed here.
        let l = unsafe { std::net::TcpListener::from_raw_socket(handle) };
        let local_addr = l.local_addr();
        std::mem::forget(l);
        local_addr
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
        TcpListener::from(Socket::from(SharedIoHandle::new(fd)))
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
        TcpListener::from(Socket::from(SharedIoHandle::new_socket(socket)))
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
