//! Public owned-split capability tests using an external mock transport.

use karmaio::{
    buf::{BufResult, IoBuf, IoBufMut},
    io::{AsyncRead, AsyncWrite},
    net::split::{IntoOwnedSplit, ReuniteError, ReuniteErrorKind, ReuniteOwned},
};

#[derive(Debug, PartialEq, Eq)]
struct TestDuplex(u8);

struct SplitOnlyDuplex(u8);

#[derive(Debug)]
struct TestReadHalf(u8);

#[derive(Debug)]
struct TestWriteHalf(u8);

impl AsyncRead for TestReadHalf {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        BufResult(Ok(0), buf)
    }
}

impl AsyncWrite for TestWriteHalf {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        BufResult(Ok(buf.as_init().len()), buf)
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl IntoOwnedSplit for TestDuplex {
    type ReadHalf = TestReadHalf;
    type WriteHalf = TestWriteHalf;

    fn into_split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        (TestReadHalf(self.0), TestWriteHalf(self.0))
    }
}

impl IntoOwnedSplit for SplitOnlyDuplex {
    type ReadHalf = TestReadHalf;
    type WriteHalf = TestWriteHalf;

    fn into_split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        (TestReadHalf(self.0), TestWriteHalf(self.0))
    }
}

impl ReuniteOwned for TestDuplex {
    type ReuniteError = ReuniteError<Self::ReadHalf, Self::WriteHalf>;

    fn reunite(read: Self::ReadHalf, write: Self::WriteHalf) -> Result<Self, Self::ReuniteError> {
        if read.0 == write.0 {
            Ok(Self(read.0))
        } else {
            Err(ReuniteError::mismatched(read, write))
        }
    }
}

fn split_generic<S: IntoOwnedSplit>(stream: S) -> (S::ReadHalf, S::WriteHalf) {
    stream.into_split()
}

fn reunite_generic<S: ReuniteOwned>(read: S::ReadHalf, write: S::WriteHalf) -> Result<S, S::ReuniteError> {
    S::reunite(read, write)
}

#[test]
fn external_transport_can_split_without_supporting_reunion() {
    let (read, write) = split_generic(SplitOnlyDuplex(3));
    assert_eq!(read.0, 3);
    assert_eq!(write.0, 3);
}

#[test]
fn external_transport_can_reunite_through_optional_capability() {
    let (read, write) = TestDuplex(7).into_split();
    assert_eq!(reunite_generic::<TestDuplex>(read, write).unwrap(), TestDuplex(7));

    let (read, _) = TestDuplex(1).into_split();
    let (_, write) = TestDuplex(2).into_split();
    let failure = reunite_generic::<TestDuplex>(read, write).unwrap_err();
    assert_eq!(failure.kind(), ReuniteErrorKind::Mismatched);
    assert_eq!(failure.halves().0.0, 1);
    assert_eq!(failure.halves().1.0, 2);
    let (read, write) = failure.into_halves();
    assert_eq!(read.0, 1);
    assert_eq!(write.0, 2);
}
