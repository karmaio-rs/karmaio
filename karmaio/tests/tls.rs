#![cfg(feature = "tls-ring")]

use std::any::Any;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::{Future, pending, poll_fn};
use std::io::{self, Cursor, Read, Write};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::Poll;

use karmaio::Runtime;
use karmaio::buf::{BufResult, IoBuf, IoBufMut, Slice};
use karmaio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use karmaio::tls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use karmaio::tls::rustls::{
    ClientConfig, ClientConnection, Connection, HandshakeKind, ProtocolVersion, RootCertStore, ServerConfig,
    ServerConnection, SupportedProtocolVersion, version,
};
use karmaio::tls::{TlsAcceptor, TlsConnector};

const CA_DER: &[u8] = include_bytes!("fixtures/tls/ca.der");
const LOCALHOST_DER: &[u8] = include_bytes!("fixtures/tls/localhost.der");
const LOCALHOST_KEY_DER: &[u8] = include_bytes!("fixtures/tls/localhost-key.der");

fn provider() -> Arc<karmaio::tls::rustls::crypto::CryptoProvider> {
    Arc::new(karmaio::tls::rustls::crypto::ring::default_provider())
}

fn configs() -> (Arc<ClientConfig>, Arc<ServerConfig>) {
    configs_for(&[&version::TLS13])
}

fn configs_for(versions: &[&'static SupportedProtocolVersion]) -> (Arc<ClientConfig>, Arc<ServerConfig>) {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(CA_DER.to_vec())).unwrap();

    let mut client = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(versions)
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(LOCALHOST_KEY_DER.to_vec()));
    let mut server = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(versions)
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(LOCALHOST_DER.to_vec())], key)
        .unwrap();
    server.alpn_protocols = vec![b"h2".to_vec()];

    (Arc::new(client), Arc::new(server))
}

struct ScriptState {
    peer: Connection,
    to_adapter: VecDeque<u8>,
    plaintext: Vec<u8>,
    read_limit: usize,
    write_limit: usize,
    eof: bool,
    zero_write: bool,
    read_error: Option<io::ErrorKind>,
    write_error: Option<io::ErrorKind>,
    pending_read: bool,
    pending_write: bool,
    pending_flush: bool,
    pending_shutdown: bool,
    read_calls: usize,
    write_calls: usize,
    flush_calls: usize,
    shutdown_calls: usize,
    peer_closed: bool,
    input_allocation: Option<usize>,
    output_allocation: Option<usize>,
}

