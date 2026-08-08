mod buf_result;
mod io_buf;
mod io_buf_ext;
mod io_buf_mut;
mod io_buf_mut_ext;
mod io_vec_buf;
mod io_vec_buf_iter;
mod io_vec_buf_mut;
mod slice;
mod uninit_slice;

pub use buf_result::BufResult;
pub use io_buf::IoBuf;
pub use io_buf_ext::IoBufExt;
pub use io_buf_mut::{IoBufMut, SetLen};
pub use io_buf_mut_ext::IoBufMutExt;
pub use io_vec_buf::IoVectoredBuf;
pub use io_vec_buf_iter::VectoredBufIterator;
pub use io_vec_buf_mut::IoVectoredBufMut;
pub use slice::{Slice, VectoredSlice};
pub use uninit_slice::UninitSlice;

/// Recovers the owned value wrapped by a buffer adapter.
pub trait IntoInner {
    /// The wrapped value.
    type Inner;

    /// Consumes the adapter and returns its wrapped value.
    fn into_inner(self) -> Self::Inner;
}
