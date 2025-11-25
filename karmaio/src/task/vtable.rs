use std::{
    future::Future,
    mem, panic,
    ptr::NonNull,
    task::{Context, Poll, Waker},
};

use crate::{
    runtime::Schedule,
    task::{
        Task,
        state::{Snapshot, TransitionToIdle, TransitionToRunning},
        trailer::Trailer,
        utils::UnsafeCellExt,
        waker::waker_ref,
    },
};

use super::{header::Header, internal::InternalTask, raw::RawTask};

/// The `VTable` for a specific future type.
///
/// This is a form of manual type erasure. The scheduler operates on `Task`
/// handles, which are pointers to a `Header`. The `VTable` allows the scheduler
/// to call the correct methods for the underlying `Future` type without
/// having to know the concrete type.
pub(super) struct VTable {
    /// Poll the future
    pub(super) poll: fn(NonNull<Header>),

    /// Read the task output, if complete
    pub(super) try_read_output: fn(NonNull<Header>, *mut (), &Waker),

    /// The join handle has been dropped
    pub(super) drop_join_handle: fn(NonNull<Header>),

    /// Deallocate the memory
    pub(super) dealloc: fn(NonNull<Header>),

    /// Schedule a task into the scheduler
    pub(super) schedule: fn(NonNull<Header>),
}

impl VTable {
    #[must_use]
    pub(super) fn new<F: Future, S: Schedule>() -> &'static Self {
        &VTable {
            poll: poll::<F, S>,
            try_read_output: try_read_output::<F, S>,
            drop_join_handle: drop_join_handle::<F, S>,
            dealloc: dealloc::<F, S>,
            schedule: schedule::<F, S>,
        }
    }
}

/// Polls the future stored inside the task `Cell`.
///
/// # Safety
///
/// `ptr` must be a non-null pointer to the `Header` of a `Cell<F>`.
fn poll<F: Future, S: Schedule>(ptr: NonNull<Header>) {
    let internal_task_ptr: NonNull<InternalTask<F, S>> = ptr.cast();
    let internal_task = unsafe { internal_task_ptr.as_ref() };

    match internal_task.header.state.transition_to_running() {
        TransitionToRunning::Success => {
            // Poll the future
            let waker = waker_ref::<S>(&ptr);
            let cx = Context::from_waker(&waker);
            let res = poll_future(internal_task, cx);

            if res == Poll::Ready(()) {
                // The future completed. Move on to complete the task.
                complete(internal_task_ptr);
                return;
            }

            match internal_task.header.state.transition_to_idle() {
                TransitionToIdle::Ok => return,
                TransitionToIdle::OkNotified => {
                    internal_task.header.state.ref_inc();
                    let task = unsafe { Task::from_raw(internal_task_ptr.cast()) };
                    internal_task.scheduler.yield_now(task);
                }
                TransitionToIdle::OkDealloc => dealloc::<F, S>(ptr),
                TransitionToIdle::Cancelled => complete(internal_task_ptr),
            }
        }
        TransitionToRunning::Cancelled => complete(internal_task_ptr),
        TransitionToRunning::Failed => return,
        TransitionToRunning::Dealloc => dealloc::<F, S>(ptr),
    }
}

/// Copies the output of the future to a given destination pointer.
///
/// # Safety
///
/// The caller must ensure that the task is complete and that `dst` is a valid
/// pointer to a memory location that can hold the future's output.
fn try_read_output<F: Future, S: Schedule>(raw_task_ptr: NonNull<Header>, dst: *mut (), waker: &Waker) {
    let out = unsafe { &mut *(dst as *mut Poll<F::Output>) };

    let internal_task_ptr: NonNull<InternalTask<F, S>> = raw_task_ptr.cast();
    let internal_task = unsafe { internal_task_ptr.as_ref() };

    if can_read_output(&internal_task.header, &internal_task.trailer, waker) {
        *out = Poll::Ready(internal_task.take_output());
    }
}

