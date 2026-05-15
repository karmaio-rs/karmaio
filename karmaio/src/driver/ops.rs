use std::{
    pin::Pin,
    task::{Context, Poll, Waker},
};

use crate::driver::{Handle, Submission};

pub(crate) enum State {
    // The operation has been submitted to the driver and is currently in-flight
    Submitted,

    // The submitter is waiting for the completion of the operation
    Waiting(Waker),

    // Used in poll based systems, signifies that the op is ready and can resume the syscall
    Ready,

    // The submitter no longer has interest in the operation result.
    // The state must be passed to the driver and held until the operation completes.
    Ignored(Box<dyn std::any::Any>),

    // The operation has completed with a single cqe result
    Completed(Completion),
}

pub(crate) struct Op<T> {
    driver: Handle,
    index: usize,
    data: Option<T>,
}

pub(crate) struct Completion {
    pub(crate) result: std::io::Result<u32>,
    pub(crate) flags: u32,
}

pub(crate) trait Submittable {
    // Build a backend-specific submission entry.
    fn submit(&mut self) -> Submission;
}

pub(crate) trait Completable {
    type Result;

    // `complete` will be called for every op once done
    fn complete(self, completion_entry: Completion) -> Self::Result;
}

pub(crate) trait Operable: Submittable + Completable {}

impl<T> Op<T> {
    pub(crate) fn new(index: usize, data: T, driver: Handle) -> Self {
        Self {
            driver,
            index,
            data: Option::Some(data),
        }
    }

    pub(super) fn index(&self) -> usize {
        self.index
    }

    pub(super) fn take_data(&mut self) -> Option<T> {
        self.data.take()
    }

    pub(super) fn data_mut(&mut self) -> Option<&mut T> {
        self.data.as_mut()
    }
}

impl<T: Unpin + Operable> Future for Op<T> {
    type Output = T::Result;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.driver
            .upgrade()
            .expect("Not in runtime context")
            .poll_op(self.get_mut(), cx)
    }
}

impl<T> Drop for Op<T> {
    fn drop(&mut self) {
        self.driver.upgrade().expect("Not in runtime context").remove_op(self);
    }
}

impl State {
    // Processes the completion for the state and it's associated op
    // Returns whether to keep the op or drop it
    pub(crate) fn complete(&mut self, completion: Completion) -> bool {
        match self {
            State::Submitted => {
                *self = State::Completed(completion);
                // The completion still has to be read, so don't drop
                false
            }
            State::Waiting(_) => {
                let old = std::mem::replace(self, State::Completed(completion));
                match old {
                    // waker is woken to notify the caller to process the result
                    State::Waiting(waker) => {
                        waker.wake();
                    }
                    _ => unreachable!("invalid operation state"),
                }
                // The completion still has to be read, so don't drop
                false
            }
            State::Ignored(..) => {
                // The caller isn't interested in the result, so we drop
                true
            }
            State::Ready => {
                // Calling readinies state via completion call is a no-op
                // This should not be triggered in normal operation
                unreachable!("invalid operation state");
            }
            State::Completed(..) => {
                // Calling complete on an already completed state is a no-op
                // This should not be triggered in normal operation
                unreachable!("invalid operation state");
            }
        }
    }

    // Processes a readiness notification for the state and its associated op.
    // Returns whether to keep the op or drop it.
    pub(crate) fn ready(&mut self) -> bool {
        match self {
            State::Submitted => {
                *self = State::Ready;
                false
            }
            State::Waiting(_) => {
                let old = std::mem::replace(self, State::Ready);
                match old {
                    State::Waiting(waker) => {
                        waker.wake();
                    }
                    _ => unreachable!("invalid operation state"),
                }
                false
            }
            State::Ignored(..) => true,
            State::Ready => false,
            State::Completed(..) => unreachable!("invalid operation state"),
        }
    }
}
