// --- Task states ---
// These flags define the lifecycle of a task.

use std::{
    fmt,
    sync::atomic::{
        AtomicUsize,
        Ordering::{AcqRel, Acquire, Release},
    },
};

/// The task is currently being run.
const RUNNING: usize = 0b0001;

/// The task is complete.
///
/// Once this bit is set, it is never unset.
const COMPLETE: usize = 0b0010;

/// Extracts the task's lifecycle value from the state.
const LIFECYCLE_MASK: usize = 0b11;

/// Flag tracking if the task has been pushed into a run queue.
const NOTIFIED: usize = 0b100;

/// The join handle is still around.
const JOIN_INTEREST: usize = 0b1_000;

/// A join handle waker has been set.
const JOIN_WAKER: usize = 0b10_000;

/// The task has been forcibly cancelled.
const CANCELLED: usize = 0b100_000;

/// All bits.
const STATE_MASK: usize = LIFECYCLE_MASK | NOTIFIED | JOIN_INTEREST | JOIN_WAKER | CANCELLED;

/// Bits used by the ref count portion of the state.
const REF_COUNT_MASK: usize = !STATE_MASK;

/// Number of positions to shift the ref count.
const REF_COUNT_SHIFT: usize = REF_COUNT_MASK.count_zeros() as usize;

/// One ref count.
const REF_ONE: usize = 1 << REF_COUNT_SHIFT;

/// State a task is initialized with
///
/// A task is initialized with two references:
///
///  * A reference for Task.
///  * A reference for the JoinHandle.
///
/// As the task starts with a `JoinHandle`, `JOIN_INTEREST` is set.
/// As the task starts with a `Notified`, `NOTIFIED` is set.
const INITIAL_STATE: usize = (REF_ONE * 2) | JOIN_INTEREST | NOTIFIED;

#[must_use]
pub(super) enum TransitionToRunning {
    Success,
    Cancelled,
    Failed,
    Dealloc,
}

#[must_use]
pub(super) enum TransitionToIdle {
    Ok,
    OkNotified,
    OkDealloc,
    Cancelled,
}

#[must_use]
pub(super) enum TransitionToNotified {
    DoNothing,
    Submit,
    Dealloc,
}

#[must_use]
pub(super) struct TransitionToJoinHandleDrop {
    pub(super) drop_waker: bool,
    pub(super) drop_output: bool,
}

// A wrapper around the state value and methods to manupulate it performantly
pub(super) struct State(AtomicUsize);

/// Current state value.
#[derive(Copy, Clone)]
pub(super) struct Snapshot(usize);

impl State {
    #[must_use]
    pub(super) fn new() -> State {
        State(AtomicUsize::new(INITIAL_STATE))
    }

    pub(super) fn get_snapshot(&self) -> Snapshot {
        Snapshot(self.0.load(Acquire))
    }

    /// Attempts to transition the lifecycle to `Running`. This sets the
    /// notified bit to false so notifications during the poll can be detected.
    pub(super) fn transition_to_running(&self) -> TransitionToRunning {
        self.fetch_update(|mut state| {
            assert!(state.is_notified());

            let action;

            if !state.is_idle() {
                // This happens if the task is either currently running or if it
                // has already completed, e.g. if it was cancelled during
                // shutdown. Consume the ref-count and return.
                state.ref_dec();
                if state.ref_count() == 0 {
                    action = TransitionToRunning::Dealloc;
                } else {
                    action = TransitionToRunning::Failed;
                }
            } else {
                // We are able to lock the RUNNING bit.
                state.set_running();
                state.unset_notified();

                if state.is_cancelled() {
                    action = TransitionToRunning::Cancelled;
                } else {
                    action = TransitionToRunning::Success;
                }
            }

            (action, Some(state))
        })
    }

