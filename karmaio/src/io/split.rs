use super::{AsyncRead, AsyncWrite};

/// Consumes a duplex value and produces independently owned read and write halves.
///
/// Each half owns the underlying resource strongly enough to outlive the
/// original value, so they can be retained by separate completion operations,
/// moved into independently supervised `'static` local tasks, and used
/// concurrently.
///
/// The halves must also make independent progress: a pending operation on one
/// half must not prevent an operation on the other half from being submitted
/// or completed. A wrapper that holds a shared lock across `read().await` or
/// `write().await` does not satisfy this contract.
///
/// Neither half needs to be [`Send`] or [`Sync`]; they are intended to work on
/// Karmaio's local, share-nothing runtime.
///
/// Implementations must guarantee that dropping one half does not close the
/// underlying resource while the other half or an in-flight operation still
/// owns it. Types with intrinsically coupled protocol state, such as Karmaio's
/// current TLS streams, should not implement this trait until they provide
/// coordinated duplex progress.
pub trait IntoOwnedSplit: Sized {
    /// Independently owned readable half.
    type ReadHalf: AsyncRead + 'static;

    /// Independently owned writable half.
    type WriteHalf: AsyncWrite + 'static;

    /// Splits this value into its independently owned halves.
    fn into_split(self) -> (Self::ReadHalf, Self::WriteHalf);
}

impl<R, W> IntoOwnedSplit for (R, W)
where
    R: AsyncRead + 'static,
    W: AsyncWrite + 'static,
{
    type ReadHalf = R;
    type WriteHalf = W;

    #[inline]
    fn into_split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        self
    }
}
