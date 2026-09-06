use std::cell::RefCell;
use std::io::{self, Cursor, Write};
use std::mem;
use std::rc::Rc;

use crate::buf::{BufResult, IoBuf, IoBufExt, IoBufMut, IoVectoredBuf, IoVectoredBufMut, SetLen};
use crate::io::{AsyncRead, AsyncWrite, IntoOwnedSplit, ReuniteError, ReuniteOwned};
use crate::runtime::is_operation_canceled;

use super::engine::{Engine, PlaintextState, Session, TransportReadState, WriteState};

type SharedSession = Rc<RefCell<Session>>;
type TlsReuniteError<S> =
    ReuniteError<ReadHalf<<S as IntoOwnedSplit>::ReadHalf>, WriteHalf<<S as IntoOwnedSplit>::WriteHalf>>;

struct AbandonGuard<'a> {
    session: &'a RefCell<Session>,
    armed: bool,
}

impl<'a> AbandonGuard<'a> {
    fn new(session: &'a RefCell<Session>) -> Self {
        Self { session, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AbandonGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.session.borrow_mut().abandon();
        }
    }
}

pub(super) struct ReadHalf<R> {
    io: R,
    session: SharedSession,
    input: Vec<u8>,
    input_start: usize,
    transport_read_state: TransportReadState,
}

pub(super) struct WriteHalf<W> {
    io: W,
    session: SharedSession,
    output: Vec<u8>,
    output_start: usize,
}

pub(super) fn split<S>(engine: Engine<S>) -> (ReadHalf<S::ReadHalf>, WriteHalf<S::WriteHalf>)
where
    S: IntoOwnedSplit,
{
    let Engine {
        io,
        session,
        input,
        input_start,
        output,
        output_start,
        transport_read_state,
    } = engine;
    let (read, write) = io.into_split();
    let session = Rc::new(RefCell::new(session));
    (
        ReadHalf {
            io: read,
            session: session.clone(),
            input,
            input_start,
            transport_read_state,
        },
        WriteHalf {
            io: write,
            session,
            output,
            output_start,
        },
    )
}

pub(super) fn is_pair_of<R, W>(read: &ReadHalf<R>, write: &WriteHalf<W>) -> bool {
    Rc::ptr_eq(&read.session, &write.session)
}

pub(super) fn reunite<S>(
    read: ReadHalf<S::ReadHalf>,
    write: WriteHalf<S::WriteHalf>,
) -> Result<Engine<S>, TlsReuniteError<S>>
where
    S: ReuniteOwned,
{
    if !is_pair_of(&read, &write) {
        return Err(ReuniteError::mismatched(read, write));
    }
    if Rc::strong_count(&read.session) != 2 {
        return Err(ReuniteError::not_quiescent(read, write));
    }

    let ReadHalf {
        io: read_io,
        session,
        input,
        input_start,
        transport_read_state,
    } = read;
    let WriteHalf {
        io: write_io,
        session: write_session,
        output,
        output_start,
    } = write;

    match S::reunite(read_io, write_io) {
        Ok(io) => {
            drop(write_session);
            let session = match Rc::try_unwrap(session) {
                Ok(session) => session.into_inner(),
                Err(_) => unreachable!("TLS session ownership changed during synchronous reunion"),
            };
            Ok(Engine {
                io,
                session,
                input,
                input_start,
                output,
                output_start,
                transport_read_state,
            })
        }
        Err(error) => Err(error.map_halves(
            |read_io| ReadHalf {
                io: read_io,
                session,
                input,
                input_start,
                transport_read_state,
            },
            |write_io| WriteHalf {
                io: write_io,
                session: write_session,
                output,
                output_start,
            },
        )),
    }
}

impl<R: AsyncRead> ReadHalf<R> {
    async fn receive_tls(&mut self) -> io::Result<()> {
        if self.input_start == self.input.len() {
            self.input.clear();
            self.input_start = 0;

            if self.transport_read_state == TransportReadState::Eof {
                return Ok(());
            }

            let input = mem::take(&mut self.input);
            let mut guard = AbandonGuard::new(self.session.as_ref());
            let BufResult(result, input) = self.io.read(input).await;
            guard.disarm();
            self.input = input;

            let read = match result {
                Ok(read) => read,
                Err(error) => {
                    let mut session = self.session.borrow_mut();
                    if is_operation_canceled(&error) {
                        session.fail_both(&error);
                    } else {
                        session.fail_read(&error);
                    }
                    return Err(error);
                }
            };

            if read != self.input.len() {
                let error = io::Error::new(
                    io::ErrorKind::InvalidData,
                    "underlying transport returned an inconsistent TLS read length",
                );
                self.session.borrow_mut().fail_read(&error);
                return Err(error);
            }
            if read == 0 {
                self.transport_read_state = TransportReadState::Eof;
            }
        }

        let available = &self.input[self.input_start..];
        let mut reader = Cursor::new(available);
        let mut session = self.session.borrow_mut();
        let consumed = match session.connection.read_tls(&mut reader) {
            Ok(consumed) => consumed,
            Err(error) => {
                session.fail_both(&error);
                return Err(error);
            }
        };
        if consumed > available.len() {
            let error = io::Error::other("Rustls over-consumed TLS input");
            session.fail_both(&error);
            return Err(error);
        }
        self.input_start += consumed;

        if consumed == 0 && !available.is_empty() {
            self.input_start = self.input.len();
        }

        if let Err(error) = session.connection.process_new_packets() {
            let error = io::Error::new(io::ErrorKind::InvalidData, error);
            session.fail_protocol(&error);
            return Err(error);
        }

        // Materialize deferred TLS 1.3 KeyUpdate output. The write half drains
        // it without making read progress depend on transport write progress.
        let accepted = match session.connection.writer().write(&[]) {
            Ok(accepted) => accepted,
            Err(error) => {
                session.fail_both(&error);
                return Err(error);
            }
        };
        debug_assert_eq!(accepted, 0);
        Ok(())
    }