    /// Transitions the task from `Running` -> `Idle`.
    ///
    /// The transition to `Idle` fails if the task has been flagged to be
    /// cancelled.
    pub(super) fn transition_to_idle(&self) -> TransitionToIdle {
        self.fetch_update(|mut state| {
            assert!(state.is_running());

            if state.is_cancelled() {
                return (TransitionToIdle::Cancelled, None);
            }

            let action;

            state.unset_running();

            if !state.is_notified() {
                state.ref_dec();

                if state.ref_count() == 0 {
                    action = TransitionToIdle::OkDealloc;
                } else {
                    action = TransitionToIdle::Ok;
                }
            } else {
                // The caller will schedule a new notification, so we create a
                // new ref-count for the notification. Our own ref-count is kept
                // for now, and the caller will drop it shortly.
                state.ref_inc();
                action = TransitionToIdle::OkNotified;
            }

            (action, Some(state))
        })
    }

    /// Transitions the task from `Running` -> `Complete`.
    pub(super) fn transition_to_complete(&self) -> Snapshot {
        const DELTA: usize = RUNNING | COMPLETE;

        let prev_state = Snapshot(self.0.fetch_xor(DELTA, AcqRel));

        assert!(prev_state.is_running());
        assert!(!prev_state.is_complete());

        Snapshot(prev_state.0 ^ DELTA)
    }

    /// Transitions the state to `NOTIFIED`.
    ///
    /// If no task needs to be submitted, a ref-count is consumed.
    ///
    /// If a task needs to be submitted, the ref-count is incremented for the new Notified.
    pub(super) fn transition_to_notified_by_val(&self) -> TransitionToNotified {
        self.fetch_update(|mut state| {
            let action;

            if state.is_running() {
                // If the task is running, we mark it as notified, but we should
                // not submit anything as the thread currently running the
                // future is responsible for that.
                state.set_notified();
                state.ref_dec();

                // The thread that set the running bit also holds a ref-count.
                assert!(state.ref_count() > 0);

                action = TransitionToNotified::DoNothing;
            } else if state.is_complete() || state.is_notified() {
                // We do not need to submit any notifications, but we have to
                // decrement the ref-count.
                state.ref_dec();

                if state.ref_count() == 0 {
                    action = TransitionToNotified::Dealloc;
                } else {
                    action = TransitionToNotified::DoNothing;
                }
            } else {
                // We create a new notified that we can submit. The caller
                // retains ownership of the ref-count they passed in.
                state.set_notified();
                state.ref_inc();
                action = TransitionToNotified::Submit;
            }

            (action, Some(state))
        })
    }

    /// Transitions the state to `NOTIFIED`.
    pub(super) fn transition_to_notified_by_ref(&self) -> TransitionToNotified {
        self.fetch_update(|mut state| {
            if state.is_complete() || state.is_notified() {
                // There is nothing to do in this case.
                (TransitionToNotified::DoNothing, None)
            } else if state.is_running() {
                // If the task is running, we mark it as notified, but we should
                // not submit as the thread currently running the future is
                // responsible for that.
                state.set_notified();
                (TransitionToNotified::DoNothing, Some(state))
            } else {
                // The task is idle and not notified. We should submit a
                // notification.
                state.set_notified();
                state.ref_inc();
                (TransitionToNotified::Submit, Some(state))
            }
        })
    }

    /// Optimistically tries to swap the state assuming the join handle is
    /// __immediately__ dropped on spawn.
    pub(super) fn drop_join_handle_fast(&self) -> Result<(), ()> {
        use std::sync::atomic::Ordering::Relaxed;

        // Relaxed is acceptable as if this function is called and succeeds,
        // then nothing has been done w/ the join handle.
        //
        // The moment the join handle is used (polled), the `JOIN_WAKER` flag is
        // set, at which point the CAS will fail.
        //
        // Given this, there is no risk if this operation is reordered.
        self.0
            .compare_exchange_weak(
                INITIAL_STATE,
                (INITIAL_STATE - REF_ONE) & !JOIN_INTEREST,
                Release,
                Relaxed,
            )
            .map(|_| ())
            .map_err(|_| ())
    }

