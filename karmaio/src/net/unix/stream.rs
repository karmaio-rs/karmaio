use std::path::Path;

use socket2::SockAddr;

use crate::{
    buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut},
    driver::helpers::socket::Socket,
    io::{AsyncRead, AsyncWrite},
    net::split::{ReadHalf, WriteHalf, split},
};

/// A Unix stream connected to a remote Unix domain socket endpoint.
///
/// # Closing
///
/// Prefer [`UnixStream::close`] so close errors are reported. Dropping the stream
/// still closes the OS socket synchronously when the last reference is dropped.
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

    /// Closes the stream after in-flight operations complete.
    ///
    /// Prefer this over dropping when close errors must be observed.
    pub async fn close(self) -> std::io::Result<()> {
        self.inner.close().await
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
        self.inner
            .handle
            .local_addr()?
            .as_unix()
            .ok_or_else(|| std::io::Error::other("Could not get socket path"))
    }

    /// Returns the socket address of the remote half of this Unix connection.
    pub fn peer_addr(&self) -> std::io::Result<std::os::unix::net::SocketAddr> {
        self.inner
            .handle
            .peer_addr()?
            .as_unix()
            .ok_or_else(|| std::io::Error::other("Could not get peer path"))
    }

    /// Splits a [`TcpStream`] into a read half and a write half, which can be
    /// used to read and write the stream concurrently.
    ///
    /// This method is more efficient than
    /// [`into_split`](TcpStream::into_split), but the halves cannot
    /// be moved into independently spawned tasks.
    pub fn split(&self) -> (ReadHalf<'_, Self>, WriteHalf<'_, Self>) {
        split(self)
    }
}

impl AsyncRead for &UnixStream {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.inner.recv(buf).await
    }

    async fn read_vectored<V: IoVectoredBufMut>(&mut self, bufs: V) -> BufResult<usize, V> {
        let (result, bufs) = self.inner.recvmsg(bufs).await.into_parts();
        BufResult(result.map(|(n, _)| n), bufs)
    }
}

impl AsyncRead for UnixStream {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.inner.recv(buf).await
    }

    async fn read_vectored<V: IoVectoredBufMut>(&mut self, bufs: V) -> BufResult<usize, V> {
        let (result, bufs) = self.inner.recvmsg(bufs).await.into_parts();
        BufResult(result.map(|(n, _)| n), bufs)
    }
}

impl AsyncWrite for &UnixStream {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        self.inner.send(buf).await
    }

    async fn write_vectored<V: IoVectoredBuf>(&mut self, bufs: V) -> BufResult<usize, V> {
        let (res, bufs) = self.inner.sendmsg(bufs, None, None::<Vec<u8>>).await.into_parts();
        BufResult(res.map(|(n, _)| n), bufs)
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        self.inner.shutdown(std::net::Shutdown::Write)
    }
}

impl AsyncWrite for UnixStream {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        self.inner.send(buf).await
    }

    async fn write_vectored<V: IoVectoredBuf>(&mut self, bufs: V) -> BufResult<usize, V> {
        let (res, bufs) = self.inner.sendmsg(bufs, None, None::<Vec<u8>>).await.into_parts();
        BufResult(res.map(|(n, _)| n), bufs)
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        self.inner.shutdown(std::net::Shutdown::Write)
    }
}

impl From<Socket> for UnixStream {
    fn from(inner: Socket) -> Self {
        Self { inner }
    }
}

impl UnixStream {
    /// Creates a Unix stream from a standard-library Unix stream.
    ///
    /// This function is intended to wrap a Unix stream from the standard library.
    /// The conversion assumes nothing about the underlying socket.
    /// It is left up to the user to decide what socket options are appropriate for their use case.
    ///
    /// This can be used in conjunction with socket2's `Socket` interface to configure a socket before it's handed off,
    /// such as setting options like `reuse_address` or binding to multiple addresses.
    pub fn from_std(socket: std::os::unix::net::UnixStream) -> std::io::Result<Self> {
        let inner = Socket::from_socket(socket2::Socket::from(socket))?;
        Ok(Self { inner })
    }
}
