//! Where the Rex server is, and how to compare addresses against the allowlist.
//!
//! Accepted forms:
//!
//! - `https://host[:port]` or `wss://host[:port]`: TLS, such as Tailscale Serve.
//! - `http://host[:port]`, `ws://host[:port]` or `tcp://host:port`: plain TCP.
//! - `unix:///path/to/server.sock` or a bare absolute path: a local socket.
//!
//! A URL may end in `/` or `/api/control`; any other path, a query, a fragment, or user
//! credentials are rejected. Every form normalizes to one canonical string, which is what the
//! allowlist compares and what the audit log records.

use std::fmt;
use std::path::PathBuf;

use anyhow::{Result, bail};

/// A parsed, normalized Rex endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A TCP listener, with or without TLS.
    Tcp { tls: bool, host: String, port: u16 },
    /// A local Unix socket.
    Unix(PathBuf),
}

impl Endpoint {
    pub fn parse(address: &str) -> Result<Self> {
        let address = address.trim();
        if address.is_empty() {
            bail!("empty endpoint");
        }
        if address.starts_with('/') {
            return Ok(Endpoint::Unix(PathBuf::from(address)));
        }
        if let Some(rest) = address.strip_prefix("unix://") {
            let path = url::Url::parse(&format!("file://{rest}"))
                .ok()
                .and_then(|u| u.to_file_path().ok())
                .filter(|p| p.is_absolute());
            return match path {
                Some(path) => Ok(Endpoint::Unix(path)),
                None => bail!("invalid unix endpoint {address:?}: expected unix:///absolute/path"),
            };
        }
        let url = url::Url::parse(address)
            .map_err(|e| anyhow::anyhow!("invalid endpoint {address:?}: {e}"))?;
        let tls = match url.scheme() {
            "https" | "wss" => true,
            "http" | "ws" | "tcp" => false,
            other => {
                bail!("unsupported endpoint scheme {other:?}; use https://, http://, or unix://")
            }
        };
        if !url.username().is_empty() || url.password().is_some() {
            bail!("endpoint must not contain credentials");
        }
        if url.query().is_some() || url.fragment().is_some() {
            bail!("endpoint must not contain a query or fragment");
        }
        if !matches!(url.path(), "" | "/" | "/api/control" | "/api/control/") {
            bail!(
                "endpoint path {:?} is not supported; give the server root, such as https://rex-host.example.ts.net",
                url.path()
            );
        }
        let host = match url.host() {
            Some(url::Host::Domain(d)) => d.to_ascii_lowercase(),
            Some(url::Host::Ipv4(ip)) => ip.to_string(),
            Some(url::Host::Ipv6(ip)) => ip.to_string(),
            None => bail!("endpoint {address:?} has no host"),
        };
        let port = match (url.port(), url.scheme()) {
            (Some(port), _) => port,
            (None, "tcp") => bail!("tcp:// endpoints need a port"),
            (None, _) => default_port(tls),
        };
        Ok(Endpoint::Tcp { tls, host, port })
    }

    /// The `ws://` or `wss://` URL of `path` on this server.
    pub fn ws_url(&self, path: &str) -> String {
        match self {
            Endpoint::Unix(_) => format!("ws://localhost{path}"),
            Endpoint::Tcp { tls, host, port } => {
                let scheme = if *tls { "wss" } else { "ws" };
                format!("{scheme}://{}:{port}{path}", bracket(host))
            }
        }
    }
}

fn default_port(tls: bool) -> u16 {
    if tls { 443 } else { 80 }
}

fn bracket(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

/// The canonical form: `https://host`, `http://host:8080`, `unix:///path`.
impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Endpoint::Unix(path) => write!(f, "unix://{}", path.display()),
            Endpoint::Tcp { tls, host, port } => {
                let scheme = if *tls { "https" } else { "http" };
                write!(f, "{scheme}://{}", bracket(host))?;
                if *port != default_port(*tls) {
                    write!(f, ":{port}")?;
                }
                Ok(())
            }
        }
    }
}

/// Whether `endpoint` is on the allowlist. Entries that don't parse never match.
pub fn is_allowed(endpoint: &Endpoint, allowlist: &[String]) -> bool {
    allowlist
        .iter()
        .filter_map(|entry| Endpoint::parse(entry).ok())
        .any(|allowed| &allowed == endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(address: &str) -> String {
        Endpoint::parse(address).unwrap().to_string()
    }

    #[test]
    fn normalizes_equivalent_forms() {
        for address in [
            "https://rex-host.example.ts.net",
            "https://REX-HOST.example.ts.net/",
            "wss://rex-host.example.ts.net:443",
            "https://rex-host.example.ts.net/api/control",
        ] {
            assert_eq!(canonical(address), "https://rex-host.example.ts.net");
        }
        assert_eq!(canonical("ws://127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(canonical("tcp://127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(canonical("http://[::1]:7000/"), "http://[::1]:7000");
        assert_eq!(canonical("unix:///tmp/rex.sock"), "unix:///tmp/rex.sock");
        assert_eq!(canonical("/tmp/rex.sock"), "unix:///tmp/rex.sock");
        assert_eq!(
            canonical("unix:///tmp/a%20b/rex.sock"),
            "unix:///tmp/a b/rex.sock"
        );
    }

    #[test]
    fn builds_control_urls() {
        let tls = Endpoint::parse("https://rex-host.example.ts.net").unwrap();
        assert_eq!(
            tls.ws_url("/api/control"),
            "wss://rex-host.example.ts.net:443/api/control"
        );
        let plain = Endpoint::parse("http://[::1]:7000").unwrap();
        assert_eq!(plain.ws_url("/api/control"), "ws://[::1]:7000/api/control");
        let unix = Endpoint::parse("unix:///tmp/rex.sock").unwrap();
        assert_eq!(unix.ws_url("/api/control"), "ws://localhost/api/control");
    }

    #[test]
    fn rejects_unsafe_or_odd_forms() {
        for address in [
            "",
            "rex-host",
            "ftp://rex-host.example.ts.net",
            "https://user:secret@rex-host.example.ts.net",
            "https://rex-host.example.ts.net/?token=x",
            "https://rex-host.example.ts.net/#x",
            "https://rex-host.example.ts.net/other",
            "tcp://rex-host.example.ts.net",
            "unix://relative/path",
        ] {
            assert!(Endpoint::parse(address).is_err(), "{address:?} parsed");
        }
    }

    #[test]
    fn allowlist_matches_normalized_entries() {
        let allowlist = vec![
            "https://REX-HOST.example.ts.net/".to_owned(),
            "not a url".to_owned(),
        ];
        let ok = Endpoint::parse("wss://rex-host.example.ts.net").unwrap();
        let other = Endpoint::parse("https://other.example.ts.net").unwrap();
        let port = Endpoint::parse("https://rex-host.example.ts.net:8443").unwrap();
        assert!(is_allowed(&ok, &allowlist));
        assert!(!is_allowed(&other, &allowlist));
        assert!(!is_allowed(&port, &allowlist));
        assert!(!is_allowed(&ok, &[]));
    }
}
