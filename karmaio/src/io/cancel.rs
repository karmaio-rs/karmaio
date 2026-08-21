//! Opt-in eager cancellation for one-shot completion I/O.
//!
//! Ordinary `read` / `write` futures still **detach** on drop: the kernel may
//! keep the payload until a terminal completion. [`Canceller::cancel`] requests
//! platform cancellation of the currently registered operation. The observing
//! future must still be awaited; a completion that races cancellation may win
//! and return `Ok`.
//!
//! `cancel()` is sticky: later submits with the same handle never reach the
//! kernel. A successful (non-canceled) operation returns the handle to idle
//! so one canceller can serve sequential ops on the same activity. Read and
//! write activities must use separate [`Canceller`]s.

use std::{cell::RefCell, fmt, io, rc::Rc};

use crate::driver::ops::OpKey;
use crate::runtime::local::CURRENT_DRIVER;

/// Marker error for a user-requested cancellation that won the race with
/// completion.
///
/// Carried as the payload of an [`io::Error`]. Never constructed as
/// [`io::ErrorKind::Interrupted`], which helpers such as `write_all` retry.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct OperationCanceled;

impl fmt::Display for OperationCanceled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("operation canceled")
    }
}

impl std::error::Error for OperationCanceled {}

/// Construct an [`io::Error`] that [`is_operation_canceled`] recognizes.
pub fn operation_canceled() -> io::Error {
    io::Error::other(OperationCanceled)
}

/// Returns true when `err` is a user-requested or platform cancellation.
///
/// Matches [`OperationCanceled`] and the platform cancel codes (`ECANCELED` /
/// `ERROR_OPERATION_ABORTED`) so a leaked raw kernel cancel still classifies.
pub fn is_operation_canceled(err: &io::Error) -> bool {
    err.get_ref().is_some_and(|inner| inner.is::<OperationCanceled>()) || is_raw_canceled(err)
}

pub(crate) fn is_raw_canceled(err: &io::Error) -> bool {
    #[cfg(unix)]
    {
        err.raw_os_error() == Some(libc::ECANCELED)
    }
    #[cfg(windows)]
    {
        err.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED as i32)
    }
}

/// Rewrite platform cancel codes to [`OperationCanceled`] at the Cancellable API
/// boundary. Successful results are left unchanged (completion won the race).
pub(crate) fn map_cancel_result<T>(result: io::Result<T>) -> io::Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if is_raw_canceled(&error) => Err(operation_canceled()),
        Err(error) => Err(error),
    }
}

enum CancellationState {
    Idle,
    Registering,
    RegisteringCanceled,
    InFlight(OpKey),
    // The key is not carried here: the driver owns the in-flight cancel and
    // the terminal completion still arrives on the original op's key.
    Canceling,
    Canceled,
}

/// Owner of a cancellation registration.
///
/// Not `Clone`: the supervisor holds this and calls [`cancel`](Self::cancel).
/// Pass [`handle`](Self::handle) into I/O. At most one in-flight operation.
pub struct Canceller {
    inner: Rc<RefCell<CancellationState>>,
}

/// Cloneable handle passed into Cancellable I/O methods.
#[derive(Clone)]
pub struct CancelHandle {
    inner: Rc<RefCell<CancellationState>>,
}

impl Canceller {
    /// Create a canceller that is not yet bound to an operation.
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(CancellationState::Idle)),
        }
    }

    /// Handle to pass into Cancellable I/O. Cloning the handle does not clone
    /// the ability to cancel; only [`Canceller::cancel`] does that.
    pub fn handle(&self) -> CancelHandle {
        CancelHandle {
            inner: Rc::clone(&self.inner),
        }
    }

    /// Request cancellation of the registered operation, if any.
    ///
    /// Idempotent and sticky. Does not complete the I/O future; the caller
    /// that owns the future must still await it.
    pub fn cancel(&self) {
        let mut state = self.inner.borrow_mut();
        match &*state {
            CancellationState::Idle => {
                *state = CancellationState::Canceled;
            }
            CancellationState::Registering => {
                *state = CancellationState::RegisteringCanceled;
            }
            CancellationState::InFlight(key) => {
                let key = *key;
                *state = CancellationState::Canceling;
                drop(state);
                CURRENT_DRIVER.with(|driver| driver.cancel_op(key));
            }
            CancellationState::RegisteringCanceled | CancellationState::Canceling | CancellationState::Canceled => {}
        }
    }
}

