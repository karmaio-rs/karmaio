//! Cancellation implementation for the runtime's public cancellation API.

use std::{
    fmt,
    future::Future,
    io,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use crate::driver::{Handle, helpers::scopes::ScopeId};
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

/// Rewrite platform cancel codes to [`OperationCanceled`] at the I/O boundary.
/// Successful results are left unchanged (completion won the race).
pub(crate) fn map_cancel_result<T>(result: io::Result<T>) -> io::Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if is_raw_canceled(&error) => Err(operation_canceled()),
        Err(error) => Err(error),
    }
}

/// Owner of a sticky cancellation scope covering zero or more in-flight ops.
///
/// Not `Clone`: only this value can [`cancel`](Self::cancel). Share it with
/// `Rc<CancellationSource>` if several owners need the authority. Tokens
/// obtained from [`token`](Self::token) observe and register but cannot cancel.
///
/// # Panics
///
/// [`CancellationSource::new`] panics outside a karmaio runtime.
pub struct CancellationSource {
    id: ScopeId,
    driver: Handle,
    _local: PhantomData<*const ()>,
}

/// Cloneable, `Copy` handle that registers I/O with a [`CancellationSource`].
///
/// A token whose source has been dropped, or whose runtime is gone, reports
/// cancelled.
#[derive(Clone, Copy)]
pub struct CancellationToken {
    id: ScopeId,
    _local: PhantomData<*const ()>,
}

impl CancellationSource {
    /// Create a live cancellation scope on the current runtime.
    ///
    /// # Panics
    ///
    /// Panics if called outside a karmaio runtime.
    pub fn new() -> Self {
        CURRENT_DRIVER.with(|driver| {
            let driver = driver.clone();
            Self {
                id: driver.insert_scope(),
                driver,
                _local: PhantomData,
            }
        })
    }

    /// A token to pass into [`FutureExt::with_cancellation`].
    pub fn token(&self) -> CancellationToken {
        CancellationToken {
            id: self.id,
            _local: PhantomData,
        }
    }

    /// Request cancellation of every operation currently registered, and of
    /// every later attach.
    ///
    /// Idempotent and non-blocking. Does not complete observing futures.
    pub fn cancel(&self) {
        self.driver.cancel_scope(self.id);
    }

    /// Whether [`cancel`](Self::cancel) has been called or the owning runtime is gone.
    pub fn is_cancel_requested(&self) -> bool {
        self.driver.scope_is_cancelled(self.id)
    }
}

impl Default for CancellationSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CancellationSource {
    fn drop(&mut self) {
        self.driver.remove_scope(self.id);
    }
}

impl CancellationToken {
    /// Whether the paired source has requested cancellation or been dropped.
    ///
    /// # Panics
    ///
    /// Panics if called outside a karmaio runtime.
    pub fn is_cancel_requested(&self) -> bool {
        assert!(CURRENT_DRIVER.is_set(), "Not in runtime context");
        CURRENT_DRIVER.with(|driver| driver.scope_is_cancelled(self.id))
    }

    /// Completes when cancellation has been requested.
    ///
    /// # Panics
    ///
    /// Panics if polled outside a karmaio runtime.
    pub fn cancelled(&self) -> WaitFuture {
        WaitFuture {
            id: self.id,
            driver: None,
            registration: None,
            _local: PhantomData,
        }
    }
}

/// Future returned by [`CancellationToken::cancelled`].
///
/// Dropping this future removes its waker registration from the source.
#[must_use = "futures do nothing unless polled or awaited"]
pub struct WaitFuture {
    id: ScopeId,
    driver: Option<Handle>,
    registration: Option<crate::driver::helpers::scopes::WaiterId>,
    _local: PhantomData<*const ()>,
}

impl Future for WaitFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.driver.is_none() {
            assert!(CURRENT_DRIVER.is_set(), "Not in runtime context");
            self.driver = Some(CURRENT_DRIVER.with(Clone::clone));
        }
        let driver = self.driver.as_ref().expect("cancellation waiter driver missing");
        match driver.subscribe_scope(self.id, self.registration, cx.waker().clone()) {
            crate::driver::helpers::scopes::SubscribeResult::Ready => {
                self.registration = None;
                Poll::Ready(())
            }
            crate::driver::helpers::scopes::SubscribeResult::Pending(registration) => {
                self.registration = Some(registration);
                Poll::Pending
            }
        }
    }
}

