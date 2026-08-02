//! Blocking thread pool for offloading synchronous syscalls and user work.
//!
//! Async runtimes must not run long-blocking work on the executor thread.
//! Operations such as process waits, path-based filesystem calls on kqueue Unix /
//! Windows, and DNS lookups are offloaded here via [`crate::runtime::spawn_blocking`].
//!
//! # Design
//!
//! - Dynamic worker growth up to a configurable cap.
//! - Workers exit after an idle keep-alive period.
//! - Zero extra dependencies: `Mutex` + `Condvar` + `VecDeque`.
//! - Completions wake the runtime through the driver's [`Wakeup`] token.
//!
//! # When not to use this pool
//!
//! Long-lived or infinite loops should use a dedicated `std::thread` instead.
//! Occupying a pool thread indefinitely reduces capacity for other blocking work.

use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    io,
    panic::{self, AssertUnwindSafe},
    pin::Pin,
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll, Waker},
    thread,
    time::Duration,
};

use crate::driver::Wakeup;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// Owned blocking pool. Dropping it signals shutdown and joins workers.
///
/// This is the **owner**: only one exists per pool lifetime. It holds the same
/// `Arc` as every [`BlockingPoolHandle`], but its `Drop` runs shutdown. Handles
/// are cheap clones for dispatch without taking ownership.
pub struct BlockingPool {
    inner: Arc<PoolInner>,
}

/// Cloneable, non-owning handle used to dispatch work (driver, `spawn_blocking`).
///
/// Cloning a handle does **not** keep the pool alive past the owning
/// [`BlockingPool`]'s drop in the sense of delaying shutdown semantics: drop of
/// the owner still shuts the pool down. Handles only share access to the inner
/// state so workers and the runtime can enqueue jobs without moving the owner.
#[derive(Clone)]
pub struct BlockingPoolHandle {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    shared: Mutex<Shared>,
    condvar: Condvar,
    thread_cap: usize,
    keep_alive: Duration,
}

struct Shared {
    queue: VecDeque<Job>,
    /// Legitimate wakeups still outstanding (guards against spurious condvar wakes).
    num_notify: u32,
    num_threads: usize,
    num_idle: usize,
    shutdown: bool,
    shutdown_complete: bool,
    /// Workers reserved under the mutex but whose `JoinHandle` has not yet
    /// been recorded. Shutdown waits for this to reach zero before taking the
    /// worker list.
    num_starting_workers: usize,
    worker_threads: Vec<thread::JoinHandle<()>>,
    next_thread_id: usize,
}

impl BlockingPool {
    /// Create a pool with the given thread cap and idle keep-alive.
    pub fn new(thread_cap: usize, keep_alive: Duration) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                shared: Mutex::new(Shared {
                    queue: VecDeque::new(),
                    num_notify: 0,
                    num_threads: 0,
                    num_idle: 0,
                    shutdown: false,
                    shutdown_complete: false,
                    num_starting_workers: 0,
                    worker_threads: Vec::new(),
                    next_thread_id: 0,
                }),
                condvar: Condvar::new(),
                thread_cap,
                keep_alive,
            }),
        }
    }

    /// Cloneable handle for dispatching work without owning the pool.
    pub fn handle(&self) -> BlockingPoolHandle {
        BlockingPoolHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Dispatch a job onto the pool.
    ///
    /// Panics if the OS cannot spawn a worker and no workers are available.
    pub fn dispatch<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.handle().dispatch(f);
    }

    /// Stop accepting new work, discard queued jobs, and join every worker.
    ///
    /// Jobs already running are allowed to finish. The operation is idempotent;
    /// concurrent callers wait for the first shutdown to finish joining.
    pub(crate) fn shutdown_and_join(&self) {
        self.inner.shutdown_and_join();
    }

    /// Number of live worker threads (for tests / diagnostics).
    #[cfg(test)]
    pub(crate) fn num_threads(&self) -> usize {
        self.inner.shared.lock().unwrap_or_else(|e| e.into_inner()).num_threads
    }
}

impl Drop for BlockingPool {
    fn drop(&mut self) {
        self.shutdown_and_join();
    }
}

