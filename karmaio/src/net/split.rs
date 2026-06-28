use std::ops::Deref;

use crate::{
    buf::{BoundedIoBuf, BoundedIoBufMut, BufResult},
    io::{AsyncRead, AsyncWrite},
};

pub(super) fn split<'a, T>(stream: &'a T) -> (ReadHalf<'a, T>, WriteHalf<'a, T>)
where
    &'a T: AsyncRead + AsyncWrite,
{
    (ReadHalf(stream), WriteHalf(stream))
}

/// Borrowed read half.
#[derive(Debug)]
pub struct ReadHalf<'a, T>(&'a T);

impl<'a, T> AsyncRead for ReadHalf<'a, T>
where
    &'a T: AsyncRead,
{
    async fn read<B: BoundedIoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.0.read(buf).await
    }

    async fn read_vectored<B: BoundedIoBufMut>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>> {
        self.0.read_vectored(bufs).await
    }
}

impl<T> Deref for ReadHalf<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

/// Borrowed read half.
#[derive(Debug)]
pub struct WriteHalf<'a, T>(&'a T);

impl<'a, T> AsyncWrite for WriteHalf<'a, T>
where
    &'a T: AsyncWrite,
{
    async fn write<B: BoundedIoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        self.0.write(buf).await
    }

    async fn write_vectored<B: BoundedIoBuf>(&mut self, bufs: Vec<B>) -> BufResult<usize, Vec<B>> {
        self.0.write_vectored(bufs).await
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush().await
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        self.0.shutdown().await
    }
}

impl<T> Deref for WriteHalf<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}