    async fn wait_for_plaintext(&mut self) -> io::Result<bool> {
        loop {
            match self.session.borrow_mut().plaintext_state()? {
                PlaintextState::Available => return Ok(true),
                PlaintextState::PeerClosed => return Ok(false),
                PlaintextState::Pending => {}
            }

            if self.transport_read_state == TransportReadState::Eof {
                let error = io::Error::from(io::ErrorKind::UnexpectedEof);
                self.session.borrow_mut().fail_read(&error);
                return Err(error);
            }
            self.receive_tls().await?;
        }
    }

    pub(super) async fn read<B: IoBufMut>(&mut self, mut buffer: B) -> BufResult<usize, B> {
        let capacity = buffer.as_uninit().len();
        if capacity == 0 {
            return BufResult(Ok(0), buffer);
        }

        let result = match self.wait_for_plaintext().await {
            Ok(false) => Ok(0),
            Ok(true) => {
                let destination = buffer.as_uninit().as_mut_ptr().cast::<u8>();
                let mut offset = 0;
                self.session.borrow_mut().copy_plaintext(capacity, |plaintext| {
                    // Safety: the caller owns the writable range and Rustls
                    // initialized each source chunk.
                    unsafe {
                        std::ptr::copy_nonoverlapping(plaintext.as_ptr(), destination.add(offset), plaintext.len());
                    }
                    offset += plaintext.len();
                })
            }
            Err(error) => Err(error),
        };
        if let Ok(read) = result {
            // Safety: `copy_plaintext` initialized exactly this prefix.
            unsafe { buffer.set_len(read) };
        }
        BufResult(result, buffer)
    }

    pub(super) async fn read_vectored<V: IoVectoredBufMut>(&mut self, mut buffers: V) -> BufResult<usize, V> {
        let capacity = buffers.total_capacity();
        if capacity == 0 {
            return BufResult(Ok(0), buffers);
        }

        let result = match self.wait_for_plaintext().await {
            Ok(false) => Ok(0),
            Ok(true) => {
                let mut destinations = buffers.iter_uninit_slice();
                let mut destination = destinations.next();
                let mut offset = 0;
                self.session.borrow_mut().copy_plaintext(capacity, |mut plaintext| {
                    while !plaintext.is_empty() {
                        let target = destination.as_deref_mut().expect("aggregate capacity is consistent");
                        let count = plaintext.len().min(target.len() - offset);
                        // Safety: the destination component owns this writable
                        // range and Rustls initialized the source bytes.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                plaintext.as_ptr(),
                                target.as_mut_ptr().cast::<u8>().add(offset),
                                count,
                            );
                        }
                        plaintext = &plaintext[count..];
                        offset += count;
                        if offset == target.len() {
                            destination = destinations.next();
                            offset = 0;
                        }
                    }
                })
            }
            Err(error) => Err(error),
        };
        if let Ok(read) = result {
            // Safety: the loop initialized exactly the aggregate prefix.
            unsafe { SetLen::set_len(&mut buffers, read) };
        }
        BufResult(result, buffers)
    }
}

