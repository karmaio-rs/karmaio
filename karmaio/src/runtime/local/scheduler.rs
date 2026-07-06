use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    thread::{self, ThreadId},
};

use crate::{
    driver::Wakeup,
    runtime::{
        Schedule,
        local::{CURRENT_SCHEDULER, queue::LocalTaskQueue},
    },
    task::Task,
};

/// The scheduler for a single-threaded (current-thread) runtime.
///
/// It maintains a local task queue for work on the owning thread and a
/// remote queue for tasks scheduled from other threads (via wakers or
/// `Handle`-style APIs).
///
/// The design follows patterns from Tokio's current_thread scheduler and
/// compio's executor: remote work is drained at the start of each "tick"
/// before running local tasks.
pub(crate) struct Scheduler {
    pub(crate) tasks: LocalTaskQueue<ScheduleHandle>,
    remote: RemoteTaskQueue,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self {
            tasks: LocalTaskQueue::default(),
            remote: RemoteTaskQueue::default(),
        }
    }
}

impl Scheduler {
    pub(crate) fn set_wakeup(&mut self, wakeup: Wakeup) {
        self.remote.wakeup = Some(wakeup);
    }

    pub(crate) fn handle(&self) -> ScheduleHandle {
        ScheduleHandle {
            owner: thread::current().id(),
            remote: self.remote.clone(),
        }
    }

    /// Perform scheduler tick preparation: drain any tasks that were
    /// scheduled from other threads (or outside the current scheduler
    /// context) into the local task queue.
    ///
    /// This must be called at the very beginning of every scheduler tick,
    /// before running any local tasks. This ensures remote work is picked
    /// up promptly and matches patterns used in other single-threaded
    /// completion-based runtimes (e.g. compio).
    pub(crate) fn tick(&self) {
        self.remote.drain_into(&self.tasks);
    }
}

#[derive(Clone)]
pub(crate) struct ScheduleHandle {
    owner: ThreadId,
    remote: RemoteTaskQueue,
}

impl Schedule for ScheduleHandle {
    fn schedule(&self, task: Task<Self>) {
        if thread::current().id() == self.owner && CURRENT_SCHEDULER.is_set() {
            CURRENT_SCHEDULER.with(|scheduler| scheduler.tasks.push_back(task));
        } else {
            self.remote.push_back(task);
        }
    }

    fn yield_now(&self, task: Task<Self>) {
        self.schedule(task);
    }
}

#[derive(Clone)]
struct RemoteTaskQueue {
    // The queue itself is required to transfer `Task` objects from other threads
    // into the thread-local run queue. Even with a perfect wakeup, the scheduled
    // task handle must be stored somewhere that the owner thread can drain.
    // This is still needed as long as we allow `Task` / wakers to be `Send`.
    queue: Arc<Mutex<VecDeque<Task<ScheduleHandle>>>>,
    /// Optional wakeup token. When present, a push from a remote thread will
    /// use it to promptly wake the owner runtime's poller.
    wakeup: Option<Wakeup>,
}

impl Default for RemoteTaskQueue {
    fn default() -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            wakeup: None,
        }
    }
}

impl RemoteTaskQueue {
    fn push_back(&self, task: Task<ScheduleHandle>) {
        self.queue.lock().expect("remote task queue poisoned").push_back(task);
        if let Some(w) = &self.wakeup {
            w.wake();
        }
    }

    fn drain_into(&self, local: &LocalTaskQueue<ScheduleHandle>) {
        let mut remote = self.queue.lock().expect("remote task queue poisoned");

        while let Some(task) = remote.pop_front() {
            local.push_back(task);
        }
    }
}
