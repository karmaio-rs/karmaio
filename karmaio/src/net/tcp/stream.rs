use std::net::SocketAddr;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

#[cfg(unix)]
use crate::driver::helpers::io_handle::SharedIoHandle;
use crate::{
    buf::{BoundedIoBuf, BoundedIoBufMut, BufResult},
    driver::helpers::socket::Socket,
};

#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, RawSocket};

/// A TCP stream between a local and a remote socket.
///
/// A TCP stream can either be created by connecting to an endpoint
/// via the [`connect`] method, or by [`accepting`] a connection from a [`listener`].
pub struct TcpStream {
    pub(super) inner: Socket,
}

impl TcpStream {
    /// Opens a TCP connection to a remote host at the given `SocketAddr`
    pub async fn connect(addr: SocketAddr) -> std::io::Result<TcpStream> {
        let socket = Socket::new(addr, libc::SOCK_STREAM)?;
        socket.connect(socket2::SockAddr::from(addr)).await?;
        let tcp_stream = TcpStream { inner: socket };
        Ok(tcp_stream)
    }

    /// Read some data from the stream into the buffer.
    ///
    /// Returns the original buffer and quantity of data read.
    pub async fn read<B: BoundedIoBufMut>(&self, buf: B) -> BufResult<usize, B> {
        self.inner.recv(buf).await
    }

    /// Reads data from multiple buffers into this socket using the scatter/gather IO style.
    ///
    /// This function will attempt to read the entire contents into `bufs`, but the entire read may not succeed,
    /// or the read may also generate an error.
    ///
    /// # Return
    ///
    /// The method returns the operation result and the same array of buffers passed in as an argument.
    /// A return value of `0` typically means that the underlying socket is no longer able to accept bytes
    /// and will likely not be able to in the future as well, or that the buffer provided is empty.
    ///
    /// # Errors
    ///
    /// Each call to `read` may generate an I/O error indicating that the operation could not be completed.
    /// If an error is returned then no bytes in the buffer were written to this reader.
    ///
    /// It is **not** considered an error if the entire buffer could not be read to this reader.
    ///
    /// [`Ok(n)`]: Ok
    pub async fn readv<B: BoundedIoBufMut>(&self, buf: Vec<B>) -> BufResult<usize, Vec<B>> {
        let (result, bufs) = self.inner.recvmsg(buf).await;
        (result.map(|(n, _)| n), bufs)
    }

    /// Write some data to the stream from the buffer.
    ///
    /// Returns the original buffer and quantity of data written.
    pub async fn write<B: BoundedIoBuf>(&self, buf: B) -> BufResult<usize, B> {
        self.inner.send(buf).await
    }

    /// Writes data from multiple buffers into this socket using the scatter/gather IO style.
    ///
    /// This function will attempt to write the entire contents of `bufs`, but the entire write may not succeed,
    /// or the write may also generate an error. The bytes will be written starting at the specified offset.
    ///
    /// # Return
    ///
    /// The method returns the operation result and the same array of buffers passed in as an argument.
    /// A return value of `0` typically means that the underlying socket is no longer able to accept bytes
    /// and will likely not be able to in the future as well, or that the buffer provided is empty.
    ///
    /// # Errors
    ///
    /// Each call to `write` may generate an I/O error indicating that the operation could not be completed.
    /// If an error is returned then no bytes in the buffer were written to this writer.
    ///
    /// It is **not** considered an error if the entire buffer could not be written to this writer.
    ///
    /// [`Ok(n)`]: Ok
    pub async fn writev<B: BoundedIoBuf, C: BoundedIoBuf>(
        &self,
        bufs: Vec<B>,
        control: Option<C>,
    ) -> BufResult<(usize, Option<C>), Vec<B>> {
        self.inner.sendmsg(bufs, None, control).await
    }

    /// Shuts down the read, write, or both halves of this connection.
    ///
    /// This function will cause all pending and future I/O on the specified portions to return
    /// immediately with an appropriate value.
    pub fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
        self.inner.shutdown(how)
    }

    /// Sets the value of the TCP_NODELAY option on this socket.
    ///
    /// If set, this option disables the Nagle algorithm.
    /// This means that segments are always sent as soon as possible, even if there is only a small amount of data.
    /// When not set, data is buffered until there is a sufficient amount to send out,
    /// thereby avoiding the frequent sending of small packets.
    pub fn set_nodelay(&self, nodelay: bool) -> std::io::Result<()> {
        self.inner.set_nodelay(nodelay)
    }

    /// Returns the socket address of the local half of this TCP connection.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        let fd = self.inner.as_raw_fd();
        let s = unsafe { std::net::TcpStream::from_raw_fd(fd) };
        let addr = s.local_addr();
        std::mem::forget(s);
        addr
    }

    /// Returns the socket address of the remote half of this TCP connection.
    pub fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        let fd = self.inner.as_raw_fd();
        let s = unsafe { std::net::TcpStream::from_raw_fd(fd) };
        let addr = s.peer_addr();
        std::mem::forget(s);
        addr
    }
}

#[cfg(unix)]
impl FromRawFd for TcpStream {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        TcpStream::from(Socket::from(SharedIoHandle::new(fd)))
    }
}

#[cfg(unix)]
impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(windows)]
impl FromRawSocket for TcpStream {
    unsafe fn from_raw_socket(socket: RawSocket) -> Self {
        TcpStream::from(Socket::from(SharedIoHandle::new_socket(socket)))
    }
}

#[cfg(windows)]
impl AsRawSocket for TcpStream {
    fn as_raw_socket(&self) -> RawSocket {
        self.inner.as_raw_socket()
    }
}

impl From<Socket> for TcpStream {
    fn from(inner: Socket) -> Self {
        Self { inner }
    }
}

impl From<std::net::TcpStream> for TcpStream {
    /// Creates new `TcpStream` from a previously bound `std::net::TcpStream`.
    ///
    /// This function is intended to be used to wrap a TCP listener from the standard library.
    /// The conversion assumes nothing about the underlying socket.
    /// It is left up to the user to decide what socket options are appropriate for their use case.
    ///
    /// This can be used in conjunction with socket2's `Socket` interface to configure a socket before it's handed off,
    /// such as setting options like `reuse_address` or binding to multiple addresses.
    fn from(socket: std::net::TcpStream) -> Self {
        let inner = Socket::from(socket);
        Self { inner }
    }
}
