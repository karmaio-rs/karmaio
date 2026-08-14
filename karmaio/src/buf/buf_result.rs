use std::{fmt::Debug, io};

use crate::buf::IntoInner;

/// A completion result that always returns ownership of its buffer.
///
/// Completion-based I/O must retain the buffer until the operation finishes.
/// This type keeps buffer recovery explicit on both the success and error paths.
#[must_use = "completion results contain both the operation outcome and returned buffer"]
#[derive(Debug)]
pub struct BufResult<T, B>(pub io::Result<T>, pub B);

impl<T, B> BufResult<T, B> {
    /// Returns `true` when the operation completed successfully.
    #[inline]
    pub const fn is_ok(&self) -> bool {
        self.0.is_ok()
    }

    /// Returns `true` when the operation failed.
    #[inline]
    pub const fn is_err(&self) -> bool {
        self.0.is_err()
    }

    /// Maps a successful result while allowing the buffer to be updated.
    #[inline]
    pub fn map<U>(self, f: impl FnOnce(T, B) -> (U, B)) -> BufResult<U, B> {
        match self.0 {
            Ok(value) => {
                let (value, buffer) = f(value, self.1);
                BufResult(Ok(value), buffer)
            }
            Err(error) => BufResult(Err(error), self.1),
        }
    }

    /// Maps a successful result and changes the buffer type.
    ///
    /// `f_err` converts the returned buffer when the operation failed and
    /// `f_ok` therefore cannot run.
    #[inline]
    pub fn map2<U, C>(self, f_ok: impl FnOnce(T, B) -> (U, C), f_err: impl FnOnce(B) -> C) -> BufResult<U, C> {
        match self.0 {
            Ok(value) => {
                let (value, buffer) = f_ok(value, self.1);
                BufResult(Ok(value), buffer)
            }
            Err(error) => BufResult(Err(error), f_err(self.1)),
        }
    }

    /// Maps a successful result without changing the returned buffer.
    #[inline]
    pub fn map_result<U>(self, f: impl FnOnce(T) -> U) -> BufResult<U, B> {
        BufResult(self.0.map(f), self.1)
    }

    /// Maps the returned buffer without changing the operation result.
    #[inline]
    pub fn map_buffer<C>(self, f: impl FnOnce(B) -> C) -> BufResult<T, C> {
        BufResult(self.0, f(self.1))
    }

    /// Chains a fallible transformation that may also update the buffer.
    #[inline]
    pub fn and_then<U>(self, f: impl FnOnce(T, B) -> (io::Result<U>, B)) -> BufResult<U, B> {
        match self.0 {
            Ok(value) => BufResult::from(f(value, self.1)),
            Err(error) => BufResult(Err(error), self.1),
        }
    }

    /// Returns the successful value and buffer, panicking with `message` on error.
    #[inline]
    #[track_caller]
    pub fn expect(self, message: &str) -> (T, B) {
        (self.0.expect(message), self.1)
    }

    /// Returns the successful value and buffer, panicking on error.
    #[inline]
    #[track_caller]
    pub fn unwrap(self) -> (T, B) {
        (self.0.unwrap(), self.1)
    }

    /// Separates the operation result from its returned buffer.
    #[inline]
    pub fn into_parts(self) -> (io::Result<T>, B) {
        (self.0, self.1)
    }
}

impl<T: Debug, B> BufResult<T, B> {
    /// Returns the error and buffer, panicking when the operation succeeded.
    #[inline]
    #[track_caller]
    pub fn unwrap_err(self) -> (io::Error, B) {
        (self.0.unwrap_err(), self.1)
    }
}

impl<T, B> From<(io::Result<T>, B)> for BufResult<T, B> {
    #[inline]
    fn from((result, buffer): (io::Result<T>, B)) -> Self {
        Self(result, buffer)
    }
}

impl<T, B> From<BufResult<T, B>> for (io::Result<T>, B) {
    #[inline]
    fn from(BufResult(result, buffer): BufResult<T, B>) -> Self {
        (result, buffer)
    }
}

impl<T, B: IntoInner> IntoInner for BufResult<T, B> {
    type Inner = BufResult<T, B::Inner>;

    #[inline]
    fn into_inner(self) -> Self::Inner {
        BufResult(self.0, self.1.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_result_and_buffer_independently() {
        let result = BufResult(Ok(2), String::from("buffer"))
            .map_result(|value| value * 3)
            .map_buffer(String::into_bytes);

        assert_eq!(result.unwrap(), (6, b"buffer".to_vec()));
    }

    #[test]
    fn preserves_buffer_when_mapping_an_error() {
        let result = BufResult::<usize, _>(Err(io::Error::other("failed")), vec![1, 2]).map_result(|value| value * 3);

        let (error, buffer) = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(buffer, [1, 2]);
    }

    #[test]
    fn converts_the_buffer_on_both_result_paths() {
        let success = BufResult(Ok(2), String::from("buffer"))
            .map2(|value, buffer| (value * 3, buffer.into_bytes()), String::into_bytes);
        assert_eq!(success.unwrap(), (6, b"buffer".to_vec()));

        let failure = BufResult::<usize, _>(Err(io::Error::other("failed")), String::from("buffer"))
            .map2(|value, buffer| (value, buffer.into_bytes()), String::into_bytes);
        assert_eq!(failure.unwrap_err().1, b"buffer".to_vec());
    }
}
