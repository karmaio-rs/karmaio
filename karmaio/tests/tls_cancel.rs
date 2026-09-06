#![cfg(all(feature = "macros", feature = "net", feature = "tls-ring"))]

use std::cell::Cell;
use std::future::{Future, poll_fn};
use std::io::{self, Cursor, Write as _};
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use karmaio::buf::{BufResult, IoBuf, IoBufMut};
use karmaio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, IntoOwnedSplit};
use karmaio::net::tcp::{TcpListener, TcpStream};
use karmaio::runtime::{CancellationSource, FutureExt, is_operation_canceled, spawn_local};
use karmaio::time::{sleep, timeout};
use karmaio::tls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use karmaio::tls::rustls::{ClientConfig, RootCertStore, ServerConfig, version};
use karmaio::tls::{TlsAcceptor, TlsConnector};

const CA_DER: &[u8] = include_bytes!("fixtures/tls/ca.der");
const LOCALHOST_DER: &[u8] = include_bytes!("fixtures/tls/localhost.der");
const LOCALHOST_KEY_DER: &[u8] = include_bytes!("fixtures/tls/localhost-key.der");

#[derive(Clone, Copy, Eq, PartialEq)]
enum BlockPhase {
    None,
    Write,
    Flush,
    Shutdown,
}

struct BlockControl {
    phase: Cell<BlockPhase>,
    calls: Cell<usize>,
    write_attempts: Cell<usize>,
}

struct BlockingTransport {
    inner: TcpStream,
    blocker: TcpStream,
    _blocker_peer: TcpStream,
    control: Rc<BlockControl>,
}

impl BlockingTransport {
    async fn new(inner: TcpStream) -> (Self, Rc<BlockControl>) {
        let (listener, address) = bind();
        let connector = spawn_local(async move { TcpStream::connect(address).await.unwrap() });
        let (blocker_peer, _) = listener.accept().await.unwrap();
        let blocker = connector.await.unwrap();
        let control = Rc::new(BlockControl {
            phase: Cell::new(BlockPhase::None),
            calls: Cell::new(0),
            write_attempts: Cell::new(0),
        });
        (
            Self {
                inner,
                blocker,
                _blocker_peer: blocker_peer,
                control: control.clone(),
            },
            control,
        )
    }
}

async fn block_until_canceled(control: &BlockControl, blocker: &mut TcpStream) -> io::Error {
    control.calls.set(control.calls.get() + 1);
    let BufResult(result, _) = blocker.read(Vec::with_capacity(1)).await;
    match result {
        Ok(_) => io::Error::other("quiet cancellation socket unexpectedly became readable"),
        Err(error) => error,
    }
}

impl AsyncRead for BlockingTransport {
    async fn read<B: IoBufMut>(&mut self, buffer: B) -> BufResult<usize, B> {
        self.inner.read(buffer).await
    }
}

impl AsyncWrite for BlockingTransport {
    async fn write<B: IoBuf>(&mut self, buffer: B) -> BufResult<usize, B> {
        self.control.write_attempts.set(self.control.write_attempts.get() + 1);
        if self.control.phase.get() == BlockPhase::Write {
            let error = block_until_canceled(&self.control, &mut self.blocker).await;
            return BufResult(Err(error), buffer);
        }
        self.inner.write(buffer).await
    }

    async fn flush(&mut self) -> io::Result<()> {
        if self.control.phase.get() == BlockPhase::Flush {
            return Err(block_until_canceled(&self.control, &mut self.blocker).await);
        }
        self.inner.flush().await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        if self.control.phase.get() == BlockPhase::Shutdown {
            return Err(block_until_canceled(&self.control, &mut self.blocker).await);
        }
        AsyncWrite::shutdown(&mut self.inner).await
    }
}

struct BlockingReadHalf {
    inner: <TcpStream as IntoOwnedSplit>::ReadHalf,
}

struct BlockingWriteHalf {
    inner: <TcpStream as IntoOwnedSplit>::WriteHalf,
    blocker: TcpStream,
    _blocker_peer: TcpStream,
    control: Rc<BlockControl>,
}

impl AsyncRead for BlockingReadHalf {
    async fn read<B: IoBufMut>(&mut self, buffer: B) -> BufResult<usize, B> {
        self.inner.read(buffer).await
    }
}

