use std::os::fd::AsRawFd;
use std::path::Path;

use socket2::SockAddr;

use crate::{
    buf::{BoundedIoBuf, BoundedIoBufMut, BufResult},
    driver::helpers::socket::Socket,
};

pub struct UnixStream {
    pub(super) inner: Socket,
}

impl UnixStream {
    /// Opens a Unix connection to the specified file path. There must be a
    /// `UnixListener` or equivalent listening on the corresponding Unix domain socket
    /// to successfully connect and return a `UnixStream`.
    pub async fn connect<P: AsRef<Path>>(path: P) -> std::io::Result<UnixStream> {
        let socket = Socket::new_unix(libc::SOCK_STREAM)?;
        socket.connect(SockAddr::unix(path)?).await?;
        let unix_stream = UnixStream { inner: socket };
        Ok(unix_stream)
    }

    /// Read some data from the stream into the buffer, returning the original buffer and quantity of data read.
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

    /// Write some data to the stream from the buffer, returning the original buffer and quantity of data written.
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

    /// Returns the socket address of the local half of this Unix connection.
    pub fn local_addr(&self) -> std::io::Result<std::os::unix::net::SocketAddr> {
        use std::os::fd::FromRawFd;
        let fd = self.inner.as_raw_fd();
        let s = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
        let addr = s.local_addr();
        std::mem::forget(s);
        addr
    }

    /// Returns the socket address of the remote half of this Unix connection.
    pub fn peer_addr(&self) -> std::io::Result<std::os::unix::net::SocketAddr> {
        use std::os::fd::FromRawFd;
        let fd = self.inner.as_raw_fd();
        let s = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
        let addr = s.peer_addr();
        std::mem::forget(s);
        addr
    }
}

impl From<Socket> for UnixStream {
    fn from(inner: Socket) -> Self {
        Self { inner }
    }
}

impl From<std::os::unix::net::UnixStream> for UnixStream {
    /// Creates new `UnixStream` from a previously bound `std::os::unix::net::UnixStream`.
    ///
    /// This function is intended to be used to wrap a TCP listener from the standard library.
    /// The conversion assumes nothing about the underlying socket.
    /// It is left up to the user to decide what socket options are appropriate for their use case.
    ///
    /// This can be used in conjunction with socket2's `Socket` interface to configure a socket before it's handed off,
    /// such as setting options like `reuse_address` or binding to multiple addresses.
    fn from(socket: std::os::unix::net::UnixStream) -> Self {
        let inner = Socket::from(socket);
        Self { inner }
    }
}
