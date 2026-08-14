//! Multishot recvmsg (recv_from) using io_uring provided buffers.
//!
//! Requires Linux 6.12+. No kernel probe. Ancillary data is not requested
//! (`msg_controllen = 0`).

#![cfg(all(feature = "net", target_os = "linux"))]

use std::io;
use std::mem;
use std::ptr;

use socket2::{SockAddr, SockAddrStorage};

use crate::buf::{BufferPool, SetLen};
use crate::driver::backends::iouring::{MultishotCleanup, Submission as UringSubmission, UringMultishotOperation};
use crate::driver::helpers::io_handle::SharedIoHandle;
use crate::driver::ops::{Completion, MultiOp};
use crate::net::udp::RecvDatagram;
use crate::runtime::local::CURRENT_DRIVER;

/// Multishot `recvmsg` with buffer select; each item is payload + peer address.
pub(crate) struct RecvFromMulti {
    io_handle: SharedIoHandle<socket2::Socket>,
    pool: BufferPool,
    buffer_group: u16,
    /// Template msghdr: only namelen/controllen matter for multishot.
    msghdr: Box<libc::msghdr>,
}

impl MultiOp<RecvFromMulti> {
    /// Submit multishot recv_from. No auto-rearm; see stream docs on sockets.
    pub(crate) fn recv_datagram_multi(
        io_handle: &SharedIoHandle<socket2::Socket>,
        capture_peer: bool,
    ) -> io::Result<Self> {
        let pool = CURRENT_DRIVER.with(|h| h.upgrade().expect("Not in a runtime context").buffer_pool())?;
        let buffer_group = pool.buffer_group()?;

        let mut msghdr: Box<libc::msghdr> = Box::new(unsafe { mem::zeroed() });
        // Fixed name field size for every selected buffer layout.
        if capture_peer {
            msghdr.msg_namelen = mem::size_of::<libc::sockaddr_storage>() as _;
        }
        msghdr.msg_controllen = 0;

        let data = RecvFromMulti {
            io_handle: io_handle.clone(),
            pool,
            buffer_group,
            msghdr,
        };
        CURRENT_DRIVER.with(|handle| {
            handle
                .upgrade()
                .expect("Not in a runtime context")
                .submit_multi_op(data)
        })
    }
}

fn parse_item(pool: &BufferPool, msghdr: &libc::msghdr, completion: &Completion) -> io::Result<RecvDatagram> {
    let bid = io_uring::cqueue::buffer_select(completion.flags)
        .ok_or_else(|| io::Error::other("multishot recv_from completed without a buffer id"))?;
    let mut buf = pool
        .take(bid)?
        .ok_or_else(|| io::Error::other("multishot recv_from buffer id missing from pool"))?;

    let n = *completion.result.as_ref().map_err(|e| {
        // Caller handles Err branch; this only runs on Ok path normally.
        io::Error::new(e.kind(), e.to_string())
    })?;
    // Safety: kernel filled `n` bytes of the selected buffer with the packed layout.
    unsafe { buf.set_len(n as usize) };

    let parsed = io_uring::types::RecvMsgOut::parse(&buf, msghdr).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "failed to parse multishot recvmsg buffer layout",
        )
    })?;

    let peer = if msghdr.msg_namelen > 0 {
        let name = parsed.name_data();
        if name.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "multishot recv_from returned empty peer address",
            ));
        }
        // Rebuild a SockAddr from the packed name bytes.
        let mut storage = SockAddrStorage::zeroed();
        let copy_len = name.len().min(mem::size_of::<libc::sockaddr_storage>());
        Some(unsafe {
            ptr::copy_nonoverlapping(name.as_ptr(), storage.view_as::<u8>(), copy_len);
            let sock = SockAddr::new(storage, copy_len as _);
            sock.as_socket().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid peer address in multishot recv_from",
                )
            })?
        })
    } else {
        None
    };

    // Expose only the payload as a view into the selected buffer. The lease
    // retains the allocation's base pointer so dropping it still recycles the
    // correct address into the provided-buffer ring.
    let payload = parsed.payload_data();
    // Safety: `RecvMsgOut::parse` derived `payload` from this same buffer.
    let payload_offset = unsafe { payload.as_ptr().offset_from(buf.as_ptr()) as usize };
    let payload_len = payload.len();
    let original_len = parsed.incoming_payload_len() as usize;
    let flags = parsed.flags();
    buf.set_view(payload_offset..payload_offset + payload_len)?;

    Ok(RecvDatagram::new(buf, peer, flags, original_len))
}

unsafe impl UringMultishotOperation for RecvFromMulti {
    type Item = io::Result<RecvDatagram>;

    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        opcode::RecvMsgMulti::new(
            types::Fd(self.io_handle.raw_fd()),
            self.msghdr.as_ref() as *const _,
            self.buffer_group,
        )
        .build()
    }

    fn complete_item(&mut self, completion: Completion) -> Option<Self::Item> {
        Some(match completion.result {
            Ok(_) => parse_item(&self.pool, &self.msghdr, &completion),
            Err(err) => {
                if let Some(bid) = io_uring::cqueue::buffer_select(completion.flags) {
                    self.pool.recycle_selected(bid);
                }
                Err(err)
            }
        })
    }

    fn completion_cleanup(&self) -> MultishotCleanup {
        MultishotCleanup::ProvidedBuffer(self.pool.clone())
    }
}
