//! Unix signal handling.
//!
//! An OS signal handler is installed lazily, once per distinct signal.
//! When the signal fires, the handler walks the global registry
//! (read through an async-signal-safe [`HalfLock`](super::half_lock::HalfLock))
//! and notifies every [`Signal`] listening for that signal directly:
//! it bumps a per-listener counter and wakes the stored waker.
//! Listeners compare the counter against the last value they observed,
//! so rapid deliveries coalesce into a single notification.

use std::{
    collections::HashMap,
    future::{Future, poll_fn},
    io,
    pin::Pin,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

use slab::Slab;

use crate::signal::half_lock::HalfLock;

/// Per-listener shared state.
///
/// The signal handler bumps `counter` and wakes `waker`; the [`Signal`] future
/// observes `counter` to decide whether it has been notified since it last checked.
struct SignalState {
    counter: AtomicUsize,
    waker: Mutex<Option<Waker>>,
}

/// A single registered listener, stored in the global registry.
#[derive(Clone)]
struct Entry {
    signum: libc::c_int,
    state: Arc<SignalState>,
}

/// Registry of all active listeners, read from the signal handler.
static REGISTRY: LazyLock<HalfLock<Slab<Entry>>> = LazyLock::new(HalfLock::default);

/// Previous `sigaction` for each signal we installed a handler for, so it can be
/// restored once the last listener for that signal goes away. Only touched by
/// the (serialized) register/unregister paths, never by the signal handler.
static SAVED: LazyLock<Mutex<HashMap<libc::c_int, libc::sigaction>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Identifies a Unix signal.
///
/// Common signals are available through the named constructors; use
/// [`SignalKind::new`] for anything else.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SignalKind(libc::c_int);

impl SignalKind {
    /// Create a `SignalKind` from a raw signal number.
    #[inline]
    pub const fn new(raw: libc::c_int) -> Self {
        Self(raw)
    }

    /// Return the raw signal number.
    #[inline]
    pub const fn as_raw(self) -> libc::c_int {
        self.0
    }

    /// `SIGINT` — interrupt from keyboard (Ctrl-C).
    #[inline]
    pub const fn interrupt() -> Self {
        Self(libc::SIGINT)
    }

    /// `SIGTERM` — termination request.
    #[inline]
    pub const fn terminate() -> Self {
        Self(libc::SIGTERM)
    }

    /// `SIGHUP` — controlling terminal closed or daemon reload.
    #[inline]
    pub const fn hangup() -> Self {
        Self(libc::SIGHUP)
    }

    /// `SIGQUIT` — quit from keyboard.
    #[inline]
    pub const fn quit() -> Self {
        Self(libc::SIGQUIT)
    }

    /// `SIGUSR1` — user-defined signal 1.
    #[inline]
    pub const fn user_defined1() -> Self {
        Self(libc::SIGUSR1)
    }

    /// `SIGUSR2` — user-defined signal 2.
    #[inline]
    pub const fn user_defined2() -> Self {
        Self(libc::SIGUSR2)
    }

    /// `SIGCHLD` — child process stopped or terminated.
    #[inline]
    pub const fn child() -> Self {
        Self(libc::SIGCHLD)
    }

    /// `SIGALRM` — timer expired.
    #[inline]
    pub const fn alarm() -> Self {
        Self(libc::SIGALRM)
    }

    /// `SIGPIPE` — write to a pipe with no readers.
    #[inline]
    pub const fn pipe() -> Self {
        Self(libc::SIGPIPE)
    }
}

impl From<SignalKind> for libc::c_int {
    #[inline]
    fn from(kind: SignalKind) -> libc::c_int {
        kind.0
    }
}

/// A listener for a specific Unix signal.
///
/// Created with [`signal`] or [`Signal::new`]. Call [`Signal::recv`] to wait for
/// the next delivery. Multiple listeners may exist for the same signal; every
/// one of them is notified on each delivery.
///
/// # Examples
///
/// ```no_run
/// # async fn run() -> std::io::Result<()> {
/// use karmaio::signal::{signal, SignalKind};
///
/// let mut hup = signal(SignalKind::hangup())?;
/// hup.recv().await?;
/// # Ok(())
/// # }
/// ```
pub struct Signal {
    kind: SignalKind,
    state: Arc<SignalState>,
    key: usize,
    last_seen: usize,
}

impl Signal {
    /// Register a listener for `kind`, installing the OS handler if needed.
    pub fn new(kind: SignalKind) -> io::Result<Self> {
        let (state, key) = register(kind)?;
        let last_seen = state.counter.load(Ordering::Acquire);
        Ok(Self {
            kind,
            state,
            key,
            last_seen,
        })
    }

    /// Return the signal this listener is waiting for.
    #[inline]
    pub fn kind(&self) -> SignalKind {
        self.kind
    }

    /// Wait for the next delivery of the signal.
    ///
    /// Deliveries coalesce: if several arrive before this future is polled, they
    /// are observed as a single notification.
    pub async fn recv(&mut self) -> io::Result<()> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Fast path: a signal arrived since we last looked.
        let current = self.state.counter.load(Ordering::Acquire);
        if current != self.last_seen {
            self.last_seen = current;
            return Poll::Ready(Ok(()));
        }

        // Register our waker, then re-check the counter. The re-check closes the
        // race where the handler bumps the counter between the load above and
        // the waker being stored.
        {
            let mut slot = self.state.waker.lock().unwrap_or_else(|e| e.into_inner());
            match &*slot {
                Some(existing) if existing.will_wake(cx.waker()) => {}
                _ => *slot = Some(cx.waker().clone()),
            }
        }

        let current = self.state.counter.load(Ordering::Acquire);
        if current != self.last_seen {
            self.last_seen = current;
            return Poll::Ready(Ok(()));
        }

        Poll::Pending
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
        unregister(self.kind, self.key);
    }
}

