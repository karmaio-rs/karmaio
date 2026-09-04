//! A certificate-verifying TLS echo client.
//!
//! Run `tls_server` first, then pass this example the test CA certificate:
//!
//! ```text
//! cargo run -p karmaio-examples --example tls_client -- \
//!   karmaio/tests/fixtures/tls/ca.der
//! ```

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use karmaio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use karmaio::net::tcp::TcpStream;
use karmaio::tls::TlsConnector;
use karmaio::tls::rustls::pki_types::{CertificateDer, ServerName};
use karmaio::tls::rustls::{ClientConfig, RootCertStore};

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[karmaio::main]
async fn main() -> io::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let ca_path = arguments
        .next()
        .ok_or_else(|| invalid_input("usage: tls_client <ca.der> [address] [server-name]"))?;
    let address: SocketAddr = arguments
        .next()
        .unwrap_or_else(|| "127.0.0.1:8443".to_owned())
        .parse()
        .map_err(|error| invalid_input(format!("invalid address: {error}")))?;
    let server_name = arguments.next().unwrap_or_else(|| "localhost".to_owned());
    let server_name = ServerName::try_from(server_name).map_err(|error| invalid_input(error.to_string()))?;

    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(std::fs::read(ca_path)?))
        .map_err(|error| invalid_input(format!("invalid CA certificate: {error}")))?;
    let provider = Arc::new(karmaio::tls::rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| invalid_input(error.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"vakya/1".to_vec()];

    let transport = TcpStream::connect(address).await?;
    let mut stream = TlsConnector::new(Arc::new(config))
        .connect(server_name, transport)
        .await?;
    println!(
        "connected with ALPN {:?}",
        stream.alpn_protocol().map(String::from_utf8_lossy)
    );

    let message = b"hello over karmaio TLS\n".to_vec();
    let response_len = message.len();
    let (result, _) = stream.write_all(message).await.into_parts();
    result?;
    let (result, response) = stream.read_exact(Vec::with_capacity(response_len)).await.into_parts();
    result?;
    println!("echo: {}", String::from_utf8_lossy(&response));

    stream.shutdown().await
}
