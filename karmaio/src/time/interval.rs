use std::time::{Duration, Instant};

use super::sleep::sleep_until;

/// Interval returned by [`interval`] and [`interval_at`].
///
/// This type allows you to wait on a sequence of instants with a certain
/// duration between each instant. Unlike calling [`super::sleep`] in a loop,
/// this lets you count the time spent between the calls to [`super::sleep`] as
/// well.
///
/// Interval timers are tied to the current runtime thread and must be used from
/// within a [`Runtime`](crate::runtime::local::Runtime).
#[derive(Debug)]
pub struct Interval {
    first_ticked: bool,
    start: Instant,
    period: Duration,
}

impl Interval {
    pub(crate) fn new(start: Instant, period: Duration) -> Self {
        Self {
            first_ticked: false,
            start,
            period,
        }
    }

    /// Resets the interval to the provided `start` instant.
    ///
    /// The next call to [`tick`](Self::tick) will wait until `start` is reached,
    /// then subsequent ticks will fire every `period` after that.
    ///
    /// If `start` is not provided, it defaults to [`Instant::now()`],
    /// meaning the next tick fires after one `period` from now.
    pub fn reset(&mut self, start: Option<Instant>) {
        self.start = start.unwrap_or_else(Instant::now);
        self.first_ticked = false;
    }

    /// Completes when the next instant in the interval has been reached.
    pub async fn tick(&mut self) -> Instant {
        if !self.first_ticked {
            sleep_until(self.start).await;
            self.first_ticked = true;
            self.start
        } else {
            let now = Instant::now();
            let elapsed_ns = (now - self.start).as_nanos();
            let period_ns = self.period.as_nanos();
            // Both values are capped by the std::time::Instant bounds, so the difference fits in u128.
            // The modulo result is always < period_ns which fits in u64 for any practical duration (< ~584 years).
            let drift_ns = (elapsed_ns % period_ns) as u64;
            let next = now + self.period - Duration::from_nanos(drift_ns);
            sleep_until(next).await;
            next
        }
    }
}

/// Creates new [`Interval`] that yields with interval of `period`. The first
/// tick completes immediately.
///
/// This function is equivalent to [`interval_at(Instant::now(), period)`](interval_at).
///
/// # Panics
///
/// This function panics if `period` is zero.
pub fn interval(period: Duration) -> Interval {
    interval_at(Instant::now(), period)
}

/// Creates new [`Interval`] that yields with interval of `period` with the
/// first tick completing at `start`.
///
/// # Panics
///
/// This function panics if `period` is zero.
pub fn interval_at(start: Instant, period: Duration) -> Interval {
    assert!(period > Duration::ZERO, "`period` must be non-zero.");
    Interval::new(start, period)
}