    // Unsets the `JOIN_INTEREST` flag. If `COMPLETE` is not set, the `JOIN_WAKER`
    /// flag is also unset.
    /// The returned `TransitionToJoinHandleDrop` indicates whether the `JoinHandle` should drop
    /// the output of the future or the join waker after the transition.
    pub(super) fn transition_to_join_handle_dropped(&self) -> TransitionToJoinHandleDrop {
        self.fetch_update(|mut state| {
            assert!(state.is_join_interested());

            let mut transition = TransitionToJoinHandleDrop {
                drop_waker: false,
                drop_output: false,
            };

            state.unset_join_interested();

            if !state.is_complete() {
                // If `COMPLETE` is unset we also unset `JOIN_WAKER` to give the
                // `JoinHandle` exclusive access to the waker following rule 6 in task/mod.rs.
                // The `JoinHandle` will drop the waker if it has exclusive access
                // to drop it.
                state.unset_join_waker();
            } else {
                // If `COMPLETE` is set the task is completed so the `JoinHandle` is responsible
                // for dropping the output.
                transition.drop_output = true;
            }

            if !state.is_join_waker_set() {
                // If the `JOIN_WAKER` bit is unset and the `JOIN_HANDLE` has exclusive access to
                // the join waker and should drop it following this transition.
                // This might happen in two situations:
                //  1. The task is not completed and we just unset the `JOIN_WAKer` above in this
                //     function.
                //  2. The task is completed. In that case the `JOIN_WAKER` bit was already unset
                //     by the runtime during completion.
                transition.drop_waker = true;
            }

            (transition, Some(state))
        })
    }

    /// Sets the `JOIN_WAKER` bit.
    ///
    /// Returns `Ok` if the bit is set, `Err` otherwise. This operation fails if
    /// the task has completed.
    pub(super) fn set_join_waker(&self) -> Result<Snapshot, Snapshot> {
        let res = self.0.fetch_update(AcqRel, Acquire, |curr| {
            let state = Snapshot(curr);

            assert!(state.is_join_interested());
            assert!(!state.is_join_waker_set());

            if state.is_complete() {
                return None;
            }

            let mut next = state;
            next.set_join_waker();

            Some(next.0)
        });

        match res {
            Err(val) => Err(Snapshot(val)),
            Ok(val) => Ok(Snapshot(val)),
        }
    }

    /// Unsets the `JOIN_WAKER` bit.
    ///
    /// Returns `Ok` has been unset, `Err` otherwise. This operation fails if
    /// the task has completed.
    pub(super) fn unset_waker(&self) -> Result<Snapshot, Snapshot> {
        let res = self.0.fetch_update(AcqRel, Acquire, |curr| {
            let state = Snapshot(curr);

            assert!(state.is_join_interested());

            if state.is_complete() {
                return None;
            }

            // If the task is completed, this bit may have been unset by
            // `unset_waker_after_complete`.
            assert!(state.is_join_waker_set());

            let mut next = state;
            next.unset_join_waker();

            Some(next.0)
        });

        match res {
            Err(val) => Err(Snapshot(val)),
            Ok(val) => Ok(Snapshot(val)),
        }
    }

    /// Unsets the `JOIN_WAKER` bit unconditionally after task completion.
    ///
    /// This operation requires the task to be completed.
    pub(super) fn unset_waker_after_complete(&self) -> Snapshot {
        let prev = Snapshot(self.0.fetch_and(!JOIN_WAKER, AcqRel));
        assert!(prev.is_complete());
        assert!(prev.is_join_waker_set());
        Snapshot(prev.0 & !JOIN_WAKER)
    }

