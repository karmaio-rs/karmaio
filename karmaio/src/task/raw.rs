use std::{ptr::NonNull, task::Waker};

use crate::{
    runtime::Schedule,
    task::{header::Header, internal::InternalTask, state::TransitionToNotified},
};

pub(crate) struct RawTask {
    task_ptr: NonNull<Header>,
}

impl Clone for RawTask {
    fn clone(&self) -> Self {
        *self
    }
}

impl Copy for RawTask {}

impl RawTask {
    pub(crate) fn new<F: Future, S: Schedule>(future: F, scheduler: S) -> RawTask {
        let task_pointer = Box::into_raw(InternalTask::new(future, scheduler));
        let header_pointer = unsafe { NonNull::new_unchecked(task_pointer as *mut Header) };

        RawTask {
            task_ptr: header_pointer,
        }
    }

    pub(super) fn from_raw(task_ptr: NonNull<Header>) -> RawTask {
        RawTask { task_ptr }
    }

    pub(crate) fn header(&self) -> &Header {
        unsafe { self.task_ptr.as_ref() }
    }

    /// Safety: mutual exclusion is required to call this function.
    pub(crate) fn poll(self) {
        let vtable = self.header().vtable;
        (vtable.poll)(self.task_ptr)
    }

    pub(crate) fn dealloc(self) {
        let vtable = self.header().vtable;
        (vtable.dealloc)(self.task_ptr);
    }

    /// Safety: `dst` must be a `*mut Poll<super::Result<F::Output>>`
    /// where `F` is the future stored by the task.
    pub(crate) fn try_read_output(self, dst: *mut (), waker: &Waker) {
        let vtable = self.header().vtable;
        (vtable.try_read_output)(self.task_ptr, dst, waker);
    }

    pub(crate) fn drop_join_handle(self) {
        let vtable = self.header().vtable;
        (vtable.drop_join_handle)(self.task_ptr);
    }

    pub(super) fn schedule(self) {
        let vtable = self.header().vtable;
        (vtable.schedule)(self.task_ptr);
    }

    // Bunch of functions for working with the Waker
    // Kept here instead of in the Pack to reduce monomorph codegen
    // Since these do not need to be highly typesafe
    pub(super) fn drop_reference(self) {
        if self.header().state.ref_dec() {
            self.dealloc();
        }
    }

    /// This call consumes a ref-count and notifies the task. This will create a
    /// new Notified and submit it if necessary.
    ///
    /// The caller does not need to hold a ref-count besides the one that was
    /// passed to this call.
    pub(super) fn wake_by_val(&self) {
        match self.header().state.transition_to_notified_by_val() {
            TransitionToNotified::Submit => {
                // The caller has given us a ref-count, and the transition has
                // created a new ref-count, so we now hold two. We turn the new
                // ref-count Notified and pass it to the call to `schedule`.
                //
                // The old ref-count is retained for now to ensure that the task
                // is not dropped during the call to `schedule` if the call
                // drops the task it was given.
                self.schedule();

                // Now that we have completed the call to schedule, we can
                // release our ref-count.
                self.drop_reference();
            }
            TransitionToNotified::Dealloc => {
                self.dealloc();
            }
            TransitionToNotified::DoNothing => {}
        }
    }

    /// This call notifies the task. It will not consume any ref-counts, but the
    /// caller should hold a ref-count.  This will create a new Notified and
    /// submit it if necessary.
    pub(super) fn wake_by_ref(&self) {
        match self.header().state.transition_to_notified_by_ref() {
            TransitionToNotified::Submit => {
                // The transition above incremented the ref-count for a new task
                // and the caller also holds a ref-count. The caller's ref-count
                // ensures that the task is not destroyed even if the new task
                // is dropped before `schedule` returns.
                self.schedule();
            }
            TransitionToNotified::DoNothing => {}
            TransitionToNotified::Dealloc => {}
        }
    }
}
