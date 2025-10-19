use std::{marker::PhantomData, ptr::NonNull};

use header::Header;

use crate::task::{join::JoinHandle, raw::RawTask};

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
pub struct Task<S: Schedule> {
    raw: RawTask,
    _p: PhantomData<S>,
}

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

    pub(crate) fn run(self) {
        self.raw.poll();
    }
}

impl<S: Schedule> Drop for Task<S> {
    fn drop(&mut self) {
        if self.header().state.ref_dec() {
            self.raw.dealloc();
        }
    }
}

// TODO: Move this out to the executer module once that is written
pub trait Schedule: Sized + 'static {
    /// Schedule the task
    fn schedule(&self, task: Task<Self>);

    /// Schedule the task to run in the near future, yielding the thread to
    /// other tasks.
    fn yield_now(&self, task: Task<Self>) {
        self.schedule(task);
    }

    /// Polling the task resulted in a panic. Should the runtime shutdown?
    fn unhandled_panic(&self) {
        // By default, do nothing
    }
}

pub fn new_task<F, S>(future: F, scheduler: S) -> (Task<S>, JoinHandle<F::Output>)
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