    fn fetch_update<F, T>(&self, mut f: F) -> T
    where
        F: FnMut(Snapshot) -> (T, Option<Snapshot>),
    {
        let mut curr = self.get_snapshot();

        loop {
            let (output, next) = f(curr);
            let next = match next {
                Some(next) => next,
                None => return output,
            };

            let res = self.0.compare_exchange(curr.0, next.0, AcqRel, Acquire);

            match res {
                Ok(_) => return output,
                Err(actual) => curr = Snapshot(actual),
            }
        }
    }

    pub(super) fn ref_inc(&self) {
        use std::{process, sync::atomic::Ordering::Relaxed};

        // Using a relaxed ordering is alright here, as knowledge of the
        // original reference prevents other threads from erroneously deleting
        // the object.
        //
        // As explained in the [Boost documentation][1], Increasing the
        // reference counter can always be done with memory_order_relaxed: New
        // references to an object can only be formed from an existing
        // reference, and passing an existing reference from one thread to
        // another must already provide any required synchronization.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        let prev = self.0.fetch_add(REF_ONE, Relaxed);

        // If the reference count overflowed, abort.
        if prev > isize::MAX as usize {
            process::abort();
        }
    }

    /// Returns `true` if the task should be released.
    pub(super) fn ref_dec(&self) -> bool {
        let prev = Snapshot(self.0.fetch_sub(REF_ONE, AcqRel));
        assert!(prev.ref_count() >= 1);
        prev.ref_count() == 1
    }
}

impl Snapshot {
    /// Returns `true` if the task is in an idle state.
    pub(super) fn is_idle(&self) -> bool {
        self.0 & (RUNNING | COMPLETE) == 0
    }

    /// Returns `true` if the task has been flagged as notified.
    pub(super) fn is_notified(&self) -> bool {
        self.0 & NOTIFIED == NOTIFIED
    }

    pub(super) fn unset_notified(&mut self) {
        self.0 &= !NOTIFIED
    }

    pub(super) fn set_notified(&mut self) {
        self.0 |= NOTIFIED
    }

    pub(super) fn is_running(&self) -> bool {
        self.0 & RUNNING == RUNNING
    }

    pub(super) fn set_running(&mut self) {
        self.0 |= RUNNING;
    }

    pub(super) fn unset_running(&mut self) {
        self.0 &= !RUNNING;
    }

    pub(super) fn is_cancelled(self) -> bool {
        self.0 & CANCELLED == CANCELLED
    }

    /// Returns `true` if the task's future has completed execution.
    pub(super) fn is_complete(&self) -> bool {
        self.0 & COMPLETE == COMPLETE
    }

    pub(super) fn is_join_interested(&self) -> bool {
        self.0 & JOIN_INTEREST == JOIN_INTEREST
    }

    pub(super) fn unset_join_interested(&mut self) {
        self.0 &= !JOIN_INTEREST
    }

    pub(super) fn is_join_waker_set(self) -> bool {
        self.0 & JOIN_WAKER == JOIN_WAKER
    }

    fn set_join_waker(&mut self) {
        self.0 |= JOIN_WAKER;
    }

    fn unset_join_waker(&mut self) {
        self.0 &= !JOIN_WAKER;
    }

    pub(super) fn ref_count(self) -> usize {
        (self.0 & REF_COUNT_MASK) >> REF_COUNT_SHIFT
    }

    fn ref_inc(&mut self) {
        assert!(self.0 <= isize::MAX as usize);
        self.0 += REF_ONE;
    }

    pub(super) fn ref_dec(&mut self) {
        assert!(self.ref_count() > 0);
        self.0 -= REF_ONE;
    }
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Snapshot")
            .field("is_running", &self.is_running())
            .field("is_complete", &self.is_complete())
            .field("is_notified", &self.is_notified())
            .field("is_cancelled", &self.is_cancelled())
            .field("is_join_interested", &self.is_join_interested())
            .field("is_join_waker_set", &self.is_join_waker_set())
            .field("ref_count", &self.ref_count())
            .finish()
    }
}
