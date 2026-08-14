//! Managed oneshot recvfrom using io_uring provided buffers.
//!
//! Requires Linux 6.12+ (project floor). No kernel probe.

#![cfg(all(feature = "net", target_os = "linux"))]

use std::io;
use std::mem;
use std::ptr;

use socket2::SockAddr;

use crate::buf::{BufferPool, SetLen};
use crate::driver::backends::iouring::{Submission as UringSubmission, UringOperation};
use crate::driver::helpers::io_handle::SharedIoHandle;
use crate::driver::ops::{Completion, Op};
use crate::net::udp::RecvDatagram;
use crate::runtime::local::CURRENT_DRIVER;

/// One-shot `recvmsg` with buffer select, capturing the peer address.
pub(crate) struct RecvFromManaged {
    #[allow(unused)]
    io_handle: SharedIoHandle<socket2::Socket>,
    pool: BufferPool,
    buffer_group: u16,
    buffer_len: usize,
    socket_addr: Box<SockAddr>,
    msghdr: Box<libc::msghdr>,
    iovec: Box<libc::iovec>,
    capture_peer: bool,
}

impl Op<RecvFromManaged> {
    pub(crate) fn recv_datagram_managed(
        io_handle: &SharedIoHandle<socket2::Socket>,
        len: usize,
        capture_peer: bool,
    ) -> io::Result<Op<RecvFromManaged>> {
        let pool = CURRENT_DRIVER.with(|h| h.upgrade().expect("Not in a runtime context").buffer_pool())?;
        let buffer_group = pool.buffer_group()?;
        let pool_len = pool.buffer_len()?;
        let buffer_len = if len == 0 { pool_len } else { len.min(pool_len) };

        let socket_addr = Box::new(unsafe { SockAddr::try_init(|_, _| Ok(()))?.1 });
        let mut msghdr: Box<libc::msghdr> = Box::new(unsafe { mem::zeroed() });
        let mut iovec: Box<libc::iovec> = Box::new(unsafe { mem::zeroed() });
        iovec.iov_base = ptr::null_mut();
        iovec.iov_len = buffer_len;
        if capture_peer {
            msghdr.msg_name = socket_addr.as_ptr() as *mut _;
            msghdr.msg_namelen = socket_addr.len();
        }
        msghdr.msg_iov = iovec.as_mut() as *mut libc::iovec;
        msghdr.msg_iovlen = 1;

        let data = RecvFromManaged {
            io_handle: io_handle.clone(),
            pool,
            buffer_group,
            buffer_len,
            socket_addr,
            msghdr,
            iovec,
            capture_peer,
        };
        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

unsafe impl UringOperation for RecvFromManaged {
    type Output = io::Result<RecvDatagram>;

    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, squeue::Flags, types};

        // Refresh pointers in case the boxes moved (they should not after pin,
        // but keep msghdr consistent with current storage).
        self.iovec.iov_base = ptr::null_mut();
        self.iovec.iov_len = self.buffer_len;
        if self.capture_peer {
            self.msghdr.msg_name = self.socket_addr.as_ptr() as *mut _;
            self.msghdr.msg_namelen = self.socket_addr.len();
        }
        self.msghdr.msg_iov = self.iovec.as_mut() as *mut libc::iovec;
        self.msghdr.msg_iovlen = 1;

        opcode::RecvMsg::new(types::Fd(self.io_handle.raw_fd()), self.msghdr.as_mut() as *mut _)
            .flags(libc::MSG_TRUNC as u32)
            .buf_group(self.buffer_group)
            .build()
            .flags(Flags::BUFFER_SELECT)
    }

    fn complete(mut self, completion: Completion) -> Self::Output {
        let n = match completion.result {
            Ok(n) => n as usize,
            Err(err) => {
                if let Some(bid) = io_uring::cqueue::buffer_select(completion.flags) {
                    self.pool.recycle_selected(bid);
                }
                return Err(err);
            }
        };

        let Some(bid) = io_uring::cqueue::buffer_select(completion.flags) else {
            return Err(io::Error::other("managed recv_from completed without a buffer id"));
        };

        let mut buf = self
            .pool
            .take(bid)?
            .ok_or_else(|| io::Error::other("managed recv_from buffer id was not available in the pool"))?;
        let copied_len = n.min(self.buffer_len);
        // Safety: the kernel initialized the copied portion of the selected buffer.
        unsafe { buf.set_len(copied_len) };

        let peer = if self.capture_peer {
            // Sync sockaddr length written through msg_namelen.
            let name_len = self.msghdr.msg_namelen as usize;
            if name_len > 0 && name_len <= self.socket_addr.len() as usize {
                unsafe {
                    self.socket_addr.set_length(name_len as _);
                }
            }
            Some(self.socket_addr.as_socket().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "kernel returned an invalid socket address")
            })?)
        } else {
            None
        };

        let mut flags = self.msghdr.msg_flags as u32;
        if n > copied_len {
            flags |= libc::MSG_TRUNC as u32;
        }

        Ok(RecvDatagram::new(buf, peer, flags, n))
    }
}
