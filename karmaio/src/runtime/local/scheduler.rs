use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
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
/// Remote work is drained at the start of each "tick" before running local tasks.
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

impl Drop for Scheduler {
    fn drop(&mut self) {
        // Break the remote-queue ownership cycle before fields are dropped:
        // each queued `Task` holds a `ScheduleHandle` that clones the same
        // `Arc` as this queue. Mark closed, drain, and drop those tasks while
        // our `Arc` is still alive so the queue can empty cleanly.
        self.remote.shutdown();
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
    /// completion-based runtimes.
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

/// Shared remote run queue used by wakers and other threads.
///
/// The queue lives behind an `Arc` so `ScheduleHandle` (stored inside every
/// task) can push from any thread. That creates a potential cycle:
/// `Arc → Task → ScheduleHandle → Arc`. [`RemoteTaskQueue::shutdown`] and
/// [`Drop`] for the storage break that cycle at runtime teardown.
#[derive(Clone)]
struct RemoteTaskQueue {
    // The queue itself is required to transfer `Task` objects from other threads
    // into the thread-local run queue. Even with a perfect wakeup, the scheduled
    // task handle must be stored somewhere that the owner thread can drain.
    // This is still needed as long as we allow `Task` / wakers to be `Send`.
    inner: Arc<RemoteQueueInner>,
    /// Optional wakeup token. When present, a push from a remote thread will
    /// use it to promptly wake the owner runtime's poller.
    wakeup: Option<Wakeup>,
}

struct RemoteQueueInner {
    queue: Mutex<VecDeque<Task<ScheduleHandle>>>,
    /// Once set, further pushes drop the task instead of enqueueing. Prevents
    /// resurrecting the Arc cycle after the scheduler has shut down.
    closed: AtomicBool,
}

impl Drop for RemoteQueueInner {
    fn drop(&mut self) {
        // Last Arc clone is going away — drop any remaining tasks so we never
        // leave an Arc cycle if shutdown was skipped.
        self.closed.store(true, Ordering::Relaxed);
        if let Ok(queue) = self.queue.get_mut() {
            queue.clear();
        }
    }
}

impl Default for RemoteTaskQueue {
    fn default() -> Self {
        Self {
            inner: Arc::new(RemoteQueueInner {
                queue: Mutex::new(VecDeque::new()),
                closed: AtomicBool::new(false),
            }),
            wakeup: None,
        }
    }
}

impl RemoteTaskQueue {
    fn push_back(&self, task: Task<ScheduleHandle>) {
        {
            let mut remote = self.inner.queue.lock().expect("remote task queue poisoned");
            if self.inner.closed.load(Ordering::Acquire) {
                // Scheduler is gone (or going). Drop the task handle rather than
                // re-enqueue and recreate an Arc cycle.
                drop(task);
                return;
            }
            remote.push_back(task);
        }
        if let Some(w) = &self.wakeup {
            w.wake();
        }
    }

    fn drain_into(&self, local: &LocalTaskQueue<ScheduleHandle>) {
        let mut remote = self.inner.queue.lock().expect("remote task queue poisoned");

        while let Some(task) = remote.pop_front() {
            local.push_back(task);
        }
    }

    /// Mark the remote queue closed and drop all queued tasks.
    ///
    /// Called from [`Scheduler`]'s `Drop` to break the
    /// `Arc → Task → ScheduleHandle → Arc` cycle before the scheduler's
    /// `Arc` clone is released.
    fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::Release);
        let mut remote = self.inner.queue.lock().expect("remote task queue poisoned");
        remote.clear();
    }
}
