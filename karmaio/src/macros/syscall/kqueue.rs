//! Kqueue syscall helpers for `KqueueOperation` implementations.

/// Execute a syscall that returns a non-negative value on success.
///
/// Works for `ssize_t`/`fd` returns as well as `int` syscalls that return `0` on success
/// and `-1` on error (e.g. `close`, `connect`, `fstat`).
macro_rules! kqueue_syscall {
    ($e:expr) => {{
        #[allow(unused_unsafe)]
        let res = unsafe { $e };
        if res < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            u32::try_from(res)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "system call result exceeds u32"))
        }
    }};
}

macro_rules! __kqueue_syscall_ready {
    ($result:expr) => {
        return $crate::driver::backends::kqueue::KqueueAttempt::Ready(
            $crate::driver::ops::Completion::new($result),
        )
    };
}

macro_rules! __kqueue_syscall_register {
    ($fd:expr, $filter:expr) => {
        return $crate::driver::backends::kqueue::KqueueAttempt::Register {
            interest: $crate::driver::backends::kqueue::Interest::new($fd, $filter),
            on_ready: $crate::driver::backends::kqueue::KqueueReadyAction::Retry,
        }
    };
}

macro_rules! __kqueue_syscall_register_connect {
    ($fd:expr) => {
        return $crate::driver::backends::kqueue::KqueueAttempt::Register {
            interest: $crate::driver::backends::kqueue::Interest::new(
                $fd,
                $crate::driver::backends::kqueue::Direction::Write,
            ),
            on_ready: $crate::driver::backends::kqueue::KqueueReadyAction::CompleteSocketError,
        }
    };
}

/// Package a synchronous syscall as [`KqueueAttempt::Blocking`] for the thread pool.
///
/// Captures outer locals with `move`. Prefer this for path-based FS ops and other
/// syscalls that cannot use kqueue readiness (open, mkdir, rename, fstat, …).
macro_rules! kqueue_syscall_blocking {
    ($block:block) => {{
        // EINTR is retried by [`BlockingJob::run`]; the closure itself is a single attempt.
        $crate::driver::backends::kqueue::KqueueAttempt::Blocking($crate::driver::ops::BlockingJob::new(move || {
            match { $block } {
                Ok(val) => $crate::driver::ops::Completion::new(Ok(val)),
                Err(err) => $crate::driver::ops::Completion::new(Err(err)),
            }
        }))
    }};
}

/// Retry a syscall on `EINTR` and return a `KqueueAttempt`.
///
/// The non-blocking form registers a one-shot kqueue filter on `EAGAIN`/`WouldBlock`.
/// The `connect` form treats `EISCONN` as success and registers a write filter
/// whose readiness result is read from `SO_ERROR`.
macro_rules! kqueue_syscall_submit {
    ($block:block) => {
        kqueue_syscall_submit!(@loop $block)
    };
    ($fd:expr, $filter:expr, $block:block) => {
        kqueue_syscall_submit!(@loop $block;
            register ($fd, $filter)
        )
    };
    (connect $fd:expr, $block:block) => {
        kqueue_syscall_submit!(@loop $block;
            connect $fd
        )
    };
    (@loop $block:block) => {{
        loop {
            match { $block } {
                Ok(val) => __kqueue_syscall_ready!(Ok(val)),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => __kqueue_syscall_ready!(Err(err)),
            }
        }
    }};
    (@loop $block:block; register ($fd:expr, $filter:expr)) => {{
        loop {
            match { $block } {
                Ok(val) => __kqueue_syscall_ready!(Ok(val)),
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.raw_os_error() == Some(libc::EAGAIN) =>
                {
                    __kqueue_syscall_register!($fd, $filter);
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => __kqueue_syscall_ready!(Err(err)),
            }
        }
    }};
    (@loop $block:block; connect $fd:expr) => {{
        loop {
            match { $block } {
                Ok(val) => __kqueue_syscall_ready!(Ok(val)),
                Err(err) if err.raw_os_error() == Some(libc::EISCONN) => {
                    __kqueue_syscall_ready!(Ok(0));
                }
                Err(err)
                    if err.raw_os_error() == Some(libc::EINPROGRESS)
                        || err.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    __kqueue_syscall_register_connect!($fd);
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => __kqueue_syscall_ready!(Err(err)),
            }
        }
    }};
}
