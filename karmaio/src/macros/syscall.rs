//! Platform-specific syscall helpers for backend-native operation implementations.

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
#[macro_use]
mod kqueue;

#[cfg(windows)]
#[macro_use]
mod windows;
