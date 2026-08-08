//! Pure-async sink traits for karmaio.
//!
//! These are intentionally independent of framed I/O so any consumer of async
//! values can implement them (network framed writers, channels, logs, …).

use crate::io::Stream;

/// A sink into which values can be sent asynchronously in pure async/await.
///
/// Futures are `!Send` by design in this share-nothing runtime.
#[allow(async_fn_in_trait)]
pub trait Sink<Item> {
    /// The type of value produced by the sink when an error occurs.
    type Error;

    /// Send an item into the sink.
    async fn send(&mut self, item: Item) -> Result<(), Self::Error>;

    /// Flush any remaining buffered output from this sink.
    async fn flush(&mut self) -> Result<(), Self::Error>;

    /// Flush remaining output and close this sink.
    async fn close(&mut self) -> Result<(), Self::Error>;
}

impl<T, S: ?Sized + Sink<T>> Sink<T> for &mut S {
    type Error = S::Error;

    #[inline]
    async fn send(&mut self, item: T) -> Result<(), Self::Error> {
        (**self).send(item).await
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), Self::Error> {
        (**self).flush().await
    }

    #[inline]
    async fn close(&mut self) -> Result<(), Self::Error> {
        (**self).close().await
    }
}

/// Extension methods for [`Sink`].
///
/// Futures are `!Send` by design in this share-nothing runtime.
#[allow(async_fn_in_trait)]
pub trait SinkExt<Item>: Sink<Item> {
    /// Sends all items from `stream` into this sink, then flushes.
    async fn send_all<S>(&mut self, stream: &mut S) -> Result<(), Self::Error>
    where
        S: Stream<Item = Item> + ?Sized,
    {
        while let Some(item) = stream.next().await {
            self.send(item).await?;
        }
        self.flush().await
    }
}

impl<Item, S: Sink<Item> + ?Sized> SinkExt<Item> for S {}
