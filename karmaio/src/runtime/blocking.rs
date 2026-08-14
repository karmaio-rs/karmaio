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
//! - Completions wake the runtime through the driver's internal `Wakeup` token.
//!
//! # Shutdown semantics
//!
//! Every job carries a `mandatory` flag. Driver-dispatched syscalls (for
//! example the driver's close operation) are mandatory: their side effect —
//! usually releasing an OS resource such as an fd — must happen even during
//! shutdown, otherwise the resource leaks. User work spawned through
//! [`crate::runtime::spawn_blocking`] is optional and may be dropped if the
//! runtime shuts down before it runs.
//!
//! At shutdown the owner drains the queue itself, running mandatory jobs and
//! dropping optional ones, then waits up to a configurable timeout for the
//! workers to finish whatever they were already executing. A timed-out shutdown
//! detaches the remaining workers; they still finish their in-flight job and
//! exit on their own.
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
    num::NonZeroUsize,
    panic::{self, AssertUnwindSafe},
    pin::Pin,
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll},
    thread,
    time::{Duration, Instant},
};

use crate::{
    driver::Wakeup,
    runtime::Schedule,
    task::{JoinHandle, Task, new_task},
};

/// Boxed unit of work dispatched to the pool.
type JobWork = Box<dyn FnOnce() + Send + 'static>;

/// A unit of work, tagged with whether it must run even during shutdown.
///
/// Mandatory jobs are dispatched by the driver for operations whose side
/// effects (releasing fds/handles) must not be skipped. Optional jobs are user
/// work from [`crate::runtime::spawn_blocking`].
struct Job {
    work: JobWork,
    mandatory: bool,
}

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
    thread_cap: NonZeroUsize,
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
    worker_threads: Vec<thread::JoinHandle<()>>,
    next_thread_id: usize,
}

