use std::io;

use crate::{
    buf::{BoundedIoBufMut, BufResult},
    driver::{
        Submission,
        helpers::io_handle::SharedIoHandle,
        ops::{Completable, Completion, Op, Operable, Submittable},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct Readv<B: BoundedIoBufMut> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(unused)]
    io_handle: SharedIoHandle,

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
    pub(crate) fn readv(io_handle: &SharedIoHandle, mut bufs: Vec<B>, offset: u64) -> io::Result<Op<Readv<B>>> {
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

impl<B: BoundedIoBufMut> Operable for Readv<B> {}

#[cfg(target_os = "linux")]
impl<B: BoundedIoBufMut> Submittable for Readv<B> {
    fn submit(&mut self) -> Submission {
        use io_uring::{opcode, types};

        opcode::Readv::new(
            types::Fd(self.io_handle.raw_fd()),
            self.iovs.as_ptr(),
            self.iovs.len() as u32,
        )
        .offset(self.offset as _)
        .build()
    }
}

#[cfg(target_os = "macos")]
impl<B: BoundedIoBufMut> Submittable for Readv<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::kqueue::Interest;

        loop {
            let res = if self.offset == 0 {
                unsafe {
                    libc::readv(
                        self.io_handle.raw_fd(),
                        self.iovs.as_ptr() as *const libc::iovec,
                        self.iovs.len() as i32,
                    )
                }
            } else {
                unsafe {
                    libc::preadv(
                        self.io_handle.raw_fd(),
                        self.iovs.as_ptr() as *const libc::iovec,
                        self.iovs.len() as i32,
                        self.offset as i64,
                    )
                }
            };

            if res >= 0 {
                return Submission::Ready(Completion {
                    result: Ok(res as u32),
                    flags: 0,
                });
            }

            let err = io::Error::last_os_error();

            if err.kind() == io::ErrorKind::WouldBlock || err.raw_os_error() == Some(libc::EAGAIN) {
                return Submission::Register(Interest::new(
                    self.io_handle.raw_fd(),
                    libc::EVFILT_READ,
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
impl<B: BoundedIoBufMut> Submittable for Readv<B> {
    fn submit(&mut self) -> Submission {
        use crate::driver::backends::iocp::Interest;
        use crate::driver::helpers::io_handle::OsRawHandle;
        use windows_sys::Win32::Foundation::ERROR_IO_PENDING;
        use windows_sys::Win32::Storage::FileSystem::ReadFileScatter;

        let total_bytes: u32 = self
            .bufs
            .iter()
            .map(|b| (b.bytes_total() - b.bytes_init()) as u32)
            .sum();

        match self.io_handle.raw_os_handle() {
            OsRawHandle::Handle(handle) => {
                let mut interest = Interest::new(handle as _);

                unsafe {
                    let overlapped = &mut *interest.as_mut_ptr();
                    overlapped.Anonymous.Anonymous.Offset = (self.offset & 0xFFFF_FFFF) as u32;
                    overlapped.Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;
                }

                let result = unsafe {
                    ReadFileScatter(
                        handle as _,
                        self.segments.as_ptr(),
                        total_bytes,
                        std::ptr::null(),
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
                result: Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "ReadFileScatter requires a file handle",
                )),
                flags: 0,
            }),
        }
    }
}

impl<B: BoundedIoBufMut> Completable for Readv<B> {
    type Result = BufResult<usize, Vec<B>>;

    fn complete(self, completion_entry: Completion) -> Self::Result {
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
