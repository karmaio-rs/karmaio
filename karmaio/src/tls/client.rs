//! Client-side TLS support.

use std::fmt;
use std::io;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, Connection, HandshakeKind, ProtocolVersion, SupportedCipherSuite};

use crate::buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut};
use crate::io::{AsyncRead, AsyncWrite};

use super::engine::Engine;

/// Creates client-side TLS connections from a shared Rustls configuration.
#[derive(Clone)]
pub struct TlsConnector {
    config: Arc<ClientConfig>,
}

impl TlsConnector {
    /// Creates a connector that uses `config` for every new TLS connection.
    pub fn new(config: Arc<ClientConfig>) -> Self {
        Self { config }
    }

    /// Returns the shared Rustls configuration used for new connections.
    pub fn config(&self) -> &Arc<ClientConfig> {
        &self.config
    }

    /// Performs a client handshake over `stream` for `server_name`.
    ///
    /// The returned stream has sent all final handshake records. On failure,
    /// the incomplete transport is dropped after all owned buffers have been
    /// recovered.
    pub async fn connect<S>(&self, server_name: ServerName<'static>, stream: S) -> io::Result<TlsStream<S>>
    where
        S: AsyncRead + AsyncWrite,
    {
        let connection = ClientConnection::new(self.config.clone(), server_name)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Self::connect_connection(connection, stream).await
    }

    /// Performs a client handshake with an ALPN offer specific to this connection.
    ///
    /// `alpn_protocols` replaces the protocols from [`ClientConfig`] for this
    /// connection without mutating the shared configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if Rustls rejects the connection parameters or the TLS
    /// handshake cannot be completed over `stream`.
    pub async fn connect_with_alpn<S>(
        &self,
        server_name: ServerName<'static>,
        stream: S,
        alpn_protocols: Vec<Vec<u8>>,
    ) -> io::Result<TlsStream<S>>
    where
        S: AsyncRead + AsyncWrite,
    {
        let connection = ClientConnection::new_with_alpn(self.config.clone(), server_name, alpn_protocols)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Self::connect_connection(connection, stream).await
    }

    async fn connect_connection<S>(connection: ClientConnection, stream: S) -> io::Result<TlsStream<S>>
    where
        S: AsyncRead + AsyncWrite,
    {
        let engine = Engine::new(stream, Connection::Client(connection)).handshake().await?;
        Ok(TlsStream { engine })
    }
}

impl From<Arc<ClientConfig>> for TlsConnector {
    #[inline]
    fn from(config: Arc<ClientConfig>) -> Self {
        Self::new(config)
    }
}

impl fmt::Debug for TlsConnector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("TlsConnector").finish_non_exhaustive()
    }
}

/// An established client-side TLS stream.
pub struct TlsStream<S> {
    engine: Engine<S>,
}

impl<S> TlsStream<S> {
    /// Returns immutable references to the transport and Rustls connection.
    pub fn get_ref(&self) -> (&S, &ClientConnection) {
        let (stream, connection) = self.engine.get_ref();
        match connection {
            Connection::Client(connection) => (stream, connection),
            Connection::Server(_) => unreachable!("client TLS stream contains a server connection"),
        }
    }

    /// Returns the negotiated ALPN protocol, if the peers selected one.
    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        self.engine.alpn_protocol()
    }

    /// Returns the server certificate chain after authentication.
    pub fn peer_certificates(&self) -> Option<&[CertificateDer<'static>]> {
        self.engine.peer_certificates()
    }

    /// Returns the negotiated TLS protocol version.
    pub fn protocol_version(&self) -> Option<ProtocolVersion> {
        self.engine.protocol_version()
    }

    /// Returns the negotiated cipher suite.
    pub fn negotiated_cipher_suite(&self) -> Option<SupportedCipherSuite> {
        self.engine.negotiated_cipher_suite()
    }

    /// Returns whether the connection used a full or resumed handshake.
    pub fn handshake_kind(&self) -> Option<HandshakeKind> {
        self.engine.handshake_kind()
    }

    /// Derives key material from the established TLS connection.
    ///
    /// The output buffer is returned on success and discarded on failure so
    /// partially derived key material cannot be observed.
    ///
    /// # Errors
    ///
    /// Returns an error if the handshake is not complete, the output is empty,
    /// or Rustls cannot derive the requested material.
    pub fn export_keying_material<T: AsMut<[u8]>>(
        &self,
        output: T,
        label: &[u8],
        context: Option<&[u8]>,
    ) -> Result<T, rustls::Error> {
        self.engine.export_keying_material(output, label, context)
    }

    /// Extracts the transport and Rustls state without sending `close_notify`
    /// or shutting down the transport.
    pub fn into_parts(self) -> (S, ClientConnection) {
        let (stream, connection) = self.engine.into_parts();
        match connection {
            Connection::Client(connection) => (stream, connection),
            Connection::Server(_) => unreachable!("client TLS stream contains a server connection"),
        }
    }
}

impl<S> fmt::Debug for TlsStream<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ClientTlsStream").finish_non_exhaustive()
    }
}

impl<S: AsyncRead + AsyncWrite> AsyncRead for TlsStream<S> {
    #[inline]
    async fn read<B: IoBufMut>(&mut self, buffer: B) -> BufResult<usize, B> {
        self.engine.read(buffer).await
    }

    #[inline]
    async fn read_vectored<V: IoVectoredBufMut>(&mut self, buffers: V) -> BufResult<usize, V> {
        self.engine.read_vectored(buffers).await
    }
}

impl<S: AsyncRead + AsyncWrite> AsyncWrite for TlsStream<S> {
    #[inline]
    async fn write<B: IoBuf>(&mut self, buffer: B) -> BufResult<usize, B> {
        self.engine.write(buffer).await
    }

    #[inline]
    async fn write_vectored<V: IoVectoredBuf>(&mut self, buffers: V) -> BufResult<usize, V> {
        self.engine.write_vectored(buffers).await
    }

    #[inline]
    async fn flush(&mut self) -> io::Result<()> {
        self.engine.flush().await
    }

    #[inline]
    async fn shutdown(&mut self) -> io::Result<()> {
        self.engine.shutdown().await
    }
}
