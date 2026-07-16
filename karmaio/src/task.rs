use std::{future::Future, marker::PhantomData, ptr::NonNull};

use header::Header;

use crate::{
    runtime::Schedule,
    task::{join::JoinHandle, raw::RawTask},
};

pub(crate) mod header;
pub(crate) mod internal;
pub(crate) mod join;
pub(crate) mod raw;
pub(crate) mod state;
pub(crate) mod trailer;
mod utils;
mod vtable;
pub(crate) mod waker;

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
    pub(crate) fn run(self) {
        let raw = self.raw;
        // Transfer the ref-count into `poll`; do not run `Drop for Task`.
        std::mem::forget(self);
        raw.poll();
    }
}

impl<S: Schedule> Drop for Task<S> {
    fn drop(&mut self) {
        if self.header().state.ref_dec() {
            self.raw.dealloc();
        }
    }
}

pub(crate) fn new_task<F, S>(future: F, scheduler: S) -> (Task<S>, JoinHandle<F::Output>)
where
    S: Schedule,
    F: Future + 'static,
    F::Output: 'static,
{
    let raw = RawTask::new(future, scheduler);
    let task = Task { raw, _p: PhantomData };
    let join = JoinHandle::new(raw);

    (task, join)
}
