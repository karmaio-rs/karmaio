mod bounded_buf;
mod bounded_buf_mut;
mod io_buf;
mod io_buf_mut;
mod slice;

pub use io_buf::IoBuf;
pub use io_buf_mut::IoBufMut;

pub use bounded_buf::BoundedIoBuf;
pub use bounded_buf_mut::BoundedIoBufMut;

pub use slice::Slice;

// A customized result that returns both the result of the operation and the buffer used for it.
// This is needed because `io-uring` needs full ownership of the buffer
pub type BufResult<T, B> = (std::io::Result<T>, B);
