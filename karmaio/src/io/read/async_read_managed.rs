//! Managed async reads that return runtime pool buffers.

use crate::buf::IoBuf;
use std::io;

/// Asynchronously reads into a runtime-provided buffer.
///
/// Unlike [`crate::io::AsyncRead`], the application does not supply a buffer.
/// The implementation selects one from the runtime pool and returns a lease
/// ([`crate::buf::PooledBuf`] on Linux).
///
/// # Buffer ownership
///
/// Returned buffers are **leases**. Drop or explicitly release them promptly so
/// the pool can reuse slots. Holding many leases without recycling can exhaust
/// the pool and cause later receives to fail with `ENOBUFS`.
///
/// # Platform
///
/// On Linux this maps to io_uring buffer selection (and related multishot APIs).
/// Other platforms may leave the trait unimplemented or unavailable.
#[allow(async_fn_in_trait)]
pub trait AsyncReadManaged {
    /// Filled buffer type returned by managed reads.
    type Buffer: IoBuf;

    /// Read some bytes and return a pool buffer lease.
    ///
    /// - `Ok(None)` is analogous to reading zero bytes / EOF on a stream.
    /// - If `len == 0`, implementations should use the full pool buffer size.
    /// - If `len > 0`, at most `min(len, buffer_size)` bytes are received.
    async fn read_managed(&mut self, len: usize) -> io::Result<Option<Self::Buffer>>;
}

impl<B: ?Sized + AsyncReadManaged> AsyncReadManaged for &mut B {
    type Buffer = B::Buffer;

    #[inline]
    async fn read_managed(&mut self, len: usize) -> io::Result<Option<Self::Buffer>> {
        (**self).read_managed(len).await
    }
}
