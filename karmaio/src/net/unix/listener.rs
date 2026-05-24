use std::path::Path;

use crate::{driver::helpers::socket::Socket, net::unix::UnixStream};

pub struct UnixListener {
    pub(super) inner: Socket,
}

impl UnixListener {
    /// Creates a new UnixListener, which will be bound to the specified file path.
    /// The file path cannnot yet exist, and will be cleaned up upon dropping `UnixListener`
    pub fn bind<P: AsRef<Path>>(path: P) -> std::io::Result<UnixListener> {
        let socket = Socket::bind_unix(path, libc::SOCK_STREAM)?;
        //TODO: Make this configurable later?
        socket.listen(1024)?;
        Ok(UnixListener { inner: socket })
    }

    /// Returns the local address that this listener is bound to.
    pub fn local_addr(&self) -> std::io::Result<std::os::unix::net::SocketAddr> {
        use std::os::unix::io::{AsRawFd, FromRawFd};

        let fd = self.inner.as_raw_fd();
        // SAFETY: Our fd is the handle the kernel has given us for a UnixListener.
        // Create a std::net::UnixListener long enough to call its local_addr method
        // and then forget it so the socket is not closed here.
        let l = unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) };
        let local_addr = l.local_addr();
        std::mem::forget(l);
        local_addr
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
