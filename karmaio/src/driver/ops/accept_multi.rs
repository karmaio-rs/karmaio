//! Multishot accept for Linux io_uring.
//!
//! Requires Linux 6.12+. karmaio does not probe the kernel version; callers
//! must ensure the floor is met.

use std::io;
use std::os::fd::{FromRawFd, RawFd};

use crate::driver::backends::iouring::{MultishotCleanup, Submission as UringSubmission, UringMultishotOperation};
use crate::driver::helpers::io_handle::SharedIoHandle;
use crate::driver::helpers::socket::Socket;
use crate::driver::ops::{Completion, MultiOp};
use crate::runtime::local::CURRENT_DRIVER;

/// Multishot accept payload for a listening stream socket.
pub(crate) struct AcceptMulti {
    io_handle: SharedIoHandle<socket2::Socket>,
    pending_completion_limit: usize,
}

impl MultiOp<AcceptMulti> {
    /// Submit a multishot accept on `io_handle`.
    ///
    /// Each successful completion yields one accepted [`Socket`]. Peer
    /// addresses are not filled by the kernel for multishot accept; callers
    /// should use `getpeername` / [`socket2::Socket::peer_addr`] as needed.
    ///
    /// Dropping the returned stream cancels the multishot request.
    pub(crate) fn accept_multi(io_handle: &SharedIoHandle<socket2::Socket>) -> io::Result<Self> {
        CURRENT_DRIVER.with(|handle| {
            let driver = handle.upgrade().expect("Not in a runtime context");
            let data = AcceptMulti {
                io_handle: io_handle.clone(),
                pending_completion_limit: driver.multishot_accept_capacity(),
            };
            Ok(driver.defer_multi_op(data))
        })
    }
}

unsafe impl UringMultishotOperation for AcceptMulti {
    type Item = io::Result<Socket>;

    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        // Multishot accept does not take a shared sockaddr buffer; remote
        // addresses are retrieved after accept if needed.
        opcode::AcceptMulti::new(types::Fd(self.io_handle.raw_fd()))
            .flags(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)
            .build()
    }

    fn complete_item(&mut self, completion: Completion) -> Option<Self::Item> {
        Some((|| {
            let raw_fd = completion.result? as RawFd;
            // Safety: a successful multishot accept CQE transfers ownership of
            // one new socket descriptor to userspace.
            let sock = unsafe { socket2::Socket::from_raw_fd(raw_fd) };
            let socket = Socket::from_socket(sock)?;
            socket.set_async_flags()?;
            Ok(socket)
        })())
    }

    fn submission_error(error: io::Error) -> Self::Item {
        Err(error)
    }

    fn completion_cleanup(&self) -> MultishotCleanup {
        MultishotCleanup::AcceptedFd
    }

    fn pending_completion_limit(&self) -> Option<usize> {
        Some(self.pending_completion_limit)
    }
}
