use crate::{
    buf::{BoundedIoBuf, BufResult},
    driver::{
        helpers::io_handle::SharedIoHandle,
        ops::{BackendComplete, BackendSubmission, BackendSubmit, Op},
    },
    runtime::local::CURRENT_DRIVER,
};

pub(crate) struct WriteAt<B: BoundedIoBuf> {
    // Holds a strong ref to the FD, preventing the file from being closed while the operation is in-flight.
    #[allow(dead_code)]
    io_handle: SharedIoHandle<std::fs::File>,

    // Reference to the in-flight buffer.
    pub(crate) buf: B,

    // Write offset
    offset: u64,
}

impl<B: BoundedIoBuf> Op<WriteAt<B>> {
    pub(crate) fn write_at(
        io_handle: &SharedIoHandle<std::fs::File>,
        buf: B,
        offset: u64,
    ) -> std::io::Result<Op<WriteAt<B>>> {
        let data = WriteAt {
            io_handle: io_handle.clone(),
            buf,
            offset,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
impl<B: BoundedIoBuf> BackendSubmit for WriteAt<B> {
    fn submit(&mut self) -> BackendSubmission {
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
impl<B: BoundedIoBuf> BackendSubmit for WriteAt<B> {
    fn submit(&mut self) -> BackendSubmission {
        // Regular files always report ready under kqueue, and pwrite can block on
        // disk I/O — offload to the blocking pool so the runtime thread stays free.
        // Buffer pointers remain valid: the Op (and its buffers) stay alive until
        // the pool job completes and `complete` runs.
        let fd = self.io_handle.raw_fd();
        let ptr = self.buf.stable_read_ptr() as usize;
        let len = self.buf.bytes_init();
        let offset = self.offset as i64;
        macos_syscall_blocking!({ macos_syscall!(libc::pwrite(fd, ptr as *const libc::c_void, len, offset)) })
    }
}

#[cfg(windows)]
impl<B: BoundedIoBuf> BackendSubmit for WriteAt<B> {
    fn submit(&mut self) -> BackendSubmission {
        use crate::driver::backends::iocp::Interest;
        use windows_sys::Win32::Storage::FileSystem::WriteFile;

        let ptr = self.buf.stable_read_ptr();
        let len = self.buf.bytes_init() as u32;
        let handle = self.io_handle.raw_handle();

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
}

impl<B: BoundedIoBuf> BackendComplete for WriteAt<B> {
    type Result = BufResult<usize, B>;

    fn complete(self, completion_entry: super::Completion) -> Self::Result {
        // Convert the operation result to `usize`
        let res = completion_entry.result.map(|v| v as usize);
        // Recover the buffer
        let buf = self.buf;

        (res, buf)
    }
}
