//! A TLS echo server using an explicitly configured Rustls provider.
//!
//! The checked-in test identity can be used for local experimentation:
//!
//! ```text
//! cargo run -p karmaio-examples --example tls_server -- \
//!   karmaio/tests/fixtures/tls/localhost.der \
//!   karmaio/tests/fixtures/tls/localhost-key.der
//! ```

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use karmaio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use karmaio::net::tcp::TcpListener;
use karmaio::runtime::spawn_local;
use karmaio::tls::TlsAcceptor;
use karmaio::tls::rustls::ServerConfig;
use karmaio::tls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[karmaio::main]
async fn main() -> io::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let certificate_path = arguments
        .next()
        .ok_or_else(|| invalid_input("usage: tls_server <certificate.der> <private-key.der> [address]"))?;
    let key_path = arguments
        .next()
        .ok_or_else(|| invalid_input("usage: tls_server <certificate.der> <private-key.der> [address]"))?;
    let address: SocketAddr = arguments
        .next()
        .unwrap_or_else(|| "127.0.0.1:8443".to_owned())
        .parse()
        .map_err(|error| invalid_input(format!("invalid address: {error}")))?;

    let certificate = CertificateDer::from(std::fs::read(certificate_path)?);
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(std::fs::read(key_path)?));
    let provider = Arc::new(karmaio::tls::rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| invalid_input(error.to_string()))?
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .map_err(|error| invalid_input(format!("invalid server identity: {error}")))?;
    config.alpn_protocols = vec![b"vakya/1".to_vec()];
    let acceptor = TlsAcceptor::new(Arc::new(config));

    let listener = TcpListener::bind(address)?;
    println!("listening on {address}");
    loop {
        let (transport, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        spawn_local(async move {
            let mut stream = match acceptor.accept(transport).await {
                Ok(stream) => stream,
                Err(error) => {
                    eprintln!("TLS handshake from {peer} failed: {error}");
                    return;
                }
            };
            let mut buffer = Vec::with_capacity(16 * 1024);
            loop {
                let (result, returned) = stream.read(buffer).await.into_parts();
                buffer = returned;
                match result {
                    Ok(0) => {
                        if let Err(error) = stream.shutdown().await {
                            eprintln!("TLS shutdown for {peer} failed: {error}");
                        }
                        return;
                    }
                    Ok(_) => {
                        let (result, returned) = stream.write_all(buffer).await.into_parts();
                        buffer = returned;
                        if let Err(error) = result {
                            eprintln!("TLS write to {peer} failed: {error}");
                            return;
                        }
                        buffer.clear();
                    }
                    Err(error) => {
                        eprintln!("TLS read from {peer} failed: {error}");
                        return;
                    }
                }
            }
        });
    }
}
