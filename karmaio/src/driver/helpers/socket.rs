use std::net::SocketAddr;
use std::os::fd::{AsFd, IntoRawFd};
use std::{io::Result, os::raw::c_int};

use crate::buf::{BoundedIoBuf, BoundedIoBufMut, BufResult};
use crate::driver::helpers::io_handle::SharedIoHandle;
use crate::driver::ops::Op;

#[cfg(unix)]
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};

#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, AsRawSocket, AsSocket, BorrowedSocket, IntoRawSocket, RawSocket};

// This is an internal wrapper around socket operations for the runtime.
// This wrapper abstracts and handles all the driver operations and os compatiblity,
// presenting a clean, reusable api for the top level socket modules
#[derive(Clone)]
pub(crate) struct Socket {
    // Open file descriptor
    pub(crate) handle: SharedIoHandle,
}

impl Socket {
    /// Creates a new network socket (TCP/UDP)
    pub(crate) fn new(socket_addr: SocketAddr, socket_type: c_int) -> Result<Self> {
        let socket = socket2::Socket::new(socket2::Domain::for_address(socket_addr), socket_type.into(), None)?;

        socket.set_nonblocking(true)?;

        // This will not crash the entire program when writing to a closed socket
        #[cfg(target_os = "macos")]
        socket.set_nosigpipe(true)?;

        let handle = Self::shared_handle_from_socket(socket)?;

        Ok(Self { handle })
    }

    /// Creates a new UNIX socket
    #[cfg(unix)]
    pub(crate) fn new_unix(socket_type: c_int) -> Result<Self> {
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket_type.into(), None)?;

        socket.set_nonblocking(true)?;

        // This will not crash the entire program when writing to a closed socket
        #[cfg(target_os = "macos")]
        socket.set_nosigpipe(true)?;

        let handle = Self::shared_handle_from_socket(socket)?;

        Ok(Self { handle })
    }

    /// Binds a socket to the specified address.
    pub(crate) fn bind(socket_addr: SocketAddr, socket_type: c_int) -> Result<Self> {
        Self::bind_internal(
            socket_addr.into(),
            socket2::Domain::for_address(socket_addr),
            socket_type.into(),
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
        socket.set_reuse_port(true)?;
        socket.set_nonblocking(true)?;

        // This will not crash the entire program when writing to a closed socket
        #[cfg(target_os = "macos")]
        socket.set_nosigpipe(true)?;

        socket.bind(&socket_addr)?;

        let handle = Self::shared_handle_from_socket(socket)?;

        Ok(Self { handle })
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
        let socket = socket2::SockRef::from(self);
        socket.listen(backlog)
    }

    /// Shuts down the read, write, or both halves of this connection.
    ///
    /// This function will cause all pending and future I/O on the specified portions to return immediately with an appropriate value.
    pub fn shutdown(&self, how: std::net::Shutdown) -> Result<()> {
        let socket_ref = socket2::SockRef::from(self);
        socket_ref.shutdown(how)
    }

    /// Set the value of the `TCP_NODELAY` option on this socket.
    ///
    /// If set, this option disables the Nagle algorithm.
    /// This means that segments are always sent as soon as possible, even if there is only a small amount of data.
    /// When not set, data is buffered until there is a sufficient amount to send out, thereby avoiding the frequent sending of small packets.
    pub fn set_nodelay(&self, nodelay: bool) -> Result<()> {
        let socket_ref = socket2::SockRef::from(self);
        socket_ref.set_tcp_nodelay(nodelay)
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

    #[cfg(unix)]
    fn shared_handle_from_socket(socket: socket2::Socket) -> Result<SharedIoHandle> {
        Ok(SharedIoHandle::new(socket.into_raw_fd()))
    }

    #[cfg(windows)]
    fn shared_handle_from_socket(socket: socket2::Socket) -> Result<SharedIoHandle> {
        Ok(SharedIoHandle::new_socket(socket.into_raw_socket()))
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
        // Safety: `self.handle.raw_fd()` returns a valid, open file descriptor
        // that is owned by this `Socket` and will remain valid for the lifetime of `&self`.
        unsafe { BorrowedFd::borrow_raw(self.handle.raw_fd()) }
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
    // Required method
    fn as_raw_socket(&self) -> RawSocket {
        self.handle.raw_socket()
    }
}

impl From<SharedIoHandle> for Socket {
    fn from(value: SharedIoHandle) -> Self {
        Self { handle: value }
    }
}

#[cfg(unix)]
impl<T: IntoRawFd> From<T> for Socket {
    fn from(socket: T) -> Self {
        let fd = SharedIoHandle::new(socket.into_raw_fd());
        Self::from(fd)
    }
}

#[cfg(windows)]
impl<T: IntoRawSocket> From<T> for Socket {
    fn from(socket: T) -> Self {
        let socket = SharedIoHandle::new_socket(socket.into_raw_socket());
        Self::from(socket)
    }
}
