use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll, Waker},
    time::Instant,
};

use crate::runtime::local::CURRENT_TIMER;

/// Key identifying a registered timer entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TimerKey {
    deadline: Instant,
    key: u64,
    _local_marker: PhantomData<*const ()>,
}

/// Timer wheel state for the current runtime thread.
pub(crate) struct Timer {
    key: u64,
    wheel: std::collections::BTreeMap<TimerKey, Waker>,
}

impl Timer {
    pub(crate) fn new() -> Self {
        Self {
            key: 0,
            wheel: std::collections::BTreeMap::default(),
        }
    }

    pub(crate) fn is_completed(&self, key: &TimerKey) -> bool {
        !self.wheel.contains_key(key)
    }

    /// Insert a new timer. Returns `None` if the deadline is already in the past.
    pub(crate) fn insert(&mut self, deadline: Instant) -> Option<TimerKey> {
        if deadline <= Instant::now() {
            return None;
        }

        let key = TimerKey {
            deadline,
            key: self.key,
            _local_marker: PhantomData,
        };
        self.wheel.insert(key, Waker::noop().clone());
        self.key += 1;

        Some(key)
    }

    pub(crate) fn update_waker(&mut self, key: &TimerKey, waker: &Waker) {
        if let Some(w) = self.wheel.get_mut(key)
            && !waker.will_wake(w)
        {
            *w = waker.clone();
        }
    }

    pub(crate) fn cancel(&mut self, key: &TimerKey) {
        self.wheel.remove(key);
    }

    /// Returns the duration until the next timer expires, if any.
    pub(crate) fn min_timeout(&self) -> Option<std::time::Duration> {
        self.wheel.first_key_value().map(|(key, _)| {
            let now = Instant::now();
            key.deadline.saturating_duration_since(now)
        })
    }

    /// Wake all timers that have reached their deadline.
    pub(crate) fn wake(&mut self) {
        let now = Instant::now();

        // Pop expired entries from the front of the sorted BTreeMap one at a time.
        // This avoids the allocation that `split_off` + `replace` would require on every tick.
        // For small-to-moderate timer counts (typical in a thread-per-core runtime) this is efficient;
        // the BTreeMap's `pop_first` is O(log n) per entry but k (expired count) is usually small relative to n (total timers).
        loop {
            let expired = self.wheel.keys().next().is_some_and(|key| key.deadline <= now);
            if !expired {
                break;
            }

            if let Some((_, waker)) = self.wheel.pop_first() {
                waker.wake();
            }
        }
    }

    pub(crate) fn poll_timer(&mut self, cx: &mut Context<'_>, key: &TimerKey) -> Poll<()> {
        if self.is_completed(key) {
            Poll::Ready(())
        } else {
            self.update_waker(key, cx.waker());
            Poll::Pending
        }
    }
}

pub(crate) fn with_timer<F, R>(f: F) -> R
where
    F: FnOnce(&mut Timer) -> R,
{
    CURRENT_TIMER.with(|timer| f(&mut timer.borrow_mut()))
}

fn with_timer_or_panic<F, R>(f: F) -> R
where
    F: FnOnce(&mut Timer) -> R,
{
    assert!(
        CURRENT_TIMER.is_set(),
        "time utilities must be used from within a karmaio runtime"
    );
    with_timer(f)
}

/// Internal future used by [`super::sleep_until`].
///
/// Timer futures are tied to the current runtime thread and are neither `Send`
/// nor `Sync`.
pub(crate) struct TimerFuture {
    key: TimerKey,
    _local: PhantomData<*const ()>,
}

impl TimerFuture {
    pub(crate) fn new(key: TimerKey) -> Self {
        Self {
            key,
            _local: PhantomData,
        }
    }
}

impl Future for TimerFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        with_timer_or_panic(|timer| timer.poll_timer(cx, &self.key))
    }
}

impl Drop for TimerFuture {
    fn drop(&mut self) {
        if CURRENT_TIMER.is_set() {
            with_timer(|timer| timer.cancel(&self.key));
        }
    }
}

pub(crate) async fn create_timer(deadline: Instant) {
    let key = with_timer_or_panic(|timer| timer.insert(deadline));
    if let Some(key) = key {
        TimerFuture::new(key).await;
    }
}

macro_rules! assert_not_impl {
    ($x:ty, $($t:path),+ $(,)*) => {
        const _: fn() -> () = || {
            struct Check<T: ?Sized>(T);
            trait AmbiguousIfImpl<A> {
                fn some_item() {}
            }
            impl<T: ?Sized> AmbiguousIfImpl<()> for Check<T> {}
            impl<T: ?Sized $(+ $t)*> AmbiguousIfImpl<u8> for Check<T> {}

            <Check::<$x> as AmbiguousIfImpl<_>>::some_item()
        };
    };
}

assert_not_impl!(TimerFuture, Send);
assert_not_impl!(TimerFuture, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_timeout_returns_nearest_deadline() {
        let mut runtime = Timer::new();
        assert_eq!(runtime.min_timeout(), None);

        let now = Instant::now();
        runtime.insert(now + std::time::Duration::from_secs(1));
        runtime.insert(now + std::time::Duration::from_secs(10));

        let min_timeout = runtime.min_timeout().unwrap().as_secs_f32();
        assert!(min_timeout < 1.);
    }
}
