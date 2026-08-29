//! Multishot managed async reads (stream of pool buffers).

use crate::io::{AsyncReadManaged, Stream};
use std::io;

/// Asynchronously reads a stream of runtime-provided buffers.
///
/// Extends [`AsyncReadManaged`] with a multishot / streaming API. On Linux this
/// maps to io_uring multishot receive with provided buffers.
///
/// # Ending and re-arm
///
/// The stream ends when the kernel finishes the multishot request (final CQE
/// without `IORING_CQE_F_MORE`), including error completions such as `ENOBUFS`.
/// Implementations do **not** auto-rearm; call [`read_multi`](Self::read_multi)
/// again after recycling outstanding buffer leases if more data is expected.
///
/// # Buffer ownership
///
/// Each item is a pool lease. Drop or release buffers promptly. Exhausting the
/// pool without recycle is a common cause of `ENOBUFS` and a short stream.
/// Implementations may defer kernel submission until the first
/// [`Stream::next`] poll so the stream can be wrapped in a cancellation scope.
/// Wrap before that first poll; wrapping after submission does not attach the
/// existing request retroactively.
#[allow(async_fn_in_trait)]
pub trait AsyncReadMulti: AsyncReadManaged {
    /// Start a multishot managed read stream.
    ///
    /// Each completion may use the full configured pool-buffer capacity.
    /// Setup failures are returned before a stream is created. A deferred
    /// kernel-submission failure is yielded as an error item.
    fn read_multi(&mut self) -> io::Result<impl Stream<Item = io::Result<Self::Buffer>>>;
}
