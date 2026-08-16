use std::{cell::Cell, rc::Rc};

use karmaio::{
    buf::{BufResult, IoBuf, IoBufExt, IoBufMut},
    io::{AsyncRead, AsyncWrite},
};

/// Test adapter that limits each operation while preserving the original buffer.
pub struct PartialReader<R> {
    inner: R,
    max_per_operation: usize,
    calls: Rc<Cell<usize>>,
}

impl<R> PartialReader<R> {
    pub fn new(inner: R, max_per_operation: usize, calls: Rc<Cell<usize>>) -> Self {
        assert!(max_per_operation > 0);
        Self {
            inner,
            max_per_operation,
            calls,
        }
    }
}

impl<R: AsyncRead> AsyncRead for PartialReader<R> {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        let len = self.max_per_operation.min(buf.as_init().len());
        let (result, buf) = self.inner.read(buf.slice(..len)).await.into_parts();
        self.calls.set(self.calls.get() + 1);
        BufResult(result, buf.into_inner())
    }
}

/// Test adapter that limits each operation while preserving the original buffer.
pub struct PartialWriter<W> {
    inner: W,
    max_per_operation: usize,
    calls: Rc<Cell<usize>>,
}

impl<W> PartialWriter<W> {
    pub fn new(inner: W, max_per_operation: usize, calls: Rc<Cell<usize>>) -> Self {
        assert!(max_per_operation > 0);
        Self {
            inner,
            max_per_operation,
            calls,
        }
    }
}

impl<W: AsyncWrite> AsyncWrite for PartialWriter<W> {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let len = self.max_per_operation.min(buf.as_init().len());
        let (result, buf) = self.inner.write(buf.slice(..len)).await.into_parts();
        self.calls.set(self.calls.get() + 1);
        BufResult(result, buf.into_inner())
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush().await
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        self.inner.shutdown().await
    }
}
