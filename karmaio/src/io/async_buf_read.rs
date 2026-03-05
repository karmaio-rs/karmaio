use crate::io::AsyncRead;

// Async reader with buffered content support
pub trait AsyncBufRead: AsyncRead {
    // Fill the internal buffer (or return what is already there) and give a view.
    async fn fill_buf(&mut self) -> std::io::Result<&'_ [u8]>;

    // Advance the read position inside the internal buffer.
    fn consume(&mut self, amount: usize);

    // Current contents of the internal buffer (sync view).
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
