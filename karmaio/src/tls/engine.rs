use std::io::{self, BufRead, Cursor, Write};
use std::mem;

use rustls::Connection;

use crate::buf::{BufResult, IoBuf, IoBufExt, IoBufMut, IoVectoredBuf, IoVectoredBufMut, SetLen};
use crate::io::{AsyncRead, AsyncWrite};
use crate::runtime::is_operation_canceled;

use super::buffer::{CIPHERTEXT_CAPACITY, FixedWriter};
use super::error::Failure;

#[derive(Clone, Copy, Debug)]
pub(super) enum ReadState {
    Open,
    PeerClosed,
    Failed(Failure),
}

#[derive(Clone, Copy, Debug)]
pub(super) enum WriteState {
    Open,
    Closing,
    Closed,
    AlertPending(Failure),
    Failed(Failure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TransportReadState {
    Open,
    Eof,
}

pub(super) enum PlaintextState {
    Available,
    Pending,
    PeerClosed,
}

#[derive(Clone, Copy)]
struct DirectionStates {
    read: ReadState,
    write: WriteState,
}

/// Rustls protocol state that contains no transport operation or owned I/O buffer.
pub(super) struct Session {
    pub(super) connection: Connection,
    pub(super) read_state: ReadState,
    pub(super) write_state: WriteState,
}

impl Session {
    fn new(connection: Connection) -> Self {
        Self {
            connection,
            read_state: ReadState::Open,
            write_state: WriteState::Open,
        }
    }

    pub(super) fn read_failure(&self) -> Option<io::Error> {
        match self.read_state {
            ReadState::Failed(failure) => Some(failure.error()),
            _ => None,
        }
    }

    pub(super) fn write_failure(&self) -> Option<io::Error> {
        match self.write_state {
            WriteState::AlertPending(failure) | WriteState::Failed(failure) => Some(failure.error()),
            _ => None,
        }
    }

    pub(super) fn fail_read(&mut self, error: &io::Error) {
        self.record_read_failure(Failure::from_error(error));
    }

    pub(super) fn fail_both(&mut self, error: &io::Error) {
        let failure = Failure::from_error(error);
        self.record_read_failure(failure);
        self.record_write_failure(failure);
    }

    pub(super) fn fail_protocol(&mut self, error: &io::Error) {
        let failure = Failure::from_error(error);
        self.record_read_failure(failure);
        if !matches!(
            self.write_state,
            WriteState::Closed | WriteState::AlertPending(_) | WriteState::Failed(_)
        ) {
            self.write_state = WriteState::AlertPending(failure);
        }
    }

    pub(super) fn abandon(&mut self) {
        let failure = Failure::abandoned_io();
        self.record_read_failure(failure);
        self.record_write_failure(failure);
    }

    pub(super) fn plaintext_state(&mut self) -> io::Result<PlaintextState> {
        if let Some(error) = self.read_failure() {
            return Err(error);
        }
        if matches!(self.read_state, ReadState::PeerClosed) {
            return Ok(PlaintextState::PeerClosed);
        }

        match self.connection.reader().into_first_chunk() {
            Ok([]) => {
                self.read_state = ReadState::PeerClosed;
                Ok(PlaintextState::PeerClosed)
            }
            Ok(_) => Ok(PlaintextState::Available),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(PlaintextState::Pending),
            Err(error) => {
                self.fail_read(&error);
                Err(error)
            }
        }
    }

    pub(super) fn copy_plaintext(&mut self, capacity: usize, mut copy: impl FnMut(&[u8])) -> io::Result<usize> {
        let mut copied = 0;
        while copied < capacity {
            let (consumed, chunk_len) = match self.connection.reader().into_first_chunk() {
                Ok([]) => {
                    self.read_state = ReadState::PeerClosed;
                    break;
                }
                Ok(chunk) => {
                    let consumed = chunk.len().min(capacity - copied);
                    copy(&chunk[..consumed]);
                    (consumed, chunk.len())
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock && copied > 0 => break,
                Err(_) if copied > 0 => break,
                Err(error) => {
                    self.fail_read(&error);
                    return Err(error);
                }
            };

            self.connection.reader().consume(consumed);
            copied += consumed;
            if consumed < chunk_len {
                break;
            }
        }
        Ok(copied)
    }

    pub(super) fn write_plaintext(&mut self, plaintext: &[u8]) -> io::Result<usize> {
        let result = match self.connection.writer().write(plaintext) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "Rustls accepted zero plaintext bytes",
            )),
            result => result,
        };
        if let Err(error) = &result {
            self.fail_both(error);
        }
        result
    }

