use std::net::SocketAddr;
use std::time::Duration;

use socket2::SockRef;

use crate::driver::helpers::socket::Socket;
use crate::net::tcp::{TcpListener, TcpStream};

use crate::driver::helpers::io_handle::SharedIoHandle;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, RawSocket};

/// A TCP socket that has not yet been bound to or connected to anything.
///
/// `TcpSocket` provides a builder-like API that allows you to
/// configure a TCP socket before it is bound or connected.
///
/// Once configured, you can call [`bind`](TcpSocket::bind) to bind it to an address,
/// and then [`listen`](TcpSocket::listen) to start listening,
/// or [`connect`](TcpSocket::connect) to connect to a remote peer.
/// A TCP socket that has not yet been converted into a listener or stream.
///
/// # Closing
///
/// Prefer [`TcpSocket::close`] so close errors are reported. Drop still closes
/// the OS socket synchronously when the last reference is dropped.
pub struct TcpSocket {
    pub(super) inner: Socket,
}

impl TcpSocket {
    /// Closes the socket after in-flight operations complete.
    pub async fn close(self) -> std::io::Result<()> {
        self.inner.close().await
    }

    /// Creates a new socket configured for IPv4 TCP.
    pub fn new_v4() -> std::io::Result<Self> {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;

        let inner = Socket::from(socket);

        inner.set_async_flags()?;

        Ok(Self { inner })
    }

    /// Creates a new socket configured for IPv6 TCP.
    pub fn new_v6() -> std::io::Result<Self> {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV6,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;

        let inner = Socket::from(socket);

        inner.set_async_flags()?;

        Ok(Self { inner })
    }

    /// Binds the socket to the given address.
    ///
    /// This consumes the socket and returns a bound `TcpSocket`.
    pub fn bind(self, addr: SocketAddr) -> std::io::Result<Self> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.bind(&addr.into())?;
        Ok(self)
    }

    /// Initiates a connection to the given address.
    ///
    /// This consumes the socket; once the connection is established,
    /// a [`TcpStream`] is returned.
    pub async fn connect(self, addr: SocketAddr) -> std::io::Result<TcpStream> {
        self.inner.connect(socket2::SockAddr::from(addr)).await?;
        Ok(TcpStream { inner: self.inner })
    }

    /// Listens for incoming connections on the bound socket.
    ///
    /// This consumes the socket and returns a [`TcpListener`].
    pub fn listen(self, backlog: u32) -> std::io::Result<TcpListener> {
        self.inner.listen(backlog as i32)?;
        Ok(TcpListener { inner: self.inner })
    }

    /// Returns the local address that this socket is bound to.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref
            .local_addr()?
            .as_socket()
            .ok_or_else(|| std::io::Error::other("Could not get socket IP address"))
    }

    // Socket options

    /// Sets the value of `TCP_NODELAY` on this socket.
    pub fn set_nodelay(&self, nodelay: bool) -> std::io::Result<()> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.set_tcp_nodelay(nodelay)
    }

    /// Gets the value of the `TCP_NODELAY` option on this socket.
    pub fn nodelay(&self) -> std::io::Result<bool> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.tcp_nodelay()
    }

    /// Sets the value of `SO_REUSEADDR` on this socket.
    pub fn set_reuseaddr(&self, reuseaddr: bool) -> std::io::Result<()> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.set_reuse_address(reuseaddr)
    }

    /// Gets the value of the `SO_REUSEADDR` option on this socket.
    pub fn reuseaddr(&self) -> std::io::Result<bool> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.reuse_address()
    }

    /// Sets the value of `SO_REUSEPORT` on this socket.
    #[cfg(unix)]
    pub fn set_reuseport(&self, reuseport: bool) -> std::io::Result<()> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.set_reuse_port(reuseport)
    }

    /// Gets the value of the `SO_REUSEPORT` option on this socket.
    #[cfg(unix)]
    pub fn reuseport(&self) -> std::io::Result<bool> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.reuse_port()
    }

    /// Sets the value of `SO_KEEPALIVE` on this socket.
    pub fn set_keepalive(&self, keepalive: bool) -> std::io::Result<()> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.set_keepalive(keepalive)
    }

    /// Gets the value of the `SO_KEEPALIVE` option on this socket.
    pub fn keepalive(&self) -> std::io::Result<bool> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.keepalive()
    }

    /// Sets the value of `SO_LINGER` on this socket.
    pub fn set_linger(&self, dur: Option<Duration>) -> std::io::Result<()> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.set_linger(dur)
    }

    /// Gets the value of the `SO_LINGER` option on this socket.
    pub fn linger(&self) -> std::io::Result<Option<Duration>> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.linger()
    }

    /// Sets the value of `SO_RCVBUF` on this socket.
    pub fn set_recv_buffer_size(&self, size: u32) -> std::io::Result<()> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.set_recv_buffer_size(size as usize)
    }

    /// Gets the value of the `SO_RCVBUF` option on this socket.
    pub fn recv_buffer_size(&self) -> std::io::Result<u32> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.recv_buffer_size().map(|s| s as u32)
    }

    /// Sets the value of `SO_SNDBUF` on this socket.
    pub fn set_send_buffer_size(&self, size: u32) -> std::io::Result<()> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.set_send_buffer_size(size as usize)
    }

    /// Gets the value of the `SO_SNDBUF` option on this socket.
    pub fn send_buffer_size(&self) -> std::io::Result<u32> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.send_buffer_size().map(|s| s as u32)
    }

    /// Sets the value of `IP_TTL` on this socket (IPv4).
    pub fn set_ttl_v4(&self, ttl: u32) -> std::io::Result<()> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.set_ttl_v4(ttl)
    }

    /// Gets the value of the `IP_TTL` option on this socket (IPv4).
    pub fn ttl_v4(&self) -> std::io::Result<u32> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.ttl_v4()
    }

    /// Sets the value of `IPV6_UNICAST_HOPS` on this socket (IPv6).
    pub fn set_unicast_hops_v6(&self, hops: u32) -> std::io::Result<()> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.set_unicast_hops_v6(hops)
    }

    /// Gets the value of the `IPV6_UNICAST_HOPS` option on this socket (IPv6).
    pub fn unicast_hops_v6(&self) -> std::io::Result<u32> {
        let sock_ref = SockRef::from(&self.inner);
        sock_ref.unicast_hops_v6()
    }
}

#[cfg(unix)]
impl FromRawFd for TcpSocket {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        TcpSocket::from(Socket::from(unsafe { SharedIoHandle::from_raw_fd(fd) }))
    }
}

#[cfg(unix)]
impl AsRawFd for TcpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(windows)]
impl FromRawSocket for TcpSocket {
    unsafe fn from_raw_socket(socket: RawSocket) -> Self {
        TcpSocket::from(Socket::from(unsafe { SharedIoHandle::from_raw_socket(socket) }))
    }
}

#[cfg(windows)]
impl AsRawSocket for TcpSocket {
    fn as_raw_socket(&self) -> RawSocket {
        self.inner.as_raw_socket()
    }
}

impl From<Socket> for TcpSocket {
    fn from(inner: Socket) -> Self {
        Self { inner }
    }
}
