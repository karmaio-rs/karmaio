//! macOS/kqueue syscall helpers for `PollOperation` implementations.

/// Execute a syscall that returns a non-negative value on success.
///
/// Works for `ssize_t`/`fd` returns as well as `int` syscalls that return `0` on success
/// and `-1` on error (e.g. `close`, `connect`, `fstat`).
macro_rules! macos_syscall {
    ($e:expr) => {{
        #[allow(unused_unsafe)]
        let res = unsafe { $e };
        if res < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res as u32)
        }
    }};
}

macro_rules! __macos_syscall_ready {
    ($result:expr) => {
        return $crate::driver::backends::kqueue::PollAttempt::Ready($crate::driver::ops::Completion { result: $result })
    };
}

macro_rules! __macos_syscall_register {
    ($fd:expr, $filter:expr) => {
        return $crate::driver::backends::kqueue::PollAttempt::Register($crate::driver::backends::kqueue::Interest::new(
            $fd,
            $filter,
            libc::EV_ADD | libc::EV_ONESHOT,
        ))
    };
}

/// Package a synchronous syscall as [`PollAttempt::Blocking`] for the thread pool.
///
/// Captures outer locals with `move`. Prefer this for path-based FS ops and other
/// syscalls that cannot use kqueue readiness (open, mkdir, rename, fstat, …).
///
/// ```ignore
/// let path = self.path.clone();
/// macos_syscall_blocking!({
///     macos_syscall!(libc::mkdir(path.as_c_str().as_ptr(), mode))
/// })
/// ```
macro_rules! macos_syscall_blocking {
    ($block:block) => {{
        $crate::driver::backends::kqueue::PollAttempt::Blocking($crate::driver::ops::BlockingJob::new(move || {
            loop {
                match { $block } {
                    Ok(val) => {
                        return $crate::driver::ops::Completion { result: Ok(val) };
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(err) => {
                        return $crate::driver::ops::Completion { result: Err(err) };
                    }
                }
            }
        }))
    }};
}

/// Retry a syscall on `EINTR` and return a `PollAttempt`.
///
/// # Forms
///
/// Synchronous syscall (no kqueue registration) — **runs on the runtime thread**.
/// Prefer [`macos_syscall_blocking`] for FS and other true-blocking calls:
/// ```ignore
/// macos_syscall_submit!({ macos_syscall!(...) })
/// ```
///
/// Non-blocking I/O — register on `EAGAIN`/`WouldBlock`:
/// ```ignore
/// macos_syscall_submit!($fd, $kqueue_filter, { macos_syscall!(...) })
/// ```
///
/// `connect(2)` — treat `EISCONN` as success, register on `EINPROGRESS`:
/// ```ignore
/// macos_syscall_submit!(connect $fd, { macos_syscall!(...) })
/// ```
macro_rules! macos_syscall_submit {
    ($block:block) => {
        macos_syscall_submit!(@loop $block)
    };
    ($fd:expr, $filter:expr, $block:block) => {
        macos_syscall_submit!(@loop $block;
            register ($fd, $filter)
        )
    };
    (connect $fd:expr, $block:block) => {
        macos_syscall_submit!(@loop $block;
            connect $fd
        )
    };
    (@loop $block:block) => {{
        loop {
            match { $block } {
                Ok(val) => __macos_syscall_ready!(Ok(val)),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => __macos_syscall_ready!(Err(err)),
            }
        }
    }};
    (@loop $block:block; register ($fd:expr, $filter:expr)) => {{
        loop {
            match { $block } {
                Ok(val) => __macos_syscall_ready!(Ok(val)),
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.raw_os_error() == Some(libc::EAGAIN) =>
                {
                    __macos_syscall_register!($fd, $filter);
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => __macos_syscall_ready!(Err(err)),
            }
        }
    }};
    (@loop $block:block; connect $fd:expr) => {{
        loop {
            match { $block } {
                Ok(val) => __macos_syscall_ready!(Ok(val)),
                Err(err) if err.raw_os_error() == Some(libc::EISCONN) => {
                    __macos_syscall_ready!(Ok(0));
                }
                Err(err)
                    if err.raw_os_error() == Some(libc::EINPROGRESS)
                        || err.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    __macos_syscall_register!($fd, libc::EVFILT_WRITE);
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => __macos_syscall_ready!(Err(err)),
            }
        }
    }};
}