    pub(super) fn write_vectored_plaintext<'a>(
        &mut self,
        buffers: impl Iterator<Item = &'a [u8]>,
    ) -> io::Result<usize> {
        let mut accepted = 0;
        for buffer in buffers.filter(|buffer| !(*buffer).is_empty()) {
            match self.connection.writer().write(buffer) {
                Ok(0) => break,
                Ok(written) => {
                    accepted += written;
                    if written < buffer.len() {
                        break;
                    }
                }
                Err(error) => {
                    self.fail_both(&error);
                    return Err(error);
                }
            }
        }
        if accepted == 0 {
            let error = io::Error::new(io::ErrorKind::WriteZero, "Rustls accepted zero plaintext bytes");
            self.fail_both(&error);
            return Err(error);
        }
        Ok(accepted)
    }

    pub(super) fn fill_output(&mut self, output: &mut Vec<u8>, output_start: &mut usize) -> io::Result<()> {
        if *output_start < output.len() || !self.connection.wants_write() {
            return Ok(());
        }

        output.clear();
        *output_start = 0;
        let mut writer = FixedWriter::new(output, CIPHERTEXT_CAPACITY);
        let written = self.connection.write_tls(&mut writer)?;
        if written != output.len() {
            return Err(io::Error::other("Rustls reported an inconsistent TLS output length"));
        }
        if written == 0 && self.connection.wants_write() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "Rustls made no progress serializing pending TLS output",
            ));
        }
        Ok(())
    }

    fn record_read_failure(&mut self, failure: Failure) {
        if !matches!(self.read_state, ReadState::Failed(_)) {
            self.read_state = ReadState::Failed(failure);
        }
    }

    fn record_write_failure(&mut self, failure: Failure) {
        if !matches!(self.write_state, WriteState::Closed | WriteState::Failed(_)) {
            self.write_state = WriteState::Failed(failure);
        }
    }

    fn begin_transport_io(&mut self) -> DirectionStates {
        let states = DirectionStates {
            read: self.read_state,
            write: self.write_state,
        };
        let failure = Failure::abandoned_io();
        self.record_read_failure(failure);
        self.record_write_failure(failure);
        states
    }

    fn finish_transport_io(&mut self, states: DirectionStates) {
        self.read_state = states.read;
        self.write_state = states.write;
    }
}

/// The single mutable protocol state shared by client and server wrappers.
pub(super) struct Engine<S> {
    pub(super) io: S,
    pub(super) session: Session,
    pub(super) input: Vec<u8>,
    pub(super) input_start: usize,
    pub(super) output: Vec<u8>,
    pub(super) output_start: usize,
    pub(super) transport_read_state: TransportReadState,
}

impl<S> Engine<S> {
    pub(super) fn new(io: S, connection: Connection) -> Self {
        Self {
            io,
            session: Session::new(connection),
            input: Vec::with_capacity(CIPHERTEXT_CAPACITY),
            input_start: 0,
            output: Vec::with_capacity(CIPHERTEXT_CAPACITY),
            output_start: 0,
            transport_read_state: TransportReadState::Open,
        }
    }

    pub(super) fn get_ref(&self) -> (&S, &Connection) {
        (&self.io, &self.session.connection)
    }

    pub(super) fn into_parts(self) -> io::Result<(S, Connection)> {
        if let ReadState::Failed(failure) = self.session.read_state {
            return Err(failure.error());
        }
        match self.session.write_state {
            WriteState::AlertPending(failure) | WriteState::Failed(failure) => return Err(failure.error()),
            _ => {}
        }
        Ok((self.io, self.session.connection))
    }
}

