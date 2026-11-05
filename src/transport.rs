//! The control connection's stream: plain TCP, TLS over TCP (for `https://` endpoints such as
//! Tailscale Serve), or a Unix socket. rexrs's tungstenite transport runs the WebSocket over it.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rexrs::transport::TungsteniteTransport;
use rustls::pki_types::ServerName;

use crate::endpoint::Endpoint;

pub type Transport = TungsteniteTransport<Stream>;

pub enum Stream {
    Tcp(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
    #[cfg(unix)]
    Unix(UnixStream),
}

impl Stream {
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Stream::Tcp(s) => s.set_read_timeout(timeout),
            Stream::Tls(s) => s.sock.set_read_timeout(timeout),
            #[cfg(unix)]
            Stream::Unix(s) => s.set_read_timeout(timeout),
        }
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Stream::Tcp(s) => s.set_write_timeout(timeout),
            Stream::Tls(s) => s.sock.set_write_timeout(timeout),
            #[cfg(unix)]
            Stream::Unix(s) => s.set_write_timeout(timeout),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
            #[cfg(unix)]
            Stream::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
            #[cfg(unix)]
            Stream::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Tcp(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
            #[cfg(unix)]
            Stream::Unix(s) => s.flush(),
        }
    }
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

fn connect_tcp(host: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolving {host}"))?
        .collect();
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                return Ok(stream);
            }
            Err(e) => last = Some(e),
        }
    }
    match last {
        Some(e) => Err(e).with_context(|| format!("connecting to {host}:{port}")),
        None => bail!("{host} has no addresses"),
    }
}

/// Opens the stream and runs the WebSocket handshake for `path`, all within `timeout`.
pub fn connect(endpoint: &Endpoint, path: &str, timeout: Duration) -> Result<Transport> {
    let stream = match endpoint {
        Endpoint::Tcp { tls, host, port } => {
            let tcp = connect_tcp(host, *port, timeout)?;
            if *tls {
                let name = ServerName::try_from(host.clone())
                    .with_context(|| format!("invalid TLS server name {host:?}"))?;
                let connection =
                    rustls::ClientConnection::new(tls_config(), name).context("starting TLS")?;
                Stream::Tls(Box::new(rustls::StreamOwned::new(connection, tcp)))
            } else {
                Stream::Tcp(tcp)
            }
        }
        #[cfg(unix)]
        Endpoint::Unix(path) => Stream::Unix(
            UnixStream::connect(path)
                .with_context(|| format!("connecting to {}", path.display()))?,
        ),
        #[cfg(not(unix))]
        Endpoint::Unix(_) => bail!("Unix socket endpoints need a Unix platform"),
    };
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    // The TLS handshake runs lazily on the first read or write, inside the WebSocket handshake.
    TungsteniteTransport::handshake(stream, &endpoint.ws_url(path))
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("opening the control connection to {endpoint}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn builds_a_tls_config_with_roots() {
        let config = tls_config();
        assert!(!webpki_roots::TLS_SERVER_ROOTS.is_empty());
        assert!(config.alpn_protocols.is_empty());
    }

    #[test]
    fn https_endpoints_speak_tls() {
        // A server that answers in plain HTTP: the client's TLS handshake must fail, not
        // fall back to plain text.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut hello = [0u8; 5];
            stream.read_exact(&mut hello).unwrap();
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
            hello
        });
        let endpoint = Endpoint::parse(&format!("https://127.0.0.1:{port}")).unwrap();
        let error = match connect(&endpoint, "/api/control", Duration::from_secs(5)) {
            Ok(_) => panic!("connected without TLS"),
            Err(error) => format!("{error:#}"),
        };
        assert!(error.contains("https://127.0.0.1"), "{error}");
        // 0x16 is a TLS handshake record: the client opened with a ClientHello.
        assert_eq!(server.join().unwrap()[0], 0x16);
    }
}
