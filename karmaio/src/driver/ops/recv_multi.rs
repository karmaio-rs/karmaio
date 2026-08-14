//! Multishot receive using io_uring provided buffers.
//!
//! Requires Linux 6.12+. karmaio does not probe the kernel version.

#![cfg(all(feature = "net", target_os = "linux"))]

use std::io;

use crate::buf::{BufferPool, PooledBuf, SetLen};
use crate::driver::backends::iouring::{MultishotCleanup, Submission as UringSubmission, UringMultishotOperation};
use crate::driver::helpers::io_handle::SharedIoHandle;
use crate::driver::ops::{Completion, MultiOp};
use crate::runtime::local::CURRENT_DRIVER;

/// Multishot receive payload for a connected stream or datagram socket.
pub(crate) struct RecvMulti {
    io_handle: SharedIoHandle<socket2::Socket>,
    pool: BufferPool,
    buffer_group: u16,
}

impl MultiOp<RecvMulti> {
    /// Submit multishot receive on `io_handle`.
    ///
    /// Each successful completion yields one [`PooledBuf`]. The stream ends
    /// when the kernel posts a final CQE without `IORING_CQE_F_MORE` (including
    /// after `ENOBUFS` / errors). Dropping the stream cancels the request and
    /// recycles any undelivered selected buffers.
    ///
    /// There is **no auto-rearm**: call this again after recycling buffers if
    /// you want another multishot request.
    ///
    /// # Buffer ownership
    ///
    /// Each item is a pool **lease**. Drop or [`PooledBuf::release`] it
    /// promptly. Holding every pool buffer without recycle will exhaust the
    /// ring and end the stream with `ENOBUFS`.
    pub(crate) fn recv_multi(io_handle: &SharedIoHandle<socket2::Socket>) -> io::Result<Self> {
        let pool = CURRENT_DRIVER.with(|h| h.upgrade().expect("Not in a runtime context").buffer_pool())?;
        let buffer_group = pool.buffer_group()?;
        let data = RecvMulti {
            io_handle: io_handle.clone(),
            pool,
            buffer_group,
        };
        CURRENT_DRIVER.with(|handle| {
            handle
                .upgrade()
                .expect("Not in a runtime context")
                .submit_multi_op(data)
        })
    }
}

fn take_buffer(pool: &BufferPool, completion: &Completion) -> io::Result<Option<PooledBuf>> {
    let bid = match io_uring::cqueue::buffer_select(completion.flags) {
        Some(bid) => bid,
        None => return Ok(None),
    };
    let mut buf = pool
        .take(bid)?
        .ok_or_else(|| io::Error::other("multishot recv buffer id was not available in the pool"))?;
    if let Ok(n) = completion.result {
        // Safety: kernel wrote `n` bytes into the selected buffer on success.
        unsafe { buf.set_len(n as usize) };
    }
    Ok(Some(buf))
}

unsafe impl UringMultishotOperation for RecvMulti {
    type Item = io::Result<PooledBuf>;

    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        opcode::RecvMulti::new(types::Fd(self.io_handle.raw_fd()), self.buffer_group).build()
    }

    fn complete_item(&mut self, completion: Completion) -> Option<Self::Item> {
        match completion.result {
            Ok(0) => {
                // A selected buffer identifies a valid zero-length datagram.
                // Stream EOF completes without selecting a buffer or yielding
                // an error item.
                match take_buffer(&self.pool, &completion) {
                    Ok(Some(buf)) => Some(Ok(buf)),
                    Ok(None) => None,
                    Err(err) => Some(Err(err)),
                }
            }
            Ok(_) => {
                Some(take_buffer(&self.pool, &completion).and_then(|buf| {
                    buf.ok_or_else(|| io::Error::other("multishot recv completed without a buffer id"))
                }))
            }
            Err(err) => {
                // Return selected buffer to the pool on error (e.g. ENOBUFS).
                if let Some(bid) = io_uring::cqueue::buffer_select(completion.flags) {
                    self.pool.recycle_selected(bid);
                }
                Some(Err(err))
            }
        }
    }

    fn completion_cleanup(&self) -> MultishotCleanup {
        MultishotCleanup::ProvidedBuffer(self.pool.clone())
    }
}
