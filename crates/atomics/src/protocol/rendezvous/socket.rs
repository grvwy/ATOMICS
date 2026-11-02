//! Dialing the rendezvous service: TCP, TLS for `wss://` (rustls, the web PKI's roots), then the
//! WebSocket upgrade with the pass as `Authorization: Bearer`, the `atomics.rendezvous.v1`
//! subprotocol and no `Origin`: a service tells a host from a browser by that, and a browser
//! cannot send the header. The pass is never in the URL and never in an error.

use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use atomics_core::tunnel::{HEADER_BYTES, MAX_BODY_BYTES, SUBPROTOCOL};
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{HandshakeError, WebSocket};

use super::super::link::Stream;
use super::Settings;

/// Where the service is.
#[derive(Debug, Clone)]
pub struct Target {
    url: String,
    host: String,
    port: u16,
    tls: bool,
}

impl Target {
    /// A `wss://` URL, or `ws://` to a loopback host (tests, a local service). No credentials,
    /// query or fragment: a pass never goes in a URL, and nothing else should either.
    pub fn parse(text: &str) -> Result<Self, String> {
        let url = url::Url::parse(text.trim()).map_err(|e| format!("[remote.rendezvous] url is not a URL: {e}"))?;
        let host = url.host_str().filter(|h| !h.is_empty()).ok_or("[remote.rendezvous] url has no host")?;
        let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
        if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
            return Err("[remote.rendezvous] url must not hold credentials, a query or a fragment".into());
        }
        let loopback =
            host.eq_ignore_ascii_case("localhost") || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        let tls = match url.scheme() {
            "wss" => true,
            "ws" if loopback => false,
            "ws" => return Err("[remote.rendezvous] url must be wss:// (ws:// only reaches this machine)".into()),
            other => return Err(format!("[remote.rendezvous] url must be wss://, not {other}://")),
        };
        let port = url.port_or_known_default().ok_or("[remote.rendezvous] url has no port")?;
        Ok(Self { url: url.to_string(), host, port, tls })
    }

    /// For status lines: the service's host, never its path.
    pub fn host(&self) -> &str {
        &self.host
    }
}

/// The socket under the WebSocket: the loopback stream with its waker, in TLS for `wss://`.
pub enum Conn {
    Plain(Stream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, Stream>>),
}

impl Conn {
    pub fn stream(&mut self) -> &mut Stream {
        match self {
            Conn::Plain(stream) => stream,
            Conn::Tls(tls) => &mut tls.sock,
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(stream) => stream.read(buf),
            Conn::Tls(tls) => tls.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(stream) => stream.write(buf),
            Conn::Tls(tls) => tls.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(stream) => stream.flush(),
            Conn::Tls(tls) => tls.flush(),
        }
    }
}

/// Why a dial failed.
#[derive(Debug, PartialEq, Eq)]
pub enum DialError {
    /// The service answered the upgrade with this HTTP status: 401 for a refused pass.
    Refused(u16),
    Failed(String),
}

static TLS: LazyLock<Result<Arc<rustls::ClientConfig>, String>> = LazyLock::new(|| {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
});

pub fn socket_config() -> WebSocketConfig {
    let message = HEADER_BYTES + MAX_BODY_BYTES;
    WebSocketConfig::default()
        .max_message_size(Some(message))
        .max_frame_size(Some(message))
        .max_write_buffer_size(2 * message)
}

/// Connect, upgrade and hand back the socket, its reads still plain blocking ones: the caller
/// sets how long each waits (`Stream::set_wait`).
pub fn dial(target: &Target, pass: &str, settings: &Settings) -> Result<WebSocket<Conn>, DialError> {
    let failed =
        |what: &str, error: &dyn std::fmt::Display| DialError::Failed(format!("{what} {}: {error}", target.host));
    let deadline = Instant::now() + settings.connect_timeout;
    let addrs = (target.host.as_str(), target.port).to_socket_addrs().map_err(|e| failed("cannot resolve", &e))?;
    let mut last = None;
    let tcp = addrs
        .into_iter()
        .find_map(|addr| match TcpStream::connect_timeout(&addr, settings.connect_timeout) {
            Ok(tcp) => Some(tcp),
            Err(error) => {
                last = Some(error);
                None
            }
        })
        .ok_or_else(|| match last.take() {
            Some(error) => failed("cannot connect to", &error),
            None => failed("no address for", &"none resolved"),
        })?;
    tcp.set_nodelay(true).map_err(|e| failed("cannot set up the socket to", &e))?;
    tcp.set_write_timeout(Some(settings.write_timeout)).map_err(|e| failed("cannot set up the socket to", &e))?;
    let stream = Stream::new(tcp).map_err(|e| failed("cannot set up the socket to", &e))?.with_deadline(deadline);
    let conn = if target.tls {
        let config = TLS.as_ref().map_err(|e| failed("no TLS for", e))?.clone();
        let name = rustls::pki_types::ServerName::try_from(target.host.clone())
            .map_err(|e| failed("not a TLS server name:", &e))?;
        let session = rustls::ClientConnection::new(config, name).map_err(|e| failed("cannot start TLS with", &e))?;
        Conn::Tls(Box::new(rustls::StreamOwned::new(session, stream)))
    } else {
        Conn::Plain(stream)
    };
    let mut request = target.url.as_str().into_client_request().map_err(|e| failed("bad request for", &e))?;
    let headers = request.headers_mut();
    let bearer = format!("Bearer {pass}");
    headers.insert(
        "Authorization",
        bearer.parse().map_err(|_| DialError::Failed("the pass is not a valid header value".into()))?,
    );
    headers.insert("Sec-WebSocket-Protocol", tungstenite::http::HeaderValue::from_static(SUBPROTOCOL));
    headers.remove("Origin");
    let mut attempt = tungstenite::client::client_with_config(request, conn, Some(socket_config()));
    let mut socket = loop {
        match attempt {
            Ok((socket, _)) => break socket,
            Err(HandshakeError::Interrupted(mid)) => {
                if Instant::now() >= deadline {
                    return Err(failed("no answer in time from", &"the upgrade timed out"));
                }
                attempt = mid.handshake();
            }
            Err(HandshakeError::Failure(tungstenite::Error::Http(response))) => {
                return Err(DialError::Refused(response.status().as_u16()));
            }
            Err(HandshakeError::Failure(error)) => return Err(failed("the upgrade failed at", &error)),
        }
    };
    socket.get_mut().stream().set_deadline(None);
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_wss_or_a_loopback_ws_url_is_dialed() {
        let ok = Target::parse("wss://rendezvous.example/atomics/tunnel").unwrap();
        assert!(ok.tls && ok.port == 443 && ok.host() == "rendezvous.example");
        for (url, tls) in [("ws://127.0.0.1:9/t", false), ("ws://localhost:9/t", false), ("ws://[::1]:9/t", false)] {
            let target = Target::parse(url).unwrap();
            assert_eq!(target.tls, tls, "{url}");
        }
        for url in [
            "ws://rendezvous.example/t",
            "https://rendezvous.example/t",
            "wss://user:secret@rendezvous.example/t",
            "wss://rendezvous.example/t?pass=x",
            "wss://rendezvous.example/t#x",
            "not a url",
        ] {
            let error = Target::parse(url).unwrap_err();
            assert!(!error.contains("secret"), "{url}: {error}");
        }
    }
}
