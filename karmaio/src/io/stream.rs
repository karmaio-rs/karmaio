//! Pure-async stream traits for karmaio.
//!
//! These are intentionally independent of framed I/O so any producer of async
//! sequences can implement them (network framed readers, channels, generators, …).

use crate::runtime::{CancellationToken, FutureExt, WithCancellation};

/// A stream of values produced asynchronously in pure async/await.
///
/// Futures are `!Send` by design in this share-nothing runtime.
#[allow(async_fn_in_trait)]
pub trait Stream {
    /// Values yielded by the stream.
    type Item;

    /// Pull the next value from the stream.
    ///
    /// Returns `None` when the stream is exhausted.
    async fn next(&mut self) -> Option<Self::Item>;

    /// Returns bounds on the remaining length of the stream, if known.
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl<S: ?Sized + Stream> Stream for &mut S {
    type Item = S::Item;

    #[inline]
    async fn next(&mut self) -> Option<Self::Item> {
        (**self).next().await
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (**self).size_hint()
    }
}

/// Extension methods for [`Stream`].
///
/// Futures are `!Send` by design in this share-nothing runtime.
#[allow(async_fn_in_trait)]
pub trait StreamExt: Stream {
    /// Register each karmaio I/O submission made by `next()` with `token`.
    ///
    /// Wrap a lazily submitted I/O stream before its first `next()` call.
    /// Wrapping an already-submitted stream does not attach its existing
    /// operation retroactively. The scope does not propagate into tasks
    /// spawned while `next()` is being polled; pass those tasks a token and
    /// wrap their karmaio I/O explicitly.
    fn with_cancellation(self, token: CancellationToken) -> WithCancellation<Self>
    where
        Self: Sized,
    {
        WithCancellation::new(self, token)
    }

    /// Counts the remaining items, consuming the stream.
    async fn count(mut self) -> usize
    where
        Self: Sized,
    {
        let mut n = 0usize;
        while self.next().await.is_some() {
            n += 1;
        }
        n
    }

    /// Returns the `n`th item (0-based), consuming preceding items.
    async fn nth(&mut self, mut n: usize) -> Option<Self::Item> {
        while n > 0 {
            self.next().await?;
            n -= 1;
        }
        self.next().await
    }

    /// Collects all remaining items into a collection.
    async fn collect<C>(mut self) -> C
    where
        Self: Sized,
        C: Default + Extend<Self::Item>,
    {
        let mut c = C::default();
        while let Some(item) = self.next().await {
            c.extend(core::iter::once(item));
        }
        c
    }

    /// Calls `f` for each item until the stream is exhausted.
    async fn for_each<F>(mut self, mut f: F)
    where
        Self: Sized,
        F: FnMut(Self::Item),
    {
        while let Some(item) = self.next().await {
            f(item);
        }
    }
}

impl<S: Stream + ?Sized> StreamExt for S {}

impl<S: Stream> Stream for WithCancellation<S> {
    type Item = S::Item;

    async fn next(&mut self) -> Option<Self::Item> {
        self.future.next().with_cancellation(self.token).await
    }
}

#[cfg(test)]
mod tests {
    use std::task::Poll;

    use super::*;
    use crate::runtime::CancellationSource;

    struct ScopeCheckingStream;

    impl Stream for ScopeCheckingStream {
        type Item = ();

        async fn next(&mut self) -> Option<Self::Item> {
            std::future::poll_fn(|_cx| {
                assert_eq!(crate::driver::helpers::scopes::current_scope_ids().len(), 1);
                Poll::Ready(Some(()))
            })
            .await
        }
    }

    #[test]
    fn with_cancellation_installs_scope_while_polling_next() {
        crate::Runtime::new().unwrap().block_on(async {
            let source = CancellationSource::new();
            let mut stream = ScopeCheckingStream.with_cancellation(source.token());
            assert_eq!(stream.next().await, Some(()));
        });
    }
}