/// Create a listener for `kind`.
///
/// This is a convenience wrapper around [`Signal::new`].
#[inline]
pub fn signal(kind: SignalKind) -> io::Result<Signal> {
    Signal::new(kind)
}

/// A future that completes when the process receives Ctrl-C (`SIGINT`).
///
/// Returned by [`ctrl_c`].
pub struct CtrlC {
    signal: Signal,
}

impl CtrlC {
    /// Create a new Ctrl-C listener.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            signal: Signal::new(SignalKind::interrupt())?,
        })
    }
}

impl Future for CtrlC {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `CtrlC` is `Unpin` in practice; we never move out of the
        // pinned reference, only project to the inner `Signal`.
        let this = unsafe { self.get_unchecked_mut() };
        this.signal.poll_recv(cx)
    }
}

/// Return a future that completes on the next Ctrl-C (`SIGINT`).
///
/// # Examples
///
/// ```no_run
/// # async fn run() -> std::io::Result<()> {
/// karmaio::signal::ctrl_c().await?;
/// # Ok(())
/// # }
/// ```
#[inline]
pub fn ctrl_c() -> io::Result<CtrlC> {
    CtrlC::new()
}

/// The OS signal handler.
///
/// Runs on an arbitrary thread with the interrupted thread suspended, so it must
/// stay async-signal-safe. It only reads the registry via the lock-free
/// [`HalfLock`], performs atomic counter increments, and uses `try_lock` to
/// avoid ever blocking on a mutex the interrupted thread might hold.
extern "C" fn signal_handler(signum: libc::c_int) {
    for (_, entry) in REGISTRY.read().iter() {
        if entry.signum != signum {
            continue;
        }

        entry.state.counter.fetch_add(1, Ordering::Release);
        if let Ok(mut slot) = entry.state.waker.try_lock()
            && let Some(waker) = slot.take()
        {
            waker.wake();
        }
        // If `try_lock` fails, the counter bump is still visible: the poll path
        // re-checks the counter after registering its waker, so no wakeup is
        // lost.
    }
}