impl ScriptState {
    fn queue_peer_output(&mut self) -> io::Result<()> {
        let mut output = Vec::new();
        while self.peer.wants_write() {
            let before = output.len();
            self.peer.write_tls(&mut output)?;
            if output.len() == before {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
        }
        self.to_adapter.extend(output);
        Ok(())
    }

    fn drain_peer_plaintext(&mut self) -> io::Result<()> {
        let mut plaintext = [0; 16 * 1024];
        loop {
            match self.peer.reader().read(&mut plaintext) {
                Ok(0) => {
                    self.peer_closed = true;
                    return Ok(());
                }
                Ok(read) => self.plaintext.extend_from_slice(&plaintext[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }

    fn receive_from_adapter(&mut self, input: &[u8]) -> io::Result<()> {
        let mut reader = Cursor::new(input);
        let read = self.peer.read_tls(&mut reader)?;
        if read != input.len() {
            return Err(io::Error::other("scripted Rustls peer did not consume transport input"));
        }
        self.peer
            .process_new_packets()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.queue_peer_output()?;
        self.drain_peer_plaintext()
    }

    fn send_plaintext(&mut self, plaintext: &[u8]) -> io::Result<()> {
        let mut offset = 0;
        while offset < plaintext.len() {
            let written = self.peer.writer().write(&plaintext[offset..])?;
            if written == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            offset += written;
            self.queue_peer_output()?;
        }
        Ok(())
    }

    fn close_cleanly(&mut self) -> io::Result<()> {
        self.peer.send_close_notify();
        self.queue_peer_output()
    }
}

#[derive(Clone)]
struct ScriptHandle(Rc<RefCell<ScriptState>>);

impl ScriptHandle {
    fn send_plaintext(&self, plaintext: &[u8]) {
        self.0.borrow_mut().send_plaintext(plaintext).unwrap();
    }

    fn close_cleanly(&self) {
        self.0.borrow_mut().close_cleanly().unwrap();
    }

    fn set_eof(&self) {
        self.0.borrow_mut().eof = true;
    }

    fn take_plaintext(&self) -> Vec<u8> {
        std::mem::take(&mut self.0.borrow_mut().plaintext)
    }
}

struct ScriptedTransport {
    state: Rc<RefCell<ScriptState>>,
}

impl ScriptedTransport {
    fn new(peer: Connection, read_limit: usize, write_limit: usize) -> (Self, ScriptHandle) {
        let state = Rc::new(RefCell::new(ScriptState {
            peer,
            to_adapter: VecDeque::new(),
            plaintext: Vec::new(),
            read_limit,
            write_limit,
            eof: false,
            zero_write: false,
            read_error: None,
            write_error: None,
            pending_read: false,
            pending_write: false,
            pending_flush: false,
            pending_shutdown: false,
            read_calls: 0,
            write_calls: 0,
            flush_calls: 0,
            shutdown_calls: 0,
            peer_closed: false,
            input_allocation: None,
            output_allocation: None,
        }));
        (Self { state: state.clone() }, ScriptHandle(state))
    }

    fn client(config: Arc<ClientConfig>, read_limit: usize, write_limit: usize) -> (Self, ScriptHandle) {
        let name = ServerName::try_from("localhost").unwrap();
        let peer = ClientConnection::new(config, name).unwrap();
        let (transport, handle) = Self::new(Connection::Client(peer), read_limit, write_limit);
        handle.0.borrow_mut().queue_peer_output().unwrap();
        (transport, handle)
    }

    fn server(config: Arc<ServerConfig>, read_limit: usize, write_limit: usize) -> (Self, ScriptHandle) {
        let peer = ServerConnection::new(config).unwrap();
        Self::new(Connection::Server(peer), read_limit, write_limit)
    }
}

impl AsyncRead for ScriptedTransport {
    async fn read<B: IoBufMut>(&mut self, mut buffer: B) -> BufResult<usize, B> {
        let allocation = (&buffer as &dyn Any)
            .downcast_ref::<Vec<u8>>()
            .expect("TLS engine reads with its Vec allocation");
        assert_eq!(allocation.capacity(), 18 * 1024);
        let pointer = allocation.as_ptr() as usize;
        let should_block = {
            let mut state = self.state.borrow_mut();
            state.read_calls += 1;
            match state.input_allocation {
                Some(expected) => assert_eq!(pointer, expected),
                None => state.input_allocation = Some(pointer),
            }
            state.pending_read
        };
        if should_block {
            pending::<()>().await;
            unreachable!("pending scripted read completed")
        }

        let mut state = self.state.borrow_mut();
        if let Some(kind) = state.read_error.take() {
            return BufResult(Err(io::Error::new(kind, "injected transport read failure")), buffer);
        }
        let count = state
            .read_limit
            .min(buffer.as_uninit().len())
            .min(state.to_adapter.len());

        if count == 0 && !state.eof {
            return BufResult(Err(io::Error::from(io::ErrorKind::WouldBlock)), buffer);
        }

        for destination in &mut buffer.as_uninit()[..count] {
            destination.write(state.to_adapter.pop_front().unwrap());
        }
        // Safety: the loop initialized exactly the first `count` bytes.
        unsafe { buffer.set_len(count) };
        BufResult(Ok(count), buffer)
    }
}

impl AsyncWrite for ScriptedTransport {
    async fn write<B: IoBuf>(&mut self, buffer: B) -> BufResult<usize, B> {
        let allocation = (&buffer as &dyn Any)
            .downcast_ref::<Slice<Vec<u8>>>()
            .expect("TLS engine writes with an owned slice of its Vec allocation")
            .get_ref();
        assert_eq!(allocation.capacity(), 18 * 1024);
        let pointer = allocation.as_ptr() as usize;
        let should_block = {
            let mut state = self.state.borrow_mut();
            state.write_calls += 1;
            match state.output_allocation {
                Some(expected) => assert_eq!(pointer, expected),
                None => state.output_allocation = Some(pointer),
            }
            state.pending_write
        };
        if should_block {
            pending::<()>().await;
            unreachable!("pending scripted write completed")
        }

        let mut state = self.state.borrow_mut();
        if let Some(kind) = state.write_error.take() {
            return BufResult(Err(io::Error::new(kind, "injected transport write failure")), buffer);
        }
        if state.zero_write {
            return BufResult(Ok(0), buffer);
        }
        let count = state.write_limit.min(buffer.as_init().len());
        let result = state.receive_from_adapter(&buffer.as_init()[..count]).map(|()| count);
        BufResult(result, buffer)
    }

    async fn flush(&mut self) -> io::Result<()> {
        let should_block = {
            let mut state = self.state.borrow_mut();
            state.flush_calls += 1;
            state.pending_flush
        };
        if should_block {
            pending::<()>().await;
            unreachable!("pending scripted flush completed")
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        let should_block = {
            let mut state = self.state.borrow_mut();
            state.shutdown_calls += 1;
            state.pending_shutdown
        };
        if should_block {
            pending::<()>().await;
            unreachable!("pending scripted shutdown completed")
        }
        Ok(())
    }
}

async fn poll_pending_once<F: Future>(mut future: Pin<&mut F>) {
    poll_fn(|context| match future.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("operation unexpectedly completed"),
    })
    .await;
}

#[test]
fn client_stream_handles_fragmentation_buffer_identity_and_shutdown() {
    let (client, server) = configs();
    let (transport, handle) = ScriptedTransport::server(server, 7, 11);
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let name = ServerName::try_from("localhost").unwrap();
        let mut stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
        assert_eq!(stream.alpn_protocol(), Some(&b"h2"[..]));
        assert_eq!(stream.peer_certificates().unwrap().len(), 1);
        assert_eq!(stream.protocol_version(), Some(ProtocolVersion::TLSv1_3));
        assert!(stream.negotiated_cipher_suite().is_some());
        assert_eq!(stream.handshake_kind(), Some(HandshakeKind::Full));

        let client_export = stream
            .export_keying_material(vec![0; 32], b"karmaio test exporter", Some(b"context"))
            .unwrap();
        let peer_export = handle
            .0
            .borrow()
            .peer
            .export_keying_material(vec![0; 32], b"karmaio test exporter", Some(b"context"))
            .unwrap();
        assert_eq!(client_export, peer_export);

        let outbound = vec![42; 20 * 1024];
        let outbound_pointer = outbound.as_ptr();
        let BufResult(result, outbound) = stream.write(outbound).await;
        assert_eq!(result.unwrap(), outbound.len());
        assert_eq!(outbound.as_ptr(), outbound_pointer);
        assert_eq!(handle.take_plaintext(), outbound);

        handle.send_plaintext(b"fragmented response");
        let inbound = Vec::with_capacity(64);
        let inbound_pointer = inbound.as_ptr();
        let BufResult(result, inbound) = stream.read(inbound).await;
        assert_eq!(result.unwrap(), 19);
        assert_eq!(inbound.as_ptr(), inbound_pointer);
        assert_eq!(inbound, b"fragmented response");

        stream.shutdown().await.unwrap();
        stream.shutdown().await.unwrap();
        {
            let state = handle.0.borrow();
            assert!(state.peer_closed);
            assert_eq!(state.flush_calls, 1);
            assert_eq!(state.shutdown_calls, 1);
            assert!(state.read_calls > 1);
            assert!(state.write_calls > 1);
        }

        let BufResult(result, _) = stream.write(vec![1]).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    });
}

#[test]
fn server_stream_completes_handshake_and_exchanges_data() {
    let (client, server) = configs();
    let (transport, handle) = ScriptedTransport::client(client, 5, 13);
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let mut stream = TlsAcceptor::new(server).accept(transport).await.unwrap();
        assert_eq!(stream.server_name(), Some("localhost"));

        handle.send_plaintext(b"request");
        let BufResult(result, request) = stream.read(Vec::with_capacity(32)).await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(request, b"request");

        let BufResult(result, _) = stream.write(b"response".to_vec()).await;
        assert_eq!(result.unwrap(), 8);
        assert_eq!(handle.take_plaintext(), b"response");
    });
}

#[test]
fn per_connection_alpn_does_not_mutate_shared_config() {
    let (client, mut server) = configs();
    Arc::get_mut(&mut server).unwrap().alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let connector = TlsConnector::from(client.clone());
    let acceptor = TlsAcceptor::from(server.clone());
    assert!(Arc::ptr_eq(connector.config(), &client));
    assert!(Arc::ptr_eq(acceptor.config(), &server));

    let (transport, _) = ScriptedTransport::server(server, 64, 64);
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let name = ServerName::try_from("localhost").unwrap();
        let stream = connector
            .connect_with_alpn(name, transport, vec![b"http/1.1".to_vec()])
            .await
            .unwrap();

        assert_eq!(stream.alpn_protocol(), Some(&b"http/1.1"[..]));
        assert_eq!(
            connector.config().alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
        assert!(Arc::ptr_eq(connector.config(), &client));
    });
}

