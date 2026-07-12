use crate::{
    buf::{BoundedIoBuf, BufResult},
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Completion, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct WriteAt<B: BoundedIoBuf> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(dead_code)]
    io_handle: SharedIoHandle,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Write offset
    offset: u64,
}

impl<B: BoundedIoBuf> Op<WriteAt<B>> {
    pub(crate) fn write_at(io_handle: &SharedIoHandle, buf: B, offset: u64) -> std::io::Result<Op<WriteAt<B>>> {
        let data = WriteAt {
            io_handle: io_handle.clone(),
            buf,
            offset,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl<B: BoundedIoBuf> Operable for WriteAt<B> {}

#[cfg(target_os = "linux")]
impl<B: BoundedIoBuf> Submittable for WriteAt<B> {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        // Get raw buffer info
        let ptr = self.buf.stable_read_ptr();
        let len = self.buf.bytes_init();

        opcode::Write::new(types::Fd(self.io_handle.raw_fd()), ptr, len as _)
            .offset(self.offset as _)
            .build()
    }
}

#[cfg(target_os = "macos")]
impl<B: BoundedIoBuf> Submittable for WriteAt<B> {
    fn submit(&mut self) -> Submission {
        macos_syscall_submit!(self.io_handle.raw_fd(), libc::EVFILT_WRITE, {
            let ptr = self.buf.stable_read_ptr();
            let len = self.buf.bytes_init();

            macos_syscall!(libc::pwrite(
                self.io_handle.raw_fd(),
                ptr as *const libc::c_void,
                len,
                self.offset as i64,
            ))
        })
    }
}

#[cfg(windows)]
impl<B: BoundedIoBuf> Submittable for WriteAt<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use crate::driver::helpers::io_handle::OsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::WriteFile;

        let ptr = self.buf.stable_read_ptr();
        let len = self.buf.bytes_init() as u32;

        match self.io_handle.raw_os_handle() {
            OsRawHandle::Handle(handle) => {
                let mut interest = Interest::new(handle as _);

                unsafe {
                    let overlapped = &mut *interest.as_mut_ptr();
                    overlapped.Anonymous.Anonymous.Offset = (self.offset & 0xFFFF_FFFF) as u32;
                    overlapped.Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;
                }

                let mut bytes_written = 0u32;
                windows_syscall_submit_overlapped!(interest, file, {
                    WriteFile(
                        handle as _,
                        ptr as *const u8,
                        len,
                        &mut bytes_written,
                        interest.as_mut_ptr(),
                    )
                })
            }
            OsRawHandle::Socket(_) => Submission::Ready(Completion {
                result: Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "use send for socket writes on Windows",
                )),
                flags: 0,
            }),
        }
    }
}

impl<B: BoundedIoBuf> Completable for WriteAt<B> {
    type Result = BufResult<usize, B>;

    fn complete(self, completion_entry: super::Completion) -> Self::Result {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);
        // Recover the buffer
        let buf = self.buf;

        (res, buf)
    }
}
