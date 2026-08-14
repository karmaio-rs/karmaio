use std::net::SocketAddr;

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

use socket2::SockAddr;

#[cfg(target_os = "linux")]
use crate::buf::PooledBuf;
use crate::{
    buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut},
    driver::helpers::socket::Socket,
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
/// - one to one: [`connect`](`UdpSocket::connect`) and associate with a single address, using
///   [`send`](`UdpSocket::send`) and [`recv`](`UdpSocket::recv`) to communicate only with that remote address
///
/// # Closing
///
/// Prefer [`UdpSocket::close`] so close errors are reported. Dropping the socket
/// still closes the OS handle synchronously when the last reference is dropped.
pub struct UdpSocket {
    pub(super) inner: Socket,
}

/// Message flags returned with a managed UDP datagram (Linux only).
#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecvFlags(u32);

#[cfg(target_os = "linux")]
impl RecvFlags {
    pub(crate) fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Return the platform `MSG_*` bits reported by the kernel.
    pub fn bits(self) -> u32 {
        self.0
    }

    /// Whether the datagram payload was larger than the returned buffer.
    pub fn is_truncated(self) -> bool {
        self.0 & libc::MSG_TRUNC as u32 != 0
    }

    /// Whether ancillary data was truncated.
    pub fn is_control_truncated(self) -> bool {
        self.0 & libc::MSG_CTRUNC as u32 != 0
    }
}

/// One datagram received into a runtime-provided buffer (Linux only).
#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
#[derive(Debug)]
pub struct RecvDatagram {
    /// Received payload bytes. This is a lease on the runtime buffer pool.
    pub buffer: PooledBuf,
    /// Sending peer when the address-returning API was used.
    pub peer: Option<SocketAddr>,
    /// Message flags reported by `recvmsg`.
    pub flags: RecvFlags,
    /// Original datagram payload length before any truncation.
    pub original_len: usize,
}

#[cfg(target_os = "linux")]
impl RecvDatagram {
    pub(crate) fn new(buffer: PooledBuf, peer: Option<SocketAddr>, flags: u32, original_len: usize) -> Self {
        Self {
            buffer,
            peer,
            flags: RecvFlags::from_bits(flags),
            original_len,
        }
    }

    /// Whether the original datagram did not fit in the returned buffer.
    pub fn is_truncated(&self) -> bool {
        self.flags.is_truncated() || self.original_len > self.buffer.len()
    }
}

impl UdpSocket {
    /// Creates a new UDP socket and attempt to bind it to the addr provided.
    ///
    /// Returns a new instance of [`UdpSock  et`] on success,
    /// or an [`io::Error`](std::io::Error) on failure.
    pub async fn bind(socket_addr: SocketAddr) -> std::io::Result<UdpSocket> {
        let socket = Socket::bind(socket_addr, socket2::Type::DGRAM)?;
        socket.set_async_flags()?;

        Ok(UdpSocket { inner: socket })
    }

    /// Closes the socket after in-flight operations complete.
    ///
    /// Prefer this over dropping when close errors must be observed.
    pub async fn close(self) -> std::io::Result<()> {
        self.inner.close().await
    }

    /// Returns the local address to which this UDP socket is bound.
    ///
    /// This can be useful, for example, when binding to port 0 to figure out which port was actually bound.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner
            .handle
            .local_addr()?
            .as_socket()
            .ok_or_else(|| std::io::Error::other("Could not get socket IP address"))
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
    pub async fn send<B: IoBuf>(&self, buf: B) -> BufResult<usize, B> {
        self.inner.send(buf).await
    }

    /// Sends data on the socket to the given address.
    ///
    /// On success, returns the number of bytes written.
    pub async fn send_to<B: IoBuf>(&self, buf: B, socket_addr: SocketAddr) -> BufResult<usize, B> {
        self.inner.send_to(buf, socket_addr).await
    }

    /// Sends a message on the socket using a msghdr.
    ///
    /// Returns a tuple of:
    ///
    /// * Result containing bytes written on success
    /// * The original `io_slices` `Vec<B>`
    /// * The original `msg_contol` `Option<C>`
    pub async fn sendmsg<V: IoVectoredBuf, C: IoBuf>(
        &self,
        io_slices: V,
        socket_addr: Option<SocketAddr>,
        msg_control: Option<C>,
    ) -> BufResult<(usize, Option<C>), V> {
        self.inner.sendmsg(io_slices, socket_addr, msg_control).await
    }