impl Drop for WaitFuture {
    fn drop(&mut self) {
        if let (Some(driver), Some(registration)) = (&self.driver, self.registration) {
            driver.unsubscribe_scope(self.id, registration);
        }
    }
}

/// Extension methods for attaching a [`CancellationToken`] to a future.
pub trait FutureExt: Future {
    /// Register karmaio I/O operations submitted while this future is polled
    /// with `token`.
    ///
    /// Fail-slow: when the token is cancelled the inner future is still polled
    /// to completion so buffers can be recovered. Nested combinators all apply.
    ///
    /// Bind the token before the operation's first poll. Wrapping an operation
    /// after it has already been submitted does not attach it retroactively.
    /// This combinator does not make arbitrary futures complete on cancellation
    /// and does not propagate into independently polled spawned tasks. Pass a
    /// token into each spawned task and wrap its karmaio I/O there.
    fn with_cancellation(self, token: CancellationToken) -> WithCancellation<Self>
    where
        Self: Sized,
    {
        WithCancellation::new(self, token)
    }
}

impl<F: Future> FutureExt for F {}

/// Future returned by [`FutureExt::with_cancellation`].
///
/// Also implements [`crate::io::Stream`] when wrapping a stream. Wrap the
/// stream before its first [`crate::io::Stream::next`] call; already-submitted
/// operations are not attached retroactively.
pub struct WithCancellation<F> {
    pub(crate) future: F,
    pub(crate) token: CancellationToken,
}

impl<F> WithCancellation<F> {
    pub(crate) fn new(future: F, token: CancellationToken) -> Self {
        Self { future, token }
    }
}

