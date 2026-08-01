use std::io;

use crate::{
    buf::{BoundedIoBuf, BufResult},
    driver::{
        helpers::io_handle::SharedIoHandle,
        ops::{BackendComplete, BackendSubmission, BackendSubmit, Completion, Op},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Writev<B: BoundedIoBuf> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle<std::fs::File>,

    // Reference to the in-flight buffers.
    pub(crate) bufs: Vec<B>,

    // Internal pointers to the IOVEC strcuts
    #[cfg(unix)]
    iovs: Vec<libc::iovec>,

    // FILE_SEGMENT_ELEMENT array for Windows WriteFileGather (page-aligned required).
    // TODO: ensure upper layers provide page-aligned buffers before using this path
    #[cfg(windows)]
    segments: Vec<windows_sys::Win32::Storage::FileSystem::FILE_SEGMENT_ELEMENT>,

    // Write offset
    offset: u64,
}

impl<B: BoundedIoBuf> Op<Writev<B>> {
    pub(crate) fn writev(
        io_handle: &SharedIoHandle<std::fs::File>,
        bufs: Vec<B>,
        offset: u64,
    ) -> io::Result<Op<Writev<B>>> {
        #[cfg(unix)]
        let iovs: Vec<libc::iovec> = bufs
            .iter()
            .map(|buf| libc::iovec {
                iov_base: buf.stable_read_ptr() as *mut libc::c_void,
                iov_len: buf.bytes_init(),
            })
            .collect();

        #[cfg(windows)]
        let segments = {
            let mut segs: Vec<windows_sys::Win32::Storage::FileSystem::FILE_SEGMENT_ELEMENT> = bufs
                .iter()
                .map(|buf| windows_sys::Win32::Storage::FileSystem::FILE_SEGMENT_ELEMENT {
                    Buffer: buf.stable_read_ptr() as *mut core::ffi::c_void,
                })
                .collect();
            segs.push(windows_sys::Win32::Storage::FileSystem::FILE_SEGMENT_ELEMENT {
                Buffer: std::ptr::null_mut(),
            });
            segs
        };

        let data = Writev {
            io_handle: io_handle.clone(),
            bufs,
            #[cfg(unix)]
            iovs,
            #[cfg(windows)]
            segments,
            offset,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
impl<B: BoundedIoBuf> BackendSubmit for Writev<B> {
    fn submit(&mut self) -> BackendSubmission {
        use io_uring::{opcode, types};

        opcode::Writev::new(
            types::Fd(self.io_handle.raw_fd()),
            self.iovs.as_ptr(),
            self.iovs.len() as u32,
        )
        .offset(self.offset as _)
        .build()
    }
}

#[cfg(target_os = "macos")]
impl<B: BoundedIoBuf> BackendSubmit for Writev<B> {
    fn submit(&mut self) -> BackendSubmission {
        // Same rationale as WriteAt: kqueue is useless for regular files and
        // pwritev may block. Keep iovecs/buffers alive in the Op while the pool runs.
        let fd = self.io_handle.raw_fd();
        let iovs = self.iovs.as_ptr() as usize;
        let iovcnt = self.iovs.len() as i32;
        let offset = self.offset as i64;
        macos_syscall_blocking!({ macos_syscall!(libc::pwritev(fd, iovs as *const libc::iovec, iovcnt, offset)) })
    }
}

#[cfg(windows)]
impl<B: BoundedIoBuf> BackendSubmit for Writev<B> {
    fn submit(&mut self) -> BackendSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Storage::FileSystem::WriteFileGather;

        let total_bytes: u32 = self.bufs.iter().map(|b| b.bytes_init() as u32).sum();
        let handle = self.io_handle.raw_handle();
        let mut interest = Interest::new(handle as _);

        unsafe {
            let overlapped = &mut *interest.as_mut_ptr();
            overlapped.Anonymous.Anonymous.Offset = (self.offset & 0xFFFF_FFFF) as u32;
            overlapped.Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;
        }

        windows_syscall_submit_overlapped!(interest, file, {
            WriteFileGather(
                handle as _,
                self.segments.as_ptr(),
                total_bytes,
                std::ptr::null(),
                interest.as_mut_ptr(),
            )
        })
    }
}

impl<B: BoundedIoBuf> BackendComplete for Writev<B> {
    type Result = BufResult<usize, Vec<B>>;

    fn complete(self, completion_entry: Completion) -> Self::Result {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);

        // Recover the buffer
        let bufs = self.bufs;

        (res, bufs)
    }
}