/// Handles the `JoinHandle` being dropped.
///
/// # Safety
///
/// The `ptr` must point to the header of a `Cell<F>`. This function is called
/// when the associated `JoinHandle` is dropped.
fn drop_join_handle<F: Future, S: Schedule>(ptr: NonNull<Header>) {
    let internal_task_ptr: NonNull<InternalTask<F, S>> = ptr.cast();
    let internal_task = unsafe { internal_task_ptr.as_ref() };

    // Try to unset `JOIN_INTEREST` and `JOIN_WAKER`. This must be done as a first step in case the task concurrently completed.
    let next_state = internal_task.header.state.transition_to_join_handle_dropped();

    if next_state.drop_output {
        // It is our responsibility to drop the output. This is critical as
        // the task output may not be `Send` and as such must remain with
        // the scheduler or `JoinHandle`. i.e. if the output remains in the
        // task structure until the task is deallocated, it may be dropped
        // by a Waker on any arbitrary thread.
        //
        // Panics are delivered to the user via the `JoinHandle`.
        // Given that they are dropping the `JoinHandle`,
        // we assume they are not interested in the panic and ignore it.
        let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            internal_task.drop_future_or_output();
        }));
    }

    if next_state.drop_waker {
        // If the JOIN_WAKER flag is unset at this point, the task is either
        // or not complete so the `JoinHandle` is responsible for dropping the waker.
        //
        // Safety:
        // If the JOIN_WAKER bit is not set the join handle has exclusive
        // access to the waker as per rule 2 in task.rs
        // This can only be the case at this point in two scenarios:
        // 1. The task completed and the runtime unset `JOIN_WAKER` flag
        //    after accessing the waker during task completion. So the
        //    `JoinHandle` is the only one to access the  join waker here.
        // 2. The task is not completed so the `JoinHandle` was able to unset
        //    `JOIN_WAKER` bit itself to get mutable access to the waker.
        //    The runtime will not access the waker when this flag is unset.
        internal_task.trailer.set_waker(None);
    }

    // Drop the `JoinHandle` reference, possibly deallocating the task
    RawTask::from_raw(ptr).drop_reference();
}

/// Deallocates the memory used by the task.
///
/// # Safety
///
/// The `ptr` must point to the header of a `Cell<F>` that was allocated
/// using `Box::new`.
fn dealloc<F: Future, S: Schedule>(ptr: NonNull<Header>) {
    let internal_task_ptr: NonNull<InternalTask<F, S>> = ptr.cast();
    let internal_task = unsafe { internal_task_ptr.as_ref() };

    // Observe that we expect to have mutable access to these objects
    // Because we are going to drop them.
    internal_task.trailer.waker.with_mut(|_| ());
    internal_task.future.with_mut(|_| ());

    // Safety: The caller of this method just transitioned our ref-count to
    // zero, so it is our responsibility to release the allocation.
    //
    // We don't hold any references into the allocation at this point, but
    // it is possible for another thread to still hold a `&State` into the
    // allocation if that other thread has decremented its last ref-count,
    // but has not yet returned from the relevant method on `State`.
    //
    // However, the `State` type consists of just an `AtomicUsize`, and an
    // `AtomicUsize` wraps the entirety of its contents in an `UnsafeCell`.
    // As explained in the documentation for `UnsafeCell`, such references
    // are allowed to be dangling after their last use, even if the
    // reference has not yet gone out of scope.
    unsafe {
        drop(Box::from_raw(internal_task_ptr.as_ptr()));
    }
}

fn schedule<F: Future, S: Schedule>(raw_task_ptr: NonNull<Header>) {
    unsafe {
        raw_task_ptr
            .cast::<InternalTask<F, S>>()
            .as_ref()
            .scheduler
            .schedule(Task::from_raw(raw_task_ptr.cast()))
    };
}

// Internal helper functions.

