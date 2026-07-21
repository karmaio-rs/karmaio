use std::net::SocketAddr;
use std::{io::Result, os::raw::c_int};

#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, AsSocket, BorrowedSocket, FromRawSocket, OwnedSocket, RawSocket};

use crate::buf::{BoundedIoBuf, BoundedIoBufMut, BufResult};
use crate::driver::helpers::io_handle::SharedIoHandle;
use crate::driver::ops::Op;

// This is an internal wrapper around socket operations for the runtime.
// This wrapper abstracts and handles all the driver operations and os compatiblity,
// presenting a clean, reusable api for the top level socket modules.
//
// The owned resource is a `socket2::Socket` so control-plane APIs (listen, nodelay,
// etc.) go through `SharedIoHandle`/`Deref` without reconverting through raw FDs.
#[derive(Clone)]
pub(crate) struct Socket {
    pub(crate) handle: SharedIoHandle<socket2::Socket>,
}

/// Configure a socket for async use on the current platform.
///
/// Sets non-blocking mode, and on macOS also `CLOEXEC` and `NOSIGPIPE`.
/// Used by create, bind, and accept paths so flags stay consistent.
fn configure_async_socket(socket: &socket2::Socket) -> Result<()> {
    socket.set_nonblocking(true)?;
    #[cfg(target_os = "macos")]
    {
        socket.set_cloexec(true)?;
        // Avoid SIGPIPE killing the process when writing to a closed socket.
        socket.set_nosigpipe(true)?;
    }
    Ok(())
}

impl Socket {
    pub(crate) fn set_async_flags(&self) -> Result<()> {
        configure_async_socket(&*self.handle)
    }

    /// Creates a new network socket (TCP/UDP)
    pub(crate) fn new(socket_addr: SocketAddr, socket_type: socket2::Type) -> Result<Self> {
        let socket = socket2::Socket::new(socket2::Domain::for_address(socket_addr), socket_type, None)?;
        configure_async_socket(&socket)?;

        Ok(Self {
            handle: SharedIoHandle::new(socket),
        })
    }

    /// Creates a new UNIX socket
    #[cfg(unix)]
    pub(crate) fn new_unix(socket_type: c_int) -> Result<Self> {
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket_type.into(), None)?;
        configure_async_socket(&socket)?;

        Ok(Self {
            handle: SharedIoHandle::new(socket),
        })
    }

    /// Binds a socket to the specified address.
    pub(crate) fn bind(socket_addr: SocketAddr, socket_type: socket2::Type) -> Result<Self> {
        Self::bind_internal(
            socket_addr.into(),
            socket2::Domain::for_address(socket_addr),
            socket_type,
        )
    }

    /// Binds a Unix domain socket to the specified path.
    #[cfg(unix)]
    pub(crate) fn bind_unix<P: AsRef<std::path::Path>>(path: P, socket_type: c_int) -> Result<Self> {
        let addr = socket2::SockAddr::unix(path.as_ref())?;
        Self::bind_internal(addr, socket2::Domain::UNIX, socket_type.into())
    }

    fn bind_internal(
        socket_addr: socket2::SockAddr,
        domain: socket2::Domain,
        socket_type: socket2::Type,
    ) -> Result<Socket> {
        let socket = socket2::Socket::new(domain, socket_type, None)?;

        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;

        configure_async_socket(&socket)?;

        socket.bind(&socket_addr)?;

        Ok(Self {
            handle: SharedIoHandle::new(socket),
        })
    }

    // ================================
    //  Connection Operations
    // ================================

    /// Initiates a connection to the specified address.
    pub(crate) async fn connect(&self, socket_addr: socket2::SockAddr) -> Result<()> {
        let op = Op::connect(&self.handle, socket_addr)?;
        op.await
    }

    /// Accepts a new incoming connection.
    pub(crate) async fn accept(&self) -> Result<(Self, Option<SocketAddr>)> {
        let op = Op::accept(&self.handle)?;
        op.await
    }

    // ================================
    //  Connection Control
    // ================================

    /// Begins listening for incoming connections.
    pub(crate) fn listen(&self, backlog: c_int) -> Result<()> {
        self.handle.listen(backlog)
    }

    /// Shuts down the read, write, or both halves of this connection.
    ///
    /// This function will cause all pending and future I/O on the specified portions to return immediately with an appropriate value.
    pub fn shutdown(&self, how: std::net::Shutdown) -> Result<()> {
        self.handle.shutdown(how)
    }

    /// Closes the socket, waiting for in-flight operations to complete.
    ///
    /// Prefer this over dropping when close errors must be observed. Drop still
    /// closes the handle synchronously when the last reference is dropped.
    pub(crate) async fn close(self) -> Result<()> {
        self.handle.close().await
    }

    /// Set the value of the `TCP_NODELAY` option on this socket.
    ///
    /// If set, this option disables the Nagle algorithm.
    /// This means that segments are always sent as soon as possible, even if there is only a small amount of data.
    /// When not set, data is buffered until there is a sufficient amount to send out, thereby avoiding the frequent sending of small packets.
    pub fn set_nodelay(&self, nodelay: bool) -> Result<()> {
        self.handle.set_tcp_nodelay(nodelay)
    }

    // ================================
    //  Read Operations
    // ================================

    /// Reads a message from the socket from the connected address
    pub(crate) async fn recv<B: BoundedIoBufMut>(&self, buf: B) -> BufResult<usize, B> {
        let op = Op::recv(&self.handle, buf).unwrap();
        op.await
    }

    /// Reads a message from the socket along with the receiver address
    pub(crate) async fn recv_from<B: BoundedIoBufMut>(&self, buf: B) -> BufResult<(usize, SocketAddr), B> {
        let op = Op::recv_from(&self.handle, buf).unwrap();
        op.await
    }

    /// Performs a scattered read into the supplied buffers along with the receiver address
    pub(crate) async fn recvmsg<B: BoundedIoBufMut>(&self, buf: Vec<B>) -> BufResult<(usize, SocketAddr), Vec<B>> {
        let op = Op::recvmsg(&self.handle, buf).unwrap();
        op.await
    }

    // ================================
    //  Write Operations
    // ================================

    /// Writes the buffer on the connected socket
    pub(crate) async fn send<B: BoundedIoBuf>(&self, buf: B) -> BufResult<usize, B> {
        let op = Op::send(&self.handle, buf).unwrap();
        op.await
    }

    /// Writes the buffer to the specified address on the socket
    pub(crate) async fn send_to<B: BoundedIoBuf>(&self, buf: B, socket_addr: SocketAddr) -> BufResult<usize, B> {
        let op = Op::send_to(&self.handle, buf, socket_addr).unwrap();
        op.await
    }

    /// Performes a gather write on the socket with data from the specified buffers
    /// Needs an address if the socket is not connected to an address
    pub(crate) async fn sendmsg<B: BoundedIoBuf, C: BoundedIoBuf>(
        &self,
        io_slices: Vec<B>,
        socket_addr: Option<SocketAddr>,
        control: Option<C>,
    ) -> BufResult<(usize, Option<C>), Vec<B>> {
        let op = Op::sendmsg(&self.handle, io_slices, control, socket_addr).unwrap();
        op.await
    }
}

