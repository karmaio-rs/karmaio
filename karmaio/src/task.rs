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
