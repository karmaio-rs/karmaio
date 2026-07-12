//! Windows/IOCP syscall helpers for `Submittable` implementations.
//!
//! Inspired by the syscall macros in compio-driver.

/// Execute a synchronous Windows API call and return `io::Result<u32>`.
///
/// - `BOOL` — non-zero indicates success (e.g. `CreateDirectoryW`, `CloseHandle`)
/// - `BOOLEAN` — `true` indicates success (e.g. `CreateSymbolicLinkW`)
/// - `SOCKET` — zero indicates success (e.g. `closesocket`)
/// - `HANDLE` — anything other than `INVALID_HANDLE_VALUE` is success (e.g. `CreateFileW`)
macro_rules! windows_syscall {
    (BOOL, $e:expr) => {{
        #[allow(unused_unsafe)]
        let res = unsafe { $e };
        if res == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(0u32)
        }
    }};
    (BOOLEAN, $e:expr) => {{
        #[allow(unused_unsafe)]
        let res = unsafe { $e };
        if !res {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(0u32)
        }
    }};
    (SOCKET, $e:expr) => {{
        #[allow(unused_unsafe)]
        let res = unsafe { $e };
        if res != 0 {
            Err(std::io::Error::from_raw_os_error(unsafe {
                windows_sys::Win32::Networking::WinSock::WSAGetLastError()
            }))
        } else {
            Ok(0u32)
        }
    }};
    (HANDLE, $e:expr) => {{
        #[allow(unused_unsafe)]
        let res = unsafe { $e };
        if res == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res as u32)
        }
    }};
}

/// Package a synchronous Windows call as [`Submission::Blocking`] for the thread pool.
///
/// Prefer this for path-based FS APIs (`CreateDirectoryW`, `DeleteFileW`, …).
macro_rules! windows_syscall_blocking {
    ($block:block) => {{
        $crate::driver::Submission::Blocking($crate::driver::ops::BlockingJob::new(move || match { $block } {
            Ok(val) => $crate::driver::ops::Completion {
                result: Ok(val),
                flags: 0,
            },
            Err(err) => $crate::driver::ops::Completion {
                result: Err(err),
                flags: 0,
            },
        }))
    }};
}

/// Map a synchronous syscall `Result` to `Submission::Ready`.
///
/// Runs on the runtime thread. Prefer [`windows_syscall_blocking`] for true-blocking FS.
macro_rules! windows_syscall_submit {
    ($block:block) => {{
        match { $block } {
            Ok(val) => $crate::driver::Submission::Ready($crate::driver::ops::Completion {
                result: Ok(val),
                flags: 0,
            }),
            Err(err) => $crate::driver::Submission::Ready($crate::driver::ops::Completion {
                result: Err(err),
                flags: 0,
            }),
        }
    }};
}

/// Map an overlapped API result to `Submission::Pending` or `Submission::Ready(Err)`.
///
/// - `file` — Win32 `BOOL` APIs (`ReadFile`, `WriteFile`); uses `last_os_error`
/// - `socket` — Winsock APIs returning `0` on success (`WSARecv`, `WSASend`); uses `WSAGetLastError`
/// - `winsock` — Winsock `BOOL` APIs (`AcceptEx`, `ConnectEx`); uses `WSAGetLastError`
macro_rules! windows_syscall_submit_overlapped {
    ($interest:expr, file, $call:expr) => {{
        #[allow(unused_unsafe)]
        let result = unsafe { $call };
        if result != 0 {
            return $crate::driver::Submission::Pending($interest);
        }

        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_IO_PENDING as i32) {
            return $crate::driver::Submission::Pending($interest);
        }

        $crate::driver::Submission::Ready($crate::driver::ops::Completion {
            result: Err(err),
            flags: 0,
        })
    }};
    ($interest:expr, socket, $call:expr) => {{
        #[allow(unused_unsafe)]
        let result = unsafe { $call };
        if result == 0 {
            return $crate::driver::Submission::Pending($interest);
        }

        let err = unsafe { windows_sys::Win32::Networking::WinSock::WSAGetLastError() };
        if err == windows_sys::Win32::Networking::WinSock::WSA_IO_PENDING {
            return $crate::driver::Submission::Pending($interest);
        }

        $crate::driver::Submission::Ready($crate::driver::ops::Completion {
            result: Err(std::io::Error::from_raw_os_error(err)),
            flags: 0,
        })
    }};
    ($interest:expr, winsock, $call:expr) => {{
        #[allow(unused_unsafe)]
        let result = unsafe { $call };
        if result != 0 {
            return $crate::driver::Submission::Pending($interest);
        }

        let err = unsafe { windows_sys::Win32::Networking::WinSock::WSAGetLastError() };
        if err == windows_sys::Win32::Networking::WinSock::WSA_IO_PENDING {
            return $crate::driver::Submission::Pending($interest);
        }

        $crate::driver::Submission::Ready($crate::driver::ops::Completion {
            result: Err(std::io::Error::from_raw_os_error(err)),
            flags: 0,
        })
    }};
}
