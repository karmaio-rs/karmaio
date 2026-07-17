//! Windows Ctrl-C handling.
//!
//! A console control handler is installed once via `SetConsoleCtrlHandler`.
//! When Ctrl-C is received it bumps a counter and wakes every registered waker.
//! Listeners compare the counter against the last value they observed,
//! so rapid deliveries coalesce into a single notification.
//! Only Ctrl-C is supported; arbitrary signals do not exist on Windows.

use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

use windows_sys::Win32::System::Console::{CTRL_C_EVENT, SetConsoleCtrlHandler};

/// Shared Ctrl-C state, updated from the console control handler.
struct CtrlCState {
    counter: AtomicUsize,
    wakers: Mutex<Vec<Waker>>,
}

/// Global state; the control handler is installed on first access.
static CTRL_C_STATE: LazyLock<io::Result<Arc<CtrlCState>>> = LazyLock::new(init_state);

/// A future that completes when the process receives Ctrl-C.
///
/// Returned by [`ctrl_c`].
pub struct CtrlC {
    state: Arc<CtrlCState>,
    last_seen: usize,
}

impl CtrlC {
    /// Create a new Ctrl-C listener.
    pub fn new() -> io::Result<Self> {
        let state = state()?;
        let last_seen = state.counter.load(Ordering::Acquire);
        Ok(Self { state, last_seen })
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let current = self.state.counter.load(Ordering::Acquire);
        if current != self.last_seen {
            self.last_seen = current;
            return Poll::Ready(Ok(()));
        }

        register_waker(&self.state, cx.waker());

        let current = self.state.counter.load(Ordering::Acquire);
        if current != self.last_seen {
            self.last_seen = current;
            return Poll::Ready(Ok(()));
        }

        Poll::Pending
    }
}

impl Future for CtrlC {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `CtrlC` is not self-referential; we never move out of `self`.
        let this = unsafe { self.get_unchecked_mut() };
        this.poll_recv(cx)
    }
}

/// Return a future that completes on the next Ctrl-C.
///
/// # Examples
///
/// ```no_run
/// # async fn run() -> std::io::Result<()> {
/// karmaio::signal::ctrl_c()?.await?;
/// # Ok(())
/// # }
/// ```
#[inline]
pub fn ctrl_c() -> io::Result<CtrlC> {
    CtrlC::new()
}

fn state() -> io::Result<Arc<CtrlCState>> {
    match &*CTRL_C_STATE {
        Ok(state) => Ok(Arc::clone(state)),
        Err(err) => Err(io::Error::new(err.kind(), err.to_string())),
    }
}

fn init_state() -> io::Result<Arc<CtrlCState>> {
    let state = Arc::new(CtrlCState {
        counter: AtomicUsize::new(0),
        wakers: Mutex::new(Vec::new()),
    });

    let ok = unsafe { SetConsoleCtrlHandler(Some(ctrl_c_handler), 1) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(state)
}

fn register_waker(state: &CtrlCState, waker: &Waker) {
    let mut wakers = state.wakers.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = wakers.iter_mut().find(|existing| existing.will_wake(waker)) {
        existing.clone_from(waker);
    } else {
        wakers.push(waker.clone());
    }
}

unsafe extern "system" fn ctrl_c_handler(ctrl_type: u32) -> i32 {
    if ctrl_type != CTRL_C_EVENT {
        return 0;
    }

    if let Ok(state) = &*CTRL_C_STATE {
        state.counter.fetch_add(1, Ordering::Release);
        let woken = {
            let mut wakers = state.wakers.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *wakers)
        };
        for waker in woken {
            waker.wake();
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::runtime::Runtime;
    use crate::time::timeout;

    #[test]
    fn ctrl_c_wakes_on_handler() {
        let mut rt = Runtime::new().expect("runtime start");
        let result = rt.block_on(async {
            let ctrlc = ctrl_c()?;
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                unsafe {
                    let _ = ctrl_c_handler(CTRL_C_EVENT);
                }
            });
            timeout(Duration::from_secs(2), ctrlc)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "ctrl-c timeout"))?
        });
        assert!(result.is_ok(), "got: {result:?}");
    }
}
