use std::{
    cell::RefCell,
    future::Future,
    io,
    pin::pin,
    task::{Context, Waker},
};

use std::rc::Rc;

use crate::{
    builder::{RuntimeBuilder, RuntimeConfig},
    driver::{Driver, Handle},
    runtime::blocking::{BlockingPool, run_blocking},
    runtime::local::scheduler::Scheduler,
    task::{join::JoinHandle, new_task},
    time::Timer,
};

pub mod queue;
pub mod scheduler;

// One scheduler per OS thread — only accessible inside the runtime
scoped_thread_local!(static CURRENT_SCHEDULER: Scheduler);
scoped_thread_local!(pub(crate) static CURRENT_DRIVER: Handle);
scoped_thread_local!(pub(crate) static CURRENT_TIMER: Rc<RefCell<Timer>>);

/// Single-threaded (current-thread) karmaio runtime.
///
/// # Shutdown
///
/// Dropping a [`Runtime`] tears down the scheduler, I/O driver, and blocking
/// pool. Tasks still sitting in run queues are dropped; in-flight driver ops
/// are cancelled or drained by the platform backend.
///
/// To shut down cleanly:
///
/// 1. Finish (or drop) the future passed to [`Runtime::block_on`].
/// 2. Drop or fully await every [`JoinHandle`] you still care about **before**
///    dropping the runtime.
/// 3. Then drop the [`Runtime`].
///
/// Awaiting a [`JoinHandle`] *after* its runtime has been dropped will hang:
/// the task is no longer polled. Dropping a [`JoinHandle`] does **not** abort
/// the task (use [`JoinHandle::abort`]).
///
/// # I/O cancellation
///
/// Dropping an in-progress I/O future (for example via `select!` or a timeout)
/// **detaches** from the result; it does not always cancel the kernel
/// operation. On Linux (io_uring) the submission stays in flight until
/// completion and buffers remain alive until then. IOCP requests cancel; kqueue
/// removes readiness interest. Buffers and other op state are kept alive until
/// the kernel (or blocking pool) finishes, so this is memory-safe, but it is
/// not the same as eager cancellation.
pub struct Runtime {
    pub(crate) driver: Driver,
    pub(crate) scheduler: Scheduler,
    pub(crate) timer: Rc<RefCell<Timer>>,
    /// Owns the blocking pool. Dropped before `driver` (fields drop in reverse
    /// declaration order) so workers finishing during shutdown can still wake
    /// a live driver.
    _blocking: BlockingPool,
}

impl Runtime {
    /// Create a runtime with default settings.
    ///
    /// Equivalent to `RuntimeBuilder::new().build()`.
    pub fn new() -> io::Result<Self> {
        RuntimeBuilder::new().build()
    }

    /// Returns a [`RuntimeBuilder`] pre-populated with default settings.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    /// Build a runtime from an explicit [`RuntimeConfig`].
    pub(crate) fn from_config(config: RuntimeConfig) -> io::Result<Self> {
        let blocking = BlockingPool::new(config.blocking_threads, config.blocking_keep_alive);
        let driver = Driver::new(blocking.handle(), config.driver_capacity)?;
        let mut scheduler = Scheduler::default();
        scheduler.set_wakeup(driver.wakeup());

        Ok(Self {
            driver,
            scheduler,
            timer: Rc::new(RefCell::new(Timer::new())),
            // Declared last so it drops first (Rust drops fields in reverse order).
            _blocking: blocking,
        })
    }

    /// Runs `future` to completion on this runtime.
    ///
    /// Nested `block_on` calls (a runtime inside a runtime) panic.
    ///
    /// When this method returns, the main future has finished, but other
    /// tasks spawned during the call may still be queued or waiting on I/O.
    /// See [Shutdown](Runtime#shutdown) before dropping the runtime.
    pub fn block_on<F: Future + 'static>(&mut self, future: F) -> F::Output {
        assert!(!CURRENT_SCHEDULER.is_set(), "Can not start a runtime inside a runtime");

        let waker = Waker::noop();
        let mut cx = Context::from_waker(&waker);
        let handle: Handle = (&self.driver).into();

