//! In-memory / mock tests for framed I/O adapters.

use std::{
    cell::Cell,
    collections::VecDeque,
    future::Future,
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    task::Poll,
};

use karmaio::Runtime;
use karmaio::buf::{BufResult, IoBuf, IoBufMut, Slice};
use karmaio::io::{
    AsyncRead, AsyncWrite, BytesCodec, Encoder, Frame, Framed, FramedRead, FramedWrite, Framer, LengthDelimited,
    LineDelimited, Sink, Stream,
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

struct PanicOnceEncoder {
    should_panic: bool,
}

impl Encoder<Vec<u8>, Vec<u8>> for PanicOnceEncoder {
    type Error = io::Error;

    fn encode(&mut self, item: Vec<u8>, buffer: &mut Vec<u8>) -> Result<(), Self::Error> {
        buffer.extend_from_slice(&item);
        if std::mem::take(&mut self.should_panic) {
            panic!("scripted encoder panic");
        }
        Ok(())
    }
}

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

struct PendingIo {
    read: Option<Vec<u8>>,
    written: Vec<u8>,
    read_pending: bool,
    write_pending: bool,
    read_calls: Rc<Cell<usize>>,
    write_calls: Rc<Cell<usize>>,
}

struct PendingErrorWrite {
    pending: bool,
}

impl AsyncWrite for PendingErrorWrite {
    async fn write<B: IoBuf>(&mut self, buffer: B) -> karmaio::buf::BufResult<usize, B> {
        std::future::poll_fn(|context| {
            if self.pending {
                self.pending = false;
                context.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        karmaio::buf::BufResult(Err(io::Error::other("scripted write failure")), buffer)
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl PendingIo {
    fn new(read: Vec<u8>) -> (Self, Rc<Cell<usize>>, Rc<Cell<usize>>) {
        let read_calls = Rc::new(Cell::new(0));
        let write_calls = Rc::new(Cell::new(0));
        (
            Self {
                read: Some(read),
                written: Vec::new(),
                read_pending: true,
                write_pending: true,
                read_calls: read_calls.clone(),
                write_calls: write_calls.clone(),
            },
            read_calls,
            write_calls,
        )
    }
}

impl AsyncRead for PendingIo {
    async fn read<B: IoBufMut>(&mut self, mut buffer: B) -> BufResult<usize, B> {
        self.read_calls.set(self.read_calls.get() + 1);
        std::future::poll_fn(|context| {
            if self.read_pending {
                self.read_pending = false;
                context.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        let bytes = self.read.take().unwrap_or_default();
        let count = bytes.len().min(buffer.as_uninit().len());
        if count != 0 {
            // Safety: `count` is bounded by both source and destination.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.as_uninit().as_mut_ptr().cast(), count);
                buffer.set_len(count);
            }
        }
        BufResult(Ok(count), buffer)
    }
}

impl AsyncWrite for PendingIo {
    async fn write<B: IoBuf>(&mut self, buffer: B) -> BufResult<usize, B> {
        self.write_calls.set(self.write_calls.get() + 1);
        std::future::poll_fn(|context| {
            if self.write_pending {
                self.write_pending = false;
                context.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        self.written.extend_from_slice(buffer.as_init());
        let count = buffer.as_init().len();
        BufResult(Ok(count), buffer)
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn poll_once<F: Future>(mut future: std::pin::Pin<&mut F>) {
    std::future::poll_fn(|context| {
        assert!(future.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
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
        assert_eq!(framed.read_buffer(), Some(&b"x"[..]));
    });
}

#[test]
fn framed_write_length_delimited() {
    let mut rt = Runtime::new().unwrap();
    rt.block_on(async {
        let mut framed = FramedWrite::new(
            MockWrite { data: Vec::new() },
            BytesCodec::new(),
            LengthDelimited::new(),
        );
        framed.send(b"hi".to_vec()).await.unwrap();
        framed.send(b"there".to_vec()).await.unwrap();
        framed.close().await.unwrap();
        let w = match framed.try_into_parts() {
            Ok(parts) => parts.io,
            Err(_) => panic!("completed writer must be settled"),
        };
        assert_eq!(&w.data[..], b"\x00\x00\x00\x02hi\x00\x00\x00\x05there");
    });
}

#[test]
fn encoder_panic_leaves_the_writer_idle() {
    let mut runtime = Runtime::new().unwrap();
    let mut framed = FramedWrite::new(
        MockWrite { data: Vec::new() },
        PanicOnceEncoder { should_panic: true },
        LineDelimited::new(),
    );

    let panic = catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(framed.send(b"discarded".to_vec())).unwrap();
    }));
    assert!(panic.is_err());
    assert!(framed.get_ref().is_some());
    assert_eq!(framed.write_buffer(), Some(&b"discarded"[..]));

    runtime.block_on(framed.send(b"written".to_vec())).unwrap();
    let parts = match framed.try_into_parts() {
        Ok(parts) => parts,
        Err(_) => panic!("writer must remain settled after an encoder panic"),
    };
    assert_eq!(parts.io.data, b"written\n");
}

#[test]
fn framed_round_trip() {
    let mut rt = Runtime::new().unwrap();
    rt.block_on(async {
        let mut framed = Framed::with_duplex(
            (mock! {}, MockWrite { data: Vec::new() }),
            BytesCodec::new(),
            LengthDelimited::new(),
        );
        framed.send(b"hello".to_vec()).await.unwrap();
        framed.send(b"world".to_vec()).await.unwrap();
        framed.flush().await.unwrap();
        let parts = match framed.try_into_parts() {
            Ok(parts) => parts,
            Err(_) => panic!("completed duplex must be settled"),
        };
        let mut data = parts.writer.data;
        let second = data.split_off(9);
        let mut framed = Framed::new(
            mock! { Ok(data), Ok(second) },
            MockWrite { data: Vec::new() },
            BytesCodec::new(),
            LengthDelimited::new(),
        );
        assert_eq!(framed.next().await.unwrap().unwrap(), b"hello");
        assert_eq!(framed.next().await.unwrap().unwrap(), b"world");
        assert!(framed.next().await.is_none());
    });
}

#[test]
fn framed_line_delimited_round_trip() {
    let mut rt = Runtime::new().unwrap();
    rt.block_on(async {
        let mut framed = Framed::new(
            mock! {},
            MockWrite { data: Vec::new() },
            BytesCodec::new(),
            LineDelimited::new(),
        );
        framed.send(b"one".to_vec()).await.unwrap();
        framed.send(b"two".to_vec()).await.unwrap();
        framed.flush().await.unwrap();
        let parts = match framed.try_into_parts() {
            Ok(parts) => parts,
            Err(_) => panic!("completed duplex must be settled"),
        };
        let mut framed = Framed::new(
            mock! { Ok(parts.writer.data) },
            MockWrite { data: Vec::new() },
            BytesCodec::new(),
            LineDelimited::new(),
        );
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

        let parts = match framed.try_into_parts() {
            Ok(parts) => parts,
            Err(_) => panic!("completed reader must be settled"),
        };
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
    let mut parts = match framed.try_into_parts() {
        Ok(parts) => parts,
        Err(_) => panic!("new reader must be settled"),
    };
    let start = 5;
    let end = 2;
    parts.unread = start..end; // start > end
    assert!(FramedRead::from_parts(parts).is_err());
}

#[test]
fn write_parts_round_trip() {
    Runtime::new().unwrap().block_on(async {
        let mut framed = FramedWrite::new(
            MockWrite { data: Vec::new() },
            BytesCodec::new(),
            LengthDelimited::new(),
        );
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
        let mut framed = Framed::new(
            mock! {},
            MockWrite { data: Vec::new() },
            BytesCodec::new(),
            LengthDelimited::new(),
        );
        framed.send(b"A".to_vec()).await.unwrap();
        framed.send(b"B".to_vec()).await.unwrap();
        framed.flush().await.unwrap();
        let w = match framed.try_into_parts() {
            Ok(parts) => parts.writer,
            Err(_) => panic!("completed duplex must be settled"),
        };
        assert_eq!(&w.data[..], b"\0\0\0\x01A\0\0\0\x01B");

        // Read side: decompose and recompose, verifying buffer preservation.
        let mock = mock! { Ok(w.data.clone()) };
        let mut framed = Framed::new(
            mock,
            MockWrite { data: Vec::new() },
            BytesCodec::new(),
            LengthDelimited::new(),
        );
        assert_eq!(framed.next().await.unwrap().unwrap(), b"A");

        let parts = match framed.try_into_parts() {
            Ok(parts) => parts,
            Err(_) => panic!("completed duplex must be settled"),
        };
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
    let framed = Framed::new(
        mock,
        MockWrite { data: Vec::new() },
        BytesCodec::new(),
        LengthDelimited::new(),
    );
    let mut parts = match framed.try_into_parts() {
        Ok(parts) => parts,
        Err(_) => panic!("new duplex must be settled"),
    };
    let start = 5;
    let end = 2;
    parts.unread = start..end;
    assert!(Framed::from_parts(parts).is_err());
}

#[test]
fn dropped_next_resumes_the_same_completion_read() {
    Runtime::new().unwrap().block_on(async {
        let (reader, read_calls, _) = PendingIo::new(b"\0\0\0\x01A".to_vec());
        let mut framed = FramedRead::new(reader, BytesCodec::new(), LengthDelimited::new());

        let mut next = Box::pin(framed.next());
        poll_once(next.as_mut()).await;
        drop(next);

        assert!(framed.is_reading());
        assert_eq!(framed.next().await.unwrap().unwrap(), b"A");
        assert_eq!(read_calls.get(), 1);
    });
}

#[test]
fn dropped_send_resumes_before_writing_the_next_frame() {
    Runtime::new().unwrap().block_on(async {
        let (writer, _, write_calls) = PendingIo::new(Vec::new());
        let mut framed = FramedWrite::new(writer, BytesCodec::new(), LineDelimited::new());

        let mut send = Box::pin(framed.send(b"A".to_vec()));
        poll_once(send.as_mut()).await;
        drop(send);

        assert!(framed.is_writing());
        framed.send(b"B".to_vec()).await.unwrap();
        let parts = match framed.try_into_parts() {
            Ok(parts) => parts,
            Err(_) => panic!("writes must be settled"),
        };
        assert_eq!(parts.io.written, b"A\nB\n");
        assert_eq!(write_calls.get(), 2);
    });
}

#[test]
fn duplex_directions_progress_independently_after_cancellation() {
    Runtime::new().unwrap().block_on(async {
        let (reader, read_calls, _) = PendingIo::new(b"in\n".to_vec());
        let (writer, _, write_calls) = PendingIo::new(Vec::new());
        let mut framed = Framed::new(reader, writer, BytesCodec::new(), LineDelimited::new());

        let mut next = Box::pin(framed.next());
        poll_once(next.as_mut()).await;
        drop(next);
        assert!(framed.is_reading());
        assert!(framed.reader_ref().is_none());
        assert!(framed.writer_ref().is_some());

        let mut send = Box::pin(framed.send(b"out".to_vec()));
        poll_once(send.as_mut()).await;
        drop(send);
        assert!(framed.is_reading());
        assert!(framed.is_writing());
        assert!(framed.reader_ref().is_none());
        assert!(framed.writer_ref().is_none());

        let settled = framed.into_parts().await;
        settled.read_result.unwrap();
        settled.write_result.unwrap();
        assert_eq!(&settled.parts.read_buf[settled.parts.unread.clone()], b"in\n");
        assert_eq!(settled.parts.writer.written, b"out\n");
        assert_eq!(read_calls.get(), 1);
        assert_eq!(write_calls.get(), 1);
    });
}

#[test]
fn fixed_initialized_write_buffer_returns_an_error() {
    Runtime::new().unwrap().block_on(async {
        let mut framed = FramedWrite::with_buffer(
            MockWrite { data: Vec::new() },
            BytesCodec::new(),
            LineDelimited::new(),
            [0_u8; 4],
        );
        let error = framed.send(b"x".to_vec()).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    });
}

#[test]
fn framing_errors_terminate_until_parts_are_rebuilt() {
    Runtime::new().unwrap().block_on(async {
        let mock = mock! {
            Ok(b"four".to_vec()),
            Ok(b"\nok\n".to_vec()),
        };
        let mut framed = FramedRead::new(mock, BytesCodec::new(), LineDelimited::new_with_max_length(3));

        let error = framed.next().await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(framed.next().await.is_none());
        assert_eq!(framed.read_buffer(), Some(&b"four"[..]));

        let mut parts = match framed.try_into_parts() {
            Ok(parts) => parts,
            Err(_) => panic!("errored reads are settled"),
        };
        parts.read_buf.clear();
        parts.unread = 0..0;
        let mut framed = match FramedRead::from_parts(parts) {
            Ok(framed) => framed,
            Err(_) => panic!("repaired parts are valid"),
        };
        assert_eq!(framed.next().await.unwrap().unwrap(), b"");
        assert_eq!(framed.next().await.unwrap().unwrap(), b"ok");
    });
}

#[test]
fn settled_write_errors_preserve_the_encoded_frame() {
    Runtime::new().unwrap().block_on(async {
        let mut framed = FramedWrite::new(
            PendingErrorWrite { pending: true },
            BytesCodec::new(),
            LineDelimited::new(),
        );
        let mut send = Box::pin(framed.send(b"frame".to_vec()));
        poll_once(send.as_mut()).await;
        drop(send);

        let settled = framed.into_parts().await;
        assert_eq!(settled.write_result.unwrap_err().kind(), io::ErrorKind::Other);
        assert_eq!(settled.parts.buffer, b"frame\n");
    });
}
