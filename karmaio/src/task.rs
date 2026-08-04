use std::{future::Future, marker::PhantomData, ptr::NonNull, rc::Rc};

use header::Header;
use raw::RawTask;

use crate::runtime::Schedule;

pub(crate) mod header;
pub(crate) mod internal;
pub(crate) mod join;
pub(crate) mod raw;
pub(crate) mod state;
pub(crate) mod trailer;
mod utils;
mod vtable;
pub(crate) mod waker;

// Public task API (re-exported from `runtime` / crate root as needed).
pub use join::{JoinError, JoinHandle};

/// Stable identity for a task allocation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct TaskId(NonNull<Header>);

#[repr(transparent)]
pub(crate) struct Task<S: Schedule> {
    raw: RawTask,
    _p: PhantomData<S>,
}

// SAFETY: `Task` is a schedulable handle to an allocation guarded by the task
// state machine. Sending it between threads only transfers the right to enqueue
// or drop that handle; polling is still controlled by the owning scheduler.
unsafe impl<S: Schedule> Send for Task<S> {}
// SAFETY: Shared access to `Task` does not expose the inner future. All shared
// coordination goes through atomics in the task header.
unsafe impl<S: Schedule> Sync for Task<S> {}

impl<S: Schedule> Task<S> {
    unsafe fn from_raw(task_ptr: NonNull<Header>) -> Task<S> {
        Task {
            raw: RawTask::from_raw(task_ptr),
            _p: PhantomData,
        }
    }

    fn header(&self) -> &Header {
        self.raw.header()
    }

    /// Run the task.
    ///
    /// Consumes the `Task` ref-count and transfers ownership of it into the
    /// poll path. The task state machine is responsible for releasing that
    /// ref-count (via `transition_to_idle` / `drop_reference` / `dealloc`).
    /// The `Task` must not drop after `poll` returns, or refs would be double-decremented.
    pub(crate) fn run(self) -> TaskId {
        let id = TaskId(self.raw.task_ptr());
        let raw = self.raw;
        // Transfer the ref-count into `poll`; do not run `Drop for Task`.
        std::mem::forget(self);
        raw.poll();
        id
    }
}

impl<S: Schedule> Drop for Task<S> {
    fn drop(&mut self) {
        if self.header().state.ref_dec() {
            self.raw.dealloc();
        }
    }
}

/// Scheduler-owned reference that keeps a local task alive until it reaches a
/// terminal state.
///
/// This reference never leaves the task's home scheduler. In particular, it
/// guarantees that runtime shutdown can cancel and destroy a `!Send` future on
/// its owner thread before remote wakers are allowed to become the allocation's
/// final references.
pub(crate) struct OwnedTask<S: Schedule> {
    raw: RawTask,
    _scheduler: PhantomData<S>,
    _local: PhantomData<Rc<()>>,
}

impl<S: Schedule> OwnedTask<S> {
    fn new(raw: RawTask) -> Self {
        raw.header().state.ref_inc();
        Self {
            raw,
            _scheduler: PhantomData,
            _local: PhantomData,
        }
    }

    pub(crate) fn id(&self) -> TaskId {
        TaskId(self.raw.task_ptr())
    }

    pub(crate) fn abort(&self) {
        self.raw.cancel();
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.raw.header().state.get_snapshot().is_complete()
    }
}

impl<S: Schedule> Drop for OwnedTask<S> {
    fn drop(&mut self) {
        self.raw.drop_reference();
    }
}

pub(crate) fn new_task<F, S>(future: F, scheduler: S) -> (Task<S>, JoinHandle<F::Output>)
where
    S: Schedule,
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let raw = RawTask::new(future, scheduler);
    let task = Task { raw, _p: PhantomData };
    let join = JoinHandle::new(raw);

    (task, join)
}

pub(crate) fn new_owned_task<F, S>(future: F, scheduler: S) -> (Task<S>, JoinHandle<F::Output>, OwnedTask<S>)
where
    S: Schedule,
    F: Future + 'static,
    F::Output: 'static,
{
    let raw = RawTask::new(future, scheduler);
    let task = Task { raw, _p: PhantomData };
    let join = JoinHandle::new(raw);
    let owned = OwnedTask::new(raw);

    (task, join, owned)
}