        CURRENT_TIMER.set(&self.timer, || {
            CURRENT_SCHEDULER.set(&self.scheduler, || {
                CURRENT_DRIVER.set(&handle, || {
                    let mut join_handle = pin!(future);

                    loop {
                        loop {
                            // Start of scheduler tick: drain remote first.
                            self.scheduler.tick();

                            // Expire any timers whose deadlines have passed.
                            self.timer.borrow_mut().wake();

                            // Consume tasks (max rounds ≈ 2 × current length after drain
                            // to prevent I/O starvation from a single yielding task).
                            let mut max_round = self.scheduler.tasks.len() * 2;
                            while let Some(t) = self.scheduler.tasks.pop_front() {
                                t.run();
                                if max_round == 0 {
                                    // maybe there's a looping task
                                    break;
                                } else {
                                    max_round -= 1;
                                }
                            }

                            // Check main future
                            if let std::task::Poll::Ready(t) = join_handle.as_mut().poll(&mut cx) {
                                return t;
                            }

                            if self.scheduler.tasks.is_empty() {
                                // No task to execute, we should wait for io blockingly
                                break;
                            }

                            // Cold path: tasks remain after a batch, so flush
                            // the submission queue without parking. Prevents io_uring SQEs from
                            // sitting in userspace until the SQ fills or we wait.
                            let _ = self.driver.submit();
                        }

                        // Wait for I/O (or a cross-thread wake from the remote
                        // scheduler / blocking pool), then apply completions.
                        let timeout = self.timer.borrow().min_timeout();
                        let _completed = match timeout {
                            Some(duration) => self
                                .driver
                                .wait_with_duration(duration)
                                .expect("Failed to wait for I/O events"),
                            None => self.driver.wait().expect("Failed to wait for I/O events"),
                        };
                        // Runtime owns the blocking pool: drain its completions
                        // as an explicit phase, then platform I/O completions.
                        self.driver.drain_blocking_completions();
                        self.driver.dispatch_completions();
                        self.timer.borrow_mut().wake();
                        // Note: we do *not* drain the remote task queue here. The
                        // next iteration of the inner loop drains at tick start.
                    }
                })
            })
        })
    }

    /// Spawns a future onto this runtime.
    ///
    /// The returned [`JoinHandle`] can be awaited for the output, or dropped
    /// to detach (the task keeps running). Dropping the handle does not cancel
    /// the task; call [`JoinHandle::abort`] for cooperative cancellation.
    ///
    /// The runtime must outlive any handle you still intend to poll. See
    /// [Shutdown](Runtime#shutdown).
    pub fn spawn<F: Future + 'static>(&self, future: F) -> JoinHandle<F::Output> {
        let (task, join_handle) = new_task(future, self.scheduler.handle());

        self.scheduler.tasks.push_back(task);

        join_handle
    }

    /// Runs the provided closure on a thread dedicated to blocking operations.
    ///
    /// Returns a [`JoinHandle`] for the result. The work is not cancelled if the
    /// handle is dropped; the pool continues the job to completion.
    ///
    /// # Panics
    ///
    /// If the blocking closure panics, the join handle resolves with a panic error.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use karmaio::runtime::Runtime;
    ///
    /// let mut rt = Runtime::new().unwrap();
    /// let handle = rt.spawn_blocking(|| {
    ///     // blocking work
    ///     42
    /// });
    /// let value = rt.block_on(async { handle.await.unwrap() });
    /// assert_eq!(value, 42);
    /// ```
    pub fn spawn_blocking<F, R>(&self, f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let future = run_blocking(self.driver.blocking_pool(), self.driver.wakeup(), f);
        self.spawn(future)
    }
}