impl AsyncWrite for BlockingWriteHalf {
    async fn write<B: IoBuf>(&mut self, buffer: B) -> BufResult<usize, B> {
        self.control.write_attempts.set(self.control.write_attempts.get() + 1);
        if self.control.phase.get() == BlockPhase::Write {
            let error = block_until_canceled(&self.control, &mut self.blocker).await;
            return BufResult(Err(error), buffer);
        }
        self.inner.write(buffer).await
    }

    async fn flush(&mut self) -> io::Result<()> {
        if self.control.phase.get() == BlockPhase::Flush {
            return Err(block_until_canceled(&self.control, &mut self.blocker).await);
        }
        self.inner.flush().await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        if self.control.phase.get() == BlockPhase::Shutdown {
            return Err(block_until_canceled(&self.control, &mut self.blocker).await);
        }
        self.inner.shutdown().await
    }
}

impl IntoOwnedSplit for BlockingTransport {
    type ReadHalf = BlockingReadHalf;
    type WriteHalf = BlockingWriteHalf;

    fn into_split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        let (read, write) = self.inner.into_split();
        (
            BlockingReadHalf { inner: read },
            BlockingWriteHalf {
                inner: write,
                blocker: self.blocker,
                _blocker_peer: self._blocker_peer,
                control: self.control,
            },
        )
    }
}

fn bind() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let address = listener.local_addr().unwrap();
    (listener, address)
}

fn configs() -> (Arc<ClientConfig>, Arc<ServerConfig>) {
    let provider = || Arc::new(karmaio::tls::rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(CA_DER.to_vec())).unwrap();
    let client = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(LOCALHOST_KEY_DER.to_vec()));
    let server = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(LOCALHOST_DER.to_vec())], key)
        .unwrap();
    (Arc::new(client), Arc::new(server))
}

fn cancellation_after(duration: Duration) -> karmaio::runtime::CancellationToken {
    let source = CancellationSource::new();
    let token = source.token();
    spawn_local(async move {
        sleep(duration).await;
        source.cancel();
    });
    token
}

async fn poll_pending_once<F: Future>(mut future: Pin<&mut F>) {
    poll_fn(|context| match future.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("operation unexpectedly completed"),
    })
    .await;
}

#[karmaio::test]
async fn cancel_pending_client_handshake_is_recognizable() {
    let (client, _) = configs();
    let (listener, address) = bind();
    let _server = spawn_local(async move {
        let _socket = listener.accept().await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let socket = TcpStream::connect(address).await.unwrap();
    let source = CancellationSource::new();
    let token = source.token();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        source.cancel();
    });

    let name = ServerName::try_from("localhost").unwrap();
    let error = TlsConnector::new(client)
        .connect(name, socket)
        .with_cancellation(token)
        .await
        .unwrap_err();
    assert!(is_operation_canceled(&error), "{error:?}");
}

#[karmaio::test]
async fn cancel_pending_server_handshake_is_recognizable() {
    let (_, server) = configs();
    let (listener, address) = bind();
    let client = spawn_local(async move {
        let _socket = TcpStream::connect(address).await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });
    let (socket, _) = listener.accept().await.unwrap();

    let source = CancellationSource::new();
    let token = source.token();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        source.cancel();
    });

    let error = TlsAcceptor::new(server)
        .accept(socket)
        .with_cancellation(token)
        .await
        .unwrap_err();
    assert!(is_operation_canceled(&error), "{error:?}");
    client.abort();
}