impl fmt::Debug for BlockingPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shared = self.inner.shared.lock().unwrap_or_else(|e| e.into_inner());
        f.debug_struct("BlockingPool")
            .field("thread_cap", &self.inner.thread_cap)
            .field("num_threads", &shared.num_threads)
            .finish()
    }
}

impl BlockingPoolHandle {
    /// Dispatch a job onto the pool.
    ///
    /// Panics if the OS cannot spawn a worker and no workers are available.
    pub fn dispatch<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        if let Err(err) = self.try_dispatch(Box::new(f)) {
            panic!("OS can't spawn blocking worker thread: {err}");
        }
    }

    /// Fallible dispatch used by tests and internal callers.
    pub(crate) fn try_dispatch(&self, job: Job) -> io::Result<()> {
        self.inner.dispatch(job)
    }
}

impl fmt::Debug for BlockingPoolHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shared = self.inner.shared.lock().unwrap_or_else(|e| e.into_inner());
        f.debug_struct("BlockingPoolHandle")
            .field("thread_cap", &self.inner.thread_cap)
            .field("num_threads", &shared.num_threads)
            .finish()
    }
}

impl PoolInner {
    fn dispatch(self: &Arc<Self>, job: Job) -> io::Result<()> {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());

        if shared.shutdown {
            // Tell the caller immediately. Silently dropping a driver-owned
            // completion job would leave its operation registered forever.
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "blocking pool is shut down"));
        }

        shared.queue.push_back(job);

        if shared.num_idle > 0 {
            shared.num_idle -= 1;
            shared.num_notify = shared.num_notify.saturating_add(1);
            self.condvar.notify_one();
            return Ok(());
        }

        if shared.num_threads < self.thread_cap {
            let id = shared.next_thread_id;
            shared.next_thread_id = shared.next_thread_id.wrapping_add(1);
            // Reserve the slot under the lock so concurrent dispatches do not
            // overshoot `thread_cap`.
            shared.num_threads += 1;
            shared.num_starting_workers += 1;
            let other_live = shared.num_threads - 1;
            drop(shared);

            match self.spawn_worker(id) {
                Ok(handle) => {
                    let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                    shared.num_starting_workers = shared.num_starting_workers.saturating_sub(1);
                    shared.worker_threads.push(handle);
                    self.condvar.notify_all();
                    Ok(())
                }
                Err(err) => {
                    let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                    shared.num_starting_workers = shared.num_starting_workers.saturating_sub(1);
                    shared.num_threads = shared.num_threads.saturating_sub(1);
                    self.condvar.notify_all();
                    if shared.shutdown {
                        // Shutdown cleared the queue while worker startup was
                        // in progress, so the dropped job is already canceled.
                        return Ok(());
                    }
                    if other_live > 0 {
                        // Existing workers will eventually drain the queue.
                        Ok(())
                    } else {
                        let _ = shared.queue.pop_back();
                        Err(err)
                    }
                }
            }
        } else {
            // At capacity: job stays queued until a worker frees up.
            Ok(())
        }
    }

    fn spawn_worker(self: &Arc<Self>, id: usize) -> io::Result<thread::JoinHandle<()>> {
        let inner = Arc::clone(self);
        thread::Builder::new()
            .name(format!("karmaio-blocking-{id}"))
            .spawn(move || inner.worker_loop())
    }

    fn worker_loop(self: Arc<Self>) {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());

        loop {
            // BUSY: drain the queue.
            while let Some(job) = shared.queue.pop_front() {
                drop(shared);
                // Panics in jobs must not kill the worker thread.
                // Job-level panic propagation is handled by `run_blocking`.
                let _ = panic::catch_unwind(AssertUnwindSafe(job));
                shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            }

            if shared.shutdown {
                break;
            }

            // IDLE: wait for work or keep-alive timeout.
            shared.num_idle += 1;
            let (guard, timeout_result) = self
                .condvar
                .wait_timeout(shared, self.keep_alive)
                .unwrap_or_else(|e| e.into_inner());
            shared = guard;

            if shared.num_notify > 0 {
                // Legitimate wakeup: `dispatch` already decremented `num_idle`.
                shared.num_notify -= 1;
                continue;
            }

            // Spurious wake or timeout. Undo the idle count we just added.
            shared.num_idle = shared.num_idle.saturating_sub(1);

            if shared.shutdown {
                break;
            }

            if timeout_result.timed_out() && shared.queue.is_empty() {
                // Idle exit.
                break;
            }
            // Spurious wakeup with empty queue: loop back to idle.
        }

        shared.num_threads = shared.num_threads.saturating_sub(1);
    }

    fn shutdown_and_join(&self) {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        if shared.shutdown {
            while !shared.shutdown_complete {
                shared = self.condvar.wait(shared).unwrap_or_else(|e| e.into_inner());
            }
            return;
        }
        shared.shutdown = true;
        // Drop queued jobs so oneshot receivers observe cancellation.
        shared.queue.clear();
        self.condvar.notify_all();

        // A dispatcher may have reserved a worker slot and released the mutex
        // to call `thread::spawn`. Wait until it records the JoinHandle before
        // taking the worker list.
        while shared.num_starting_workers > 0 {
            shared = self.condvar.wait(shared).unwrap_or_else(|e| e.into_inner());
        }

        let workers = std::mem::take(&mut shared.worker_threads);
        drop(shared);

        for handle in workers {
            let _ = handle.join();
        }

        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        shared.shutdown_complete = true;
        self.condvar.notify_all();
    }
}

