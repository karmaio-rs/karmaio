use std::{
    error::Error,
    fmt::{self, Display},
    future::Future,
    pin::pin,
    task::Poll,
    time::{Duration, Instant},
};

use super::sleep::{sleep, sleep_until};

/// Error returned by [`timeout`] and [`timeout_at`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed(());

impl Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("deadline has elapsed")
    }
}

impl Error for Elapsed {}

/// Require a [`Future`] to complete before the specified duration has elapsed.
///
/// If the future completes before the duration has elapsed, then the completed
/// value is returned. Otherwise, an error is returned and the future is
/// cancelled.
pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
    let mut future = pin!(future);
    let mut delay = pin!(sleep(duration));

    std::future::poll_fn(|cx| {
        if let Poll::Ready(output) = future.as_mut().poll(cx) {
            return Poll::Ready(Ok(output));
        }

        if delay.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(Elapsed(())));
        }

        Poll::Pending
    })
    .await
}

/// Require a [`Future`] to complete before the specified instant in time.
///
/// If the future completes before the instant is reached, then the completed
/// value is returned. Otherwise, an error is returned and the future is
/// cancelled.
///
/// If `deadline` is in the past, the future is still polled once before  returning [`Elapsed`],
/// giving it a chance to complete synchronously.
pub async fn timeout_at<F: Future>(deadline: Instant, future: F) -> Result<F::Output, Elapsed> {
    let mut future = pin!(future);
    let mut delay = pin!(sleep_until(deadline));

    std::future::poll_fn(|cx| {
        if let Poll::Ready(output) = future.as_mut().poll(cx) {
            return Poll::Ready(Ok(output));
        }

        if delay.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(Elapsed(())));
        }

        Poll::Pending
    })
    .await
}
