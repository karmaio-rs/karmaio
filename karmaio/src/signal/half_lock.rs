// Adapted from signal-hook's `half_lock` structure.
// Original: https://github.com/vorner/signal-hook/blob/7c8c5199ffe9939f567066a5c11c2a660f7309b8/signal-hook-registry/src/half_lock.rs
// License: MIT OR Apache-2.0

//! The half-lock structure.
//!
//! We need a way to protect the structure holding the configured signal
//! listeners: a signal may be delivered on an arbitrary thread and needs to
//! read that structure while another thread might be mutating it.
//!
//! Under ordinary circumstances a `Mutex<..>` would suffice. However, we read
//! it from inside a signal handler, where we are severely limited in what we
//! may safely do (no allocation, no locking a `Mutex` the interrupted thread
//! may already hold, etc.). So we implement a spin-lock-like structure using
//! only atomics.
//!
//! The reader simply locks and then unlocks, making sure the data doesn't
//! disappear while in use. The writer has a separate `Mutex` (used outside the
//! signal handler, so only one writer runs at a time), makes a copy of the
//! data, and swaps an atomic pointer. It then spins until every reader has
//! released the old data before dropping it. A generation counter ensures new
//! readers lock a different slot so the writer can observe the old one drain.
//!
//! This trades a spinning writer for an async-signal-safe reader, which is a
//! reasonable deal because signals are rare and short.

use std::{
    hint,
    marker::PhantomData,
    ops::Deref,
    process::abort,
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicPtr, AtomicUsize, Ordering},
    },
    thread,
};

const YIELD_EVERY: usize = 16;
const MAX_GUARDS: usize = (isize::MAX) as usize;

/// Guard returned by [`HalfLock::read`]; keeps the data alive while borrowed.
pub(crate) struct ReadGuard<'a, T: 'a> {
    data: &'a T,
    lock: &'a AtomicUsize,
}

impl<'a, T> Deref for ReadGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.data
    }
}

impl<'a, T> Drop for ReadGuard<'a, T> {
    fn drop(&mut self) {
        // We effectively unlock; `Release` would be enough.
        self.lock.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Guard returned by [`HalfLock::write`]; only one exists at a time.
pub(crate) struct WriteGuard<'a, T: 'a> {
    _guard: MutexGuard<'a, ()>,
    lock: &'a HalfLock<T>,
    data: &'a T,
}

impl<'a, T> WriteGuard<'a, T> {
    /// Publish a new value, waiting until no reader holds the previous one
    /// before dropping it.
    pub(crate) fn store(&mut self, val: T) {
        // Move to the heap and convert to a raw pointer for the `AtomicPtr`.
        let new = Box::into_raw(Box::new(val));

        self.data = unsafe { &*new };

        // Swap the new value in; we only need to worry about dropping the old
        // one once no reader can observe it any more.
        let old = self.lock.data.swap(new, Ordering::SeqCst);

        // Make sure no reader still holds the old data.
        self.lock.write_barrier();

        drop(unsafe { Box::from_raw(old) });
    }
}

impl<'a, T> Deref for WriteGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // Protected by the writer mutex.
        self.data
    }
}

/// A lock with an async-signal-safe read path and a spinning write path.
pub(crate) struct HalfLock<T> {
    // Conceptually we contain an instance of `T`.
    _t: PhantomData<T>,
    // The actual data, as a heap pointer.
    data: AtomicPtr<T>,
    // The generation of the data; selects which slot of the lock counter to use.
    generation: AtomicUsize,
    // How many active read locks are there, per generation slot?
    lock: [AtomicUsize; 2],
    // Serializes writers; only one writer at a time.
    write_mutex: Mutex<()>,
}

impl<T: Default> Default for HalfLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> HalfLock<T> {
    pub(crate) fn new(data: T) -> Self {
        // Move to the heap so we can safely point there, then hand the pointer
        // to the `AtomicPtr`, which acts like a `Box` for us semantically.
        let ptr = Box::into_raw(Box::new(data));
        Self {
            _t: PhantomData,
            data: AtomicPtr::new(ptr),
            generation: AtomicUsize::new(0),
            lock: [AtomicUsize::new(0), AtomicUsize::new(0)],
            write_mutex: Mutex::new(()),
        }
    }

    /// Acquire a read guard. Safe to call from a signal handler.
    pub(crate) fn read(&self) -> ReadGuard<'_, T> {
        let generation = self.generation.load(Ordering::SeqCst);
        let lock = &self.lock[generation % 2];
        let guard_cnt = lock.fetch_add(1, Ordering::SeqCst);

        // Guard against overflowing the counter in degenerate cases, which
        // could lead to freeing data while still in use. This is practically
        // impossible to hit, but we keep it out of caution.
        if guard_cnt > MAX_GUARDS {
            abort()
        }

        let data = self.data.load(Ordering::SeqCst);
        // Safe:
        // * It pointed to valid data when stored.
        // * It is protected by the lock counter, so it is still valid.
        let data = unsafe { &*data };

        ReadGuard { data, lock }
    }

    fn update_seen(&self, seen_zero: &mut [bool; 2]) {
        for (seen, slot) in seen_zero.iter_mut().zip(&self.lock) {
            *seen = *seen || slot.load(Ordering::SeqCst) == 0;
        }
    }

    fn write_barrier(&self) {
        // Check for zeroes before switching the generation. At least one slot
        // should be zero by now, since we drained it in the previous writer.
        let mut seen_zero = [false; 2];
        self.update_seen(&mut seen_zero);
        // Switch the active slot so the current one starts draining while the
        // other starts filling.
        self.generation.fetch_add(1, Ordering::SeqCst); // Overflow is fine.

        let mut iter = 0usize;
        while !seen_zero.iter().all(|s| *s) {
            iter = iter.wrapping_add(1);

            // Be less aggressive while spinning; yield to other threads.
            if cfg!(not(miri)) {
                if iter.is_multiple_of(YIELD_EVERY) {
                    thread::yield_now();
                } else {
                    hint::spin_loop();
                }
            }

            self.update_seen(&mut seen_zero);
        }
    }

    /// Acquire the (mutually exclusive) write guard. Must not be called from a
    /// signal handler.
    pub(crate) fn write(&self) -> WriteGuard<'_, T> {
        // Our own code in `store` doesn't panic and swaps atomically, so a
        // poisoned mutex carries no broken invariants; recover from it.
        let guard = self.write_mutex.lock().unwrap_or_else(PoisonError::into_inner);

        let data = self.data.load(Ordering::SeqCst);
        // Safe:
        // * Stored as valid data.
        // * Only this method (under the mutex) changes the pointer, so it is
        //   still valid.
        let data = unsafe { &*data };

        WriteGuard {
            data,
            _guard: guard,
            lock: self,
        }
    }
}

impl<T> Drop for HalfLock<T> {
    fn drop(&mut self) {
        // During drop there are no other borrows, so we simply take the last
        // instance out. In practice this is only used as a global and won't be
        // dropped, but we provide it for completeness.
        //
        // Safe: the pointer is always valid; we take the last instance out.
        unsafe {
            let data = Box::from_raw(self.data.load(Ordering::SeqCst));
            drop(data);
        }
    }
}
