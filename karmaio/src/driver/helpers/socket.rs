use std::net::SocketAddr;
use std::{io::Result, os::raw::c_int};

#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, RawFd};

#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, AsSocket, BorrowedSocket, FromRawSocket, RawSocket};

use crate::buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut};
use crate::driver::helpers::attached_handle::AttachedHandle;
#[cfg(target_os = "linux")]
use crate::driver::ops::MultiOp;
use crate::driver::ops::Op;
#[cfg(target_os = "linux")]
use crate::driver::ops::accept_multi::AcceptMulti;
#[cfg(target_os = "linux")]
use crate::io::Stream;
use crate::io::{CancelHandle, Register, TerminalGuard, map_cancel_result, operation_canceled};

// This is an internal wrapper around socket operations for the runtime.
// This wrapper abstracts and handles all the driver operations and os compatiblity,
// presenting a clean, reusable api for the top level socket modules.
//
// The owned resource is a `socket2::Socket` so control-plane APIs (listen, nodelay,
// etc.) go through `AttachedHandle`/`Deref` without reconverting through raw FDs.
#[derive(Clone, Debug)]
pub(crate) struct Socket {
    pub(crate) handle: AttachedHandle<socket2::Socket>,
}

/// Configure a socket for async use on the current platform.
///
/// Sets non-blocking mode, and configures close-on-exec on kqueue Unix targets.
/// Apple targets additionally enable `NOSIGPIPE`.
/// Used by create, bind, and accept paths so flags stay consistent.
fn configure_async_socket(socket: &socket2::Socket) -> Result<()> {
    socket.set_nonblocking(true)?;
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    {
        socket.set_cloexec(true)?;
    }
    #[cfg(target_vendor = "apple")]
    {
        // Avoid SIGPIPE killing the process when writing to a closed socket.
        // macOS can reject SO_NOSIGPIPE on Unix domain sockets (and accepted
        // sockets inherit it from the listener anyway), so skip unix sockets.
        let is_unix = socket
            .local_addr()
            .map(|a| i32::from(a.family()) == libc::AF_UNIX)
            .unwrap_or(false);
        if !is_unix {
            socket.set_nosigpipe(true)?;
        }
    }
    Ok(())
}

impl Socket {
    fn attach(socket: socket2::Socket) -> Result<AttachedHandle<socket2::Socket>> {
        #[cfg(windows)]
        {
            AttachedHandle::new_socket(socket)
        }

        #[cfg(unix)]
        {
            AttachedHandle::new(socket)
        }
    }

    pub(crate) fn set_async_flags(&self) -> Result<()> {
        configure_async_socket(&self.handle)
    }

    /// Creates a new network socket (TCP/UDP)
    pub(crate) fn new(socket_addr: SocketAddr, socket_type: socket2::Type) -> Result<Self> {
        let socket = socket2::Socket::new(socket2::Domain::for_address(socket_addr), socket_type, None)?;
        configure_async_socket(&socket)?;

        Ok(Self {
            handle: Self::attach(socket)?,
        })
    }

    /// Creates a new UNIX socket
    #[cfg(unix)]
    pub(crate) fn new_unix(socket_type: c_int) -> Result<Self> {
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket_type.into(), None)?;
        configure_async_socket(&socket)?;