impl<F: Future> Future for WithCancellation<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let token = self.token;
        // Safety: `token` is `Copy` and is not pinned. We only project `future`.
        let future = unsafe { self.map_unchecked_mut(|this| &mut this.future) };
        crate::driver::helpers::scopes::with_scope(token.id, || future.poll(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        mem::ManuallyDrop,
        rc::Rc,
        task::{RawWaker, RawWakerVTable, Waker},
    };

    struct LocalWake(Box<dyn Fn()>);

    fn local_waker(callback: impl Fn() + 'static) -> Waker {
        unsafe fn clone(data: *const ()) -> RawWaker {
            // Safety: `data` was created by `Rc::into_raw` for `LocalWake`.
            let state = ManuallyDrop::new(unsafe { Rc::<LocalWake>::from_raw(data.cast()) });
            let cloned = Rc::clone(&state);
            RawWaker::new(Rc::into_raw(cloned).cast(), &VTABLE)
        }

        unsafe fn wake(data: *const ()) {
            // Safety: `wake` consumes the strong reference represented by data.
            let state = unsafe { Rc::<LocalWake>::from_raw(data.cast()) };
            (state.0)();
        }

        unsafe fn wake_by_ref(data: *const ()) {
            // Safety: the `ManuallyDrop` keeps the borrowed strong reference alive.
            let state = ManuallyDrop::new(unsafe { Rc::<LocalWake>::from_raw(data.cast()) });
            (state.0)();
        }

        unsafe fn drop_waker(data: *const ()) {
            // Safety: `drop_waker` consumes the strong reference represented by data.
            drop(unsafe { Rc::<LocalWake>::from_raw(data.cast()) });
        }

        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);

        let raw = RawWaker::new(Rc::into_raw(Rc::new(LocalWake(Box::new(callback)))).cast(), &VTABLE);
        // Safety: the vtable maintains the `Rc<LocalWake>` strong-reference count.
        unsafe { Waker::from_raw(raw) }
    }

    #[test]
    fn operation_canceled_classifies() {
        let err = operation_canceled();
        assert!(is_operation_canceled(&err));
        assert!(!is_operation_canceled(&io::Error::other("other")));
        assert_ne!(err.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn source_token_cancel_is_sticky() {
        crate::Runtime::new().unwrap().block_on(async {
            let source = CancellationSource::new();
            let token = source.token();
            let cloned = token;
            assert!(!token.is_cancel_requested());
            source.cancel();
            source.cancel();
            assert!(source.is_cancel_requested());
            assert!(token.is_cancel_requested());
            assert!(cloned.is_cancel_requested());
        });
    }

    #[test]
    fn dropping_source_cancels_tokens() {
        crate::Runtime::new().unwrap().block_on(async {
            let token = {
                let source = CancellationSource::new();
                source.token()
            };
            assert!(token.is_cancel_requested());
        });
    }

    #[test]
    fn dropping_source_between_runtime_entries_cancels_tokens() {
        let mut runtime = crate::Runtime::new().unwrap();
        let (source, token) = runtime.block_on(async {
            let source = CancellationSource::new();
            let token = source.token();
            (source, token)
        });

        drop(source);

        runtime.block_on(async {
            assert!(token.is_cancel_requested());
            token.cancelled().await;
        });
    }

    #[test]
    fn token_observation_requires_runtime_context() {
        let mut runtime = crate::Runtime::new().unwrap();
        let (source, token) = runtime.block_on(async {
            let source = CancellationSource::new();
            let token = source.token();
            (source, token)
        });

        assert!(!source.is_cancel_requested());
        assert!(std::panic::catch_unwind(|| token.is_cancel_requested()).is_err());
    }

    #[test]
    fn token_from_another_runtime_is_cancelled() {
        let mut first = crate::Runtime::new().unwrap();
        let mut second = crate::Runtime::new().unwrap();
        let (source, token) = first.block_on(async {
            let source = CancellationSource::new();
            let token = source.token();
            (source, token)
        });

        second.block_on(async {
            assert!(token.is_cancel_requested());
            token.cancelled().await;
        });

        first.block_on(async {
            assert!(!source.is_cancel_requested());
        });
    }

    #[test]
    fn cancelled_wait_completes_after_cancel() {
        crate::Runtime::new().unwrap().block_on(async {
            let source = CancellationSource::new();
            let token = source.token();
            crate::runtime::spawn_local(async move {
                source.cancel();
            });
            token.cancelled().await;
        });
    }

    #[test]
    fn dropping_cancelled_wait_releases_its_waker() {
        crate::Runtime::new().unwrap().block_on(async {
            let source = CancellationSource::new();
            let token = source.token();
            let mut waiter = Box::pin(token.cancelled());

            std::future::poll_fn(|cx| {
                assert!(waiter.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            assert_eq!(source.driver.scope_waiter_count(source.id), 1);

            drop(waiter);
            assert_eq!(source.driver.scope_waiter_count(source.id), 0);
        });
    }

    #[test]
    fn cancellation_wakes_after_releasing_scope_table_borrow() {
        crate::Runtime::new().unwrap().block_on(async {
            let source = CancellationSource::new();
            let token = source.token();
            let driver = source.driver.clone();
            let id = source.id;
            let waker = local_waker(move || assert!(driver.scope_is_cancelled(id)));
            let mut cx = Context::from_waker(&waker);
            let mut waiter = Box::pin(token.cancelled());

            assert!(waiter.as_mut().poll(&mut cx).is_pending());
            source.cancel();
            assert!(waiter.as_mut().poll(&mut cx).is_ready());
        });
    }

    #[test]
    fn with_cancellation_installs_a_scope_frame() {
        crate::Runtime::new().unwrap().block_on(async {
            let source = CancellationSource::new();
            let token = source.token();
            std::future::poll_fn(|_cx| {
                assert_eq!(crate::driver::helpers::scopes::current_scope_ids().len(), 1);
                Poll::Ready(())
            })
            .with_cancellation(token)
            .await;
        });
    }

    #[test]
    fn nested_with_cancellation_frames_all_apply() {
        crate::Runtime::new().unwrap().block_on(async {
            let outer = CancellationSource::new();
            let inner = CancellationSource::new();
            std::future::poll_fn(|_cx| {
                assert_eq!(crate::driver::helpers::scopes::current_scope_ids().len(), 2);
                Poll::Ready(())
            })
            .with_cancellation(outer.token())
            .with_cancellation(inner.token())
            .await;
        });
    }
}