#[karmaio::test]
async fn cancel_client_handshake_during_output_write_is_recognizable() {
    let (client, _) = configs();
    let (listener, address) = bind();
    let server = spawn_local(async move {
        let _socket = listener.accept().await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let socket = TcpStream::connect(address).await.unwrap();
    let (transport, control) = BlockingTransport::new(socket).await;
    control.phase.set(BlockPhase::Write);
    let token = cancellation_after(Duration::from_millis(20));
    let name = ServerName::try_from("localhost").unwrap();
    let error = TlsConnector::new(client)
        .connect(name, transport)
        .with_cancellation(token)
        .await
        .unwrap_err();
    assert!(is_operation_canceled(&error), "{error:?}");
    assert_eq!(control.calls.get(), 1);
    server.abort();
}

#[karmaio::test]
async fn cancel_established_read_returns_buffer_and_poisons_stream() {
    let (client, server) = configs();
    let (listener, address) = bind();
    let server_task = spawn_local(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let socket = TcpStream::connect(address).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let mut stream = TlsConnector::new(client).connect(name, socket).await.unwrap();

    let source = CancellationSource::new();
    let token = source.token();
    spawn_local(async move {
        sleep(Duration::from_millis(20)).await;
        source.cancel();
    });

    let buffer = Vec::with_capacity(32);
    let pointer = buffer.as_ptr();
    let BufResult(result, buffer) = stream.read(buffer).with_cancellation(token).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    assert_eq!(buffer.as_ptr(), pointer);

    let BufResult(result, _) = stream.read(Vec::with_capacity(8)).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    server_task.abort();
}

#[karmaio::test]
async fn cancel_blocked_write_returns_buffer_and_poisons_stream() {
    let (client, server) = configs();
    let (listener, address) = bind();
    let server_task = spawn_local(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let socket = TcpStream::connect(address).await.unwrap();
    let (transport, control) = BlockingTransport::new(socket).await;
    let name = ServerName::try_from("localhost").unwrap();
    let mut stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
    control.phase.set(BlockPhase::Write);

    let buffer = vec![7; 16 * 1024];
    let pointer = buffer.as_ptr();
    let BufResult(result, buffer) = stream
        .write(buffer)
        .with_cancellation(cancellation_after(Duration::from_millis(20)))
        .await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    assert_eq!(buffer.as_ptr(), pointer);
    assert_eq!(control.calls.get(), 1);

    let BufResult(result, _) = stream.write(vec![1]).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    server_task.abort();
}

#[karmaio::test]
async fn cancel_flush_and_shutdown_poison_the_stream() {
    let (client, server) = configs();

    let (listener, address) = bind();
    let server_task = spawn_local({
        let server = server.clone();
        async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
            sleep(Duration::from_secs(30)).await;
        }
    });
    let socket = TcpStream::connect(address).await.unwrap();
    let (transport, control) = BlockingTransport::new(socket).await;
    let name = ServerName::try_from("localhost").unwrap();
    let mut stream = TlsConnector::new(client.clone())
        .connect(name, transport)
        .await
        .unwrap();
    control.phase.set(BlockPhase::Flush);
    let error = stream
        .flush()
        .with_cancellation(cancellation_after(Duration::from_millis(20)))
        .await
        .unwrap_err();
    assert!(is_operation_canceled(&error), "{error:?}");
    assert_eq!(control.calls.get(), 1);
    let BufResult(result, _) = stream.write(vec![1]).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    let BufResult(result, _) = stream.read(Vec::with_capacity(1)).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    server_task.abort();

    let (listener, address) = bind();
    let server_task = spawn_local(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });
    let socket = TcpStream::connect(address).await.unwrap();
    let (transport, control) = BlockingTransport::new(socket).await;
    let name = ServerName::try_from("localhost").unwrap();
    let mut stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
    control.phase.set(BlockPhase::Shutdown);
    let error = stream
        .shutdown()
        .with_cancellation(cancellation_after(Duration::from_millis(20)))
        .await
        .unwrap_err();
    assert!(is_operation_canceled(&error), "{error:?}");
    assert_eq!(control.calls.get(), 1);
    let BufResult(result, _) = stream.write(vec![1]).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    let BufResult(result, _) = stream.read(Vec::with_capacity(1)).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    server_task.abort();
}

#[karmaio::test]
async fn cancel_read_generated_key_update_write_returns_the_read_buffer() {
    let (client, server) = configs();
    let send_update = Rc::new(Cell::new(false));
    let update_sent = Rc::new(Cell::new(false));
    let (listener, address) = bind();
    let server_task = spawn_local({
        let send_update = send_update.clone();
        let update_sent = update_sent.clone();
        async move {
            let (socket, _) = listener.accept().await.unwrap();
            let stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
            while !send_update.get() {
                sleep(Duration::from_millis(1)).await;
            }
            let (mut socket, mut connection) = stream.into_parts().unwrap();
            connection.refresh_traffic_keys().unwrap();
            let mut output = Vec::new();
            while connection.wants_write() {
                connection.write_tls(&mut output).unwrap();
            }
            let BufResult(result, _) = socket.write_all(output).await;
            result.unwrap();
            update_sent.set(true);
            sleep(Duration::from_secs(30)).await;
        }
    });

    let socket = TcpStream::connect(address).await.unwrap();
    let (transport, control) = BlockingTransport::new(socket).await;
    let name = ServerName::try_from("localhost").unwrap();
    let mut stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
    control.phase.set(BlockPhase::Write);
    send_update.set(true);
    while !update_sent.get() {
        sleep(Duration::from_millis(1)).await;
    }

    let buffer = Vec::with_capacity(32);
    let pointer = buffer.as_ptr();
    let BufResult(result, buffer) = stream
        .read(buffer)
        .with_cancellation(cancellation_after(Duration::from_millis(20)))
        .await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    assert_eq!(buffer.as_ptr(), pointer);
    assert_eq!(control.calls.get(), 1);
    let BufResult(result, _) = stream.write(vec![1]).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    let BufResult(result, _) = stream.read(Vec::with_capacity(1)).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    server_task.abort();
}

#[karmaio::test]
async fn cancel_fatal_alert_drain_preserves_the_protocol_error() {
    let (client, server) = configs();
    let send_invalid_record = Rc::new(Cell::new(false));
    let invalid_record_sent = Rc::new(Cell::new(false));
    let (listener, address) = bind();
    let server_task = spawn_local({
        let send_invalid_record = send_invalid_record.clone();
        let invalid_record_sent = invalid_record_sent.clone();
        async move {
            let (socket, _) = listener.accept().await.unwrap();
            let stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
            while !send_invalid_record.get() {
                sleep(Duration::from_millis(1)).await;
            }
            let (mut socket, _) = stream.into_parts().unwrap();
            // An unencrypted, unknown handshake message is invalid after the
            // TLS 1.3 handshake and causes Rustls to queue a fatal alert.
            let invalid_record = vec![22, 3, 3, 0, 4, 0xff, 0, 0, 0];
            let BufResult(result, _) = socket.write_all(invalid_record).await;
            result.unwrap();
            invalid_record_sent.set(true);
            sleep(Duration::from_secs(30)).await;
        }
    });

    let socket = TcpStream::connect(address).await.unwrap();
    let (transport, control) = BlockingTransport::new(socket).await;
    let name = ServerName::try_from("localhost").unwrap();
    let mut stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
    control.phase.set(BlockPhase::Write);
    send_invalid_record.set(true);
    while !invalid_record_sent.get() {
        sleep(Duration::from_millis(1)).await;
    }

    let buffer = Vec::with_capacity(32);
    let pointer = buffer.as_ptr();
    let BufResult(result, buffer) = stream
        .read(buffer)
        .with_cancellation(cancellation_after(Duration::from_millis(20)))
        .await;
    let error = result.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(!is_operation_canceled(&error));
    assert_eq!(buffer.as_ptr(), pointer);
    assert_eq!(control.calls.get(), 1);

    let BufResult(result, _) = stream.read(Vec::with_capacity(1)).await;
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    let BufResult(result, _) = stream.write(vec![1]).await;
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    server_task.abort();
}

#[karmaio::test]
async fn client_and_server_tls_halves_run_in_separate_local_tasks() {
    let (client, server) = configs();
    let (listener, address) = bind();
    let server_task = spawn_local(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
        let (mut read, mut write) = stream.into_split();
        let reader = spawn_local(async move {
            let (result, request) = read.read_exact(vec![0; 5]).await.into_parts();
            result.unwrap();
            assert_eq!(request, b"hello");
        });
        let writer = spawn_local(async move {
            write.write_all(b"early".to_vec()).await.unwrap();
        });
        writer.await.unwrap();
        reader.await.unwrap();
    });

    let socket = TcpStream::connect(address).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let stream = TlsConnector::new(client).connect(name, socket).await.unwrap();
    let (mut read, mut write) = stream.into_split();
    let reader = spawn_local(async move {
        let (result, response) = read.read_exact(vec![0; 5]).await.into_parts();
        result.unwrap();
        assert_eq!(response, b"early");
    });
    let writer = spawn_local(async move {
        write.write_all(b"hello".to_vec()).await.unwrap();
    });

    writer.await.unwrap();
    reader.await.unwrap();
    server_task.await.unwrap();
}

#[karmaio::test]
async fn blocked_tls_write_does_not_prevent_plaintext_read() {
    let (client, server) = configs();
    let (listener, address) = bind();
    let server_task = spawn_local(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
        stream.write_all(b"read while write blocked".to_vec()).await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let socket = TcpStream::connect(address).await.unwrap();
    let (transport, control) = BlockingTransport::new(socket).await;
    let name = ServerName::try_from("localhost").unwrap();
    let stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
    let (mut read, mut write) = stream.into_split();
    control.phase.set(BlockPhase::Write);
    let writer = spawn_local(async move {
        write
            .write(b"blocked".to_vec())
            .with_cancellation(cancellation_after(Duration::from_millis(100)))
            .await
    });

    let (result, plaintext) = read.read_exact(vec![0; 24]).await.into_parts();
    result.unwrap();
    assert_eq!(plaintext, b"read while write blocked");
    let BufResult(result, payload) = writer.await.unwrap();
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    assert_eq!(payload, b"blocked");
    server_task.abort();
}

#[karmaio::test]
async fn split_read_cancellation_returns_buffer_and_poisons_both_halves() {
    let (client, server) = configs();
    let (listener, address) = bind();
    let server_task = spawn_local(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let socket = TcpStream::connect(address).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let stream = TlsConnector::new(client).connect(name, socket).await.unwrap();
    let (mut read, mut write) = stream.into_split();
    let buffer = Vec::with_capacity(32);
    let pointer = buffer.as_ptr();
    let BufResult(result, buffer) = read
        .read(buffer)
        .with_cancellation(cancellation_after(Duration::from_millis(20)))
        .await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    assert_eq!(buffer.as_ptr(), pointer);

    let BufResult(result, _) = write.write(vec![1]).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    server_task.abort();
}

#[karmaio::test]
async fn split_write_cancellation_returns_buffer_and_poisons_both_halves() {
    let (client, server) = configs();
    let (listener, address) = bind();
    let server_task = spawn_local(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let socket = TcpStream::connect(address).await.unwrap();
    let (transport, control) = BlockingTransport::new(socket).await;
    let name = ServerName::try_from("localhost").unwrap();
    let stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
    let (mut read, mut write) = stream.into_split();
    control.phase.set(BlockPhase::Write);
    let buffer = vec![7; 16 * 1024];
    let pointer = buffer.as_ptr();
    let BufResult(result, buffer) = write
        .write(buffer)
        .with_cancellation(cancellation_after(Duration::from_millis(20)))
        .await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    assert_eq!(buffer.as_ptr(), pointer);
    assert_eq!(control.calls.get(), 1);

    let BufResult(result, _) = read.read(Vec::with_capacity(1)).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    server_task.abort();
}

#[karmaio::test]
async fn split_abandonment_preserves_an_existing_cancellation() {
    let (client, server) = configs();
    let (listener, address) = bind();
    let server_task = spawn_local(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });

    let socket = TcpStream::connect(address).await.unwrap();
    let (transport, control) = BlockingTransport::new(socket).await;
    let name = ServerName::try_from("localhost").unwrap();
    let stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
    let (mut read, mut write) = stream.into_split();

    let mut pending_read = Box::pin(read.read(Vec::with_capacity(1)));
    poll_pending_once(pending_read.as_mut()).await;

    control.phase.set(BlockPhase::Write);
    let BufResult(result, _) = write
        .write(vec![1])
        .with_cancellation(cancellation_after(Duration::from_millis(20)))
        .await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");

    // Dropping the opposite in-flight operation must not replace the
    // cancellation that already made the shared TLS session unusable.
    drop(pending_read);
    let BufResult(result, _) = read.read(Vec::with_capacity(1)).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    let BufResult(result, _) = write.write(vec![1]).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    server_task.abort();
}

#[karmaio::test]
async fn split_flush_and_shutdown_cancellation_poison_both_halves() {
    let (client, server) = configs();

    let (listener, address) = bind();
    let server_task = spawn_local({
        let server = Arc::clone(&server);
        async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
            sleep(Duration::from_secs(30)).await;
        }
    });
    let socket = TcpStream::connect(address).await.unwrap();
    let (transport, control) = BlockingTransport::new(socket).await;
    let name = ServerName::try_from("localhost").unwrap();
    let stream = TlsConnector::new(Arc::clone(&client))
        .connect(name, transport)
        .await
        .unwrap();
    let (mut read, mut write) = stream.into_split();
    control.phase.set(BlockPhase::Flush);
    let error = write
        .flush()
        .with_cancellation(cancellation_after(Duration::from_millis(20)))
        .await
        .unwrap_err();
    assert!(is_operation_canceled(&error), "{error:?}");
    let BufResult(result, _) = read.read(Vec::with_capacity(1)).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    let BufResult(result, _) = write.write(vec![1]).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    server_task.abort();

    let (listener, address) = bind();
    let server_task = spawn_local(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
        sleep(Duration::from_secs(30)).await;
    });
    let socket = TcpStream::connect(address).await.unwrap();
    let (transport, control) = BlockingTransport::new(socket).await;
    let name = ServerName::try_from("localhost").unwrap();
    let stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
    let (mut read, mut write) = stream.into_split();
    control.phase.set(BlockPhase::Shutdown);
    let error = write
        .shutdown()
        .with_cancellation(cancellation_after(Duration::from_millis(20)))
        .await
        .unwrap_err();
    assert!(is_operation_canceled(&error), "{error:?}");
    let BufResult(result, _) = read.read(Vec::with_capacity(1)).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    let BufResult(result, _) = write.write(vec![1]).await;
    assert!(is_operation_canceled(result.as_ref().unwrap_err()), "{result:?}");
    server_task.abort();
}

#[karmaio::test]
async fn split_read_defers_key_update_response_to_writer() {
    let (client, server) = configs();
    let send_update = Rc::new(Cell::new(false));
    let (listener, address) = bind();
    let server_task = spawn_local({
        let send_update = Rc::clone(&send_update);
        async move {
            let (socket, _) = listener.accept().await.unwrap();
            let stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
            while !send_update.get() {
                sleep(Duration::from_millis(1)).await;
            }

            let (mut socket, mut connection) = stream.into_parts().unwrap();
            connection.refresh_traffic_keys().unwrap();
            connection.writer().write_all(b"after key update").unwrap();
            let mut output = Vec::new();
            while connection.wants_write() {
                connection.write_tls(&mut output).unwrap();
            }
            socket.write_all(output).await.unwrap();

            let BufResult(result, input) = timeout(Duration::from_secs(1), socket.read(Vec::with_capacity(4096)))
                .await
                .expect("client did not send its KeyUpdate response");
            let received = result.unwrap();
            assert!(received > 0);
            let mut input = Cursor::new(input.as_slice());
            assert_eq!(connection.read_tls(&mut input).unwrap(), received);
            connection.process_new_packets().unwrap();
        }
    });

    let socket = TcpStream::connect(address).await.unwrap();
    let (transport, control) = BlockingTransport::new(socket).await;
    let name = ServerName::try_from("localhost").unwrap();
    let stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
    let (mut read, mut write) = stream.into_split();
    let writes_after_handshake = control.write_attempts.get();
    control.phase.set(BlockPhase::Write);
    send_update.set(true);

    let (result, plaintext) = read.read_exact(vec![0; 16]).await.into_parts();
    result.unwrap();
    assert_eq!(plaintext, b"after key update");
    assert_eq!(control.write_attempts.get(), writes_after_handshake);

    control.phase.set(BlockPhase::None);
    write.flush().await.unwrap();
    server_task.await.unwrap();
}

#[karmaio::test]
async fn split_fatal_alert_is_deferred_and_original_error_is_sticky() {
    let (client, server) = configs();
    let send_invalid_record = Rc::new(Cell::new(false));
    let (listener, address) = bind();
    let server_task = spawn_local({
        let send_invalid_record = Rc::clone(&send_invalid_record);
        async move {
            let (socket, _) = listener.accept().await.unwrap();
            let stream = TlsAcceptor::new(server).accept(socket).await.unwrap();
            while !send_invalid_record.get() {
                sleep(Duration::from_millis(1)).await;
            }
            let (mut socket, _) = stream.into_parts().unwrap();
            let invalid_record = vec![22, 3, 3, 0, 4, 0xff, 0, 0, 0];
            socket.write_all(invalid_record).await.unwrap();

            let BufResult(result, _) = timeout(Duration::from_secs(1), socket.read(Vec::with_capacity(4096)))
                .await
                .expect("client did not send its fatal alert");
            assert!(result.unwrap() > 0);
        }
    });

    let socket = TcpStream::connect(address).await.unwrap();
    let (transport, control) = BlockingTransport::new(socket).await;
    let name = ServerName::try_from("localhost").unwrap();
    let stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
    let (mut read, mut write) = stream.into_split();
    let writes_after_handshake = control.write_attempts.get();
    control.phase.set(BlockPhase::Write);
    send_invalid_record.set(true);

    let BufResult(result, _) = read.read(Vec::with_capacity(32)).await;
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(control.write_attempts.get(), writes_after_handshake);

    control.phase.set(BlockPhase::None);
    let payload = b"must not be accepted".to_vec();
    let pointer = payload.as_ptr();
    let BufResult(result, payload) = write.write(payload).await;
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(payload.as_ptr(), pointer);
    let BufResult(result, _) = read.read(Vec::with_capacity(1)).await;
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    let BufResult(result, _) = write.write(vec![1]).await;
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);

    server_task.await.unwrap();
}
