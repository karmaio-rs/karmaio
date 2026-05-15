use std::{future::Future, io, pin::pin, task::Context};

use crate::{
    driver::{Driver, Handle},
    runtime::local::scheduler::{ScheduleHandle, Scheduler},
    task::{join::JoinHandle, new_task, waker::dummy_waker},
};

pub mod queue;
pub mod scheduler;

// One scheduler per OS thread — only accessible inside the runtime
scoped_thread_local!(static CURRENT_SCHEDULER: Scheduler);
scoped_thread_local!(pub(crate) static CURRENT_DRIVER: Handle);

pub struct Runtime {
    pub(crate) driver: Driver,
    pub(crate) scheduler: Scheduler,
}

impl Runtime {
    pub fn new() -> io::Result<Self> {
        let driver = Driver::new()?;
        Ok(Self {
            driver,
            scheduler: Scheduler::default(),
        })
    }

    pub fn block_on<F: Future + 'static>(&mut self, future: F) -> F::Output {
        assert!(!CURRENT_SCHEDULER.is_set(), "Can not start a runtime inside a runtime");

        let waker = dummy_waker();
        let mut cx = Context::from_waker(&waker);
        let handle: Handle = (&self.driver).into();

        CURRENT_SCHEDULER.set(&self.scheduler, || {
            CURRENT_DRIVER.set(&handle, || {
                let mut join_handle = pin!(future);

                loop {
                    loop {
                        // Consume all tasks(with max round to prevent io starvation)
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
                        // check if ready
                        if let std::task::Poll::Ready(t) = join_handle.as_mut().poll(&mut cx) {
                            return t;
                        }

                        if self.scheduler.tasks.is_empty() {
                            // No task to execute, we should wait for io blockingly
                            // Hot path
                            break;
                        }
                    }

                    // Wait for I/O events and dispatch completions
                    let _completed = self.driver.wait().expect("Failed to wait for I/O events");
                    self.driver.dispatch_completions();
                }
            })
        })
    }

    pub fn spawn<F: Future + 'static>(&self, future: F) -> JoinHandle<F::Output> {
        let (task, join_handle) = new_task(future, ScheduleHandle);

        self.scheduler.tasks.push_back(task);

        return join_handle;
    }
}
