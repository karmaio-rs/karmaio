use std::{
    cell::RefCell,
    future::Future,
    io,
    num::NonZeroU32,
    task::{Context, Waker},
    time::Duration,
};

use std::rc::Rc;

use crate::{
    builder::{RuntimeBuilder, RuntimeConfig},
    driver::{Driver, Handle},
    runtime::blocking::{BlockingPool, spawn_blocking_task},
    runtime::local::scheduler::Scheduler,
    task::join::JoinHandle,
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
/// pool. Local tasks are cancelled and their futures are dropped on the runtime
/// thread; in-flight driver ops are cancelled or drained by the platform
/// backend.
///
/// To shut down cleanly:
///
/// 1. Finish (or drop) the future passed to [`Runtime::block_on`].
/// 2. Fully await work that must finish successfully before shutdown.
/// 3. Then drop the [`Runtime`].
///
/// A retained [`JoinHandle`] for an unfinished local task observes a cancellation
/// error after the runtime is dropped. Dropping a [`JoinHandle`] while the
/// runtime is still running does **not** abort its task (use
/// [`JoinHandle::abort`]).
///
/// # I/O cancellation
///
/// There are two verbs:
///
/// - **Detach** — dropping an in-progress I/O future (for example via `select!`
///   or a timeout) requests platform cancellation and detaches from the
///   result. The payload stays alive until the target completion. The buffer
///   is forfeited; a completion that races cancellation may still have done
///   work on a stream.
/// - **Cancel** — [`crate::runtime::CancellationSource::cancel`] requests platform
///   cancellation of operations registered with
///   [`crate::runtime::FutureExt::with_cancellation`]. Await the same future
///   to observe a terminal outcome and recover the buffer. A completion that
///   races cancellation may still return `Ok`. See
///   [`crate::runtime::is_operation_canceled`].
///
/// Platform cancellation is best-effort. Do not start a same-direction stream
/// operation after dropping its predecessor when ordering matters; cancel and
/// await the predecessor's terminal completion first.
pub struct Runtime {
    pub(crate) scheduler: Scheduler,
    pub(crate) timer: Rc<RefCell<Timer>>,
    /// Joined before the driver performs its final platform cleanup.
    _blocking: BlockingPool,
    pub(crate) driver: Driver,
    event_interval: NonZeroU32,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Drop task futures and timer-held wakers while the driver remains
        // alive. Detached operations can then retain their payloads in the
        // backend until the kernel or blocking pool reaches a terminal state.
        self.scheduler.shutdown();
        self.timer.borrow_mut().clear();

        // Once the pool is joined, no worker can enqueue another completion.
        // Drain the final batch before backend Drop performs platform cleanup.
        self._blocking.shutdown_and_join();
        self.driver.drain_blocking_completions();

        // The backend is now the final owner of platform I/O state. Shut it
        // down explicitly rather than relying on struct field-drop order.
        self.driver.shutdown();
    }
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
        config.validate()?;
        let blocking = BlockingPool::new(config.blocking_threads, config.blocking_keep_alive);
        let driver = Driver::new(
            blocking.handle(),
            crate::driver::DriverConfig {
                capacity: config.driver_capacity,
                buffer_pool_size: config.buffer_pool_size,
                buffer_pool_buffer_len: config.buffer_pool_buffer_len,
                multishot_accept_capacity: config.multishot_accept_capacity,
            },
        )?;
        let mut scheduler = Scheduler::default();
        scheduler.set_wakeup(driver.wakeup());

        Ok(Self {
            scheduler,
            timer: Rc::new(RefCell::new(Timer::new())),
            _blocking: blocking,
            driver,
            event_interval: config.event_interval,
        })
    }

    /// Runs `future` to completion on this runtime.
    ///
    /// Nested `block_on` calls (a runtime inside a runtime) panic.
    ///
    /// When this method returns, the main future has finished, but other
    /// tasks spawned during the call may still be queued or waiting on I/O.
    /// See [Shutdown](Runtime#shutdown) before dropping the runtime.
    pub fn block_on<F: Future>(&mut self, future: F) -> F::Output {
        assert!(!CURRENT_SCHEDULER.is_set(), "Can not start a runtime inside a runtime");

        let handle: Handle = (&self.driver).into();
        let root_waker = Waker::from(std::sync::Arc::new(self.driver.wakeup()));
        let mut root_context = Context::from_waker(&root_waker);
        let mut root = std::pin::pin!(future);

        CURRENT_TIMER.set(&self.timer, || {
            CURRENT_SCHEDULER.set(&self.scheduler, || {
                CURRENT_DRIVER.set(&handle, || {
                    loop {
                        loop {
                            // Start of scheduler tick: drain remote first.
                            self.scheduler.tick();

                            // Expire any timers whose deadlines have passed.
                            self.timer.borrow_mut().wake();

                            // Bound each scheduler batch so a self-waking task
                            // cannot prevent the driver from servicing I/O.
                            for _ in 0..self.event_interval.get() {
                                let Some(t) = self.scheduler.tasks.pop_front() else {
                                    break;
                                };
                                self.scheduler.run_task(t);
                            }

                            // Check main future
                            if let std::task::Poll::Ready(output) = root.as_mut().poll(&mut root_context) {
                                return output;
                            }

                            if self.scheduler.tasks.is_empty() {
                                // No task to execute, we should wait for io blockingly
                                break;
                            }

                            // Runnable work remains. Poll the driver without
                            // parking before beginning another task batch.
                            self.driver
                                .turn(Some(Duration::ZERO))
                                .expect("Failed to poll I/O events");
                        }

                        // Wait for I/O (or a cross-thread wake from the remote
                        // scheduler / blocking pool), then apply completions.
                        let timeout = self.timer.borrow().min_timeout();
                        self.driver.turn(timeout).expect("Failed to wait for I/O events");
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
    /// If the runtime is dropped before the task finishes, the handle resolves
    /// with a cancellation error. See [Shutdown](Runtime#shutdown).
    pub fn spawn<F: Future + 'static>(&self, future: F) -> JoinHandle<F::Output> {
        self.scheduler.spawn(future)
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
        spawn_blocking_task(self.driver.blocking_pool(), self.driver.wakeup(), f)
    }

    /// Shut down the runtime, waiting at most `duration` for the blocking pool
    /// to finish draining before dropping it.
    ///
    /// This is a convenience for dropping a runtime from a context where
    /// blocking on in-flight blocking work would be undesirable (for example
    /// inside an async context). After the timeout elapses, blocking workers
    /// still executing are detached and finish on their own.
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use karmaio::runtime::Runtime;
    ///
    /// let mut rt = Runtime::new().unwrap();
    /// rt.block_on(async {
    ///     // work
    /// });
    /// rt.shutdown_timeout(Duration::from_millis(100));
    /// ```
    pub fn shutdown_timeout(self, duration: Duration) {
        self._blocking.shutdown_with_timeout(duration);
        // Dropping `self` runs the normal teardown; the pool already shut down
        // so its shutdown step returns immediately.
    }

    /// Shut down the runtime without waiting for any blocking work to stop.
    ///
    /// Equivalent to [`Runtime::shutdown_timeout`] with a zero duration.
    ///
    /// Blocking jobs that are mid-execution are detached and may still be
    /// running when this returns, so resources they were about to release may
    /// leak if they never finish.
    pub fn shutdown_background(self) {
        self.shutdown_timeout(Duration::ZERO);
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
        spawn_blocking_task(driver.blocking_pool(), driver.wakeup(), f)
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

    CURRENT_SCHEDULER.with(|scheduler| scheduler.spawn(future))
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

        let output = runtime.block_on(task);

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

        let err = runtime.block_on(task).expect_err("task should report panic");

        assert!(err.is_panic());
    }

    #[test]
    fn abort_cancels_task_before_it_runs() {
        let mut runtime = Runtime::new().expect("runtime should start");
        let task = runtime.spawn(pending::<usize>());

        task.abort();
        let err = runtime.block_on(task).expect_err("task should be cancelled");

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
            drop(runtime.spawn(pending::<()>()));
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
    fn dropping_runtime_joins_running_blocking_work() {
        let mut runtime = Runtime::new().expect("runtime should start");
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel();
        let (drop_started_tx, drop_started_rx) = mpsc::channel();
        let task = runtime.spawn_blocking(move || {
            started_tx.send(()).expect("report blocking work start");
            release_rx.recv().expect("release blocking work");
            completed_tx.send(()).expect("report blocking work completion");
        });

        runtime.block_on(async {
            crate::time::sleep(std::time::Duration::from_millis(1)).await;
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("blocking work should start before teardown");
        drop(task);

        let releaser = thread::spawn(move || {
            drop_started_rx.recv().expect("wait for runtime teardown");
            release_tx.send(()).expect("release blocking work");
        });

        drop_started_tx.send(()).expect("start runtime teardown");
        drop(runtime);
        releaser.join().expect("release thread should finish");
        completed_rx
            .try_recv()
            .expect("runtime drop should join the running blocking job");
    }

    #[test]
    fn shutdown_timeout_detaches_running_blocking_work() {
        let mut runtime = Runtime::new().expect("runtime should start");
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let _task = runtime.spawn_blocking(move || {
            started_tx.send(()).expect("report blocking work start");
            release_rx.recv().ok();
        });

        runtime.block_on(async {
            crate::time::sleep(std::time::Duration::from_millis(1)).await;
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("blocking work should start before teardown");

        // A zero timeout must not wait for the stuck blocking job.
        let start = std::time::Instant::now();
        runtime.shutdown_timeout(std::time::Duration::from_millis(0));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));

        drop(release_tx);
    }

    #[test]
    fn shutdown_background_is_fast() {
        let mut runtime = Runtime::new().expect("runtime should start");
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let _task = runtime.spawn_blocking(move || {
            started_tx.send(()).expect("report blocking work start");
            release_rx.recv().ok();
        });

        runtime.block_on(async {
            crate::time::sleep(std::time::Duration::from_millis(1)).await;
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("blocking work should start before teardown");

        let start = std::time::Instant::now();
        runtime.shutdown_background();
        assert!(start.elapsed() < std::time::Duration::from_secs(1));

        drop(release_tx);
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

        let output = runtime.block_on(task);
        wake_thread.join().expect("wake thread should finish");

        assert_eq!(output.expect("task should succeed"), 11);
    }

    #[test]
    fn block_on_accepts_a_borrowed_future() {
        let mut runtime = Runtime::new().expect("runtime should start");
        let value = String::from("borrowed");

        let length = runtime.block_on(async { value.len() });

        assert_eq!(length, value.len());
    }

    #[test]
    fn root_future_wake_unparks_runtime() {
        struct RemoteWake {
            ready: Arc<AtomicBool>,
            waker_tx: Option<mpsc::Sender<Waker>>,
        }

        impl Future for RemoteWake {
            type Output = ();

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                if self.ready.load(Ordering::Acquire) {
                    return Poll::Ready(());
                }

                if let Some(waker_tx) = self.waker_tx.take() {
                    waker_tx.send(cx.waker().clone()).expect("send root waker");
                }
                Poll::Pending
            }
        }

        let mut runtime = Runtime::new().expect("runtime should start");
        let ready = Arc::new(AtomicBool::new(false));
        let (waker_tx, waker_rx) = mpsc::channel::<Waker>();
        let wake_ready = Arc::clone(&ready);
        let wake_thread = thread::spawn(move || {
            let waker = waker_rx.recv().expect("receive root waker");
            wake_ready.store(true, Ordering::Release);
            waker.wake();
        });

        // This timer bounds the regression test. A no-op root waker would leave
        // the runtime asleep until this deadline instead of waking promptly.
        let _watchdog = runtime.spawn(async {
            crate::time::sleep(std::time::Duration::from_secs(2)).await;
        });
        let start = std::time::Instant::now();
        runtime.block_on(RemoteWake {
            ready,
            waker_tx: Some(waker_tx),
        });

        wake_thread.join().expect("wake thread should finish");
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn same_thread_wake_stays_with_its_runtime() {
        struct CapturedWake {
            ready: Arc<AtomicBool>,
            waker_tx: Option<mpsc::Sender<Waker>>,
        }

        impl Future for CapturedWake {
            type Output = usize;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<usize> {
                if self.ready.load(Ordering::Acquire) {
                    return Poll::Ready(17);
                }

                if let Some(waker_tx) = self.waker_tx.take() {
                    waker_tx.send(cx.waker().clone()).expect("capture task waker");
                }
                Poll::Pending
            }
        }

        let mut first = Runtime::new().expect("first runtime should start");
        let mut second = Runtime::new().expect("second runtime should start");
        let ready = Arc::new(AtomicBool::new(false));
        let (waker_tx, waker_rx) = mpsc::channel::<Waker>();
        let task = first.spawn(CapturedWake {
            ready: Arc::clone(&ready),
            waker_tx: Some(waker_tx),
        });

        first.block_on(async {});
        let waker = waker_rx.recv().expect("receive captured waker");
        ready.store(true, Ordering::Release);

        // Invoke a waker belonging to the first runtime while the second
        // runtime is scoped on the same OS thread.
        second.block_on(async move { waker.wake_by_ref() });
        assert!(!task.is_finished());

        let output = first.block_on(task);
        assert_eq!(output.expect("task should finish on its own runtime"), 17);
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

        let output = runtime.block_on(task);
        assert_eq!(output.expect("task should succeed"), 42);
    }

    #[test]
    fn shutdown_drops_non_send_future_on_owner_thread() {
        use std::rc::Rc;

        struct DropThread {
            dropped_on: mpsc::Sender<thread::ThreadId>,
            _local: Rc<()>,
        }

        impl Drop for DropThread {
            fn drop(&mut self) {
                self.dropped_on
                    .send(thread::current().id())
                    .expect("report future drop thread");
            }
        }

        let owner = thread::current().id();
        let (dropped_on_tx, dropped_on_rx) = mpsc::channel();
        let runtime = Runtime::new().expect("runtime should start");
        let drop_thread = DropThread {
            dropped_on: dropped_on_tx,
            _local: Rc::new(()),
        };
        let task = runtime.spawn(async move {
            let _drop_thread = drop_thread;
            pending::<usize>().await
        });

        drop(runtime);

        assert!(task.is_finished());
        assert_eq!(
            dropped_on_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("future should be dropped during runtime shutdown"),
            owner
        );

        // The handle is Send because its output is Send, but the local future
        // has already been consumed safely on the owner thread.
        thread::spawn(move || drop(task))
            .join()
            .expect("remote handle drop should succeed");
    }

    #[test]
    fn spawn_blocking_returns_value() {
        let mut runtime = Runtime::new().expect("runtime should start");
        let handle = runtime.spawn_blocking(|| 42usize);
        let output = runtime.block_on(handle);
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
        let err = runtime.block_on(handle).expect_err("blocking panic should surface");
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

    #[cfg(feature = "net")]
    #[test]
    fn self_waking_task_does_not_starve_io() {
        use crate::{
            net::tcp::TcpListener,
            time::{Duration, timeout},
        };

        let listener =
            TcpListener::bind("127.0.0.1:0".parse().expect("valid loopback address")).expect("bind loopback listener");
        let address = listener.local_addr().expect("read listener address");
        let connector = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(25));
            std::net::TcpStream::connect(address).expect("connect to loopback listener")
        });

        let mut runtime = Runtime::new().expect("runtime should start");
        let accepted = runtime.block_on(async move {
            let spinner = super::spawn_local(std::future::poll_fn(|cx| {
                cx.waker().wake_by_ref();
                Poll::<()>::Pending
            }));
            let accepted = timeout(Duration::from_secs(1), listener.accept()).await;
            spinner.abort();
            accepted
        });

        let _client = connector.join().expect("connector thread should finish");
        let (_stream, _peer) = accepted
            .expect("I/O should complete despite the runnable task")
            .expect("accept should succeed");
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

    /// Path FS ops use the blocking-pool fallback on kqueue Unix/Windows.
    #[cfg(feature = "fs")]
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