/// Runs the provided closure on a thread dedicated to blocking operations.
///
/// Must be called from within a running runtime (inside [`Runtime::block_on`] or a
/// spawned task). Prefer [`Runtime::spawn_blocking`] when you have a runtime handle.
///
/// # Panics
///
/// Panics if called outside a runtime context.
pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    assert!(
        CURRENT_DRIVER.is_set() && CURRENT_SCHEDULER.is_set(),
        "spawn_blocking called outside of a runtime context"
    );

    CURRENT_DRIVER.with(|handle| {
        let driver = handle.upgrade().expect("spawn_blocking: driver has been dropped");
        let future = run_blocking(driver.blocking_pool(), driver.wakeup(), f);

        CURRENT_SCHEDULER.with(|scheduler| {
            let (task, join_handle) = new_task(future, scheduler.handle());
            scheduler.tasks.push_back(task);
            join_handle
        })
    })
}

/// Spawns a `!Send` future onto the current local runtime, returning a
/// [`JoinHandle`] for its output.
///
/// Unlike [`spawn_blocking`], the future runs on the same thread as the caller
/// (there is no `Send` bound), so it may hold `Rc`-backed or otherwise
/// non-`Send` state. This is used, for example, to drain a child's piped
/// stdout/stderr concurrently: the stream handles are not `Send`, so they are
/// read through the completion driver on the local runtime rather than on the
/// blocking pool.
///
/// # Panics
///
/// Panics if called outside a runtime context.
pub fn spawn_local<F, R>(future: F) -> JoinHandle<R>
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    assert!(
        CURRENT_DRIVER.is_set() && CURRENT_SCHEDULER.is_set(),
        "spawn_local called outside of a runtime context"
    );

    CURRENT_SCHEDULER.with(|scheduler| {
        let (task, join_handle) = new_task(future, scheduler.handle());
        scheduler.tasks.push_back(task);
        join_handle
    })
}

#[cfg(test)]
mod tests {
    use std::{
        future::{Future, pending},
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        task::{Context, Poll, Waker},
        thread,
    };

    use super::{Runtime, spawn_blocking};
    use crate::task::join::JoinHandle;

    #[test]
    fn join_handle_resolves_successful_task() {
        let mut runtime = Runtime::new().expect("runtime should start");
        let task = runtime.spawn(async { 7usize });

        let output = runtime.block_on(async { task.await });

        assert_eq!(output.expect("task should succeed"), 7);
    }

    #[test]
    fn join_handle_reports_panics() {
        let mut runtime = Runtime::new().expect("runtime should start");
        let task = runtime.spawn(async {
            panic!("boom");
            #[allow(unreachable_code)]
            7usize
        });

        let err = runtime
            .block_on(async { task.await })
            .expect_err("task should report panic");

        assert!(err.is_panic());
    }

    #[test]
    fn abort_cancels_task_before_it_runs() {
        let mut runtime = Runtime::new().expect("runtime should start");
        let task = runtime.spawn(pending::<usize>());

        task.abort();
        let err = runtime
            .block_on(async { task.await })
            .expect_err("task should be cancelled");

        assert!(err.is_cancelled());
    }

    #[test]
    fn dropping_runtime_with_queued_and_remote_tasks_does_not_panic() {
        // Regression: remote queue held Task → ScheduleHandle → Arc(queue),
        // which could cycle on shutdown. Scheduler::Drop drains and closes the
        // remote queue so Runtime drop stays leak- and panic-free.
        let mut runtime = Runtime::new().expect("runtime should start");

        // Fire-and-forget tasks (JoinHandle dropped) still in the local queue.
        for _ in 0..8 {
            let _ = runtime.spawn(pending::<()>());
        }

        // One task that parks a waker on another thread, so a remote schedule
        // may race with teardown.
        let ready = Arc::new(AtomicBool::new(false));
        let (waker_tx, waker_rx) = mpsc::channel();
        let _detached = runtime.spawn({
            let ready = Arc::clone(&ready);
            async move {
                struct Once {
                    ready: Arc<AtomicBool>,
                    tx: Option<mpsc::Sender<Waker>>,
                }
                impl Future for Once {
                    type Output = ();
                    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                        if self.ready.load(Ordering::Acquire) {
                            return Poll::Ready(());
                        }
                        if let Some(tx) = self.tx.take() {
                            let _ = tx.send(cx.waker().clone());
                        }
                        Poll::Pending
                    }
                }
                Once {
                    ready,
                    tx: Some(waker_tx),
                }
                .await
            }
        });