impl BlockingPool {
    /// Create a pool with the given thread cap and idle keep-alive.
    ///
    /// # Panics
    ///
    /// Panics if `thread_cap` is zero.
    pub fn new(thread_cap: usize, keep_alive: Duration) -> Self {
        let thread_cap = NonZeroUsize::new(thread_cap).expect("blocking pool thread cap must be nonzero");
        Self {
            inner: Arc::new(PoolInner {
                shared: Mutex::new(Shared {
                    queue: VecDeque::new(),
                    num_notify: 0,
                    num_threads: 0,
                    num_idle: 0,
                    shutdown: false,
                    shutdown_complete: false,
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

    /// Stop accepting new work, run any queued mandatory jobs, discard optional
    /// queued jobs, and join every worker. Jobs already running are allowed to
    /// finish. The operation is idempotent; concurrent callers wait for the
    /// first shutdown to finish joining.
    pub(crate) fn shutdown_and_join(&self) {
        self.inner.shutdown(None);
    }

    /// Shut the pool down, but wait at most `timeout` for workers to exit.
    ///
    /// Queued mandatory jobs are still run (so driver side effects are
    /// preserved); optional jobs are dropped. If the timeout elapses while a
    /// worker is still executing, that worker is detached rather than joined.
    pub(crate) fn shutdown_with_timeout(&self, timeout: Duration) {
        self.inner.shutdown(Some(timeout));
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
    pub(crate) fn try_dispatch(&self, job: JobWork) -> io::Result<()> {
        self.inner.dispatch(Job {
            work: job,
            mandatory: false,
        })
    }

    /// Fallible dispatch of a mandatory job that must run even during shutdown.
    ///
    /// Used by the drivers to offload syscalls whose side effects (such as
    /// closing an fd) must not be dropped with the runtime.
    // Mandatory jobs are dispatched by the kqueue and IOCP backends; io_uring
    // performs the equivalent work in-kernel.
    #[allow(dead_code)]
    pub(crate) fn try_dispatch_mandatory(&self, job: JobWork) -> io::Result<()> {
        self.inner.dispatch(Job {
            work: job,
            mandatory: true,
        })
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
        self.dispatch_with(job, |id| self.spawn_worker(id))
    }

    fn dispatch_with(
        self: &Arc<Self>,
        job: Job,
        spawn_worker: impl FnOnce(usize) -> io::Result<thread::JoinHandle<()>>,
    ) -> io::Result<()> {
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

        if shared.num_threads < self.thread_cap.get() {
            let id = shared.next_thread_id;
            shared.next_thread_id = shared.next_thread_id.wrapping_add(1);
            // Keep admission and worker creation under one mutex acquisition.
            // A new worker blocks on this mutex until its count and join handle
            // have been recorded, while concurrent dispatchers cannot enqueue
            // work that depends on an unconfirmed worker.
            match spawn_worker(id) {
                Ok(handle) => {
                    shared.num_threads += 1;
                    shared.worker_threads.push(handle);
                    Ok(())
                }
                Err(err) => {
                    if shared.num_threads > 0 {
                        // Existing workers will eventually drain the queue.
                        Ok(())
                    } else {
                        // No other dispatcher could mutate the queue while the
                        // spawn was attempted, so the last job is ours.
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
                if shared.shutdown && !job.mandatory {
                    // The pool is shutting down and this job is optional: drop
                    // it without running. Mandatory jobs (driver syscalls) are
                    // still executed so their side effects are preserved.
                    drop(job);
                    continue;
                }
                drop(shared);
                // Panics in jobs must not kill the worker thread.
                // Job-level panic propagation is handled by the task system.
                let _ = panic::catch_unwind(AssertUnwindSafe(job.work));
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
        // Shutdown joins workers by waiting for `num_threads` to reach zero.
        self.condvar.notify_all();
    }

    fn shutdown(&self, timeout: Option<Duration>) {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());

        if shared.shutdown {
            // A concurrent caller is already shutting down; wait for it to
            // finish (honoring this caller's own cap).
            let deadline = timeout.map(|duration| Instant::now() + duration);
            while !shared.shutdown_complete {
                match deadline {
                    Some(deadline) => {
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        let (guard, _) = self
                            .condvar
                            .wait_timeout(shared, deadline - now)
                            .unwrap_or_else(|e| e.into_inner());
                        shared = guard;
                    }
                    None => shared = self.condvar.wait(shared).unwrap_or_else(|e| e.into_inner()),
                }
            }
            return;
        }

        shared.shutdown = true;
        self.condvar.notify_all();

        // Drain the queue ourselves so mandatory jobs run even if every worker
        // is stuck in an optional (user) job. Optional jobs are dropped.
        loop {
            match shared.queue.pop_front() {
                Some(job) if job.mandatory => {
                    drop(shared);
                    let _ = panic::catch_unwind(AssertUnwindSafe(job.work));
                    shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                }
                Some(job) => {
                    // Non-mandatory queued job: cancel it.
                    drop(job);
                }
                None => break,
            }
        }

        let workers = std::mem::take(&mut shared.worker_threads);

        // Wait for every worker to exit, bounded by `timeout`. Workers notify
        // the condvar when they decrement `num_threads` at thread exit.
        let deadline = timeout.map(|duration| Instant::now() + duration);
        while shared.num_threads > 0 {
            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    let (guard, _) = self
                        .condvar
                        .wait_timeout(shared, deadline - now)
                        .unwrap_or_else(|e| e.into_inner());
                    shared = guard;
                }
                None => shared = self.condvar.wait(shared).unwrap_or_else(|e| e.into_inner()),
            }
        }
        let all_exited = shared.num_threads == 0;

        shared.shutdown_complete = true;
        self.condvar.notify_all();
        drop(shared);

        if all_exited {
            for handle in workers {
                let _ = handle.join();
            }
        } else {
            // The timeout elapsed while a worker was still running. Detach the
            // remaining workers; they finish their in-flight job and exit on
            // their own. Their completions may arrive after the driver has shut
            // down and are simply dropped (the wakeup token is a no-op once
            // closed).
            drop(workers);
        }
    }
}

// ===== task-based blocking spawn =====

/// Future that runs a blocking closure when polled.
///
/// A pool worker polls it exactly once via [`Task::run`]; the closure runs and
/// its output becomes the task's output. Panics in the closure are caught by the
/// task system and surface through the [`JoinHandle`] as `JoinError::panic`.
pub(crate) struct BlockingTask<F> {
    func: Option<F>,
}

// SAFETY: `BlockingTask` does not rely on address stability: it is polled once
// and only to extract the wrapped closure.
impl<F> Unpin for BlockingTask<F> {}

impl<F> BlockingTask<F> {
    pub(crate) fn new(func: F) -> Self {
        Self { func: Some(func) }
    }
}

impl<F, R> Future for BlockingTask<F>
where
    F: FnOnce() -> R,
{
    type Output = R;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<R> {
        let func = self.func.take().expect("blocking task polled more than once");
        Poll::Ready(func())
    }
}

/// `Schedule` implementation for blocking tasks.
///
/// Blocking tasks are polled once by a pool worker and never yield, so they are
/// never rescheduled.
pub(crate) struct BlockingSchedule;

impl Schedule for BlockingSchedule {
    fn schedule(&self, _task: Task<Self>) {
        unreachable!("blocking tasks are never rescheduled");
    }
}

/// Spawn `f` onto the pool as a task, returning a [`JoinHandle`] for its output.
///
/// The caller's [`Wakeup`] is poked after the task completes so a runtime parked
/// in `driver.wait()` re-polls the `JoinHandle`.
pub(crate) fn spawn_blocking_task<F, R>(pool: &BlockingPoolHandle, wakeup: Wakeup, f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let (task, join_handle) = new_task(BlockingTask::new(f), BlockingSchedule);

    pool.dispatch(move || {
        task.run();
        wakeup.wake();
    });

    join_handle
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    #[test]
    #[should_panic(expected = "blocking pool thread cap must be nonzero")]
    fn zero_thread_cap_is_rejected() {
        let _pool = BlockingPool::new(0, Duration::from_secs(1));
    }

    #[test]
    fn failed_first_worker_does_not_leave_queued_work() {
        let pool = BlockingPool::new(1, Duration::from_secs(1));
        let result = pool.inner.dispatch_with(
            Job {
                work: Box::new(|| panic!("failed worker must not run this job")),
                mandatory: false,
            },
            |_| Err(io::Error::other("injected worker spawn failure")),
        );

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        {
            let shared = pool.inner.shared.lock().unwrap_or_else(|error| error.into_inner());
            assert!(shared.queue.is_empty());
            assert_eq!(shared.num_threads, 0);
        }

        let (done_tx, done_rx) = mpsc::channel();
        pool.dispatch(move || done_tx.send(()).expect("report recovered dispatch"));
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("a later dispatch should start a worker");
    }

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
    fn shutdown_runs_queued_mandatory_jobs() {
        let pool = BlockingPool::new(1, Duration::from_secs(5));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let handle = pool.handle();

        // Occupy the only worker with an optional job that blocks forever.
        pool.dispatch(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().ok();
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker should start the first job");

        // Queue a mandatory job behind the stuck optional job.
        let (ran_tx, ran_rx) = mpsc::channel();
        handle
            .try_dispatch_mandatory(Box::new(move || {
                ran_tx.send(()).unwrap();
            }))
            .expect("mandatory dispatch should succeed");

        // Shutdown must run the queued mandatory job even though the worker is
        // stuck; the owner drains the queue itself.
        let shutdown_thread = thread::spawn(move || pool.shutdown_and_join());
        ran_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("queued mandatory job should run at shutdown");

        drop(release_tx);
        shutdown_thread.join().expect("shutdown should finish");
    }

    #[test]
    fn mandatory_dispatch_rejected_after_shutdown() {
        let pool = BlockingPool::new(1, Duration::from_secs(1));
        let handle = pool.handle();
        drop(pool);

        let result = handle.try_dispatch_mandatory(Box::new(|| {}));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn shutdown_with_zero_timeout_detaches_stuck_worker() {
        let pool = BlockingPool::new(1, Duration::from_secs(5));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        pool.dispatch(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().ok();
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker should start the job");

        // A zero timeout must return immediately even though the worker is
        // stuck; the worker is detached and finishes when released.
        let start = Instant::now();
        pool.shutdown_with_timeout(Duration::from_millis(0));
        assert!(start.elapsed() < Duration::from_secs(1));

        drop(release_tx);
    }

    #[test]
    fn spawn_blocking_task_runs_and_wakes() {
        let pool = BlockingPool::new(2, Duration::from_secs(5));
        let wakes = Arc::new(AtomicUsize::new(0));
        let wakeup = crate::driver::Wakeup::new({
            let wakes = Arc::clone(&wakes);
            move || {
                wakes.fetch_add(1, Ordering::Relaxed);
            }
        });

        let handle = spawn_blocking_task(&pool.handle(), wakeup, || 42usize);
        assert!(!handle.is_finished());

        let start = Instant::now();
        loop {
            if handle.is_finished() {
                break;
            }
            if start.elapsed() > Duration::from_secs(2) {
                panic!("blocking task did not finish");
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(wakes.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn spawn_blocking_task_reports_panics() {
        let pool = BlockingPool::new(2, Duration::from_secs(5));
        let wakeup = crate::driver::Wakeup::new(|| {});

        let handle = spawn_blocking_task(&pool.handle(), wakeup, || -> usize {
            panic!("boom");
        });
        let start = Instant::now();
        loop {
            if handle.is_finished() {
                break;
            }
            if start.elapsed() > Duration::from_secs(2) {
                panic!("blocking task did not finish");
            }
            thread::sleep(Duration::from_millis(1));
        }
        // The panic must not propagate to the test process: `is_finished` only
        // observes completion, the panic is stored for the JoinHandle.
    }
}
