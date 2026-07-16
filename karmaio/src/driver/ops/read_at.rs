use crate::{
    buf::{BoundedIoBufMut, BufResult},
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

#[cfg(windows)]
use crate::driver::ops::Completion;

pub(crate) struct ReadAt<B: BoundedIoBufMut> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(dead_code)]
    io_handle: SharedIoHandle,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Read offset
    offset: u64,
}

impl<B: BoundedIoBufMut> Op<ReadAt<B>> {
    pub(crate) fn read_at(io_handle: &SharedIoHandle, buf: B, offset: u64) -> std::io::Result<Op<ReadAt<B>>> {
        let data = ReadAt {
            io_handle: io_handle.clone(),
            buf,
            offset,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl<B: BoundedIoBufMut> Operable for ReadAt<B> {}

#[cfg(target_os = "linux")]
impl<B: BoundedIoBufMut> Submittable for ReadAt<B> {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        // Get raw buffer info
        let ptr = self.buf.stable_write_ptr();
        let len = self.buf.bytes_total();
        opcode::Read::new(types::Fd(self.io_handle.raw_fd()), ptr, len as _)
            .offset(self.offset as _)
            .build()
    }
}

#[cfg(target_os = "macos")]
impl<B: BoundedIoBufMut> Submittable for ReadAt<B> {
    fn submit(&mut self) -> Submission {
        macos_syscall_submit!(self.io_handle.raw_fd(), libc::EVFILT_READ, {
            let ptr = self.buf.stable_write_ptr();
            let len = self.buf.bytes_total();

            macos_syscall!(libc::pread(
                self.io_handle.raw_fd(),
                ptr as *mut libc::c_void,
                len,
                self.offset as i64,
            ))
        })
    }
}

#[cfg(windows)]
impl<B: BoundedIoBufMut> Submittable for ReadAt<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use crate::driver::helpers::io_handle::OsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::ReadFile;

        let ptr = self.buf.stable_write_ptr();
        let len = self.buf.bytes_total() as u32;

        match self.io_handle.raw_os_handle() {
            OsRawHandle::Handle(handle) => {
                let mut interest = Interest::new(handle as _);

                unsafe {
                    let overlapped = &mut *interest.as_mut_ptr();
                    overlapped.Anonymous.Anonymous.Offset = (self.offset & 0xFFFF_FFFF) as u32;
                    overlapped.Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;
                }

                let mut bytes_read = 0u32;
                windows_syscall_submit_overlapped!(interest, file, {
                    ReadFile(handle as _, ptr as *mut u8, len, &mut bytes_read, interest.as_mut_ptr())
                })
            }
            OsRawHandle::Socket(_) => Submission::Ready(Completion {
                result: Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "use recv for socket reads on Windows",
                )),
                flags: 0,
            }),
        }
    }
}

impl<B: BoundedIoBufMut> Completable for ReadAt<B> {
    type Result = BufResult<usize, B>;

    fn complete(self, completion_entry: super::Completion) -> Self::Result {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);
        // Recover the buffer
        let mut buf = self.buf;

        // If the operation was successful, advance the initialized cursor.
        if let Ok(n) = res {
            // Safety: the kernel wrote `n` bytes to the buffer.
            unsafe {
                buf.set_init(n);
            }
        }

        (res, buf)
    }
}
