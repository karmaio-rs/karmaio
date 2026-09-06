//! Server-side TLS support.

use std::fmt;
use std::io;
use std::sync::Arc;

use rustls::pki_types::CertificateDer;
use rustls::{Connection, HandshakeKind, ProtocolVersion, ServerConfig, ServerConnection, SupportedCipherSuite};

use crate::buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut};
use crate::io::{AsyncRead, AsyncWrite, IntoOwnedSplit, ReuniteError, ReuniteOwned};

use super::engine::Engine;
use super::split;

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
        self.get_ref().1.alpn_protocol()
    }

    /// Returns the authenticated client certificate chain, if client
    /// authentication was configured and completed.
    pub fn peer_certificates(&self) -> Option<&[CertificateDer<'static>]> {
        self.get_ref().1.peer_certificates()
    }

    /// Returns the negotiated TLS protocol version.
    pub fn protocol_version(&self) -> Option<ProtocolVersion> {
        self.get_ref().1.protocol_version()
    }

    /// Returns the negotiated cipher suite.
    pub fn negotiated_cipher_suite(&self) -> Option<SupportedCipherSuite> {
        self.get_ref().1.negotiated_cipher_suite()
    }

    /// Returns whether the connection used a full or resumed handshake.
    pub fn handshake_kind(&self) -> Option<HandshakeKind> {
        self.get_ref().1.handshake_kind()
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
        self.get_ref().1.export_keying_material(output, label, context)
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

impl<S: IntoOwnedSplit + 'static> TlsStream<S> {
    /// Splits the TLS stream into independently progressing owned halves.
    ///
    /// After splitting, reads queue protocol-generated TLS output for the
    /// write half to drain during its next write, flush, or shutdown. Inspect
    /// connection metadata before splitting or after reuniting the halves.
    pub fn into_split(self) -> (OwnedReadHalf<S>, OwnedWriteHalf<S>) {
        <Self as IntoOwnedSplit>::into_split(self)
    }
}

/// Owned read half of a server TLS stream.
///
/// Reads decrypt input without performing transport writes. Rustls control
/// output is left for the matching write half. Dropping this half abandons the
/// TLS read direction but does not invalidate the write direction.
pub struct OwnedReadHalf<S: IntoOwnedSplit> {
    inner: split::ReadHalf<S::ReadHalf>,
}

impl<S: IntoOwnedSplit> fmt::Debug for OwnedReadHalf<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ServerTlsReadHalf").finish_non_exhaustive()
    }
}

impl<S: IntoOwnedSplit> AsyncRead for OwnedReadHalf<S> {
    #[inline]
    async fn read<B: IoBufMut>(&mut self, buffer: B) -> BufResult<usize, B> {
        self.inner.read(buffer).await
    }

    #[inline]
    async fn read_vectored<V: IoVectoredBufMut>(&mut self, buffers: V) -> BufResult<usize, V> {
        self.inner.read_vectored(buffers).await
    }
}

impl<S: ReuniteOwned> OwnedReadHalf<S> {
    /// Returns whether `other` came from the same TLS split operation.
    pub fn is_pair_of(&self, other: &OwnedWriteHalf<S>) -> bool {
        split::is_pair_of(&self.inner, &other.inner)
    }

    /// Attempts to reconstruct the server TLS stream from matching halves.
    ///
    /// On failure, both halves are returned unchanged and remain usable. A
    /// reunion can fail when the halves do not match or the underlying
    /// transport still has detached ownership.
    pub fn reunite(self, other: OwnedWriteHalf<S>) -> Result<TlsStream<S>, ReuniteError<Self, OwnedWriteHalf<S>>> {
        split::reunite::<S>(self.inner, other.inner)
            .map(|engine| TlsStream { engine })
            .map_err(|error| error.map_halves(|inner| Self { inner }, |inner| OwnedWriteHalf { inner }))
    }
}

/// Owned write half of a server TLS stream.
///
/// Writes and flushes first send any control output queued by the read half.
/// Call [`AsyncWrite::shutdown`] for a graceful `close_notify`; dropping this
/// half without shutdown is an abrupt TLS close.
pub struct OwnedWriteHalf<S: IntoOwnedSplit> {
    inner: split::WriteHalf<S::WriteHalf>,
}

impl<S: IntoOwnedSplit> fmt::Debug for OwnedWriteHalf<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ServerTlsWriteHalf").finish_non_exhaustive()
    }
}

impl<S: IntoOwnedSplit> AsyncWrite for OwnedWriteHalf<S> {
    #[inline]
    async fn write<B: IoBuf>(&mut self, buffer: B) -> BufResult<usize, B> {
        self.inner.write(buffer).await
    }

    #[inline]
    async fn write_vectored<V: IoVectoredBuf>(&mut self, buffers: V) -> BufResult<usize, V> {
        self.inner.write_vectored(buffers).await
    }

    #[inline]
    async fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().await
    }

    #[inline]
    async fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown().await
    }
}

impl<S: ReuniteOwned> OwnedWriteHalf<S> {
    /// Returns whether `other` came from the same TLS split operation.
    pub fn is_pair_of(&self, other: &OwnedReadHalf<S>) -> bool {
        other.is_pair_of(self)
    }

    /// Attempts to reconstruct the server TLS stream from matching halves.
    ///
    /// On failure, both halves are returned unchanged and remain usable. See
    /// [`OwnedReadHalf::reunite`] for the matching and quiescence requirements.
    pub fn reunite(self, other: OwnedReadHalf<S>) -> Result<TlsStream<S>, ReuniteError<OwnedReadHalf<S>, Self>> {
        other.reunite(self)
    }
}

impl<S: IntoOwnedSplit + 'static> IntoOwnedSplit for TlsStream<S> {
    type ReadHalf = OwnedReadHalf<S>;
    type WriteHalf = OwnedWriteHalf<S>;

    fn into_split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        let (read, write) = split::split(self.engine);
        (OwnedReadHalf { inner: read }, OwnedWriteHalf { inner: write })
    }
}

impl<S: ReuniteOwned + 'static> ReuniteOwned for TlsStream<S> {
    fn reunite(
        read: Self::ReadHalf,
        write: Self::WriteHalf,
    ) -> Result<Self, ReuniteError<Self::ReadHalf, Self::WriteHalf>> {
        read.reunite(write)
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
