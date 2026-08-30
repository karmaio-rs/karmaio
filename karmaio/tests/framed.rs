//! In-memory / mock tests for framed I/O adapters.

use std::collections::VecDeque;
use std::io;

use karmaio::Runtime;
use karmaio::buf::{BufResult, IoBufMut, Slice};
use karmaio::io::{
    AsyncRead, AsyncWrite, BytesCodec, Frame, Framed, FramedParts, FramedRead, FramedReadParts, FramedWrite,
    FramedWriteParts, Framer, LengthDelimited, LineDelimited, Sink, Stream,
};

// Scripted mock reader that returns pre-queued chunk results.
struct MockRead {
    calls: VecDeque<io::Result<Vec<u8>>>,
}

impl AsyncRead for MockRead {
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        match self.calls.pop_front() {
            Some(Ok(data)) => {
                let n = data.len().min(buf.as_uninit().len());
                if n > 0 {
                    unsafe {
                        std::ptr::copy_nonoverlapping(data.as_ptr(), buf.as_uninit().as_mut_ptr().cast::<u8>(), n);
                        buf.set_len(n);
                    }
                }
                BufResult(Ok(n), buf)
            }
            Some(Err(e)) => BufResult(Err(e), buf),
            None => BufResult(Ok(0), buf),
        }
    }
}

struct MockWrite {
    data: Vec<u8>,
}

struct InvalidFramer;

impl<B: IoBufMut> Framer<B> for InvalidFramer {
    fn enclose(&mut self, _buf: &mut B) -> io::Result<()> {
        Ok(())
    }

    fn extract(&mut self, _buf: &Slice<B>) -> io::Result<Option<Frame>> {
        Ok(Some(Frame::new(usize::MAX, 1, 0)))
    }
}

impl AsyncWrite for MockWrite {
    async fn write<B: karmaio::buf::IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let n = buf.as_init().len();
        if n > 0 {
            unsafe {
                let slice = std::slice::from_raw_parts(buf.as_init().as_ptr(), n);
                self.data.extend_from_slice(slice);
            }
        }
        BufResult(Ok(n), buf)
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
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        if self.pos >= self.data.len() {
            return BufResult(Ok(0), buf);
        }
        let remaining = &self.data[self.pos..];
        let n = remaining.len().min(buf.as_uninit().len());
        if n > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(remaining.as_ptr(), buf.as_uninit().as_mut_ptr().cast::<u8>(), n);
                buf.set_len(n);
            }
            self.pos += n;
        }
        BufResult(Ok(n), buf)
    }
}

impl AsyncWrite for Cursor {
    async fn write<B: karmaio::buf::IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let n = buf.as_init().len();
        if n > 0 {
            unsafe {
                let slice = std::slice::from_raw_parts(buf.as_init().as_ptr(), n);
                self.data.extend_from_slice(slice);
            }
        }
        BufResult(Ok(n), buf)
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
fn invalid_frame_bounds_return_an_error_without_losing_input() {
    Runtime::new().unwrap().block_on(async {
        let mock = mock! { Ok(b"x".to_vec()) };
        let mut framed = FramedRead::new(mock, BytesCodec::new(), InvalidFramer);

        let error = framed.next().await.unwrap().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(framed.read_buffer(), b"x");
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

#[test]
fn read_parts_round_trip() {
    Runtime::new().unwrap().block_on(async {
        // Two 1-byte frames: 5 bytes each = 10 bytes total, fits in 16-byte reserve.
        let mock = mock! { Ok(b"\0\0\0\x01A\0\0\0\x01B".to_vec()) };
        let mut framed = FramedRead::new(mock, BytesCodec::new(), LengthDelimited::new());
        assert_eq!(framed.next().await.unwrap().unwrap(), b"A");

        let parts = framed.into_parts();
        assert_eq!(&parts.read_buf[parts.unread.clone()], b"\0\0\0\x01B");

        let mut framed = match FramedRead::from_parts(parts) {
            Ok(f) => f,
            Err(_) => panic!("valid parts must reconstruct"),
        };
        assert_eq!(framed.next().await.unwrap().unwrap(), b"B");
        assert!(framed.next().await.is_none());
    });
}

#[test]
fn read_parts_rejects_invalid_range() {
    let mock = mock! { Ok(vec![]) };
    let framed = FramedRead::new(mock, BytesCodec::new(), LengthDelimited::new());
    let mut parts = framed.into_parts();
    parts.unread = 5..2; // start > end
    assert!(FramedRead::from_parts(parts).is_err());
}

#[test]
fn write_parts_round_trip() {
    Runtime::new().unwrap().block_on(async {
        let mut framed =
            FramedWrite::new(MockWrite { data: Vec::new() }, BytesCodec::new(), LengthDelimited::new());
        framed.send(b"hi".to_vec()).await.unwrap();

        let parts = match framed.try_into_parts() {
            Ok(p) => p,
            Err(_) => panic!("writer must be settled"),
        };
        assert!(parts.buffer.is_empty());

        let mut framed = FramedWrite::from_parts(parts);
        framed.send(b"there".to_vec()).await.unwrap();
        framed.close().await.unwrap();

        // Recover the writer and verify via a fresh reader.
        let parts = match framed.try_into_parts() {
            Ok(p) => p,
            Err(_) => panic!("writer must be settled"),
        };
        let mock = mock! { Ok(parts.io.data) };
        let mut reader = FramedRead::new(mock, BytesCodec::new(), LengthDelimited::new());
        assert_eq!(reader.next().await.unwrap().unwrap(), b"hi");
        assert_eq!(reader.next().await.unwrap().unwrap(), b"there");
    });
}

#[test]
fn duplex_parts_round_trip() {
    Runtime::new().unwrap().block_on(async {
        // Write side: encode and write to a buffer.
        let mut w = MockWrite { data: Vec::new() };
        {
            let mut framed = Framed::new(&mut w, BytesCodec::new(), LengthDelimited::new());
            framed.send(b"A".to_vec()).await.unwrap();
            framed.send(b"B".to_vec()).await.unwrap();
            framed.flush().await.unwrap();
        }
        assert_eq!(&w.data[..], b"\0\0\0\x01A\0\0\0\x01B");

        // Read side: decompose and recompose, verifying buffer preservation.
        let mock = mock! { Ok(w.data.clone()) };
        let mut framed = Framed::new(mock, BytesCodec::new(), LengthDelimited::new());
        assert_eq!(framed.next().await.unwrap().unwrap(), b"A");

        let parts = framed.into_parts();
        let mut framed = match Framed::from_parts(parts) {
            Ok(f) => f,
            Err(_) => panic!("valid parts must reconstruct"),
        };
        assert_eq!(framed.next().await.unwrap().unwrap(), b"B");
        assert!(framed.next().await.is_none());
    });
}

#[test]
fn duplex_parts_rejects_invalid_range() {
    let mock = mock! { Ok(vec![]) };
    let framed = Framed::new(mock, BytesCodec::new(), LengthDelimited::new());
    let mut parts = framed.into_parts();
    parts.unread = 5..2;
    assert!(Framed::from_parts(parts).is_err());
}
