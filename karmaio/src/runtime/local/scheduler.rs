use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    future::Future,
    sync::{Arc, Mutex},
    thread::{self, ThreadId},
};

use crate::{
    driver::Wakeup,
    runtime::{
        Schedule,
        local::{CURRENT_SCHEDULER, queue::LocalTaskQueue},
    },
    task::{JoinHandle, OwnedTask, Task, TaskId, new_owned_task},
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
    owned: RefCell<HashMap<TaskId, OwnedTask<ScheduleHandle>>>,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self {
            tasks: LocalTaskQueue::default(),
            remote: RemoteTaskQueue::default(),
            owned: RefCell::new(HashMap::new()),
        }
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.shutdown();
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

    pub(crate) fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (task, join, owned) = new_owned_task(future, self.handle());
        let replaced = self.owned.borrow_mut().insert(owned.id(), owned);
        debug_assert!(replaced.is_none());
        self.tasks.push_back(task);
        join
    }

    pub(crate) fn run_task(&self, task: Task<ScheduleHandle>) {
        let id = task.run();
        let finished = self.owned.borrow().get(&id).is_some_and(OwnedTask::is_finished);
        if finished {
            let owned = self.owned.borrow_mut().remove(&id);
            drop(owned);
        }
    }

    /// Drop queued tasks while the driver remains alive so their detached
    /// operation state can be handed back to the backend during shutdown.
    pub(crate) fn shutdown(&mut self) {
        if self.owned.get_mut().is_empty() {
            self.remote.shutdown();
            self.tasks.clear();
            return;
        }

        // Request cancellation while the remote queue remains open. Idle tasks
        // are notified into that queue because shutdown runs outside the scoped
        // scheduler context.
        for task in self.owned.get_mut().values() {
            task.abort();
        }

        // Every unfinished task is now either already notified or was notified
        // by abort. Drive those notifications on the owner thread until each
        // future has been consumed and its scheduler-owned reference released.
        while !self.owned.get_mut().is_empty() {
            self.remote.drain_into(&self.tasks);
            let task = self
                .tasks
                .pop_front()
                .expect("cancelled task missing scheduler notification");
            self.run_task(task);
        }

        self.remote.shutdown();
        self.tasks.clear();
    }
}

#[derive(Clone)]
pub(crate) struct ScheduleHandle {
    owner: ThreadId,
    remote: RemoteTaskQueue,
}

impl Schedule for ScheduleHandle {
    fn schedule(&self, task: Task<Self>) {
        if self.is_current() {
            CURRENT_SCHEDULER.with(|scheduler| scheduler.tasks.push_back(task));
        } else {
            self.remote.push_back(task);
        }
    }

    fn yield_now(&self, task: Task<Self>) {
        self.schedule(task);
    }
}

impl ScheduleHandle {
    fn is_current(&self) -> bool {
        thread::current().id() == self.owner
            && CURRENT_SCHEDULER.is_set()
            && CURRENT_SCHEDULER.with(|scheduler| self.remote.is_same_queue(&scheduler.remote))
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
    state: Mutex<RemoteQueueState>,
}

enum RemoteQueueState {
    Open(VecDeque<Task<ScheduleHandle>>),
    Closed,
}

impl Default for RemoteTaskQueue {
    fn default() -> Self {
        Self {
            inner: Arc::new(RemoteQueueInner {
                state: Mutex::new(RemoteQueueState::Open(VecDeque::new())),
            }),
            wakeup: None,
        }
    }
}

impl RemoteTaskQueue {
    fn is_same_queue(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    fn push_back(&self, task: Task<ScheduleHandle>) {
        let rejected = {
            let mut state = self.inner.state.lock().expect("remote task queue poisoned");
            match &mut *state {
                RemoteQueueState::Open(queue) => {
                    queue.push_back(task);
                    None
                }
                RemoteQueueState::Closed => Some(task),
            }
        };

        if let Some(task) = rejected {
            // A task destructor may wake another task. Run it after releasing
            // the queue lock so scheduling remains reentrant during shutdown.
            drop(task);
            return;
        }

        if let Some(w) = &self.wakeup {
            w.wake();
        }
    }

    fn drain_into(&self, local: &LocalTaskQueue<ScheduleHandle>) {
        let queued = {
            let mut state = self.inner.state.lock().expect("remote task queue poisoned");
            match &mut *state {
                RemoteQueueState::Open(queue) => std::mem::take(queue),
                RemoteQueueState::Closed => VecDeque::new(),
            }
        };

        for task in queued {
            local.push_back(task);
        }
    }

    /// Mark the remote queue closed and drop all queued tasks.
    ///
    /// Called from [`Scheduler`]'s `Drop` to break the
    /// `Arc → Task → ScheduleHandle → Arc` cycle before the scheduler's
    /// `Arc` clone is released.
    fn shutdown(&self) {
        let queued = {
            let mut state = self.inner.state.lock().expect("remote task queue poisoned");
            match std::mem::replace(&mut *state, RemoteQueueState::Closed) {
                RemoteQueueState::Open(queue) => queue,
                RemoteQueueState::Closed => VecDeque::new(),
            }
        };

        // Dropping a task can execute arbitrary future destructors. Keep that
        // work outside the mutex so a destructor may safely wake another task.
        drop(queued);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, mpsc},
        thread,
    };

    use crate::task::new_task;

    use super::{RemoteQueueInner, RemoteTaskQueue, ScheduleHandle};

    struct LockQueueOnDrop {
        inner: Arc<RemoteQueueInner>,
        lock_reentered: mpsc::Sender<()>,
    }

    impl std::future::Future for LockQueueOnDrop {
        type Output = ();

        fn poll(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
            std::task::Poll::Pending
        }
    }

    impl Drop for LockQueueOnDrop {
        fn drop(&mut self) {
            let guard = self
                .inner
                .state
                .try_lock()
                .expect("task destructor should run after unlocking the queue");
            drop(guard);
            self.lock_reentered.send(()).expect("report queue reentry");
        }
    }

    #[test]
    fn shutdown_drops_queued_tasks_after_unlocking() {
        let remote = RemoteTaskQueue::default();
        let scheduler = ScheduleHandle {
            owner: thread::current().id(),
            remote: remote.clone(),
        };
        let (lock_reentered_tx, lock_reentered_rx) = mpsc::channel();
        let (task, join) = new_task(
            LockQueueOnDrop {
                inner: Arc::clone(&remote.inner),
                lock_reentered: lock_reentered_tx,
            },
            scheduler,
        );
        drop(join);
        remote.push_back(task);

        remote.shutdown();

        lock_reentered_rx
            .recv()
            .expect("task destructor should reenter the queue lock");
    }
}
