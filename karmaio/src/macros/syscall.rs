//! Platform-specific syscall helpers for driver `Submittable` implementations.

#[cfg(target_os = "macos")]
#[macro_use]
mod macos;

#[cfg(windows)]
#[macro_use]
mod windows;
