use std::{cell::UnsafeCell, task::Waker};

use super::utils::UnsafeCellExt;

pub(crate) struct Trailer {
    pub(crate) waker: UnsafeCell<Option<Waker>>,
}

impl Trailer {
    pub(crate) fn set_waker(&self, waker: Option<Waker>) {
        unsafe {
            self.waker.with_mut(|ptr| {
                *ptr = waker;
            });
        }
    }

    pub(crate) fn will_wake(&self, waker: &Waker) -> bool {
        unsafe { self.waker.with(|ptr| (*ptr).as_ref().unwrap().will_wake(waker)) }
    }

    pub(crate) fn wake_join(&self) {
        self.waker.with(|ptr| match unsafe { &*ptr } {
            Some(waker) => waker.wake_by_ref(),
            None => panic!("waker missing"),
        });
    }
}