    /// Reads a packet of data from the socket into the buffer.
    ///
    /// Returns the original buffer and quantity of data read.
    pub async fn recv<B: IoBufMut>(&self, buf: B) -> BufResult<usize, B> {
        self.inner.recv(buf).await
    }

    /// Receive into a runtime-provided buffer (Linux only).
    ///
    /// Intended for **connected** UDP sockets (no peer address is returned).
    /// For unconnected sockets that need the sender address, use
    /// classic [`recv_from`](Self::recv_from) or a managed `recv_from` API
    /// when available.
    ///
    /// The returned [`RecvDatagram::buffer`] is a **lease**: drop or release it
    /// promptly. Holding many leases without
    /// recycling can exhaust the pool and fail later receives with `ENOBUFS`.
    ///
    /// Requires Linux 6.12+ (karmaio does not probe the kernel version).
    #[cfg(target_os = "linux")]
    #[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
    pub async fn recv_managed(&self, len: usize) -> std::io::Result<RecvDatagram> {
        self.inner.recv_datagram_managed(len, false).await
    }

    /// Multishot receive stream of runtime-provided buffers (Linux only).
    ///
    /// Intended for **connected** UDP. See
    /// [`TcpStream::recv_multi`](crate::net::tcp::TcpStream::recv_multi) for end
    /// conditions and buffer lease / `ENOBUFS` guidance.
    ///
    /// Requires Linux 6.12+ (karmaio does not probe the kernel version).
    #[cfg(target_os = "linux")]
    #[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
    pub fn recv_multi(&self) -> std::io::Result<impl crate::io::Stream<Item = std::io::Result<RecvDatagram>> + use<>> {
        self.inner.recv_datagram_multi(false)
    }

    /// Receive a datagram into a runtime-provided buffer with peer address
    /// (Linux only).
    ///
    /// Zero-length datagrams have an empty [`RecvDatagram::buffer`].
    /// `len == 0` uses the full pool buffer size.
    ///
    /// The returned payload buffer is a lease; release it promptly to avoid
    /// pool exhaustion (`ENOBUFS`).
    ///
    /// Requires Linux 6.12+ (karmaio does not probe the kernel version).
    #[cfg(target_os = "linux")]
    #[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
    pub async fn recv_from_managed(&self, len: usize) -> std::io::Result<RecvDatagram> {
        self.inner.recv_from_managed(len).await
    }

    /// Multishot datagram stream with peer addresses (Linux only).
    ///
    /// Each item is a [`RecvDatagram`]. Same end conditions as
    /// [`recv_multi`](Self::recv_multi): no auto-rearm; recycle leases to
    /// avoid `ENOBUFS`.
    ///
    /// Requires Linux 6.12+ (karmaio does not probe the kernel version).
    #[cfg(target_os = "linux")]
    #[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
    pub fn recv_from_multi(
        &self,
    ) -> std::io::Result<impl crate::io::Stream<Item = std::io::Result<RecvDatagram>> + use<>> {
        self.inner.recv_from_multi()
    }

    /// Receives a single datagram message on the socket.
    ///
    /// On success, returns the number of bytes read and the origin.
    pub async fn recv_from<B: IoBufMut>(&self, buf: B) -> BufResult<(usize, SocketAddr), B> {
        self.inner.recv_from(buf).await
    }

    /// Receives a single datagram message on the socket, into multiple buffers
    ///
    /// On success, returns the number of bytes read and the origin.
    pub async fn recvmsg<V: IoVectoredBufMut>(&self, buf: V) -> BufResult<(usize, SocketAddr), V> {
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
        // Safety: caller guarantees `fd` is an open UDP socket.
        UdpSocket::from(unsafe { Socket::from_raw_fd(fd) })
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
        // Safety: caller guarantees `socket` is an open UDP socket.
        UdpSocket::from(unsafe { Socket::from_raw_socket(socket) })
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

impl UdpSocket {
    /// Creates a UDP socket from a standard-library UDP socket.
    pub fn from_std(socket: std::net::UdpSocket) -> std::io::Result<Self> {
        let inner = Socket::from_socket(socket2::Socket::from(socket))?;
        Ok(Self { inner })
    }
}
