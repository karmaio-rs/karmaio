//! In-memory / mock tests for framed I/O adapters.

use std::collections::VecDeque;
use std::io;

use karmaio::Runtime;
use karmaio::buf::{BoundedIoBufMut, BufResult};
use karmaio::io::{
    AsyncRead, AsyncWrite, BytesCodec, Framed, FramedRead, FramedWrite, LengthDelimited, LineDelimited, Sink, Stream,
};

// Scripted mock reader that returns pre-queued chunk results.
struct MockRead {
    calls: VecDeque<io::Result<Vec<u8>>>,
}

impl AsyncRead for MockRead {
    async fn read<B: BoundedIoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        match self.calls.pop_front() {
            Some(Ok(data)) => {
                let n = data.len().min(buf.bytes_total());
                if n > 0 {
                    unsafe {
                        std::ptr::copy_nonoverlapping(data.as_ptr(), buf.stable_write_ptr(), n);
                        buf.set_init(n);
                    }
                }
                (Ok(n), buf)
            }
            Some(Err(e)) => (Err(e), buf),
            None => (Ok(0), buf),
        }
    }
}

struct MockWrite {
    data: Vec<u8>,
}

impl AsyncWrite for MockWrite {
    async fn write<B: karmaio::buf::BoundedIoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let n = buf.bytes_init();
        if n > 0 {
            unsafe {
                let slice = std::slice::from_raw_parts(buf.stable_read_ptr(), n);
                self.data.extend_from_slice(slice);
            }
        }
        (Ok(n), buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Cursor over a shared buffer for duplex in-memory tests.
struct Cursor {
    data: Vec<u8>,
    pos: usize,
}

impl AsyncRead for Cursor {
    async fn read<B: BoundedIoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        if self.pos >= self.data.len() {
            return (Ok(0), buf);
        }
        let remaining = &self.data[self.pos..];
        let n = remaining.len().min(buf.bytes_total());
        if n > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(remaining.as_ptr(), buf.stable_write_ptr(), n);
                buf.set_init(n);
            }
            self.pos += n;
        }
        (Ok(n), buf)
    }
}

impl AsyncWrite for Cursor {
    async fn write<B: karmaio::buf::BoundedIoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let n = buf.bytes_init();
        if n > 0 {
            unsafe {
                let slice = std::slice::from_raw_parts(buf.stable_read_ptr(), n);
                self.data.extend_from_slice(slice);
            }
        }
        (Ok(n), buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

macro_rules! mock {
    ($($x:expr),* $(,)?) => {{
        let mut v = VecDeque::new();
        v.extend([$($x),*]);
        MockRead { calls: v }
    }};
}

#[test]
fn framed_read_multi_u32_frames() {
    let mut rt = Runtime::new().unwrap();
    rt.block_on(async {
        let mock = mock! {
            Ok(b"\x00\x00\x00\x01A\x00\x00\x00\x01B\x00\x00\x00\x01C".to_vec()),
        };
        let mut framed = FramedRead::new(mock, BytesCodec::new(), LengthDelimited::new());
        assert_eq!(framed.next().await.unwrap().unwrap(), b"A");
        assert_eq!(framed.next().await.unwrap().unwrap(), b"B");
        assert_eq!(framed.next().await.unwrap().unwrap(), b"C");
        assert!(framed.next().await.is_none());
    });
}

#[test]
fn framed_read_split_across_packets() {
    let mut rt = Runtime::new().unwrap();
    rt.block_on(async {
        let mock = mock! {
            Ok(b"\x00\x00\x00".to_vec()),
            Ok(b"\x05hello".to_vec()),
            Ok(b"\x00\x00\x00\x05world".to_vec()),
        };
        let mut framed = FramedRead::new(mock, BytesCodec::new(), LengthDelimited::new());
        assert_eq!(framed.next().await.unwrap().unwrap(), b"hello");
        assert_eq!(framed.next().await.unwrap().unwrap(), b"world");
        assert!(framed.next().await.is_none());
    });
}

#[test]
fn framed_write_length_delimited() {
    let mut rt = Runtime::new().unwrap();
    rt.block_on(async {
        let mut w = MockWrite { data: Vec::new() };
        {
            let mut framed = FramedWrite::new(&mut w, BytesCodec::new(), LengthDelimited::new());
            framed.send(b"hi".to_vec()).await.unwrap();
            framed.send(b"there".to_vec()).await.unwrap();
            framed.close().await.unwrap();
        }
        assert_eq!(&w.data[..], b"\x00\x00\x00\x02hi\x00\x00\x00\x05there");
    });
}

#[test]
fn framed_round_trip() {
    let mut rt = Runtime::new().unwrap();
    rt.block_on(async {
        let mut cursor = Cursor {
            data: Vec::new(),
            pos: 0,
        };

        {
            let mut framed = Framed::new(&mut cursor, BytesCodec::new(), LengthDelimited::new());
            framed.send(b"hello".to_vec()).await.unwrap();
            framed.send(b"world".to_vec()).await.unwrap();
            framed.flush().await.unwrap();
        }

        // Reset read position to start of written data.
        cursor.pos = 0;

        let mut framed = Framed::new(cursor, BytesCodec::new(), LengthDelimited::new());
        assert_eq!(framed.next().await.unwrap().unwrap(), b"hello");
        assert_eq!(framed.next().await.unwrap().unwrap(), b"world");
        assert!(framed.next().await.is_none());
    });
}

#[test]
fn framed_line_delimited_round_trip() {
    let mut rt = Runtime::new().unwrap();
    rt.block_on(async {
        let mut cursor = Cursor {
            data: Vec::new(),
            pos: 0,
        };

        {
            let mut framed = Framed::new(&mut cursor, BytesCodec::new(), LineDelimited::new());
            framed.send(b"one".to_vec()).await.unwrap();
            framed.send(b"two".to_vec()).await.unwrap();
            framed.flush().await.unwrap();
        }

        cursor.pos = 0;
        let mut framed = Framed::new(cursor, BytesCodec::new(), LineDelimited::new());
        assert_eq!(framed.next().await.unwrap().unwrap(), b"one");
        assert_eq!(framed.next().await.unwrap().unwrap(), b"two");
        assert!(framed.next().await.is_none());
    });
}

#[test]
fn framed_line_eof_without_trailing_delimiter() {
    let mut rt = Runtime::new().unwrap();
    rt.block_on(async {
        // No trailing newline on the last line.
        let mock = mock! {
            Ok(b"one\ntwo".to_vec()),
        };
        let mut framed = FramedRead::new(mock, BytesCodec::new(), LineDelimited::new());
        assert_eq!(framed.next().await.unwrap().unwrap(), b"one");
        assert_eq!(framed.next().await.unwrap().unwrap(), b"two");
        assert!(framed.next().await.is_none());
    });
}

#[test]
fn framed_length_eof_with_partial_frame_errors() {
    let mut rt = Runtime::new().unwrap();
    rt.block_on(async {
        let mock = mock! {
            Ok(b"\x00\x00\x00\x05hel".to_vec()),
        };
        let mut framed = FramedRead::new(mock, BytesCodec::new(), LengthDelimited::new());
        let err = framed.next().await.unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    });
}
