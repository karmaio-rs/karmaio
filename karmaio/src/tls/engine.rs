use std::io::{self, Cursor, Read, Write};
use std::mem;

use rustls::{Connection, HandshakeKind, ProtocolVersion, SupportedCipherSuite};

use crate::buf::{BufResult, IoBuf, IoBufExt, IoBufMut, IoVectoredBuf, IoVectoredBufMut, SetLen};
use crate::io::{AsyncRead, AsyncWrite};
use crate::runtime::is_operation_canceled;

use super::buffer::{CIPHERTEXT_CAPACITY, FixedWriter, PLAINTEXT_CAPACITY, RUSTLS_BUFFER_LIMIT};
use super::error::Failure;

#[derive(Clone, Copy, Debug)]
enum ReadState {
    Open,
    PeerClosed,
    Failed(Failure),
}

#[derive(Clone, Copy, Debug)]
enum WriteState {
    Open,
    Closing,
    Closed,
    Failed(Failure),
}

#[derive(Clone, Copy)]
struct DirectionStates {
    read: ReadState,
    write: WriteState,
}

/// The single mutable protocol state shared by client and server wrappers.
pub(crate) struct Engine<S> {
    io: S,
    connection: Connection,
    input: Vec<u8>,
    input_start: usize,
    output: Vec<u8>,
    output_start: usize,
    plaintext: Vec<u8>,
    transport_eof: bool,
    read_state: ReadState,
    write_state: WriteState,
}

impl<S> Engine<S> {
    pub(crate) fn new(io: S, mut connection: Connection) -> Self {
        connection.set_buffer_limit(Some(RUSTLS_BUFFER_LIMIT));
        Self {
            io,
            connection,
            input: Vec::with_capacity(CIPHERTEXT_CAPACITY),
            input_start: 0,
            output: Vec::with_capacity(CIPHERTEXT_CAPACITY),
            output_start: 0,
            plaintext: vec![0; PLAINTEXT_CAPACITY],
            transport_eof: false,
            read_state: ReadState::Open,
            write_state: WriteState::Open,
        }
    }

    pub(crate) fn get_ref(&self) -> (&S, &Connection) {
        (&self.io, &self.connection)
    }

    pub(crate) fn into_parts(self) -> (S, Connection) {
        (self.io, self.connection)
    }

    pub(crate) fn alpn_protocol(&self) -> Option<&[u8]> {
        self.connection.alpn_protocol()
    }

    pub(crate) fn peer_certificates(&self) -> Option<&[rustls::pki_types::CertificateDer<'static>]> {
        self.connection.peer_certificates()
    }

    pub(crate) fn protocol_version(&self) -> Option<ProtocolVersion> {
        self.connection.protocol_version()
    }

    pub(crate) fn negotiated_cipher_suite(&self) -> Option<SupportedCipherSuite> {
        self.connection.negotiated_cipher_suite()
    }

    pub(crate) fn handshake_kind(&self) -> Option<HandshakeKind> {
        self.connection.handshake_kind()
    }

    pub(crate) fn export_keying_material<T: AsMut<[u8]>>(
        &self,
        output: T,
        label: &[u8],
        context: Option<&[u8]>,
    ) -> Result<T, rustls::Error> {
        self.connection.export_keying_material(output, label, context)
    }

    fn read_failure(&self) -> Option<io::Error> {
        match self.read_state {
            ReadState::Failed(failure) => Some(failure.error()),
            _ => None,
        }
    }

    fn write_failure(&self) -> Option<io::Error> {
        match self.write_state {
            WriteState::Failed(failure) => Some(failure.error()),
            _ => None,
        }
    }

    fn fail_read(&mut self, error: &io::Error) {
        self.read_state = ReadState::Failed(Failure::from_error(error));
    }

    fn fail_both(&mut self, error: &io::Error) {
        let failure = Failure::from_error(error);
        self.read_state = ReadState::Failed(failure);
        self.write_state = WriteState::Failed(failure);
    }

    /// Marks both directions failed until an owned transport operation returns.
    ///
    /// If the enclosing future is dropped at the await point, the marker stays
    /// behind and prevents reuse after the submitted buffer or wire progress
    /// became ambiguous.
    fn begin_transport_io(&mut self) -> DirectionStates {
        let states = DirectionStates {
            read: self.read_state,
            write: self.write_state,
        };
        let failure = Failure::abandoned_io();
        self.read_state = ReadState::Failed(failure);
        self.write_state = WriteState::Failed(failure);
        states
    }

    fn finish_transport_io(&mut self, states: DirectionStates) {
        self.read_state = states.read;
        self.write_state = states.write;
    }

