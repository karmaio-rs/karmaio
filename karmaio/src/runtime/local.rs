use std::{
    cell::RefCell,
    future::Future,
    io,
    pin::pin,
    task::{Context, Waker},
};

use std::rc::Rc;

use crate::{
    driver::{Driver, Handle},
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

pub struct Runtime {
    pub(crate) driver: Driver,
    pub(crate) scheduler: Scheduler,
    pub(crate) timer: Rc<RefCell<Timer>>,
}

impl Runtime {
    pub fn new() -> io::Result<Self> {
        let driver = Driver::new()?;
        let mut scheduler = Scheduler::default();
        scheduler.set_wakeup(driver.wakeup());
        Ok(Self {
            driver,
            scheduler,
            timer: Rc::new(RefCell::new(Timer::new())),
        })
    }

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
                        }

                        // Wait for I/O events and dispatch completions.
                        // The wait is woken promptly by the remote queue's wakeup token
                        // when a task is scheduled from another thread.
                        let timeout = self.timer.borrow().min_timeout();
                        let _completed = match timeout {
                            Some(duration) => self
                                .driver
                                .wait_with_duration(duration)
                                .expect("Failed to wait for I/O events"),
                            None => self.driver.wait().expect("Failed to wait for I/O events"),
                        };
                        self.driver.dispatch_completions();
                        self.timer.borrow_mut().wake();
                        // Note: we do *not* drain here. The next iteration of the inner
                        // loop will drain at the very beginning of the tick.
                    }
                })
            })
        })
    }

    pub fn spawn<F: Future + 'static>(&self, future: F) -> JoinHandle<F::Output> {
        let (task, join_handle) = new_task(future, self.scheduler.handle());

        self.scheduler.tasks.push_back(task);

        return join_handle;
    }
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

    use super::Runtime;
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
        let result = runtime.block_on(async move {
            timeout_at(past, std::future::pending::<usize>()).await
        });
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
}
