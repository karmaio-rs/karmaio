//! Compile-time selection of the runtime's concrete backend.
//!
//! There is intentionally no cross-platform backend trait here. `PlatformBackend`
//! and the operation protocol are selected together, so calls from [`Driver`]
//! are statically dispatched to the target implementation.
//!
//! # When do operations start?
//!
//! Backend protocols look similar (`submit` / `attempt` + `complete`) but the
//! driver invokes them at different points in the lifecycle:
//!
//! - **io_uring** (`UringOperation::submit`): builds the SQE in
//!   `submit_op`. The kernel owns buffers after the SQE is pushed.
//! - **IOCP** (`IocpOperation::submit`): starts the overlapped call in
//!   `submit_op` (or parks a `Blocking` job until first `poll_op`).
//! - **kqueue** (`KqueueOperation::attempt`): runs on first `poll_op` (and
//!   again when readiness re-arms). Registration / blocking offload happens
//!   there, not in `submit_op`.
//!
//! Typed `complete` always runs outside the driver's backend `RefCell` borrow
//! so drop and resource construction can re-enter the driver safely.

#[cfg(target_os = "windows")]
pub(crate) mod iocp;
#[cfg(target_os = "linux")]
pub(crate) mod iouring;
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
pub(crate) mod kqueue;

#[cfg(target_os = "windows")]
pub(crate) use self::iocp::{IocpBackend as PlatformBackend, IocpOperation as Operation};
#[cfg(target_os = "linux")]
pub(crate) use self::iouring::{IoUringBackend as PlatformBackend, UringOperation as Operation};
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
pub(crate) use self::kqueue::{KqueueBackend as PlatformBackend, KqueueOperation as Operation};