    fn fill_output(&mut self) -> io::Result<()> {
        if self.output_start < self.output.len() || !self.connection.wants_write() {
            return Ok(());
        }

        self.output.clear();
        self.output_start = 0;
        let mut writer = FixedWriter::new(&mut self.output, CIPHERTEXT_CAPACITY);
        let written = self.connection.write_tls(&mut writer)?;
        if written != self.output.len() {
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
}

impl<S: AsyncRead + AsyncWrite> Engine<S> {
    /// Drains every queued Rustls record with bounded, reusable output storage.
    async fn drain_output_raw(&mut self) -> io::Result<()> {
        while self.output_start < self.output.len() || self.connection.wants_write() {
            self.fill_output()?;
            if self.output_start == self.output.len() {
                continue;
            }

            let start = self.output_start;
            let end = self.output.len();
            let output = mem::take(&mut self.output).slice(start..end);
            let states = self.begin_transport_io();
            let BufResult(result, output) = self.io.write(output).await;
            self.finish_transport_io(states);
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
        match self.drain_output_raw().await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.fail_both(&error);
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

            if self.transport_eof {
                return Ok(());
            }

            let input = mem::take(&mut self.input);
            let states = self.begin_transport_io();
            let BufResult(result, input) = self.io.read(input).await;
            self.finish_transport_io(states);
            self.input = input;

            let read = match result {
                Ok(read) => read,
                Err(error) => {
                    if is_operation_canceled(&error) {
                        self.fail_both(&error);
                    } else {
                        self.fail_read(&error);
                    }
                    return Err(error);
                }
            };

            if read != self.input.len() {
                let error = io::Error::new(
                    io::ErrorKind::InvalidData,
                    "underlying transport returned an inconsistent TLS read length",
                );
                self.fail_read(&error);
                return Err(error);
            }
            self.transport_eof = read == 0;
        }

        let available = &self.input[self.input_start..];
        let mut reader = Cursor::new(available);
        let consumed = self.connection.read_tls(&mut reader)?;
        if consumed > available.len() {
            let error = io::Error::other("Rustls over-consumed TLS input");
            self.fail_both(&error);
            return Err(error);
        }
        self.input_start += consumed;

        if consumed == 0 && !available.is_empty() {
            // Rustls stops consuming after a peer close. Bytes following the
            // close alert are not part of this TLS connection.
            self.input_start = self.input.len();
        }

        if let Err(error) = self.connection.process_new_packets() {
            let error = io::Error::new(io::ErrorKind::InvalidData, error);
            // Rustls may have queued a fatal alert. Recover the output buffer
            // even if the best-effort transport write itself fails.
            let _ = self.drain_output_raw().await;
            self.fail_both(&error);
            return Err(error);
        }

        // Rustls defers a TLS 1.3 KeyUpdate response until its plaintext
        // writer is next touched. A zero-length write accepts no application
        // data but materializes that protocol-generated record so the read
        // path can drain it before returning or waiting for more input.
        let accepted = self.connection.writer().write(&[])?;
        debug_assert_eq!(accepted, 0);
        Ok(())
    }

    pub(crate) async fn handshake(mut self) -> io::Result<Self> {
        loop {
            self.drain_output().await?;
            if !self.connection.is_handshaking() && !self.connection.wants_write() {
                return Ok(self);
            }
            if self.transport_eof {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            self.receive_tls().await?;
        }
    }

    async fn read_plaintext(&mut self, capacity: usize) -> io::Result<usize> {
        if capacity == 0 {
            return Ok(0);
        }
        if let Some(error) = self.read_failure() {
            return Err(error);
        }
        if matches!(self.read_state, ReadState::PeerClosed) {
            return Ok(0);
        }

        let mut processed_input = false;
        loop {
            if processed_input && let Err(error) = self.drain_output().await {
                return Err(error);
            }
            let limit = capacity.min(PLAINTEXT_CAPACITY);
            match self.connection.reader().read(&mut self.plaintext[..limit]) {
                Ok(0) => {
                    self.read_state = ReadState::PeerClosed;
                    return Ok(0);
                }
                Ok(read) => return Ok(read),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    self.fail_read(&error);
                    return Err(error);
                }
            }

            // No plaintext is currently available. Drain protocol output
            // before waiting for more input, while allowing plaintext already
            // buffered at method entry to win without transport I/O.
            if !processed_input && let Err(error) = self.drain_output().await {
                return Err(error);
            }
            if self.transport_eof {
                let error = io::Error::from(io::ErrorKind::UnexpectedEof);
                self.fail_read(&error);
                return Err(error);
            }
            self.receive_tls().await?;
            // Output created while processing this batch must reach the
            // transport before newly decrypted plaintext is returned.
            processed_input = true;
        }
    }

    pub(crate) async fn read<B: IoBufMut>(&mut self, mut buffer: B) -> BufResult<usize, B> {
        let capacity = buffer.as_uninit().len();
        let result = self.read_plaintext(capacity).await;
        if let Ok(read) = result {
            // Safety: Rustls initialized plaintext[..read], the caller owns a
            // writable region of at least `capacity`, and the two allocations
            // cannot overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.plaintext.as_ptr(),
                    buffer.as_uninit().as_mut_ptr().cast::<u8>(),
                    read,
                );
                buffer.set_len(read);
            }
        }
        BufResult(result, buffer)
    }

