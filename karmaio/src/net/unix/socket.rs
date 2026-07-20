use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::Path;
use std::time::Duration;

use socket2::SockAddr;

use crate::driver::helpers::io_handle::SharedIoHandle;
use crate::driver::helpers::socket::Socket;
use crate::net::unix::{UnixListener, UnixStream};

/// A Unix domain socket that has not yet been bound to or connected to anything.
///
/// `UnixSocket` provides a builder-like API that allows you to
/// configure a Unix domain socket before it is bound or connected.
///
/// Once configured, you can call [`bind`](UnixSocket::bind) to bind it to a path,
/// and then [`listen`](UnixSocket::listen) to start listening,
/// or [`connect`](UnixSocket::connect) to connect to a remote peer.
///
/// # Closing
///
/// Prefer [`UnixSocket::close`] so close errors are reported. Drop still closes
/// the OS socket synchronously when the last reference is dropped.
pub struct UnixSocket {
    pub(super) inner: Socket,
}

impl UnixSocket {
    /// Closes the socket after in-flight operations complete.
    pub async fn close(self) -> std::io::Result<()> {
        self.inner.close().await
    }

    /// Creates a new Unix domain stream socket.
    pub fn new() -> std::io::Result<Self> {
        let inner = Socket::new_unix(libc::SOCK_STREAM)?;

        inner.set_async_flags()?;

        Ok(Self { inner })
    }

    /// Binds the socket to the given path.
    ///
    /// This consumes the socket and returns a bound `UnixSocket`.
    pub fn bind<P: AsRef<Path>>(self, path: P) -> std::io::Result<Self> {
        let addr = SockAddr::unix(path)?;
        let sock_ref = socket2::SockRef::from(&self.inner);
        sock_ref.bind(&addr)?;

        self.inner.set_async_flags()?;

        Ok(self)
    }

    /// Initiates a connection to the given path.
    ///
    /// This consumes the socket; once the connection is established,
    /// a [`UnixStream`] is returned.
    pub async fn connect<P: AsRef<Path>>(self, path: P) -> std::io::Result<UnixStream> {
        self.inner.connect(SockAddr::unix(path)?).await?;
        Ok(UnixStream { inner: self.inner })
    }

    /// Listens for incoming connections on the bound socket.
    ///
    /// This consumes the socket and returns a [`UnixListener`].
    pub fn listen(self, backlog: u32) -> std::io::Result<UnixListener> {
        self.inner.listen(backlog as i32)?;
        Ok(UnixListener { inner: self.inner })
    }

    /// Returns the local address that this socket is bound to.
    pub fn local_addr(&self) -> std::io::Result<std::os::unix::net::SocketAddr> {
        let sock_ref = socket2::SockRef::from(&self.inner);
        sock_ref
            .local_addr()?
            .as_unix()
            .ok_or_else(|| std::io::Error::other("Could not get socket path"))
    }

    // Socket options

    /// Sets the value of `SO_REUSEADDR` on this socket.
    pub fn set_reuseaddr(&self, reuseaddr: bool) -> std::io::Result<()> {
        let sock_ref = socket2::SockRef::from(&self.inner);
        sock_ref.set_reuse_address(reuseaddr)
    }

    /// Gets the value of the `SO_REUSEADDR` option on this socket.
    pub fn reuseaddr(&self) -> std::io::Result<bool> {
        let sock_ref = socket2::SockRef::from(&self.inner);
        sock_ref.reuse_address()
    }

    /// Sets the value of `SO_KEEPALIVE` on this socket.
    pub fn set_keepalive(&self, keepalive: bool) -> std::io::Result<()> {
        let sock_ref = socket2::SockRef::from(&self.inner);
        sock_ref.set_keepalive(keepalive)
    }

    /// Gets the value of the `SO_KEEPALIVE` option on this socket.
    pub fn keepalive(&self) -> std::io::Result<bool> {
        let sock_ref = socket2::SockRef::from(&self.inner);
        sock_ref.keepalive()
    }

    /// Sets the value of `SO_LINGER` on this socket.
    pub fn set_linger(&self, dur: Option<Duration>) -> std::io::Result<()> {
        let sock_ref = socket2::SockRef::from(&self.inner);
        sock_ref.set_linger(dur)
    }

    /// Gets the value of the `SO_LINGER` option on this socket.
    pub fn linger(&self) -> std::io::Result<Option<Duration>> {
        let sock_ref = socket2::SockRef::from(&self.inner);
        sock_ref.linger()
    }

    /// Sets the value of `SO_RCVBUF` on this socket.
    pub fn set_recv_buffer_size(&self, size: u32) -> std::io::Result<()> {
        let sock_ref = socket2::SockRef::from(&self.inner);
        sock_ref.set_recv_buffer_size(size as usize)
    }

    /// Gets the value of the `SO_RCVBUF` option on this socket.
    pub fn recv_buffer_size(&self) -> std::io::Result<u32> {
        let sock_ref = socket2::SockRef::from(&self.inner);
        sock_ref.recv_buffer_size().map(|s| s as u32)
    }

    /// Sets the value of `SO_SNDBUF` on this socket.
    pub fn set_send_buffer_size(&self, size: u32) -> std::io::Result<()> {
        let sock_ref = socket2::SockRef::from(&self.inner);
        sock_ref.set_send_buffer_size(size as usize)
    }

    /// Gets the value of the `SO_SNDBUF` option on this socket.
    pub fn send_buffer_size(&self) -> std::io::Result<u32> {
        let sock_ref = socket2::SockRef::from(&self.inner);
        sock_ref.send_buffer_size().map(|s| s as u32)
    }
}

impl FromRawFd for UnixSocket {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        UnixSocket::from(Socket::from(unsafe { SharedIoHandle::from_raw_fd(fd) }))
    }
}

impl AsRawFd for UnixSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl From<Socket> for UnixSocket {
    fn from(inner: Socket) -> Self {
        Self { inner }
    }
}