#[test]
fn buffered_plaintext_precedes_clean_and_unclean_eof() {
    let (client, server) = configs();
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let (transport, handle) = ScriptedTransport::server(server.clone(), 64, 64);
        let name = ServerName::try_from("localhost").unwrap();
        let mut clean = TlsConnector::new(client.clone())
            .connect(name, transport)
            .await
            .unwrap();
        handle.send_plaintext(b"final clean bytes");
        handle.close_cleanly();

        let BufResult(result, bytes) = clean.read(Vec::with_capacity(5)).await;
        assert_eq!(result.unwrap(), 5);
        assert_eq!(bytes, b"final");
        let read_calls = handle.0.borrow().read_calls;
        let BufResult(result, bytes) = clean.read(Vec::with_capacity(5)).await;
        assert_eq!(result.unwrap(), 5);
        assert_eq!(bytes, b" clea");
        assert_eq!(handle.0.borrow().read_calls, read_calls);
        let BufResult(result, bytes) = clean.read(Vec::with_capacity(64)).await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(bytes, b"n bytes");
        let BufResult(result, _) = clean.read(Vec::with_capacity(64)).await;
        assert_eq!(result.unwrap(), 0);

        let (transport, handle) = ScriptedTransport::server(server, 64, 64);
        let name = ServerName::try_from("localhost").unwrap();
        let mut unclean = TlsConnector::new(client).connect(name, transport).await.unwrap();
        handle.send_plaintext(b"final unclean bytes");
        handle.set_eof();

        let BufResult(result, bytes) = unclean.read(Vec::with_capacity(64)).await;
        assert_eq!(result.unwrap(), 19);
        assert_eq!(bytes, b"final unclean bytes");
        let BufResult(result, _) = unclean.read(Vec::with_capacity(64)).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        let BufResult(result, _) = unclean.read(Vec::with_capacity(64)).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    });
}

