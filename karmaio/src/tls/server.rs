//! Server-side TLS support.

use std::fmt;
use std::io;
use std::sync::Arc;

use rustls::pki_types::CertificateDer;
use rustls::{Connection, HandshakeKind, ProtocolVersion, ServerConfig, ServerConnection, SupportedCipherSuite};

use crate::buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut};
use crate::io::{AsyncRead, AsyncWrite};

use super::engine::Engine;

/// Accepts server-side TLS connections using a shared Rustls configuration.
#[derive(Clone)]
pub struct TlsAcceptor {
    config: Arc<ServerConfig>,
}

impl TlsAcceptor {
    /// Creates an acceptor that uses `config` for every new TLS connection.
    pub fn new(config: Arc<ServerConfig>) -> Self {
        Self { config }
    }

    /// Returns the shared Rustls configuration used for new connections.
    pub fn config(&self) -> &Arc<ServerConfig> {
        &self.config
    }

    /// Performs a server handshake over `stream`.
    ///
    /// The returned stream has sent all final handshake records. On failure,
    /// the incomplete transport is dropped after all owned buffers have been
    /// recovered.
    pub async fn accept<S>(&self, stream: S) -> io::Result<TlsStream<S>>
    where
        S: AsyncRead + AsyncWrite,
    {
        let connection = ServerConnection::new(self.config.clone())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let engine = Engine::new(stream, Connection::Server(connection)).handshake().await?;
        Ok(TlsStream { engine })
    }
}

impl From<Arc<ServerConfig>> for TlsAcceptor {
    #[inline]
    fn from(config: Arc<ServerConfig>) -> Self {
        Self::new(config)
    }
}

impl fmt::Debug for TlsAcceptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("TlsAcceptor").finish_non_exhaustive()
    }
}

/// An established server-side TLS stream.
pub struct TlsStream<S> {
    engine: Engine<S>,
}

impl<S> TlsStream<S> {
    /// Returns immutable references to the transport and Rustls connection.
    pub fn get_ref(&self) -> (&S, &ServerConnection) {
        let (stream, connection) = self.engine.get_ref();
        match connection {
            Connection::Server(connection) => (stream, connection),
            Connection::Client(_) => unreachable!("server TLS stream contains a client connection"),
        }
    }

    /// Returns the negotiated ALPN protocol, if the peers selected one.
    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        self.engine.alpn_protocol()
    }

    /// Returns the authenticated client certificate chain, if client
    /// authentication was configured and completed.
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

    /// Returns the server name supplied by the client through SNI.
    pub fn server_name(&self) -> Option<&str> {
        self.get_ref().1.server_name()
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
    ///
    /// # Errors
    ///
    /// Returns an error and drops both parts if either direction has failed or
    /// an in-flight transport operation was abandoned.
    pub fn into_parts(self) -> io::Result<(S, ServerConnection)> {
        let (stream, connection) = self.engine.into_parts()?;
        match connection {
            Connection::Server(connection) => Ok((stream, connection)),
            Connection::Client(_) => unreachable!("server TLS stream contains a client connection"),
        }
    }
}

impl<S> fmt::Debug for TlsStream<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ServerTlsStream").finish_non_exhaustive()
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
