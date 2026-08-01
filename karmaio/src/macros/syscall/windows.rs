//! Windows/IOCP syscall helpers for `IocpOperation` implementations.

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

/// Package a synchronous Windows call as [`IocpSubmission::Blocking`] for the thread pool.
///
/// Prefer this for path-based FS APIs (`CreateDirectoryW`, `DeleteFileW`, …).
macro_rules! windows_syscall_blocking {
    ($block:block) => {{
        $crate::driver::backends::iocp::IocpSubmission::Blocking($crate::driver::ops::BlockingJob::new(move || match {
            $block
        } {
            Ok(val) => $crate::driver::ops::Completion { result: Ok(val) },
            Err(err) => $crate::driver::ops::Completion { result: Err(err) },
        }))
    }};
}

/// Map an overlapped API result to `IocpSubmission::Pending` or `IocpSubmission::Ready(Err)`.
///
/// - `file` — Win32 `BOOL` APIs (`ReadFile`, `WriteFile`); uses `last_os_error`
/// - `socket` — Winsock APIs returning `0` on success (`WSARecv`, `WSASend`); uses `WSAGetLastError`
/// - `winsock` — Winsock `BOOL` APIs (`AcceptEx`, `ConnectEx`); uses `WSAGetLastError`
macro_rules! windows_syscall_submit_overlapped {
    ($interest:expr, file, $call:expr) => {{
        #[allow(unused_unsafe)]
        let result = unsafe { $call };
        if result != 0 {
            return $crate::driver::backends::iocp::IocpSubmission::Pending($interest);
        }

        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_IO_PENDING as i32) {
            return $crate::driver::backends::iocp::IocpSubmission::Pending($interest);
        }

        $crate::driver::backends::iocp::IocpSubmission::Ready($crate::driver::ops::Completion { result: Err(err) })
    }};
    ($interest:expr, socket, $call:expr) => {{
        #[allow(unused_unsafe)]
        let result = unsafe { $call };
        if result == 0 {
            return $crate::driver::backends::iocp::IocpSubmission::Pending($interest);
        }

        let err = unsafe { windows_sys::Win32::Networking::WinSock::WSAGetLastError() };
        if err == windows_sys::Win32::Networking::WinSock::WSA_IO_PENDING {
            return $crate::driver::backends::iocp::IocpSubmission::Pending($interest);
        }

        $crate::driver::backends::iocp::IocpSubmission::Ready($crate::driver::ops::Completion {
            result: Err(std::io::Error::from_raw_os_error(err)),
        })
    }};
    ($interest:expr, winsock, $call:expr) => {{
        #[allow(unused_unsafe)]
        let result = unsafe { $call };
        if result != 0 {
            return $crate::driver::backends::iocp::IocpSubmission::Pending($interest);
        }

        let err = unsafe { windows_sys::Win32::Networking::WinSock::WSAGetLastError() };
        if err == windows_sys::Win32::Networking::WinSock::WSA_IO_PENDING {
            return $crate::driver::backends::iocp::IocpSubmission::Pending($interest);
        }

        $crate::driver::backends::iocp::IocpSubmission::Ready($crate::driver::ops::Completion {
            result: Err(std::io::Error::from_raw_os_error(err)),
        })
    }};
}
