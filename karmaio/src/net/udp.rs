use std::{
    net::SocketAddr,
    os::fd::{AsRawFd, FromRawFd, RawFd},
};

use socket2::SockAddr;

use crate::{
    buf::{BoundedIoBuf, BoundedIoBufMut, BufResult},
    driver::helpers::{io_handle::SharedIoHandle, socket::Socket},
};

#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, RawSocket};

/// A UDP socket.
///
/// UDP is "connectionless" protocol, unlike TCP.
/// A `UdpSocket` is free to communicate with many different remotes, regardless of what address you've bound to.
///
/// In karmaio, there are basically two main ways to use `UdpSocket`:
/// - one to many: [`bind`](`UdpSocket::bind`) and use [`send_to`](`UdpSocket::send_to`)
///   and [`recv_from`](`UdpSocket::recv_from`) to communicate with many different addresses
/// - one to one: [`connect`](`UdpSocket::connect`) and associate with a single address, using [`write`](`UdpSocket::write`)
///   and [`read`](`UdpSocket::read`) to communicate only with that remote address
pub struct UdpSocket {
    pub(super) inner: Socket,
}

impl UdpSocket {
    /// Creates a new UDP socket and attempt to bind it to the addr provided.
    ///
    /// Returns a new instance of [`UdpSock  et`] on success,
    /// or an [`io::Error`](std::io::Error) on failure.
    pub async fn bind(socket_addr: SocketAddr) -> std::io::Result<UdpSocket> {
        let socket = Socket::bind(socket_addr, libc::SOCK_DGRAM)?;
        Ok(UdpSocket { inner: socket })
    }

    /// Returns the local address to which this UDP socket is bound.
    ///
    /// This can be useful, for example, when binding to port 0 to figure out which port was actually bound.
    #[cfg(unix)]
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        let fd = self.inner.as_raw_fd();
        // SAFETY: Our fd is the handle the kernel has given us for a UdpSocket.
        // Create a std::net::UdpSocket long enough to call its local_addr method
        // and then forget it so the socket is not closed here.
        let s = unsafe { std::net::UdpSocket::from_raw_fd(fd) };
        let local_addr = s.local_addr();
        std::mem::forget(s);
        local_addr
    }

    /// Returns the local address to which this UDP socket is bound.
    ///
    /// This can be useful, for example, when binding to port 0 to figure out which port was actually bound.
    #[cfg(windows)]
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        use std::os::windows::io::{AsRawSocket, FromRawSocket};

        let handle = self.inner.as_raw_socket();
        // SAFETY: Our handle is the handle the kernel has given us for a UdpSocket.
        // Create a std::net::UdpSocket long enough to call its local_addr method
        // and then forget it so the socket is not closed here.
        let s = unsafe { std::net::UdpSocket::from_raw_socket(handle) };
        let local_addr = s.local_addr();
        std::mem::forget(s);
        local_addr
    }

    /// "Connects" this UDP socket to a remote address.
    ///
    /// This enables `write` and `read` syscalls to be used on this instance.
    /// It also constrains the `read` to receive data only from the specified remote peer.
    ///
    /// Note: UDP is connectionless, so a successful `connect` call does not execute
    /// a handshake or validation of the remote peer of any kind.
    /// Any errors would not be detected until the first send.
    pub async fn connect(&self, socket_addr: SocketAddr) -> std::io::Result<()> {
        self.inner.connect(SockAddr::from(socket_addr)).await
    }

    /// Sends data on the connected socket
    ///
    /// On success, returns the number of bytes written.
    pub async fn send<B: BoundedIoBuf>(&self, buf: B) -> BufResult<usize, B> {
        self.inner.send(buf).await
    }

    /// Sends data on the socket to the given address.
    ///
    /// On success, returns the number of bytes written.
    pub async fn send_to<B: BoundedIoBuf>(&self, buf: B, socket_addr: SocketAddr) -> BufResult<usize, B> {
        self.inner.send_to(buf, socket_addr).await
    }

    /// Sends a message on the socket using a msghdr.
    ///
    /// Returns a tuple of:
    ///
    /// * Result containing bytes written on success
    /// * The original `io_slices` `Vec<B>`
    /// * The original `msg_contol` `Option<C>`
    ///
    /// Consider using [`Self::sendmsg_zc`] for a zero-copy alternative.
    pub async fn sendmsg<B: BoundedIoBuf, C: BoundedIoBuf>(
        &self,
        io_slices: Vec<B>,
        socket_addr: Option<SocketAddr>,
        msg_control: Option<C>,
    ) -> BufResult<(usize, Option<C>), Vec<B>> {
        self.inner.sendmsg(io_slices, socket_addr, msg_control).await
    }

    /// Reads a packet of data from the socket into the buffer.
    ///
    /// Returns the original buffer and quantity of data read.
    pub async fn recv<B: BoundedIoBufMut>(&self, buf: B) -> BufResult<usize, B> {
        self.inner.recv(buf).await
    }

    /// Receives a single datagram message on the socket.
    ///
    /// On success, returns the number of bytes read and the origin.
    pub async fn recv_from<B: BoundedIoBufMut>(&self, buf: B) -> BufResult<(usize, SocketAddr), B> {
        self.inner.recv_from(buf).await
    }

    /// Receives a single datagram message on the socket, into multiple buffers
    ///
    /// On success, returns the number of bytes read and the origin.
    pub async fn recvmsg<B: BoundedIoBufMut>(&self, buf: Vec<B>) -> BufResult<(usize, SocketAddr), Vec<B>> {
        self.inner.recvmsg(buf).await
    }

    /// Shuts down the read, write, or both halves of this connection.
    ///
    /// This function causes all pending and future I/O on the specified portions to return
    /// immediately with an appropriate value.
    pub fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
        self.inner.shutdown(how)
    }
}

#[cfg(unix)]
impl FromRawFd for UdpSocket {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        UdpSocket::from(Socket::from(SharedIoHandle::new(fd)))
    }
}

#[cfg(unix)]
impl AsRawFd for UdpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(windows)]
impl FromRawSocket for UdpSocket {
    unsafe fn from_raw_socket(socket: RawSocket) -> Self {
        UdpSocket::from(Socket::from(SharedIoHandle::new_socket(socket)))
    }
}

#[cfg(windows)]
impl AsRawSocket for UdpSocket {
    fn as_raw_socket(&self) -> RawSocket {
        self.inner.as_raw_socket()
    }
}

impl From<Socket> for UdpSocket {
    fn from(inner: Socket) -> Self {
        Self { inner }
    }
}

impl From<std::net::UdpSocket> for UdpSocket {
    fn from(socket: std::net::UdpSocket) -> Self {
        let inner = Socket::from(socket);
        Self { inner }
    }
}