fn poll_future<F: Future, S: Schedule>(internal_task: &InternalTask<F, S>, cx: Context<'_>) -> Poll<()> {
    // Poll the future.
    let output = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        struct Guard<'a, F: Future, S: Schedule> {
            core: &'a InternalTask<F, S>,
        }
        impl<'a, F: Future, S: Schedule> Drop for Guard<'a, F, S> {
            fn drop(&mut self) {
                // If the future panics on poll, we drop it inside the panic
                // guard.
                self.core.drop_future_or_output();
            }
        }
        let guard = Guard { core: internal_task };
        let res = guard.core.poll(cx);
        mem::forget(guard);
        res
    }));

    // Prepare output for being placed in the core stage.
    let output = match output {
        Ok(Poll::Pending) => return Poll::Pending,
        Ok(Poll::Ready(output)) => Ok(output),
        Err(_panic) => {
            internal_task.scheduler.unhandled_panic();
            Err(())
        }
    };

    // Catch and ignore panics if the future panics on drop.
    let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        internal_task.store_output(output.unwrap());
    }));

    if res.is_err() {
        internal_task.scheduler.unhandled_panic();
    }

    Poll::Ready(())
}

/// Complete the task. This method assumes that the state is RUNNING.
fn complete<F: Future, S: Schedule>(internal_task_ptr: NonNull<InternalTask<F, S>>) {
    let internal_task = unsafe { internal_task_ptr.as_ref() };

    // The future has completed and its output has been written to the task
    // stage. We transition from running to complete.
    let snapshot = internal_task.header.state.transition_to_complete();

    // We catch panics here in case dropping the future or waking the
    // JoinHandle panics.
    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        if !snapshot.is_join_interested() {
            // The `JoinHandle` is not interested in the output of this task.
            // It is our responsibility to drop the output.
            // The join waker was already dropped by the `JoinHandle` before.
            internal_task.drop_future_or_output();
        } else if snapshot.is_join_waker_set() {
            // Notify the join handle. The previous transition obtains the
            // lock on the waker cell.
            internal_task.trailer.wake_join();

            // Inform the `JoinHandle` that we are done waking the waker by
            // unsetting the `JOIN_WAKER` bit. If the `JoinHandle` has
            // already been dropped and `JOIN_INTEREST` is unset, then we must
            // drop the waker ourselves.
            if !internal_task
                .header
                .state
                .unset_waker_after_complete()
                .is_join_interested()
            {
                // SAFETY: We have COMPLETE=1 and JOIN_INTEREST=0, so
                // we have exclusive access to the waker.
                internal_task.trailer.set_waker(None);
            }
        }
    }));
}

fn can_read_output(header: &Header, trailer: &Trailer, waker: &Waker) -> bool {
    let state = header.state.get_snapshot();

    debug_assert!(state.is_join_interested());

    if !state.is_complete() {
        // If the task is not complete, try storing the provided waker in the task's waker field.
        let res = if state.is_join_waker_set() {
            // There already is a waker stored in the struct. If it matches
            // the provided waker, then there is no further work to do.
            // Otherwise, the waker must be swapped.
            if trailer.will_wake(waker) {
                return false;
            }

            // Unset the `JOIN_WAKER` to gain mutable access to the `waker`
            // field then update the field with the new join worker.
            //
            // This requires two atomic operations, unsetting the bit and
            // then resetting it. If the task transitions to complete
            // concurrently to either one of those operations, then setting
            // the join waker fails and we proceed to reading the task
            // output.
            header
                .state
                .unset_waker()
                .and_then(|snapshot| set_join_waker(header, trailer, waker.clone(), snapshot))
        } else {
            set_join_waker(header, trailer, waker.clone(), state)
        };

        match res {
            Ok(_) => return false,
            Err(snapshot) => {
                assert!(snapshot.is_complete());
            }
        }
    }
    true
}

fn set_join_waker(header: &Header, trailer: &Trailer, waker: Waker, state: Snapshot) -> Result<Snapshot, Snapshot> {
    assert!(state.is_join_interested());
    assert!(!state.is_join_waker_set());

    // Safety: Only the `JoinHandle` may set the `waker` field. When
    // `JOIN_INTEREST` is **not** set, nothing else will touch the field.

    trailer.set_waker(Some(waker));

    // Update the `JoinWaker` state accordingly
    let res = header.state.set_join_waker();

    // If the state could not be updated, then clear the join waker
    if res.is_err() {
        trailer.set_waker(None);
    }

    res
}
