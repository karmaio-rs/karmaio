use std::{
    pin::Pin,
    task::{Context, Poll, Waker},
};

pub(crate) enum State {
    // The operation has been submitted to the driver and is currently in-flight
    Submitted,

    // The submitter is waiting for the completion of the operation
    Waiting(Waker),

    // The submitter no longer has interest in the operation result.
    // The state must be passed to the driver and held until the operation completes.
    Ignored(Box<dyn std::any::Any>),

    // The operation has completed with a single cqe result
    Completed(usize),
}

pub(crate) struct Op<T> {
    //TODO: add a weak handle or something similar here so that the driver can handle the business logic
    // driver: Handle
    index: usize,
    state: State,
    data: Option<T>,
}

pub(crate) struct CompletionResult {
    pub(crate) result: std::io::Result<u32>,
    pub(crate) flags: u32,
}

pub(crate) trait Completable {
    type Output;
    // `complete` will be called for every op once done
    fn complete(self, cqe: CompletionResult) -> Self::Output;
}

impl<T> Op<T> {
    pub(crate) fn new(index: usize, data: T) -> Self {
        Self {
            index,
            data: Option::Some(data),
            state: State::Submitted,
        }
    }

    pub(super) fn index(&self) -> usize {
        self.index
    }

    pub(super) fn take_data(&mut self) -> Option<T> {
        self.data.take()
    }
}

impl<T: Completable> Future for Op<T> {
    type Output = T::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        //TODO: Write the polling logic here
    }
}

impl<T> Drop for Op<T> {
    fn drop(&mut self) {
        //TODO: Implement drop cleanup logic here
    }
}

impl State {
    pub(crate) fn complete(&mut self) {
        //TODO: Implement completion state machine logic here using a matcher on self
    }
}
