//! Managed oneshot receive using io_uring provided buffers.
//!
//! Requires Linux 6.12+ for the overall managed/multishot floor documented by
//! karmaio. Buffer selection itself is older; we do not probe the kernel.

#![cfg(all(feature = "net", target_os = "linux"))]

use std::io;
use std::ptr;

use crate::buf::{BufferPool, PooledBuf, SetLen};
use crate::driver::backends::iouring::{Submission as UringSubmission, UringOperation};
use crate::driver::helpers::io_handle::SharedIoHandle;
use crate::driver::ops::{Completion, Op};
use crate::runtime::local::CURRENT_DRIVER;

/// One-shot receive into a runtime pool buffer (`IOSQE_BUFFER_SELECT`).
pub(crate) struct RecvManaged {
    #[allow(unused)]
    io_handle: SharedIoHandle<socket2::Socket>,
    pool: BufferPool,
    /// Max bytes to receive; 0 means the full pool buffer length.
    len: u32,
    buffer_group: u16,
}

impl Op<RecvManaged> {
    /// Submit a managed receive.
    ///
    /// On success returns `Some(buf)` with initialized length set to the number
    /// of bytes received. `Ok(None)` means stream EOF; zero-length datagrams
    /// return an empty buffer. Errors
    /// (including `ENOBUFS` when the pool is empty) return without a buffer
    /// lease; any kernel-selected buffer is returned to the pool on failure
    /// paths that still carry a buffer id.
    pub(crate) fn recv_managed(io_handle: &SharedIoHandle<socket2::Socket>, len: usize) -> io::Result<Op<RecvManaged>> {
        let pool = CURRENT_DRIVER.with(|h| h.upgrade().expect("Not in a runtime context").buffer_pool())?;
        let buffer_group = pool.buffer_group()?;
        let pool_len = pool.buffer_len()?;
        let len = if len == 0 { pool_len } else { len.min(pool_len) };
        let len = u32::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "managed recv length too large"))?;

        let data = RecvManaged {
            io_handle: io_handle.clone(),
            pool,
            len,
            buffer_group,
        };
        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

unsafe impl UringOperation for RecvManaged {
    type Output = io::Result<Option<PooledBuf>>;

    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, squeue::Flags, types};

        opcode::Recv::new(types::Fd(self.io_handle.raw_fd()), ptr::null_mut(), self.len)
            .buf_group(self.buffer_group)
            .build()
            .flags(Flags::BUFFER_SELECT)
    }

    fn complete(self, completion: Completion) -> Self::Output {
        let n = match completion.result {
            Ok(n) => n as usize,
            Err(err) => {
                // If the kernel still selected a buffer, return it so the pool
                // does not permanently lose a slot.
                if let Some(bid) = io_uring::cqueue::buffer_select(completion.flags) {
                    self.pool.recycle_selected(bid);
                }
                return Err(err);
            }
        };

        let Some(bid) = io_uring::cqueue::buffer_select(completion.flags) else {
            if n > 0 {
                return Err(io::Error::other("managed recv completed without a buffer id"));
            }
            // Stream EOF does not consume a provided buffer. Datagram sockets
            // consume one even for a valid zero-length datagram.
            return Ok(None);
        };

        let mut buf = self
            .pool
            .take(bid)?
            .ok_or_else(|| io::Error::other("managed recv buffer id was not available in the pool"))?;
        // Safety: the kernel wrote `n` bytes into the selected buffer.
        unsafe { buf.set_len(n) };
        Ok(Some(buf))
    }
}
