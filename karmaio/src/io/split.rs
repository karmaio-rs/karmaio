use std::fmt;

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
/// owns it. Types with coupled protocol state must coordinate that state
/// without serializing transport progress.
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

/// Extends [`IntoOwnedSplit`] for transports that can reconstruct the original
/// value from matching owned halves.
///
/// Reunification succeeds only for halves from the same original value and
/// only when no incompatible ownership remains. A failed attempt returns both
/// halves unchanged so callers can correct the pairing or retry later.
pub trait ReuniteOwned: IntoOwnedSplit {
    /// Attempts to reunite matching owned halves into the original value.
    fn reunite(
        read: Self::ReadHalf,
        write: Self::WriteHalf,
    ) -> Result<Self, ReuniteError<Self::ReadHalf, Self::WriteHalf>>;
}

/// The semantic reason a reunification attempt failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReuniteErrorKind {
    /// The halves originated from different split operations.
    Mismatched,
    /// Matching halves cannot yet be reunited because another owner remains.
    NotQuiescent,
}

impl fmt::Display for ReuniteErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mismatched => write!(f, "halves originated from different split operations"),
            Self::NotQuiescent => write!(f, "matching halves are not yet quiescent"),
        }
    }
}

/// A failed reunification that preserves both owned halves.
///
/// Use [`Self::kind`] to choose whether to correct the pairing or wait for
/// outstanding ownership to end, then recover the halves with
/// [`Self::into_halves`].
#[derive(Debug)]
pub struct ReuniteError<R, W> {
    kind: ReuniteErrorKind,
    read: R,
    write: W,
}

impl<R, W> ReuniteError<R, W> {
    /// Creates an error for halves from different split operations.
    pub fn mismatched(read: R, write: W) -> Self {
        Self {
            kind: ReuniteErrorKind::Mismatched,
            read,
            write,
        }
    }

    /// Creates an error for matching halves that cannot yet be reunited.
    pub fn not_quiescent(read: R, write: W) -> Self {
        Self {
            kind: ReuniteErrorKind::NotQuiescent,
            read,
            write,
        }
    }

    /// Returns the reason the reunification attempt failed.
    pub fn kind(&self) -> ReuniteErrorKind {
        self.kind
    }

    /// Borrows the preserved read and write halves.
    pub fn halves(&self) -> (&R, &W) {
        (&self.read, &self.write)
    }

    /// Consumes the error and returns the preserved read and write halves.
    pub fn into_halves(self) -> (R, W) {
        (self.read, self.write)
    }

    pub(crate) fn map_halves<R2, W2>(
        self,
        map_read: impl FnOnce(R) -> R2,
        map_write: impl FnOnce(W) -> W2,
    ) -> ReuniteError<R2, W2> {
        ReuniteError {
            kind: self.kind,
            read: map_read(self.read),
            write: map_write(self.write),
        }
    }
}

impl<R, W> fmt::Display for ReuniteError<R, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot reunite owned halves: {}", self.kind)
    }
}

impl<R: fmt::Debug, W: fmt::Debug> std::error::Error for ReuniteError<R, W> {}