#[test]
fn handshake_rejects_invalid_name_issuer_and_alpn() {
    let (client, server) = configs();
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let (transport, _) = ScriptedTransport::server(server.clone(), 64, 64);
        let wrong_name = ServerName::try_from("not-localhost.invalid").unwrap();
        let error = TlsConnector::new(client.clone())
            .connect(wrong_name, transport)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let untrusted_client = ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&version::TLS13])
            .unwrap()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth();
        let (transport, _) = ScriptedTransport::server(server.clone(), 64, 64);
        let name = ServerName::try_from("localhost").unwrap();
        let error = TlsConnector::new(Arc::new(untrusted_client))
            .connect(name, transport)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let mut server = server;
        Arc::get_mut(&mut server).unwrap().alpn_protocols = vec![b"only-server".to_vec()];
        let (transport, _) = ScriptedTransport::server(server, 64, 64);
        let name = ServerName::try_from("localhost").unwrap();
        let error = TlsConnector::new(client).connect(name, transport).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    });
}

#[test]
fn established_zero_write_returns_buffer_and_poisons_both_directions() {
    let (client, server) = configs();
    let (transport, handle) = ScriptedTransport::server(server, 64, 64);
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let name = ServerName::try_from("localhost").unwrap();
        let mut stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
        handle.0.borrow_mut().zero_write = true;

        let outbound = b"accepted before zero write".to_vec();
        let pointer = outbound.as_ptr();
        let BufResult(result, outbound) = stream.write(outbound).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WriteZero);
        assert_eq!(outbound.as_ptr(), pointer);
        assert!(handle.take_plaintext().is_empty());

        let BufResult(result, _) = stream.write(vec![1]).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WriteZero);
        let BufResult(result, _) = stream.read(Vec::with_capacity(1)).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WriteZero);
    });
}