#[cfg(unix)]
impl AsRawFd for Socket {
    fn as_raw_fd(&self) -> RawFd {
        self.handle.raw_fd()
    }
}

#[cfg(unix)]
impl AsFd for Socket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.handle.as_fd()
    }
}

#[cfg(windows)]
impl AsSocket for Socket {
    fn as_socket(&self) -> BorrowedSocket<'_> {
        // Safety: `self.handle.raw_socket()` returns a valid, open socket
        // that is owned by this `Socket` and will remain valid for the lifetime of `&self`.
        unsafe { BorrowedSocket::borrow_raw(self.handle.raw_socket()) }
    }
}

#[cfg(windows)]
impl AsRawSocket for Socket {
    fn as_raw_socket(&self) -> RawSocket {
        self.handle.raw_socket()
    }
}

impl From<SharedIoHandle<socket2::Socket>> for Socket {
    fn from(value: SharedIoHandle<socket2::Socket>) -> Self {
        Self { handle: value }
    }
}

impl From<socket2::Socket> for Socket {
    fn from(socket: socket2::Socket) -> Self {
        Self {
            handle: SharedIoHandle::new(socket),
        }
    }
}

#[cfg(unix)]
impl From<OwnedFd> for Socket {
    fn from(fd: OwnedFd) -> Self {
        Self::from(socket2::Socket::from(fd))
    }
}

#[cfg(windows)]
impl From<OwnedSocket> for Socket {
    fn from(socket: OwnedSocket) -> Self {
        Self::from(socket2::Socket::from(socket))
    }
}

#[cfg(unix)]
impl FromRawFd for Socket {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        // Safety: caller guarantees `fd` is an open socket; ownership transfers here.
        Self::from(unsafe { socket2::Socket::from_raw_fd(fd) })
    }
}

#[cfg(windows)]
impl FromRawSocket for Socket {
    unsafe fn from_raw_socket(socket: RawSocket) -> Self {
        // Safety: caller guarantees `socket` is open; ownership transfers here.
        Self::from(unsafe { socket2::Socket::from_raw_socket(socket) })
    }
}

#[cfg(unix)]
impl From<std::net::TcpStream> for Socket {
    fn from(socket: std::net::TcpStream) -> Self {
        Self::from(socket2::Socket::from(socket))
    }
}

#[cfg(windows)]
impl From<std::net::TcpStream> for Socket {
    fn from(socket: std::net::TcpStream) -> Self {
        Self::from(socket2::Socket::from(socket))
    }
}

#[cfg(unix)]
impl From<std::net::TcpListener> for Socket {
    fn from(socket: std::net::TcpListener) -> Self {
        Self::from(socket2::Socket::from(socket))
    }
}

#[cfg(windows)]
impl From<std::net::TcpListener> for Socket {
    fn from(socket: std::net::TcpListener) -> Self {
        Self::from(socket2::Socket::from(socket))
    }
}

#[cfg(unix)]
impl From<std::net::UdpSocket> for Socket {
    fn from(socket: std::net::UdpSocket) -> Self {
        Self::from(socket2::Socket::from(socket))
    }
}

#[cfg(windows)]
impl From<std::net::UdpSocket> for Socket {
    fn from(socket: std::net::UdpSocket) -> Self {
        Self::from(socket2::Socket::from(socket))
    }
}

#[cfg(unix)]
impl From<std::os::unix::net::UnixStream> for Socket {
    fn from(socket: std::os::unix::net::UnixStream) -> Self {
        Self::from(socket2::Socket::from(socket))
    }
}

#[cfg(unix)]
impl From<std::os::unix::net::UnixListener> for Socket {
    fn from(socket: std::os::unix::net::UnixListener) -> Self {
        Self::from(socket2::Socket::from(socket))
    }
}