impl<W: AsyncWrite> WriteHalf<W> {
    async fn drain_output_raw(&mut self) -> io::Result<()> {
        loop {
            if self.output_start == self.output.len()
                && matches!(self.session.borrow().write_state, WriteState::Failed(_))
            {
                return Ok(());
            }
            if self.output_start == self.output.len() {
                self.session
                    .borrow_mut()
                    .fill_output(&mut self.output, &mut self.output_start)?;
            }
            if self.output_start == self.output.len() {
                if !self.session.borrow().connection.wants_write() {
                    return Ok(());
                }
                continue;
            }

            let start = self.output_start;
            let end = self.output.len();
            let output = mem::take(&mut self.output).slice(start..end);
            let mut guard = AbandonGuard::new(self.session.as_ref());
            let BufResult(result, output) = self.io.write(output).await;
            guard.disarm();
            self.output = output.into_inner();

            match result {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "underlying transport wrote zero TLS bytes",
                    ));
                }
                Ok(written) if written <= end - start => {
                    self.output_start += written;
                    if self.output_start == self.output.len() {
                        self.output.clear();
                        self.output_start = 0;
                    }
                }
                Ok(_) => return Err(io::Error::other("underlying transport over-reported a TLS write")),
                Err(error) => return Err(error),
            }
        }
    }

    async fn drain_output(&mut self) -> io::Result<()> {
        if let WriteState::Failed(failure) = self.session.borrow().write_state {
            return Err(failure.error());
        }

        let result = self.drain_output_raw().await;
        let deferred_failure = {
            let mut session = self.session.borrow_mut();
            match session.write_state {
                WriteState::AlertPending(failure) => {
                    session.write_state = WriteState::Failed(failure);
                    Some(failure)
                }
                WriteState::Failed(failure) => Some(failure),
                _ => None,
            }
        };
        if let Some(failure) = deferred_failure {
            return Err(failure.error());
        }
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                self.session.borrow_mut().fail_both(&error);
                Err(error)
            }
        }
    }

    fn check_writable(&self) -> io::Result<()> {
        let session = self.session.borrow();
        if let Some(error) = session.write_failure() {
            return Err(error);
        }
        if matches!(session.write_state, WriteState::Closing | WriteState::Closed) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TLS write direction is closed",
            ));
        }
        Ok(())
    }

    fn check_not_closed(&self) -> io::Result<()> {
        if matches!(self.session.borrow().write_state, WriteState::Closed) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TLS write direction is closed",
            ));
        }
        Ok(())
    }

    pub(super) async fn write<B: IoBuf>(&mut self, buffer: B) -> BufResult<usize, B> {
        if buffer.as_init().is_empty() {
            return BufResult(Ok(0), buffer);
        }
        if let Err(error) = self.check_not_closed() {
            return BufResult(Err(error), buffer);
        }
        if let Err(error) = self.drain_output().await {
            return BufResult(Err(error), buffer);
        }
        if let Err(error) = self.check_writable() {
            return BufResult(Err(error), buffer);
        }

        let accepted = match self.session.borrow_mut().write_plaintext(buffer.as_init()) {
            Ok(accepted) => accepted,
            Err(error) => return BufResult(Err(error), buffer),
        };

        match self.drain_output().await {
            Ok(()) => BufResult(Ok(accepted), buffer),
            Err(error) => BufResult(Err(error), buffer),
        }
    }

    pub(super) async fn write_vectored<V: IoVectoredBuf>(&mut self, buffers: V) -> BufResult<usize, V> {
        if buffers.iter_slice().all(|buffer| buffer.is_empty()) {
            return BufResult(Ok(0), buffers);
        }
        if let Err(error) = self.check_not_closed() {
            return BufResult(Err(error), buffers);
        }
        if let Err(error) = self.drain_output().await {
            return BufResult(Err(error), buffers);
        }
        if let Err(error) = self.check_writable() {
            return BufResult(Err(error), buffers);
        }

        let accepted = match self.session.borrow_mut().write_vectored_plaintext(buffers.iter_slice()) {
            Ok(accepted) => accepted,
            Err(error) => return BufResult(Err(error), buffers),
        };

        match self.drain_output().await {
            Ok(()) => BufResult(Ok(accepted), buffers),
            Err(error) => BufResult(Err(error), buffers),
        }
    }

    pub(super) async fn flush(&mut self) -> io::Result<()> {
        if matches!(self.session.borrow().write_state, WriteState::Closed) {
            return Ok(());
        }
        self.drain_output().await?;

        let mut guard = AbandonGuard::new(self.session.as_ref());
        let result = self.io.flush().await;
        guard.disarm();
        match result {
            Ok(()) => {
                if let Some(error) = self.session.borrow().write_failure() {
                    Err(error)
                } else {
                    Ok(())
                }
            }
            Err(error) => {
                self.session.borrow_mut().fail_both(&error);
                Err(error)
            }
        }
    }

    pub(super) async fn shutdown(&mut self) -> io::Result<()> {
        if matches!(self.session.borrow().write_state, WriteState::Closed) {
            return Ok(());
        }
        self.drain_output().await?;
        {
            let mut session = self.session.borrow_mut();
            if matches!(session.write_state, WriteState::Open) {
                session.write_state = WriteState::Closing;
                session.connection.send_close_notify();
            }
        }

        self.flush().await?;
        let mut guard = AbandonGuard::new(self.session.as_ref());
        let result = self.io.shutdown().await;
        guard.disarm();
        if let Err(error) = result {
            self.session.borrow_mut().fail_both(&error);
            return Err(error);
        }
        self.session.borrow_mut().write_state = WriteState::Closed;
        Ok(())
    }
}