#[test]
fn transport_failures_poison_only_the_required_directions() {
    let (client, server) = configs();
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let (transport, handle) = ScriptedTransport::server(server.clone(), 64, 64);
        let name = ServerName::try_from("localhost").unwrap();
        let mut stream = TlsConnector::new(client.clone())
            .connect(name, transport)
            .await
            .unwrap();
        handle.0.borrow_mut().read_error = Some(io::ErrorKind::ConnectionReset);

        let inbound = Vec::with_capacity(16);
        let pointer = inbound.as_ptr();
        let BufResult(result, inbound) = stream.read(inbound).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(inbound.as_ptr(), pointer);
        let BufResult(result, _) = stream.read(Vec::with_capacity(16)).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionReset);

        let BufResult(result, _) = stream.write(b"write still works".to_vec()).await;
        assert_eq!(result.unwrap(), 17);
        assert_eq!(handle.take_plaintext(), b"write still works");
        assert_eq!(
            stream.into_parts().err().unwrap().kind(),
            io::ErrorKind::ConnectionReset
        );

        let (transport, handle) = ScriptedTransport::server(server, 64, 64);
        let name = ServerName::try_from("localhost").unwrap();
        let mut stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
        handle.0.borrow_mut().write_error = Some(io::ErrorKind::ConnectionAborted);

        let outbound = b"ambiguous".to_vec();
        let pointer = outbound.as_ptr();
        let BufResult(result, outbound) = stream.write(outbound).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(outbound.as_ptr(), pointer);
        let BufResult(result, _) = stream.write(vec![1]).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        let BufResult(result, _) = stream.read(Vec::with_capacity(1)).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(
            stream.into_parts().err().unwrap().kind(),
            io::ErrorKind::ConnectionAborted
        );
    });
}

#[cfg(feature = "tls12")]
#[test]
fn tls12_handshake_and_payload_succeed_when_enabled() {
    let (client, server) = configs_for(&[&version::TLS12]);
    let (transport, handle) = ScriptedTransport::server(server, 3, 5);
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let name = ServerName::try_from("localhost").unwrap();
        let mut stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
        assert_eq!(stream.get_ref().1.protocol_version(), Some(ProtocolVersion::TLSv1_2));
        let BufResult(result, _) = stream.write(b"tls12".to_vec()).await;
        assert_eq!(result.unwrap(), 5);
        assert_eq!(handle.take_plaintext(), b"tls12");
    });
}

