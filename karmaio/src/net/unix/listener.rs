use std::path::Path;

use crate::{driver::helpers::socket::Socket, net::unix::UnixStream};

/// A Unix domain socket server listening for connections.
///
/// # Closing
///
/// Prefer [`UnixListener::close`] so close errors are reported. Dropping the
/// listener still closes the OS socket synchronously when the last reference is dropped.
pub struct UnixListener {
    pub(super) inner: Socket,
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
}
