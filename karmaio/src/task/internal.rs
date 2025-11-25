use std::{
    cell::UnsafeCell,
    pin::Pin,
    task::{Context, Poll},
};

use crate::{
    runtime::Schedule,
    task::{state::State, vtable::VTable},
};

use super::{header::Header, trailer::Trailer, utils::UnsafeCellExt};

pub(crate) enum Stage<F: Future> {
    Running(F),
    Finished(F::Output),
    Consumed,
}

#[repr(C)]
pub(super) struct InternalTask<F: Future, S: Schedule> {
    pub(super) header: Header,
    pub(super) scheduler: S,
    pub(super) future: UnsafeCell<Stage<F>>,
    pub(super) trailer: Trailer,
}

impl<F: Future, S: Schedule> InternalTask<F, S> {
    #[must_use]
    pub(super) fn new(future: F, scheduler: S) -> Box<Self> {
        Box::new(Self {
            header: Header {
                state: State::new(),
                vtable: VTable::new::<F, S>(),
            },
            scheduler,
            future: UnsafeCell::new(Stage::Running(future)),
            trailer: Trailer {
                waker: UnsafeCell::new(Option::None),
            },
        })
    }

    // Functions related to managing the future
    pub(crate) fn stage_with_mut<R>(&self, f: impl FnOnce(*mut Stage<F>) -> R) -> R {
        self.future.with_mut(f)
    }

    fn set_stage(&self, stage: Stage<F>) {
        unsafe { self.stage_with_mut(|ptr| *ptr = stage) }
    }

    pub(crate) fn poll(&self, mut cx: Context<'_>) -> Poll<F::Output> {
        let res = {
            self.stage_with_mut(|ptr| {
                let future = match unsafe { &mut *ptr } {
                    Stage::Running(future) => future,
                    _ => unreachable!("error: unexpected stage"),
                };

                let future = unsafe { Pin::new_unchecked(future) };

                future.poll(&mut cx)
            })
        };

        if res.is_ready() {
            self.drop_future_or_output();
        }

        res
    }

    /// Drop the future
    ///
    /// # Safety
    ///
    /// The caller must ensure it is safe to mutate the `stage` field.
    pub(crate) fn drop_future_or_output(&self) {
        // Safety: the caller ensures mutual exclusion to the field.
        self.set_stage(Stage::Consumed);
    }

    /// Store the task output
    ///
    /// # Safety
    ///
    /// The caller must ensure it is safe to mutate the `stage` field.
    pub(crate) fn store_output(&self, output: F::Output) {
        // Safety: the caller ensures mutual exclusion to the field.
        self.set_stage(Stage::Finished(output));
    }

    /// Take the task output
    ///
    /// # Safety
    ///
    /// The caller must ensure it is safe to mutate the `stage` field.
    pub(crate) fn take_output(&self) -> F::Output {
        use std::mem;

        self.stage_with_mut(|ptr| {
            // Safety:: the caller ensures mutual exclusion to the field.
            match mem::replace(unsafe { &mut *ptr }, Stage::Consumed) {
                Stage::Finished(output) => output,
                _ => panic!("error: JoinHandle polled after completion"),
            }
        })
    }
}
