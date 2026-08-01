use std::io;

#[cfg(windows)]
use crate::driver::backends::iocp::{IocpOperation, IocpSubmission};
#[cfg(target_os = "linux")]
use crate::driver::backends::iouring::{Submission as UringSubmission, UringOperation};
#[cfg(target_os = "macos")]
use crate::driver::backends::kqueue::{PollAttempt, PollOperation};

use crate::{
    buf::{BoundedIoBufMut, BufResult},
    driver::{
        helpers::io_handle::SharedIoHandle,
        ops::{Completion, Op},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Readv<B: BoundedIoBufMut> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle<std::fs::File>,

    // Reference to the in-flight buffers.
    pub(crate) bufs: Vec<B>,

    // Internal pointers to the IOVEC strcuts for Readv
    #[cfg(unix)]
    iovs: Vec<libc::iovec>,

    // FILE_SEGMENT_ELEMENT array for Windows ReadFileScatter (page-aligned required).
    // TODO: ensure upper layers provide page-aligned buffers before using this path
    #[cfg(windows)]
    segments: Vec<windows_sys::Win32::Storage::FileSystem::FILE_SEGMENT_ELEMENT>,

    // Read offset
    offset: u64,
}

impl<B: BoundedIoBufMut> Op<Readv<B>> {
    pub(crate) fn readv(
        io_handle: &SharedIoHandle<std::fs::File>,
        mut bufs: Vec<B>,
        offset: u64,
    ) -> io::Result<Op<Readv<B>>> {
        #[cfg(unix)]
        let iovs: Vec<libc::iovec> = bufs
            .iter_mut()
            .map(|buf| libc::iovec {
                iov_base: unsafe { buf.stable_write_ptr().add(buf.bytes_init()) as *mut libc::c_void },
                iov_len: buf.bytes_total() - buf.bytes_init(),
            })
            .collect();

        #[cfg(windows)]
        let segments = {
            let mut segs: Vec<windows_sys::Win32::Storage::FileSystem::FILE_SEGMENT_ELEMENT> = bufs
                .iter_mut()
                .map(|buf| windows_sys::Win32::Storage::FileSystem::FILE_SEGMENT_ELEMENT {
                    Buffer: unsafe { buf.stable_write_ptr().add(buf.bytes_init()) as *mut core::ffi::c_void },
                })
                .collect();
            segs.push(windows_sys::Win32::Storage::FileSystem::FILE_SEGMENT_ELEMENT {
                Buffer: std::ptr::null_mut(),
            });
            segs
        };

        let data = Readv {
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
unsafe impl<B: BoundedIoBufMut> UringOperation for Readv<B> {
    type Output = BufResult<usize, Vec<B>>;
    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        opcode::Readv::new(
            types::Fd(self.io_handle.raw_fd()),
            self.iovs.as_ptr(),
            self.iovs.len() as u32,
        )
        .offset(self.offset as _)
        .build()
    }

    fn complete(self, completion_entry: Completion) -> Self::Output {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);
        // Recover the buffer
        let mut bufs = self.bufs;

        // If the operation was successful, advance the initialized cursor.
        if let Ok(n) = res {
            let mut count = n;
            for buf in bufs.iter_mut() {
                let sz = std::cmp::min(count, buf.bytes_total() - buf.bytes_init());
                let pos = buf.bytes_init() + sz;
                // Safety: the kernel returns bytes written, and we have ensured that `pos` is
                // valid for current buffer.
                unsafe { buf.set_init(pos) };
                count -= sz;
                if count == 0 {
                    break;
                }
            }
            assert_eq!(count, 0);
        }

        (res, bufs)
    }
}

#[cfg(target_os = "macos")]
impl<B: BoundedIoBufMut> PollOperation for Readv<B> {
    type Output = BufResult<usize, Vec<B>>;
    fn attempt(&mut self) -> PollAttempt {
        // Same rationale as ReadAt: kqueue is useless for regular files and
        // preadv may block. Keep iovecs/buffers alive in the Op while the pool runs.
        let fd = self.io_handle.raw_fd();
        let iovs = self.iovs.as_ptr() as usize;
        let iovcnt = self.iovs.len() as i32;
        let offset = self.offset as i64;
        macos_syscall_blocking!({ macos_syscall!(libc::preadv(fd, iovs as *const libc::iovec, iovcnt, offset)) })
    }

    fn complete(self, completion_entry: Completion) -> Self::Output {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);
        // Recover the buffer
        let mut bufs = self.bufs;

        // If the operation was successful, advance the initialized cursor.
        if let Ok(n) = res {
            let mut count = n;
            for buf in bufs.iter_mut() {
                let sz = std::cmp::min(count, buf.bytes_total() - buf.bytes_init());
                let pos = buf.bytes_init() + sz;
                // Safety: the kernel returns bytes written, and we have ensured that `pos` is
                // valid for current buffer.
                unsafe { buf.set_init(pos) };
                count -= sz;
                if count == 0 {
                    break;
                }
            }
            assert_eq!(count, 0);
        }

        (res, bufs)
    }
}

#[cfg(windows)]
unsafe impl<B: BoundedIoBufMut> IocpOperation for Readv<B> {
    type Output = BufResult<usize, Vec<B>>;
    fn submit(&mut self) -> IocpSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Storage::FileSystem::ReadFileScatter;

        let total_bytes: u32 = self
            .bufs
            .iter()
            .map(|b| (b.bytes_total() - b.bytes_init()) as u32)
            .sum();

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

    fn complete(self, completion_entry: Completion) -> Self::Output {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);
        // Recover the buffer
        let mut bufs = self.bufs;

        // If the operation was successful, advance the initialized cursor.
        if let Ok(n) = res {
            let mut count = n;
            for buf in bufs.iter_mut() {
                let sz = std::cmp::min(count, buf.bytes_total() - buf.bytes_init());
                let pos = buf.bytes_init() + sz;
                // Safety: the kernel returns bytes written, and we have ensured that `pos` is
                // valid for current buffer.
                unsafe { buf.set_init(pos) };
                count -= sz;
                if count == 0 {
                    break;
                }
            }
            assert_eq!(count, 0);
        }

        (res, bufs)
    }
}
