//! Compile-time selection of the runtime's concrete backend.
//!
//! There is intentionally no cross-platform backend trait here. `PlatformBackend`
//! and the operation protocol are selected together, so calls from [`Driver`]
//! are statically dispatched to the target implementation.

#[cfg(target_os = "windows")]
pub(crate) mod iocp;
#[cfg(target_os = "linux")]
pub(crate) mod iouring;
#[cfg(target_os = "macos")]
pub(crate) mod kqueue;

#[cfg(target_os = "windows")]
pub(crate) use self::iocp::{IocpBackend as PlatformBackend, IocpOperation as Operation};
#[cfg(target_os = "linux")]
pub(crate) use self::iouring::{IoUringBackend as PlatformBackend, UringOperation as Operation};
#[cfg(target_os = "macos")]
pub(crate) use self::kqueue::{KqueueBackend as PlatformBackend, PollOperation as Operation};