    pub(crate) async fn read_vectored<V: IoVectoredBufMut>(&mut self, mut buffers: V) -> BufResult<usize, V> {
        let capacity = buffers.total_capacity();
        let result = self.read_plaintext(capacity).await;
        if let Ok(read) = result {
            let mut copied = 0;
            for destination in buffers.iter_uninit_slice() {
                if copied == read {
                    break;
                }
                let count = destination.len().min(read - copied);
                if count == 0 {
                    continue;
                }
                // Safety: Rustls initialized plaintext[..read], this component
                // owns `count` writable bytes, and the allocations cannot overlap.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.plaintext.as_ptr().add(copied),
                        destination.as_mut_ptr().cast::<u8>(),
                        count,
                    );
                }
                copied += count;
            }
            debug_assert_eq!(copied, read);
            // Safety: the loop initialized exactly the aggregate prefix `read`.
            unsafe { SetLen::set_len(&mut buffers, read) };
        }
        BufResult(result, buffers)
    }

    pub(crate) async fn write<B: IoBuf>(&mut self, buffer: B) -> BufResult<usize, B> {
        if buffer.as_init().is_empty() {
            return BufResult(Ok(0), buffer);
        }
        if let Some(error) = self.write_failure() {
            return BufResult(Err(error), buffer);
        }
        if matches!(self.write_state, WriteState::Closing | WriteState::Closed) {
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

        let limit = buffer.as_init().len().min(PLAINTEXT_CAPACITY);
        let accepted = match self.connection.writer().write(&buffer.as_init()[..limit]) {
            Ok(0) => {
                let error = io::Error::new(io::ErrorKind::WriteZero, "Rustls accepted zero plaintext bytes");
                self.fail_both(&error);
                return BufResult(Err(error), buffer);
            }
            Ok(accepted) => accepted,
            Err(error) => {
                self.fail_both(&error);
                return BufResult(Err(error), buffer);
            }
        };

        match self.drain_output().await {
            Ok(()) => BufResult(Ok(accepted), buffer),
            Err(error) => BufResult(Err(error), buffer),
        }
    }

    pub(crate) async fn write_vectored<V: IoVectoredBuf>(&mut self, buffers: V) -> BufResult<usize, V> {
        let mut length = 0;
        for buffer in buffers.iter_slice() {
            let count = buffer.len().min(PLAINTEXT_CAPACITY - length);
            self.plaintext[length..length + count].copy_from_slice(&buffer[..count]);
            length += count;
            if length == PLAINTEXT_CAPACITY {
                break;
            }
        }
        if length == 0 {
            return BufResult(Ok(0), buffers);
        }
        if let Some(error) = self.write_failure() {
            return BufResult(Err(error), buffers);
        }
        if matches!(self.write_state, WriteState::Closing | WriteState::Closed) {
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

        let accepted = match self.connection.writer().write(&self.plaintext[..length]) {
            Ok(0) => {
                let error = io::Error::new(io::ErrorKind::WriteZero, "Rustls accepted zero plaintext bytes");
                self.fail_both(&error);
                return BufResult(Err(error), buffers);
            }
            Ok(accepted) => accepted,
            Err(error) => {
                self.fail_both(&error);
                return BufResult(Err(error), buffers);
            }
        };

        match self.drain_output().await {
            Ok(()) => BufResult(Ok(accepted), buffers),
            Err(error) => BufResult(Err(error), buffers),
        }
    }

    pub(crate) async fn flush(&mut self) -> io::Result<()> {
        if let Some(error) = self.write_failure() {
            return Err(error);
        }
        if matches!(self.write_state, WriteState::Closed) {
            return Ok(());
        }
        self.drain_output().await?;
        let states = self.begin_transport_io();
        let result = self.io.flush().await;
        self.finish_transport_io(states);
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                self.fail_both(&error);
                Err(error)
            }
        }
    }

    pub(crate) async fn shutdown(&mut self) -> io::Result<()> {
        if matches!(self.write_state, WriteState::Closed) {
            return Ok(());
        }
        if let Some(error) = self.write_failure() {
            return Err(error);
        }
        if matches!(self.write_state, WriteState::Open) {
            self.write_state = WriteState::Closing;
            self.connection.send_close_notify();
        }

        self.flush().await?;
        let states = self.begin_transport_io();
        let result = self.io.shutdown().await;
        self.finish_transport_io(states);
        if let Err(error) = result {
            self.fail_both(&error);
            return Err(error);
        }
        self.write_state = WriteState::Closed;
        Ok(())
    }
}
