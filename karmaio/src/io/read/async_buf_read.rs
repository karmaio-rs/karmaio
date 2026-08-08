use crate::io::AsyncRead;

/// An asynchronous reader with buffered-content access.
///
/// Futures are `!Send` by design in this share-nothing runtime.
#[allow(async_fn_in_trait)]
pub trait AsyncBufRead: AsyncRead {
    /// Fills the internal buffer, or returns its existing contents.
    async fn fill_buf(&mut self) -> std::io::Result<&'_ [u8]>;

    /// Advances the read position inside the internal buffer.
    fn consume(&mut self, amount: usize);

    /// Returns the current contents of the internal buffer synchronously.
    fn buffer(&self) -> &[u8];
}

impl<T: AsyncBufRead + ?Sized> AsyncBufRead for &mut T {
    async fn fill_buf(&mut self) -> std::io::Result<&'_ [u8]> {
        (**self).fill_buf().await
    }

    fn consume(&mut self, amount: usize) {
        (**self).consume(amount)
    }

    fn buffer(&self) -> &[u8] {
        (**self).buffer()
    }
}