        // Drive just enough for the task to install its waker, then tear down.
        runtime.block_on(async {
            crate::time::sleep(std::time::Duration::from_millis(10)).await;
        });

        let _ = thread::spawn(move || {
            if let Ok(waker) = waker_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                ready.store(true, Ordering::Release);
                waker.wake();
            }
        });

        drop(runtime);
    }

    #[test]
    fn task_can_be_woken_from_another_thread() {
        struct RemoteWake {
            ready: Arc<AtomicBool>,
            sent_waker: bool,
            waker_tx: mpsc::Sender<Waker>,
        }

        impl Future for RemoteWake {
            type Output = usize;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                if self.ready.load(Ordering::Acquire) {
                    return Poll::Ready(11);
                }

                if !self.sent_waker {
                    self.waker_tx.send(cx.waker().clone()).expect("send waker");
                    self.sent_waker = true;
                }

                Poll::Pending
            }
        }

        let mut runtime = Runtime::new().expect("runtime should start");
        let ready = Arc::new(AtomicBool::new(false));
        let (waker_tx, waker_rx) = mpsc::channel();
        let task = runtime.spawn(RemoteWake {
            ready: Arc::clone(&ready),
            sent_waker: false,
            waker_tx,
        });

        let wake_thread = thread::spawn(move || {
            let waker = waker_rx.recv().expect("receive waker");
            ready.store(true, Ordering::Release);
            waker.wake();
        });

        let output = runtime.block_on(async { task.await });
        wake_thread.join().expect("wake thread should finish");

        assert_eq!(output.expect("task should succeed"), 11);
    }

    #[test]
    fn sleep_waits_for_duration() {
        use std::time::Duration;

        use crate::time::{Instant, sleep};

        let mut runtime = Runtime::new().expect("runtime should start");
        let start = Instant::now();

        runtime.block_on(async {
            sleep(Duration::from_millis(50)).await;
        });

        assert!(start.elapsed() >= Duration::from_millis(45));
    }

    #[test]
    fn timeout_returns_err_when_deadline_elapses() {
        use crate::time::{Duration, sleep, timeout};

        let mut runtime = Runtime::new().expect("runtime should start");

        let result = runtime.block_on(async {
            timeout(Duration::from_millis(25), async {
                sleep(Duration::from_millis(100)).await;
                7usize
            })
            .await
        });

        assert!(result.is_err());
    }

    #[test]
    fn timeout_returns_ok_when_future_completes_in_time() {
        use crate::time::{Duration, sleep, timeout};

        let mut runtime = Runtime::new().expect("runtime should start");

        let result = runtime.block_on(async {
            timeout(Duration::from_millis(100), async {
                sleep(Duration::from_millis(10)).await;
                7usize
            })
            .await
        });

        assert_eq!(result.expect("future should complete"), 7);
    }

    #[test]
    fn interval_first_tick_is_immediate() {
        use std::time::Instant as StdInstant;

        use crate::time::{Duration, interval};

        let mut runtime = Runtime::new().expect("runtime should start");
        let start = StdInstant::now();

        runtime.block_on(async {
            let mut interval = interval(Duration::from_millis(100));
            interval.tick().await;
        });

        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn interval_ticks_on_schedule() {
        use crate::time::{Duration, interval};

        let mut runtime = Runtime::new().expect("runtime should start");

        runtime.block_on(async {
            let mut interval = interval(Duration::from_millis(20));
            interval.tick().await;
            interval.tick().await;
        });
    }

    #[test]
    fn timeout_at_past_deadline_polls_future_once() {
        use std::time::Instant as StdInstant;

        use crate::time::{Duration, timeout_at};

        let mut runtime = Runtime::new().expect("runtime should start");
        let past = StdInstant::now() - Duration::from_secs(1);

        // A ready future should still return Ok even with a past deadline,
        // because timeout_at polls it once before giving up.
        let result = runtime.block_on(async move { timeout_at(past, async { 7usize }).await });
        assert_eq!(result, Ok(7));

        // A pending future with a past deadline should return Err(Elapsed).
        let result = runtime.block_on(async move { timeout_at(past, std::future::pending::<usize>()).await });
        assert!(result.is_err());
    }

    #[test]
    fn spawn_allows_non_send_future_with_send_output() {
        use std::rc::Rc;

        fn assert_send<T: Send>() {}

        let mut runtime = Runtime::new().expect("runtime should start");

        // Capture an Rc to make the future type `!Send`.
        // The output (usize) is `Send`, so the JoinHandle remains `Send`.
        let rc = Rc::new(41usize);
        let task = runtime.spawn(async move {
            // Use the Rc so it is captured by the future.
            *rc + 1
        });

        // Compile-time proof that the JoinHandle for a Send output is Send,
        // even though the spawned future itself was !Send.
        assert_send::<JoinHandle<usize>>();

        let output = runtime.block_on(async { task.await });
        assert_eq!(output.expect("task should succeed"), 42);
    }

    #[test]
    fn spawn_blocking_returns_value() {
        let mut runtime = Runtime::new().expect("runtime should start");
        let handle = runtime.spawn_blocking(|| 42usize);
        let output = runtime.block_on(async { handle.await });
        assert_eq!(output.expect("blocking task should succeed"), 42);
    }

    #[test]
    fn spawn_blocking_free_function_works_inside_runtime() {
        let mut runtime = Runtime::new().expect("runtime should start");
        let output = runtime.block_on(async { spawn_blocking(|| 7usize).await.expect("blocking task should succeed") });
        assert_eq!(output, 7);
    }

    #[test]
    fn spawn_blocking_reports_panics() {
        let mut runtime = Runtime::new().expect("runtime should start");
        let handle = runtime.spawn_blocking(|| panic!("blocking boom"));
        let err = runtime
            .block_on(async { handle.await })
            .expect_err("blocking panic should surface");
        assert!(err.is_panic());
    }

    #[test]
    fn spawn_blocking_does_not_starve_runtime() {
        use crate::time::{Duration, Instant, sleep};

        let mut runtime = Runtime::new().expect("runtime should start");
        let start = Instant::now();

        runtime.block_on(async move {
            let blocking = spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_millis(80));
                1usize
            });
            // Timer should fire while the blocking job holds a pool thread.
            sleep(Duration::from_millis(20)).await;
            assert!(start.elapsed() < Duration::from_millis(60));
            assert_eq!(blocking.await.expect("blocking ok"), 1);
        });
    }

    #[test]
    fn spawn_blocking_wakes_idle_runtime() {
        use crate::time::{Duration, Instant};

        let mut runtime = Runtime::new().expect("runtime should start");
        let start = Instant::now();

        // Runtime has no timers and no ready tasks after spawning the blocking
        // job; it must sleep in driver.wait() and be woken by the worker.
        runtime.block_on(async {
            let handle = spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_millis(30));
                99usize
            });
            assert_eq!(handle.await.expect("blocking ok"), 99);
        });

        assert!(start.elapsed() >= Duration::from_millis(25));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn spawn_blocking_many_concurrent_jobs() {
        let mut runtime = Runtime::new().expect("runtime should start");

        runtime.block_on(async {
            let mut handles = Vec::new();
            for i in 0..8usize {
                handles.push(spawn_blocking(move || {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    i
                }));
            }
            let mut sum = 0usize;
            for h in handles {
                sum += h.await.expect("job ok");
            }
            assert_eq!(sum, (0..8).sum());
        });
    }

    /// Path FS ops use `Submission::Blocking` on macOS/Windows (pool offload).
    #[test]
    fn blocking_submission_create_and_remove_dir() {
        use std::time::{SystemTime, UNIX_EPOCH};

        use crate::fs::{create_dir, remove_dir};

        let mut runtime = Runtime::new().expect("runtime should start");
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("time").as_nanos();
        let path = std::env::temp_dir().join(format!("karmaio-blocking-dir-{nanos}"));

        runtime.block_on(async move {
            create_dir(&path).await.expect("create_dir");
            assert!(path.is_dir());
            remove_dir(&path).await.expect("remove_dir");
            assert!(!path.exists());
        });
    }
}