        Ok(Self {
            handle: Self::attach(socket)?,
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
            handle: Self::attach(socket)?,
        })
    }

    /// Wrap an already-created socket and attach it to the current runtime.
    pub(crate) fn from_socket(socket: socket2::Socket) -> Result<Self> {
        Ok(Self {
            handle: Self::attach(socket)?,
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

    /// Stream of incoming connections (Linux io_uring multishot accept).
    ///
    /// Yields the same item shape as [`accept`](Self::accept): an owned
    /// [`Socket`] and optional peer IP address. Peer addresses are resolved
    /// with `getpeername` after each successful accept.
    ///
    /// Uses multishot accept under the hood (Linux 6.12+). Dropping the stream
    /// cancels the request. The multishot SQE is **not** re-armed after it
    /// terminates; call [`incoming`](Self::incoming) again to start a new one.
    #[cfg(target_os = "linux")]
    pub(crate) fn incoming(&self) -> Result<Incoming> {
        Ok(Incoming {
            inner: MultiOp::accept_multi(&self.handle)?,
        })
    }

    // ================================
    //  Connection Control
    // ================================

    /// Starts listening for incoming connections.
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
        self.handle.into_inner().close().await
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
    pub(crate) async fn recv<B: IoBufMut>(&self, buf: B) -> BufResult<usize, B> {
        let op = match Op::recv(&self.handle, buf) {
            Ok(op) => op,
            Err((error, buf)) => return BufResult(Err(error), buf),
        };
        op.await
    }

    pub(crate) async fn recv_cancellable<B: IoBufMut>(
        &self,
        buf: B,
        cancellation: &CancelHandle,
    ) -> BufResult<usize, B> {
        self.run_cancellable(buf, cancellation, |buf| Op::recv(&self.handle, buf))
            .await
    }

    /// Receive into a runtime pool buffer (Linux only).
    ///
    /// See [`crate::buf::PooledBuf`] for lease ownership and pool starvation
    /// risks. `len == 0` uses the full pool buffer size.
    #[cfg(target_os = "linux")]
    pub(crate) async fn recv_managed(&self, len: usize) -> Result<Option<crate::buf::PooledBuf>> {
        let op = Op::recv_managed(&self.handle, len)?;
        op.await
    }

    /// Multishot receive stream into runtime pool buffers (Linux only).
    ///
    /// See [`crate::buf::PooledBuf`] for lease ownership and pool starvation
    /// risks. No auto-rearm: when the stream ends (including on `ENOBUFS`),
    /// call again after recycling leases.
    #[cfg(target_os = "linux")]
    pub(crate) fn recv_multi(&self) -> Result<MultiOp<crate::driver::ops::recv_multi::RecvMulti>> {
        MultiOp::recv_multi(&self.handle)
    }

    /// Managed UDP receive with truncation and message metadata (Linux only).
    #[cfg(target_os = "linux")]
    pub(crate) async fn recv_datagram_managed(
        &self,
        len: usize,
        capture_peer: bool,
    ) -> Result<crate::net::udp::RecvDatagram> {
        let op = Op::recv_datagram_managed(&self.handle, len, capture_peer)?;
        op.await
    }

    /// Multishot UDP receive with truncation and message metadata (Linux only).
    #[cfg(target_os = "linux")]
    pub(crate) fn recv_datagram_multi(
        &self,
        capture_peer: bool,
    ) -> Result<MultiOp<crate::driver::ops::recv_from_multi::RecvFromMulti>> {
        MultiOp::recv_datagram_multi(&self.handle, capture_peer)
    }

    /// Managed oneshot recv_from into a pool buffer (Linux only).
    #[cfg(target_os = "linux")]
    pub(crate) async fn recv_from_managed(&self, len: usize) -> Result<crate::net::udp::RecvDatagram> {
        self.recv_datagram_managed(len, true).await
    }

    /// Multishot recv_from stream into pool buffers (Linux only).
    #[cfg(target_os = "linux")]
    pub(crate) fn recv_from_multi(&self) -> Result<MultiOp<crate::driver::ops::recv_from_multi::RecvFromMulti>> {
        self.recv_datagram_multi(true)
    }

    /// Reads a message from the socket along with the receiver address
    pub(crate) async fn recv_from<B: IoBufMut>(&self, buf: B) -> BufResult<(usize, SocketAddr), B> {
        let op = match Op::recv_from(&self.handle, buf) {
            Ok(op) => op,
            Err((error, buf)) => return BufResult(Err(error), buf),
        };
        op.await
    }

    pub(crate) async fn recv_from_cancellable<B: IoBufMut>(
        &self,
        buf: B,
        cancellation: &CancelHandle,
    ) -> BufResult<(usize, SocketAddr), B> {
        self.run_cancellable(buf, cancellation, |buf| Op::recv_from(&self.handle, buf))
            .await
    }

    /// Performs a scattered read into the supplied buffers along with the receiver address
    pub(crate) async fn recvmsg<V: IoVectoredBufMut>(&self, buf: V) -> BufResult<(usize, SocketAddr), V> {
        let op = match Op::recvmsg(&self.handle, buf) {
            Ok(op) => op,
            Err((error, buf)) => return BufResult(Err(error), buf),
        };
        op.await
    }

    pub(crate) async fn recvmsg_cancellable<V: IoVectoredBufMut>(
        &self,
        buf: V,
        cancellation: &CancelHandle,
    ) -> BufResult<(usize, SocketAddr), V> {
        self.run_cancellable(buf, cancellation, |buf| Op::recvmsg(&self.handle, buf))
            .await
    }

    // ================================
    //  Write Operations
    // ================================

    /// Writes the buffer on the connected socket
    pub(crate) async fn send<B: IoBuf>(&self, buf: B) -> BufResult<usize, B> {
        let op = match Op::send(&self.handle, buf) {
            Ok(op) => op,
            Err((error, buf)) => return BufResult(Err(error), buf),
        };
        op.await
    }

    pub(crate) async fn send_cancellable<B: IoBuf>(&self, buf: B, cancellation: &CancelHandle) -> BufResult<usize, B> {
        self.run_cancellable(buf, cancellation, |buf| Op::send(&self.handle, buf))
            .await
    }

    /// Writes the buffer to the specified address on the socket
    pub(crate) async fn send_to<B: IoBuf>(&self, buf: B, socket_addr: SocketAddr) -> BufResult<usize, B> {
        let op = match Op::send_to(&self.handle, buf, socket_addr) {
            Ok(op) => op,
            Err((error, buf)) => return BufResult(Err(error), buf),
        };
        op.await
    }

    pub(crate) async fn send_to_cancellable<B: IoBuf>(
        &self,
        buf: B,
        socket_addr: SocketAddr,
        cancellation: &CancelHandle,
    ) -> BufResult<usize, B> {
        self.run_cancellable(buf, cancellation, |buf| Op::send_to(&self.handle, buf, socket_addr))
            .await
    }

    /// Performes a gather write on the socket with data from the specified buffers
    /// Needs an address if the socket is not connected to an address
    pub(crate) async fn sendmsg<V: IoVectoredBuf, C: IoBuf>(
        &self,
        io_slices: V,
        socket_addr: Option<SocketAddr>,
        control: Option<C>,
    ) -> BufResult<(usize, Option<C>), V> {
        let op = match Op::sendmsg(&self.handle, io_slices, control, socket_addr) {
            Ok(op) => op,
            Err((error, io_slices)) => return BufResult(Err(error), io_slices),
        };
        op.await
    }

    pub(crate) async fn sendmsg_cancellable<V: IoVectoredBuf, C: IoBuf>(
        &self,
        io_slices: V,
        socket_addr: Option<SocketAddr>,
        control: Option<C>,
        cancellation: &CancelHandle,
    ) -> BufResult<(usize, Option<C>), V> {
        self.run_cancellable(io_slices, cancellation, |io_slices| {
            Op::sendmsg(&self.handle, io_slices, control, socket_addr)
        })
        .await
    }

    async fn run_cancellable<T, R, B>(
        &self,
        buf: B,
        cancellation: &CancelHandle,
        submit: impl FnOnce(B) -> std::result::Result<Op<T>, (std::io::Error, B)>,
    ) -> BufResult<R, B>
    where
        T: crate::driver::backends::Operation<Output = BufResult<R, B>> + 'static,
    {
        match cancellation.register() {
            Register::Canceled => BufResult(Err(operation_canceled()), buf),
            Register::Pending(registration) => {
                // On submit failure the registration guard rolls the handle
                // back to idle and the buffer is returned with the error; the
                // kernel never observed the operation.
                let op = match submit(buf) {
                    Ok(op) => op,
                    Err((error, buf)) => return BufResult(Err(error), buf),
                };
                registration.bind(op.key());
                let _terminal = TerminalGuard::new(cancellation);
                let BufResult(result, buf) = op.await;
                BufResult(map_cancel_result(result), buf)
            }
        }
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

impl From<AttachedHandle<socket2::Socket>> for Socket {
    fn from(value: AttachedHandle<socket2::Socket>) -> Self {
        Self { handle: value }
    }
}

/// Stream of incoming connections owned by the shared [`Socket`] helper.
///
/// Each item matches oneshot [`Socket::accept`]: `(Socket, Option<SocketAddr>)`.
/// Public listeners map this into their domain types (`TcpStream`, `UnixStream`).
///
/// Backed by io_uring multishot accept on Linux 6.12+. Dropping the stream
/// cancels the in-flight request. The multishot SQE is not re-armed after it
/// ends; construct a new stream via [`Socket::incoming`].
#[cfg(target_os = "linux")]
pub(crate) struct Incoming {
    inner: MultiOp<AcceptMulti>,
}

#[cfg(target_os = "linux")]
impl Stream for Incoming {
    type Item = Result<(Socket, Option<SocketAddr>)>;

    async fn next(&mut self) -> Option<Self::Item> {
        let accepted = self.inner.next().await?;
        Some(match accepted {
            Ok(socket) => match socket.handle.peer_addr() {
                Ok(addr) => Ok((socket, addr.as_socket())),
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        })
    }
}

#[cfg(unix)]
impl FromRawFd for Socket {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        // Safety: caller guarantees `fd` is an open socket; ownership transfers here.
        Self {
            handle: unsafe { AttachedHandle::new_unchecked(socket2::Socket::from_raw_fd(fd)) },
        }
    }
}

#[cfg(windows)]
impl FromRawSocket for Socket {
    unsafe fn from_raw_socket(socket: RawSocket) -> Self {
        // Safety: caller guarantees `socket` is open; ownership transfers here.
        Self {
            handle: unsafe { AttachedHandle::new_unchecked(socket2::Socket::from_raw_socket(socket)) },
        }
    }
}