#[test]
fn shared_configuration_resumes_tls13_sessions() {
    let (client, server) = configs();
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let (transport, handle) = ScriptedTransport::server(server.clone(), 64, 64);
        let name = ServerName::try_from("localhost").unwrap();
        let mut first = TlsConnector::new(client.clone())
            .connect(name, transport)
            .await
            .unwrap();
        handle.send_plaintext(b"ticket delivery");
        let BufResult(result, _) = first.read(Vec::with_capacity(15)).await;
        assert_eq!(result.unwrap(), 15);
        assert_eq!(first.get_ref().1.handshake_kind(), Some(HandshakeKind::Full));

        let (transport, _) = ScriptedTransport::server(server, 64, 64);
        let name = ServerName::try_from("localhost").unwrap();
        let second = TlsConnector::new(client).connect(name, transport).await.unwrap();
        assert_eq!(second.get_ref().1.handshake_kind(), Some(HandshakeKind::Resumed));
    });
}

#[test]
fn zero_capacity_read_does_not_touch_the_transport() {
    let (client, server) = configs();
    let (transport, handle) = ScriptedTransport::server(server, 64, 64);
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let name = ServerName::try_from("localhost").unwrap();
        let mut stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
        let calls = handle.0.borrow().read_calls;
        let BufResult(result, buffer) = stream.read(Vec::new()).await;
        assert_eq!(result.unwrap(), 0);
        assert!(buffer.is_empty());
        assert_eq!(handle.0.borrow().read_calls, calls);
    });
}

#[test]
fn retained_ciphertext_is_consumed_before_another_transport_read() {
    let (client, server) = configs();
    let (transport, handle) = ScriptedTransport::server(server, usize::MAX, 64);
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let name = ServerName::try_from("localhost").unwrap();
        let mut stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
        let read_calls = handle.0.borrow().read_calls;
        let plaintext = vec![7; 16 * 1024];
        handle.send_plaintext(&plaintext);

        let BufResult(result, received) = stream.read(Vec::with_capacity(plaintext.len())).await;
        assert_eq!(result.unwrap(), plaintext.len());
        assert_eq!(received, plaintext);
        assert_eq!(handle.0.borrow().read_calls, read_calls + 1);
    });
}

#[test]
fn dropping_in_flight_transport_operations_poison_both_directions() {
    let (client, server) = configs();
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let connect = |server| {
            let (transport, handle) = ScriptedTransport::server(server, 64, 64);
            let name = ServerName::try_from("localhost").unwrap();
            (transport, handle, name)
        };

        let (transport, handle, name) = connect(server.clone());
        let mut stream = TlsConnector::new(client.clone())
            .connect(name, transport)
            .await
            .unwrap();
        handle.0.borrow_mut().pending_read = true;
        let mut future = Box::pin(stream.read(Vec::with_capacity(8)));
        poll_pending_once(future.as_mut()).await;
        drop(future);
        let BufResult(result, _) = stream.read(Vec::with_capacity(8)).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        let BufResult(result, _) = stream.write(vec![1]).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        assert_eq!(stream.into_parts().err().unwrap().kind(), io::ErrorKind::Other);

        let (transport, handle, name) = connect(server.clone());
        let mut stream = TlsConnector::new(client.clone())
            .connect(name, transport)
            .await
            .unwrap();
        handle.0.borrow_mut().pending_write = true;
        let mut future = Box::pin(stream.write(vec![1]));
        poll_pending_once(future.as_mut()).await;
        drop(future);
        let BufResult(result, _) = stream.read(Vec::with_capacity(8)).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        let BufResult(result, _) = stream.write(vec![1]).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        assert_eq!(stream.into_parts().err().unwrap().kind(), io::ErrorKind::Other);

        let (transport, handle, name) = connect(server.clone());
        let mut stream = TlsConnector::new(client.clone())
            .connect(name, transport)
            .await
            .unwrap();
        handle.0.borrow_mut().pending_flush = true;
        let mut future = Box::pin(stream.flush());
        poll_pending_once(future.as_mut()).await;
        drop(future);
        let BufResult(result, _) = stream.write(vec![1]).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        assert_eq!(stream.into_parts().err().unwrap().kind(), io::ErrorKind::Other);

        let (transport, handle, name) = connect(server);
        let mut stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
        handle.0.borrow_mut().pending_shutdown = true;
        let mut future = Box::pin(stream.shutdown());
        poll_pending_once(future.as_mut()).await;
        drop(future);
        let BufResult(result, _) = stream.write(vec![1]).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        assert_eq!(stream.into_parts().err().unwrap().kind(), io::ErrorKind::Other);
    });
}

