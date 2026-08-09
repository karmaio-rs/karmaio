use std::path::Path;

use crate::{driver::helpers::socket::Socket, net::unix::UnixStream};

#[cfg(target_os = "linux")]
use crate::driver::helpers::socket::Incoming;
#[cfg(target_os = "linux")]
use crate::io::Stream;

/// A Unix domain socket server listening for connections.
///
/// On Linux, [`incoming`](`UnixListener::incoming`) provides a stream of accepts
/// backed by io_uring multishot accept.
///
/// # Closing
///
/// Prefer [`UnixListener::close`] so close errors are reported. Dropping the
/// listener still closes the OS socket synchronously when the last reference is dropped.
pub struct UnixListener {
    pub(super) inner: Socket,
}

/// A stream of incoming Unix connections (Linux only).
///
/// Produced by [`UnixListener::incoming`]. Thin public mapping over the shared
/// [`Socket`] accept stream. Dropping the stream cancels the underlying accept
/// request.
///
/// # Implementation notes
///
/// On Linux this uses **io_uring multishot accept** (requires kernel **6.12+**).
/// karmaio does not probe the kernel version at runtime. The multishot SQE is
/// **not** automatically re-armed after it terminates. Call
/// [`UnixListener::incoming`] again to start a new request.
#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
pub struct UnixIncoming {
    inner: Incoming,
}

#[cfg(target_os = "linux")]
impl Stream for UnixIncoming {
    type Item = std::io::Result<UnixStream>;

    async fn next(&mut self) -> Option<Self::Item> {
        let item = self.inner.next().await?;
        Some(item.map(|(socket, _peer)| UnixStream { inner: socket }))
    }
}

impl UnixListener {
    /// Creates a new UnixListener, which will be bound to the specified file path.
    /// The file path cannnot yet exist, and will be cleaned up upon dropping `UnixListener`
    pub fn bind<P: AsRef<Path>>(path: P) -> std::io::Result<UnixListener> {
        let socket = Socket::bind_unix(path, libc::SOCK_STREAM)?;
        socket.listen(128)?;
        Ok(UnixListener { inner: socket })
    }

    /// Closes the listener after in-flight operations complete.
    ///
    /// Prefer this over dropping when close errors must be observed.
    pub async fn close(self) -> std::io::Result<()> {
        self.inner.close().await
    }

    /// Returns the local address that this listener is bound to.
    pub fn local_addr(&self) -> std::io::Result<std::os::unix::net::SocketAddr> {
        self.inner
            .handle
            .local_addr()?
            .as_unix()
            .ok_or_else(|| std::io::Error::other("Could not get socket path"))
    }

    /// Accepts a new incoming connection from this listener.
    ///
    /// This function will yield once a new Unix domain socket connection
    /// is established. When established, the corresponding [`UnixStream`] and
    /// will be returned.
    ///
    /// [`UnixStream`]: struct@crate::net::UnixStream
    pub async fn accept(&self) -> std::io::Result<UnixStream> {
        let (socket, _) = self.inner.accept().await?;
        let stream = UnixStream { inner: socket };
        Ok(stream)
    }

    /// Returns a stream of incoming connections to this listener (Linux only).
    ///
    /// Prefer this over a manual `loop { accept().await }` when accepting many
    /// connections on Linux. Submission lives on the shared [`Socket`] helper
    /// (same layer as oneshot [`accept`](Self::accept)); this method maps
    /// accepted sockets into [`UnixStream`].
    ///
    /// # Implementation notes
    ///
    /// Backed by **io_uring multishot accept** (Linux **6.12+**). The runtime
    /// does not probe the kernel version. The multishot request is **not**
    /// re-armed after it ends; call this method again to start a new stream.
    /// Dropping the returned stream cancels the in-flight request.
    #[cfg(target_os = "linux")]
    #[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
    pub fn incoming(&self) -> std::io::Result<UnixIncoming> {
        Ok(UnixIncoming {
            inner: self.inner.incoming()?,
        })
    }
}