impl<S: AsyncRead + AsyncWrite> Engine<S> {
    /// Drains every queued Rustls record with bounded, reusable output storage.
    async fn drain_output_raw(&mut self) -> io::Result<()> {
        while self.output_start < self.output.len() || self.session.connection.wants_write() {
            self.session.fill_output(&mut self.output, &mut self.output_start)?;
            if self.output_start == self.output.len() {
                continue;
            }

            let start = self.output_start;
            let end = self.output.len();
            let output = mem::take(&mut self.output).slice(start..end);
            let states = self.session.begin_transport_io();
            let BufResult(result, output) = self.io.write(output).await;
            self.session.finish_transport_io(states);
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
        Ok(())
    }

    async fn drain_output(&mut self) -> io::Result<()> {
        if matches!(self.session.write_state, WriteState::Closed) {
            return Ok(());
        }
        match self.drain_output_raw().await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.session.fail_both(&error);
                Err(error)
            }
        }
    }

    /// Supplies one retained or newly read ciphertext batch and processes it
    /// exactly once.
    async fn receive_tls(&mut self) -> io::Result<()> {
        if self.input_start == self.input.len() {
            self.input.clear();
            self.input_start = 0;

            if self.transport_read_state == TransportReadState::Eof {
                return Ok(());
            }

            let input = mem::take(&mut self.input);
            let states = self.session.begin_transport_io();
            let BufResult(result, input) = self.io.read(input).await;
            self.session.finish_transport_io(states);
            self.input = input;

            let read = match result {
                Ok(read) => read,
                Err(error) => {
                    if is_operation_canceled(&error) {
                        self.session.fail_both(&error);
                    } else {
                        self.session.fail_read(&error);
                    }
                    return Err(error);
                }
            };

            if read != self.input.len() {
                let error = io::Error::new(
                    io::ErrorKind::InvalidData,
                    "underlying transport returned an inconsistent TLS read length",
                );
                self.session.fail_read(&error);
                return Err(error);
            }
            if read == 0 {
                self.transport_read_state = TransportReadState::Eof;
            }
        }

        let available = &self.input[self.input_start..];
        let mut reader = Cursor::new(available);
        let consumed = self.session.connection.read_tls(&mut reader)?;
        if consumed > available.len() {
            let error = io::Error::other("Rustls over-consumed TLS input");
            self.session.fail_both(&error);
            return Err(error);
        }
        self.input_start += consumed;

        if consumed == 0 && !available.is_empty() {
            // Rustls stops consuming after a peer close. Bytes following the
            // close alert are not part of this TLS connection.
            self.input_start = self.input.len();
        }

        if let Err(error) = self.session.connection.process_new_packets() {
            let error = io::Error::new(io::ErrorKind::InvalidData, error);
            // Rustls may have queued a fatal alert. Recover the output buffer
            // even if the best-effort transport write itself fails.
            if !matches!(self.session.write_state, WriteState::Closed) {
                let _ = self.drain_output_raw().await;
            }
            self.session.fail_both(&error);
            return Err(error);
        }

        // Rustls defers a TLS 1.3 KeyUpdate response until its plaintext
        // writer is next touched. A zero-length write accepts no application
        // data but materializes that protocol-generated record so the read
        // path can drain it before returning or waiting for more input.
        let accepted = self.session.connection.writer().write(&[])?;
        debug_assert_eq!(accepted, 0);
        Ok(())
    }

    pub(super) async fn handshake(mut self) -> io::Result<Self> {
        loop {
            self.drain_output().await?;
            if !self.session.connection.is_handshaking() && !self.session.connection.wants_write() {
                return Ok(self);
            }
            if self.transport_read_state == TransportReadState::Eof {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            self.receive_tls().await?;
        }
    }

    async fn wait_for_plaintext(&mut self) -> io::Result<bool> {
        let mut processed_input = false;
        loop {
            if processed_input && let Err(error) = self.drain_output().await {
                return Err(error);
            }
            match self.session.plaintext_state()? {
                PlaintextState::Available => return Ok(true),
                PlaintextState::PeerClosed => return Ok(false),
                PlaintextState::Pending => {}
            }

            // No plaintext is currently available. Drain protocol output
            // before waiting for more input, while allowing plaintext already
            // buffered at method entry to win without transport I/O.
            if !processed_input && let Err(error) = self.drain_output().await {
                return Err(error);
            }
            if self.transport_read_state == TransportReadState::Eof {
                let error = io::Error::from(io::ErrorKind::UnexpectedEof);
                self.session.fail_read(&error);
                return Err(error);
            }
            self.receive_tls().await?;
            // Output created while processing this batch must reach the
            // transport before newly decrypted plaintext is returned.
            processed_input = true;
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
                self.session.copy_plaintext(capacity, |plaintext| {
                    // Safety: Rustls initialized `plaintext`, the caller owns
                    // the `capacity`-byte destination, and chunks are copied
                    // into consecutive, non-overlapping ranges.
                    unsafe {
                        std::ptr::copy_nonoverlapping(plaintext.as_ptr(), destination.add(offset), plaintext.len());
                    }
                    offset += plaintext.len();
                })
            }
            Err(error) => Err(error),
        };
        if let Ok(read) = result {
            // Safety: `copy_plaintext` initialized exactly the aggregate
            // prefix `read` of the destination.
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
                self.session.copy_plaintext(capacity, |mut plaintext| {
                    while !plaintext.is_empty() {
                        let target = destination.as_deref_mut().expect("aggregate capacity is consistent");
                        let count = plaintext.len().min(target.len() - offset);
                        // Safety: Rustls initialized `plaintext`, and this
                        // component owns `count` writable bytes at `offset`.
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
            // Safety: the loop initialized exactly the aggregate prefix `read`.
            unsafe { SetLen::set_len(&mut buffers, read) };
        }
        BufResult(result, buffers)
    }

    pub(super) async fn write<B: IoBuf>(&mut self, buffer: B) -> BufResult<usize, B> {
        if buffer.as_init().is_empty() {
            return BufResult(Ok(0), buffer);
        }
        if let Some(error) = self.session.write_failure() {
            return BufResult(Err(error), buffer);
        }
        if matches!(self.session.write_state, WriteState::Closing | WriteState::Closed) {
            return BufResult(
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "TLS write direction is closed",
                )),
                buffer,
            );
        }
        if let Err(error) = self.drain_output().await {
            return BufResult(Err(error), buffer);
        }

        let accepted = match self.session.write_plaintext(buffer.as_init()) {
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
        if let Some(error) = self.session.write_failure() {
            return BufResult(Err(error), buffers);
        }
        if matches!(self.session.write_state, WriteState::Closing | WriteState::Closed) {
            return BufResult(
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "TLS write direction is closed",
                )),
                buffers,
            );
        }
        if let Err(error) = self.drain_output().await {
            return BufResult(Err(error), buffers);
        }

        let accepted = match self.session.write_vectored_plaintext(buffers.iter_slice()) {
            Ok(accepted) => accepted,
            Err(error) => return BufResult(Err(error), buffers),
        };

        match self.drain_output().await {
            Ok(()) => BufResult(Ok(accepted), buffers),
            Err(error) => BufResult(Err(error), buffers),
        }
    }

    pub(super) async fn flush(&mut self) -> io::Result<()> {
        if let Some(error) = self.session.write_failure() {
            return Err(error);
        }
        if matches!(self.session.write_state, WriteState::Closed) {
            return Ok(());
        }
        self.drain_output().await?;
        let states = self.session.begin_transport_io();
        let result = self.io.flush().await;
        self.session.finish_transport_io(states);
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                self.session.fail_both(&error);
                Err(error)
            }
        }
    }

    pub(super) async fn shutdown(&mut self) -> io::Result<()> {
        if matches!(self.session.write_state, WriteState::Closed) {
            return Ok(());
        }
        if let Some(error) = self.session.write_failure() {
            return Err(error);
        }
        if matches!(self.session.write_state, WriteState::Open) {
            self.session.write_state = WriteState::Closing;
            self.session.connection.send_close_notify();
        }

        self.flush().await?;
        let states = self.session.begin_transport_io();
        let result = self.io.shutdown().await;
        self.session.finish_transport_io(states);
        if let Err(error) = result {
            self.session.fail_both(&error);
            return Err(error);
        }
        self.session.write_state = WriteState::Closed;
        Ok(())
    }
}