impl Default for Canceller {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelHandle {
    /// Whether [`Canceller::cancel`] has been called on the paired canceller.
    pub fn is_cancel_requested(&self) -> bool {
        matches!(
            *self.inner.borrow(),
            CancellationState::Canceled | CancellationState::Canceling | CancellationState::RegisteringCanceled
        )
    }

    pub(crate) fn register(&self) -> Register<'_> {
        let mut state = self.inner.borrow_mut();
        match &*state {
            CancellationState::Canceled | CancellationState::Canceling | CancellationState::RegisteringCanceled => {
                Register::Canceled
            }
            CancellationState::Idle => {
                *state = CancellationState::Registering;
                drop(state);
                Register::Pending(Registration {
                    handle: self,
                    bound: false,
                })
            }
            CancellationState::Registering | CancellationState::InFlight(_) => {
                panic!("CancelHandle already has an in-flight operation")
            }
        }
    }

    pub(crate) fn on_op_terminal(&self) {
        let mut state = self.inner.borrow_mut();
        match &*state {
            CancellationState::InFlight(_) => {
                *state = CancellationState::Idle;
            }
            CancellationState::Canceling => {
                *state = CancellationState::Canceled;
            }
            _ => {}
        }
    }
}

pub(crate) enum Register<'a> {
    Canceled,
    Pending(Registration<'a>),
}

/// Guard that either binds a submitted key or rolls the handle back if submit
/// fails before `bind`. Lives only on the submit stack, so it borrows the
/// handle instead of cloning the `Rc`.
pub(crate) struct Registration<'a> {
    handle: &'a CancelHandle,
    bound: bool,
}

impl Registration<'_> {
    pub(crate) fn bind(mut self, key: OpKey) {
        self.bound = true;
        let mut state = self.handle.inner.borrow_mut();
        match &*state {
            CancellationState::Registering => {
                *state = CancellationState::InFlight(key);
            }
            CancellationState::RegisteringCanceled => {
                *state = CancellationState::Canceling;
                drop(state);
                CURRENT_DRIVER.with(|driver| driver.cancel_op(key));
            }
            _ => {}
        }
    }
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        if self.bound {
            return;
        }
        let mut state = self.handle.inner.borrow_mut();
        match &*state {
            CancellationState::Registering => {
                *state = CancellationState::Idle;
            }
            CancellationState::RegisteringCanceled => {
                *state = CancellationState::Canceled;
            }
            _ => {}
        }
    }
}

/// Marks the handle idle or sticky-canceled when the Cancellable future ends,
/// including on drop.
pub(crate) struct TerminalGuard<'a> {
    handle: &'a CancelHandle,
}

impl<'a> TerminalGuard<'a> {
    pub(crate) fn new(handle: &'a CancelHandle) -> Self {
        Self { handle }
    }
}

impl Drop for TerminalGuard<'_> {
    fn drop(&mut self) {
        self.handle.on_op_terminal();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_before_register_is_sticky() {
        let canceller = Canceller::new();
        let handle = canceller.handle();
        canceller.cancel();
        assert!(handle.is_cancel_requested());
        assert!(matches!(handle.register(), Register::Canceled));
        assert!(matches!(handle.register(), Register::Canceled));
    }

    #[test]
    fn registration_drop_without_bind_restores_idle() {
        let canceller = Canceller::new();
        let handle = canceller.handle();
        {
            let Register::Pending(_reg) = handle.register() else {
                panic!("expected pending registration");
            };
        }
        assert!(!handle.is_cancel_requested());
        assert!(matches!(handle.register(), Register::Pending(_)));
    }

    #[test]
    fn cancel_during_register_without_bind_stays_canceled() {
        let canceller = Canceller::new();
        let handle = canceller.handle();
        {
            let Register::Pending(_reg) = handle.register() else {
                panic!("expected pending registration");
            };
            canceller.cancel();
        }
        assert!(handle.is_cancel_requested());
        assert!(matches!(handle.register(), Register::Canceled));
    }

    #[test]
    fn operation_canceled_classifies() {
        let err = operation_canceled();
        assert!(is_operation_canceled(&err));
        assert!(!is_operation_canceled(&io::Error::other("other")));
        assert_ne!(err.kind(), io::ErrorKind::Interrupted);
    }
}
