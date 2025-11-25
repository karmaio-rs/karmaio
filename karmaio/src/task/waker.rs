use std::{
    marker::PhantomData,
    mem::ManuallyDrop,
    ops,
    ptr::NonNull,
    task::{RawWaker, RawWakerVTable, Waker},
};

use crate::{
    runtime::Schedule,
    task::{header::Header, raw::RawTask},
};

pub(super) struct WakerRef<'a, S: 'static> {
    waker: ManuallyDrop<Waker>,
    _p: PhantomData<(&'a Header, S)>,
}

/// Returns a `WakerRef` which avoids having to preemptively increase the
/// refcount if there is no need to do so.
pub(super) fn waker_ref<S>(header: &NonNull<Header>) -> WakerRef<'_, S>
where
    S: Schedule,
{
    // `Waker::will_wake` uses the VTABLE pointer as part of the check. This
    // means that `will_wake` will always return false when using the current
    // task's waker. (discussion at rust-lang/rust#66281).
    //
    // To fix this, we use a single vtable. Since we pass in a reference at this
    // point and not an *owned* waker, we must ensure that `drop` is never
    // called on this waker instance. This is done by wrapping it with
    // `ManuallyDrop` and then never calling drop.
    let waker = unsafe { ManuallyDrop::new(Waker::from_raw(raw_waker(*header))) };

    WakerRef { waker, _p: PhantomData }
}

impl<S> ops::Deref for WakerRef<'_, S> {
    type Target = Waker;

    fn deref(&self) -> &Waker {
        &self.waker
    }
}

fn clone_waker(ptr: *const ()) -> RawWaker {
    let header = unsafe { NonNull::new_unchecked(ptr as *mut Header) };
    unsafe { header.as_ref().state.ref_inc() };
    raw_waker(header)
}

fn drop_waker(ptr: *const ()) {
    let ptr = unsafe { NonNull::new_unchecked(ptr as *mut Header) };
    let raw = RawTask::from_raw(ptr);
    raw.drop_reference();
}

fn wake_by_val(ptr: *const ()) {
    let ptr = unsafe { NonNull::new_unchecked(ptr as *mut Header) };
    let raw = RawTask::from_raw(ptr);
    raw.wake_by_val();
}

// Wake without consuming the waker
fn wake_by_ref(ptr: *const ()) {
    let ptr = unsafe { NonNull::new_unchecked(ptr as *mut Header) };
    let raw = RawTask::from_raw(ptr);
    raw.wake_by_ref();
}

static WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(clone_waker, wake_by_val, wake_by_ref, drop_waker);

fn raw_waker(header: NonNull<Header>) -> RawWaker {
    let ptr = header.as_ptr() as *const ();
    RawWaker::new(ptr, &WAKER_VTABLE)
}

// Creates a waker that does nothing.
//
// This `Waker` is useful for polling a `Future` to check whether it is
// `Ready`, without doing any additional work.
pub(crate) fn dummy_waker() -> Waker {
    fn dummy_raw_waker() -> RawWaker {
        // the pointer is never dereferenced, so null is ok
        RawWaker::new(std::ptr::null::<()>(), vtable())
    }

    fn vtable() -> &'static RawWakerVTable {
        &RawWakerVTable::new(|_| dummy_raw_waker(), |_| {}, |_| {}, |_| {})
    }

    unsafe { Waker::from_raw(dummy_raw_waker()) }
}
