use std::io;

#[cfg(windows)]
use crate::driver::backends::iocp::{IocpOperation, IocpSubmission};
#[cfg(target_os = "linux")]
use crate::driver::backends::iouring::{Submission as UringSubmission, UringOperation};
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
use crate::driver::backends::kqueue::{KqueueAttempt, KqueueOperation};

use crate::{
    buf::{BufResult, IoVectoredBufMut},
    driver::{
        helpers::io_handle::SharedIoHandle,
        ops::{Completion, Op},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Readv<V: IoVectoredBufMut> {
    #[allow(unused)]
    io_handle: SharedIoHandle<std::fs::File>,
    pub(crate) bufs: V,
    #[cfg(unix)]
    iovs: Vec<libc::iovec>,
    #[cfg(windows)]
    segments: Vec<windows_sys::Win32::Storage::FileSystem::FILE_SEGMENT_ELEMENT>,
    offset: u64,
}

impl<V: IoVectoredBufMut> Op<Readv<V>> {
    pub(crate) fn readv(io_handle: &SharedIoHandle<std::fs::File>, bufs: V, offset: u64) -> io::Result<Op<Readv<V>>> {
        let data = Readv {
            io_handle: io_handle.clone(),
            bufs,
            #[cfg(unix)]
            iovs: Vec::new(),
            #[cfg(windows)]
            segments: Vec::new(),
            offset,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl<V: IoVectoredBufMut> Readv<V> {
    #[cfg(unix)]
    fn rebuild_iovs(&mut self) {
        self.iovs = self
            .bufs
            .iter_uninit_slice()
            .map(|buf| libc::iovec {
                iov_base: buf.as_mut_ptr().cast(),
                iov_len: buf.len(),
            })
            .collect();
    }

    #[cfg(windows)]
    fn rebuild_segments(&mut self) {
        self.segments = self
            .bufs
            .iter_uninit_slice()
            .map(|buf| windows_sys::Win32::Storage::FileSystem::FILE_SEGMENT_ELEMENT {
                Buffer: buf.as_mut_ptr().cast(),
            })
            .collect();
        self.segments
            .push(windows_sys::Win32::Storage::FileSystem::FILE_SEGMENT_ELEMENT {
                Buffer: std::ptr::null_mut(),
            });
    }

    fn finish(mut self, completion: Completion) -> BufResult<usize, V> {
        let capacity = self.bufs.total_capacity();
        match completion.bytes_transferred(capacity) {
            Ok(n) => {
                // Safety: bytes_transferred verified that the kernel-reported
                // initialized prefix fits within the aggregate capacity.
                unsafe { self.bufs.set_len(n) };
                BufResult(Ok(n), self.bufs)
            }
            Err(error) => BufResult(Err(error), self.bufs),
        }
    }
}

#[cfg(target_os = "linux")]
unsafe impl<V: IoVectoredBufMut> UringOperation for Readv<V> {
    type Output = BufResult<usize, V>;

    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        self.rebuild_iovs();
        opcode::Readv::new(
            types::Fd(self.io_handle.raw_fd()),
            self.iovs.as_ptr(),
            self.iovs.len() as u32,
        )
        .offset(self.offset as _)
        .build()
    }

    fn complete(self, completion: Completion) -> Self::Output {
        self.finish(completion)
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl<V: IoVectoredBufMut> KqueueOperation for Readv<V> {
    type Output = BufResult<usize, V>;

    fn attempt(&mut self) -> KqueueAttempt {
        self.rebuild_iovs();
        let fd = self.io_handle.raw_fd();
        let iovs = self.iovs.as_ptr() as usize;
        let iovcnt = self.iovs.len();
        let offset = self.offset;
        kqueue_syscall_blocking!({
            // Safety: the stable operation carrier retains the descriptor
            // array, buffers, and file until the blocking job completes.
            let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            let iovs = unsafe { std::slice::from_raw_parts(iovs as *const libc::iovec, iovcnt) };
            let mut bufs = iovs
                .iter()
                .map(|iov| {
                    let buf = unsafe { std::slice::from_raw_parts_mut(iov.iov_base.cast::<u8>(), iov.iov_len) };
                    std::io::IoSliceMut::new(buf)
                })
                .collect::<Vec<_>>();
            rustix::io::preadv(fd, &mut bufs, offset)
                .map(|n| n as u32)
                .map_err(std::io::Error::from)
        })
    }

    fn complete(self, completion: Completion) -> Self::Output {
        self.finish(completion)
    }
}

#[cfg(windows)]
unsafe impl<V: IoVectoredBufMut> IocpOperation for Readv<V> {
    type Output = BufResult<usize, V>;

    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Storage::FileSystem::ReadFileScatter;

        let total_bytes = self.bufs.total_capacity() as u32;
        self.rebuild_segments();
        let handle = self.io_handle.raw_handle();
        let mut interest = Interest::new(handle as _);

        unsafe {
            let overlapped = &mut *interest.as_mut_ptr();
            overlapped.Anonymous.Anonymous.Offset = (self.offset & 0xFFFF_FFFF) as u32;
            overlapped.Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;
        }

        windows_syscall_submit_overlapped!(interest, file, {
            ReadFileScatter(
                handle as _,
                self.segments.as_ptr(),
                total_bytes,
                std::ptr::null(),
                interest.as_mut_ptr(),
            )
        })
    }

    fn complete(self, completion: Completion) -> Self::Output {
        self.finish(completion)
    }
}