// ===== oneshot channel for spawn_blocking =====

struct OneshotState<T> {
    value: Option<T>,
    waker: Option<Waker>,
    /// `true` once the sender has been dropped without sending.
    closed: bool,
}

struct OneshotInner<T> {
    state: Mutex<OneshotState<T>>,
}

struct OneshotSender<T> {
    inner: Arc<OneshotInner<T>>,
}

struct OneshotReceiver<T> {
    inner: Arc<OneshotInner<T>>,
}

fn oneshot_channel<T>() -> (OneshotSender<T>, OneshotReceiver<T>) {
    let inner = Arc::new(OneshotInner {
        state: Mutex::new(OneshotState {
            value: None,
            waker: None,
            closed: false,
        }),
    });
    (
        OneshotSender {
            inner: Arc::clone(&inner),
        },
        OneshotReceiver { inner },
    )
}

impl<T> OneshotSender<T> {
    fn send(self, value: T) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.value = Some(value);
        if let Some(waker) = state.waker.take() {
            drop(state);
            waker.wake();
        }
    }
}

impl<T> Drop for OneshotSender<T> {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.value.is_none() {
            state.closed = true;
            if let Some(waker) = state.waker.take() {
                drop(state);
                waker.wake();
            }
        }
    }
}

impl<T> Future for OneshotReceiver<T> {
    type Output = Result<T, ()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(value) = state.value.take() {
            return Poll::Ready(Ok(value));
        }
        if state.closed {
            return Poll::Ready(Err(()));
        }
        match &state.waker {
            Some(w) if w.will_wake(cx.waker()) => {}
            _ => state.waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

/// Result of a blocking job, including panic payloads.
type JobResult<T> = Result<T, Box<dyn std::any::Any + Send + 'static>>;

/// Schedule `f` on the pool and return a future that resolves to its output.
///
/// The future panics (caught by the task system as `JoinError::panic`) if the
/// blocking closure panics. If the pool shuts down before the job finishes,
/// the future panics with a descriptive message.
pub(crate) fn run_blocking<F, R>(pool: &BlockingPoolHandle, wakeup: Wakeup, f: F) -> impl Future<Output = R> + 'static
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let (tx, rx) = oneshot_channel::<JobResult<R>>();

    pool.dispatch(move || {
        let result = panic::catch_unwind(AssertUnwindSafe(f));
        tx.send(result);
        wakeup.wake();
    });

    async move {
        match rx.await {
            Ok(Ok(value)) => value,
            Ok(Err(payload)) => panic::resume_unwind(payload),
            Err(()) => panic!("blocking pool shut down before task completed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    #[test]
    fn dispatch_runs_job() {
        let pool = BlockingPool::new(4, Duration::from_secs(5));
        let (tx, rx) = mpsc::channel();
        pool.dispatch(move || {
            tx.send(42).unwrap();
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), 42);
    }

    #[test]
    fn dispatch_after_shutdown_returns_an_error() {
        let pool = BlockingPool::new(1, Duration::from_secs(1));
        let handle = pool.handle();
        drop(pool);

        let result = handle.try_dispatch(Box::new(|| {}));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn shutdown_discards_queued_jobs_and_joins_running_work() {
        struct DropFlag(Arc<AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let pool = BlockingPool::new(1, Duration::from_secs(5));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        pool.dispatch(move || {
            started_tx.send(()).expect("report running job");
            release_rx.recv().expect("release running job");
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker should start the first job");

        let queued_job_dropped = Arc::new(AtomicBool::new(false));
        let drop_flag = DropFlag(Arc::clone(&queued_job_dropped));
        pool.dispatch(move || drop(drop_flag));

        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            release_tx.send(()).expect("release worker");
        });

        pool.shutdown_and_join();
        releaser.join().expect("release thread should finish");

        assert!(queued_job_dropped.load(Ordering::Acquire));

        // A second shutdown observes the completed first shutdown and does
        // not attempt to join workers again.
        pool.shutdown_and_join();
    }

    #[test]
    fn many_jobs_complete() {
        let pool = BlockingPool::new(4, Duration::from_secs(5));
        let counter = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();
        let n = 32usize;

        for _ in 0..n {
            let counter = Arc::clone(&counter);
            let tx = tx.clone();
            pool.dispatch(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = tx.send(());
            });
        }
        drop(tx);

        for _ in 0..n {
            rx.recv_timeout(Duration::from_secs(5)).expect("job timed out");
        }
        assert_eq!(counter.load(Ordering::SeqCst), n);
    }

    #[test]
    fn jobs_queue_when_at_cap() {
        let pool = BlockingPool::new(2, Duration::from_secs(5));
        let (start_tx, start_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();

        // Fill both workers with long jobs.
        for _ in 0..2 {
            let start_tx = start_tx.clone();
            let done_tx = done_tx.clone();
            pool.dispatch(move || {
                start_tx.send(()).unwrap();
                thread::sleep(Duration::from_millis(100));
                done_tx.send(()).unwrap();
            });
        }

        // Wait until both workers are busy, then queue more work.
        start_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        start_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(pool.num_threads() <= 2);

        for _ in 0..4 {
            let done_tx = done_tx.clone();
            pool.dispatch(move || {
                done_tx.send(()).unwrap();
            });
        }
        drop(done_tx);

        for _ in 0..6 {
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("queued job should complete");
        }
    }

    #[test]
    fn panic_in_job_does_not_kill_pool() {
        let pool = BlockingPool::new(2, Duration::from_secs(5));
        pool.dispatch(|| panic!("boom"));

        let (tx, rx) = mpsc::channel();
        pool.dispatch(move || {
            tx.send(7).unwrap();
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), 7);
    }

    #[test]
    fn idle_workers_eventually_exit() {
        let pool = BlockingPool::new(4, Duration::from_millis(80));
        let (tx, rx) = mpsc::channel();
        pool.dispatch(move || {
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        while pool.num_threads() > 0 {
            if Instant::now() > deadline {
                panic!("workers did not idle-exit; still {}", pool.num_threads());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn oneshot_delivers_value() {
        let (tx, rx) = oneshot_channel::<u32>();
        thread::spawn(move || {
            tx.send(99);
        });

        let mut rx = rx;
        let waker = Waker::noop();
        let mut cx = Context::from_waker(&waker);
        let start = Instant::now();
        loop {
            match Pin::new(&mut rx).poll(&mut cx) {
                Poll::Ready(Ok(v)) => {
                    assert_eq!(v, 99);
                    break;
                }
                Poll::Ready(Err(())) => panic!("oneshot closed"),
                Poll::Pending => {
                    if start.elapsed() > Duration::from_secs(2) {
                        panic!("oneshot timed out");
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
}