/// Insert a listener into the registry, installing the OS handler on first use
/// for a given signal.
fn register(kind: SignalKind) -> io::Result<(Arc<SignalState>, usize)> {
    let state = Arc::new(SignalState {
        counter: AtomicUsize::new(0),
        waker: Mutex::new(None),
    });

    let mut guard = REGISTRY.write();
    let already_installed = guard.iter().any(|(_, entry)| entry.signum == kind.0);

    // Install before publishing the new registry so a delivery can never reach
    // the default action once a listener is visible.
    if !already_installed {
        let prev = unsafe { install_handler(kind.0)? };
        SAVED.lock().unwrap_or_else(|e| e.into_inner()).insert(kind.0, prev);
    }

    let mut new = Slab::clone(&*guard);
    let key = new.insert(Entry {
        signum: kind.0,
        state: Arc::clone(&state),
    });
    guard.store(new);

    Ok((state, key))
}

/// Remove a listener, restoring the previous OS handler once the last listener
/// for a signal is gone.
fn unregister(kind: SignalKind, key: usize) {
    let mut guard = REGISTRY.write();
    let mut new = Slab::clone(&*guard);
    if new.contains(key) {
        new.remove(key);
    }

    let still_listening = new.iter().any(|(_, entry)| entry.signum == kind.0);
    if !still_listening && let Some(prev) = SAVED.lock().unwrap_or_else(|e| e.into_inner()).remove(&kind.0) {
        unsafe {
            let _ = restore_handler(kind.0, &prev);
        }
    }

    guard.store(new);
}

/// Install [`signal_handler`] for `signum`, returning the previous action.
///
/// # Safety
///
/// Modifies process-global signal disposition; callers must serialize
/// installation (done here via the registry write lock).
unsafe fn install_handler(signum: libc::c_int) -> io::Result<libc::sigaction> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = signal_handler as extern "C" fn(libc::c_int) as usize;
    action.sa_flags = libc::SA_RESTART;
    unsafe { libc::sigemptyset(&mut action.sa_mask) };

    let mut prev: libc::sigaction = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::sigaction(signum, &action, &mut prev) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(prev)
}

/// Restore a previously saved signal action.
///
/// # Safety
///
/// Modifies process-global signal disposition; `prev` must be an action
/// previously returned by [`install_handler`].
unsafe fn restore_handler(signum: libc::c_int, prev: &libc::sigaction) -> io::Result<()> {
    let rc = unsafe { libc::sigaction(signum, prev, std::ptr::null_mut()) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::runtime::Runtime;
    use crate::time::timeout;

    async fn with_timeout(fut: impl Future<Output = io::Result<()>>) -> io::Result<()> {
        timeout(Duration::from_secs(2), fut)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "signal timeout"))?
    }

    fn raise_after_delay(signum: libc::c_int) {
        let pid = unsafe { libc::getpid() };
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            unsafe {
                libc::kill(pid, signum);
            }
        });
    }

    #[test]
    fn signal_recv_wakes_on_delivery() {
        let mut rt = Runtime::new().expect("runtime start");
        let result = rt.block_on(async {
            let mut sig = signal(SignalKind::user_defined1())?;
            raise_after_delay(SignalKind::user_defined1().as_raw());
            with_timeout(sig.recv()).await
        });
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn ctrl_c_wakes_on_sigint() {
        let mut rt = Runtime::new().expect("runtime start");
        let result = rt.block_on(async {
            let ctrlc = ctrl_c()?;
            raise_after_delay(SignalKind::interrupt().as_raw());
            with_timeout(ctrlc).await
        });
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn multiple_listeners_all_notified() {
        let mut rt = Runtime::new().expect("runtime start");
        let result = rt.block_on(async {
            let mut a = signal(SignalKind::user_defined2())?;
            let mut b = signal(SignalKind::user_defined2())?;
            raise_after_delay(SignalKind::user_defined2().as_raw());
            with_timeout(a.recv()).await?;
            with_timeout(b.recv()).await
        });
        assert!(result.is_ok(), "got: {result:?}");
    }
}
