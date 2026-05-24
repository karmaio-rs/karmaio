use std::io;

use crate::{
    buf::{BoundedIoBuf, BufResult},
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Completion, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Write<B: BoundedIoBuf> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(dead_code)]
    io_handle: SharedIoHandle,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Write offset
    offset: u64,
}

impl<B: BoundedIoBuf> Op<Write<B>> {
    pub(crate) fn write_at(io_handle: &SharedIoHandle, buf: B, offset: u64) -> std::io::Result<Op<Write<B>>> {
        let data = Write {
            io_handle: io_handle.clone(),
            buf,
            offset,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

impl<B: BoundedIoBuf> Operable for Write<B> {}

#[cfg(target_os = "linux")]
impl<B: BoundedIoBuf> Submittable for Write<B> {
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
impl<B: BoundedIoBuf> Submittable for Write<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::kqueue::Interest;

        loop {
            let ptr = self.buf.stable_read_ptr();
            let len = self.buf.bytes_init();

            let ret = if self.offset == 0 {
                unsafe { libc::write(self.io_handle.raw_fd(), ptr as *const libc::c_void, len) }
            } else {
                unsafe {
                    libc::pwrite(
                        self.io_handle.raw_fd(),
                        ptr as *const libc::c_void,
                        len,
                        self.offset as i64,
                    )
                }
            };

            if ret >= 0 {
                return Submission::Ready(Completion {
                    result: Ok(ret as u32),
                    flags: 0,
                });
            }

            let err = io::Error::last_os_error();

            if err.kind() == io::ErrorKind::WouldBlock || err.raw_os_error() == Some(libc::EAGAIN) {
                return Submission::Register(Interest::new(
                    self.io_handle.raw_fd(),
                    libc::EVFILT_WRITE,
                    libc::EV_ADD | libc::EV_ONESHOT,
                ));
            }

            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }

            return Submission::Ready(Completion {
                result: Err(err),
                flags: 0,
            });
        }
    }
}

#[cfg(windows)]
impl<B: BoundedIoBuf> Submittable for Write<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use crate::driver::helpers::io_handle::OsRawHandle;
        use std::io;
        use windows_sys::Win32::Foundation::ERROR_IO_PENDING;
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
                let result = unsafe {
                    WriteFile(
                        handle as _,
                        ptr as *const u8,
                        len,
                        &mut bytes_written,
                        interest.as_mut_ptr(),
                    )
                };

                if result != 0 {
                    return Submission::Pending(interest);
                }

                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
                    return Submission::Pending(interest);
                }

                Submission::Ready(Completion {
                    result: Err(err),
                    flags: 0,
                })
            }
            OsRawHandle::Socket(_) => Submission::Ready(Completion {
                result: Err(io::Error::new(io::ErrorKind::Unsupported, "use send for socket writes on Windows")),
                flags: 0,
            }),
        }
    }
}

impl<B: BoundedIoBuf> Completable for Write<B> {
    type Result = BufResult<usize, B>;

    fn complete(self, completion_entry: super::Completion) -> Self::Result {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);
        // Recover the buffer
        let buf = self.buf;

        (res, buf)
    }
}