#[test]
fn vectored_write_consumes_all_components_within_rustls_capacity() {
    let (client, server) = configs();
    let (transport, handle) = ScriptedTransport::server(server, 64, 64);
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let name = ServerName::try_from("localhost").unwrap();
        let mut stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
        let buffers = [b"header: value\r\n".to_vec(), vec![7; 16 * 1024], b"ignored".to_vec()];
        let pointers = buffers.each_ref().map(|buffer| buffer.as_ptr());
        let expected = buffers.iter().flatten().copied().collect::<Vec<_>>();
        let BufResult(result, buffers) = stream.write_vectored(buffers).await;
        assert_eq!(result.unwrap(), expected.len());
        assert_eq!(buffers.each_ref().map(|buffer| buffer.as_ptr()), pointers);
        assert_eq!(handle.take_plaintext(), expected);
    });
}

#[test]
fn vectored_read_fills_components_and_preserves_allocations() {
    let (client, server) = configs();
    let (transport, handle) = ScriptedTransport::server(server, 64, 64);
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let name = ServerName::try_from("localhost").unwrap();
        let mut stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
        handle.send_plaintext(b"scatter response");

        let buffers = [
            Vec::with_capacity(0),
            Vec::with_capacity(3),
            Vec::with_capacity(4),
            Vec::with_capacity(32),
        ];
        let pointers = buffers.each_ref().map(|buffer| buffer.as_ptr());
        let BufResult(result, buffers) = stream.read_vectored(buffers).await;

        assert_eq!(result.unwrap(), 16);
        assert_eq!(buffers.each_ref().map(|buffer| buffer.as_ptr()), pointers);
        assert_eq!(buffers.each_ref().map(Vec::len), [0, 3, 4, 9]);
        assert_eq!(
            buffers.iter().flatten().copied().collect::<Vec<_>>(),
            b"scatter response"
        );

        handle.send_plaintext(&vec![7; 20 * 1024]);
        let buffers = [
            Vec::with_capacity(10 * 1024),
            Vec::with_capacity(10 * 1024),
            Vec::with_capacity(10 * 1024),
        ];
        let BufResult(result, buffers) = stream.read_vectored(buffers).await;
        assert_eq!(result.unwrap(), 16 * 1024);
        assert_eq!(buffers.each_ref().map(Vec::len), [10 * 1024, 6 * 1024, 0]);
        assert!(buffers.iter().flatten().all(|byte| *byte == 7));

        let BufResult(result, remainder) = stream.read_vectored([Vec::with_capacity(8 * 1024)]).await;
        assert_eq!(result.unwrap(), 4 * 1024);
        assert_eq!(remainder[0], vec![7; 4 * 1024]);
    });
}

#[test]
fn vectored_write_all_advances_across_tls_record_boundaries() {
    let (client, server) = configs();
    let (transport, handle) = ScriptedTransport::server(server, 64, 64);
    let mut runtime = Runtime::new().unwrap();

    runtime.block_on(async {
        let name = ServerName::try_from("localhost").unwrap();
        let mut stream = TlsConnector::new(client).connect(name, transport).await.unwrap();
        let buffers = [b"prefix".to_vec(), vec![9; 96 * 1024], b"suffix".to_vec()];
        let pointers = buffers.each_ref().map(|buffer| buffer.as_ptr());
        let expected = buffers.iter().flatten().copied().collect::<Vec<_>>();
        let BufResult(result, buffers) = stream.write_vectored_all(buffers).await;
        assert_eq!(result.unwrap(), expected.len());
        assert_eq!(buffers.each_ref().map(|buffer| buffer.as_ptr()), pointers);
        assert_eq!(handle.take_plaintext(), expected);
    });
}
