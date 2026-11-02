//! The `atomics.v1` protocol server: a thread inside the running app that serves the board and
//! diffs to clients on this machine. Off unless `[remote] enabled`; see docs/daemon.md.
//!
//! It reads only what the runtime thread publishes (`atomics_core::projection`), so it never takes
//! the `State` lock, and every socket write happens with no lock of any kind held. Each client has
//! its own thread and a bounded queue; a client that stops reading is resynchronised or dropped and
//! cannot delay the host or other clients.

mod hub;
mod link;
mod session;
#[cfg(test)]
pub(crate) mod tests;

use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use atomics_core::projection::Projections;
use atomics_core::wire::{self, MAX_QUEUED_FRAMES, Role};
use ring::rand::SecureRandom;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::http;

pub use hub::RingLimits;
pub use session::{DiffSource, SessionConfig};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// 0 picks a free port.
    pub port: u16,
    pub allowed_origins: Vec<String>,
    /// Where to write `{url, token}` for local clients; removed on stop.
    pub advertise: Option<PathBuf>,
    pub max_clients: usize,
    pub write_timeout: Duration,
    pub poll_interval: Duration,
    pub queue_frames: usize,
    pub queue_bytes: usize,
    pub ring: RingLimits,
    pub session: SessionConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 0,
            allowed_origins: Vec::new(),
            advertise: None,
            max_clients: 4,
            write_timeout: Duration::from_secs(5),
            poll_interval: Duration::from_millis(20),
            queue_frames: MAX_QUEUED_FRAMES,
            queue_bytes: 4 << 20,
            ring: RingLimits::default(),
            session: SessionConfig::default(),
        }
    }
}

pub struct Server {
    addr: SocketAddr,
    #[allow(dead_code)] // read by tests; clients get the token from protocol.json
    token: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    advertise: Option<PathBuf>,
    #[allow(dead_code)] // read by tests
    active: Arc<AtomicUsize>,
}

fn random_hex(bytes: usize) -> io::Result<String> {
    let mut buf = vec![0u8; bytes];
    ring::rand::SystemRandom::new().fill(&mut buf).map_err(|_| io::Error::other("no system randomness"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

impl Server {
    /// Bind 127.0.0.1 and start serving. The caller keeps the returned handle for the server's life.
    pub fn start(config: ServerConfig, projections: Arc<Projections>, diffs: Arc<dyn DiffSource>) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, config.port))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let token = random_hex(32)?;
        let epoch = random_hex(8)?;
        if let Some(path) = &config.advertise {
            write_advertisement(path, &format!("ws://{addr}"), &token)?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let hub = Arc::new(Mutex::new(hub::Hub::new(
            epoch,
            config.ring.clone(),
            config.queue_frames,
            config.queue_bytes,
        )));
        let advertise = config.advertise.clone();
        let active = Arc::new(AtomicUsize::new(0));
        let accept = Accept {
            shared: Arc::new(session::Shared {
                hub: hub.clone(),
                diffs,
                stop: stop.clone(),
                config: config.session.clone(),
                queue_frames: config.queue_frames,
            }),
            token: token.clone(),
            addr,
            config: config.clone(),
            active: active.clone(),
        };
        let thread = std::thread::Builder::new().name("atomics-protocol".into()).spawn({
            let stop = stop.clone();
            move || {
                while !stop.load(Ordering::Acquire) {
                    loop {
                        match listener.accept() {
                            Ok((stream, _)) => accept.client(stream),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(_) => break,
                        }
                    }
                    hub.lock().unwrap_or_else(|e| e.into_inner()).poll(&projections);
                    std::thread::sleep(accept.config.poll_interval);
                }
            }
        })?;
        Ok(Self { addr, token, stop, thread: Some(thread), advertise, active })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    #[allow(dead_code)]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Clients with a live connection, for tests and diagnostics.
    #[allow(dead_code)]
    pub fn active_clients(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(path) = self.advertise.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `{url, token}` for clients on this machine, readable by this user only.
fn write_advertisement(path: &Path, url: &str, token: &str) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(serde_json::json!({ "url": url, "token": token, "pid": std::process::id() }).to_string().as_bytes())?;
    file.sync_all()?;
    std::fs::rename(tmp, path)
}

struct Accept {
    shared: Arc<session::Shared>,
    token: String,
    addr: SocketAddr,
    config: ServerConfig,
    active: Arc<AtomicUsize>,
}

struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Accept {
    fn client(&self, stream: TcpStream) {
        if self.active.fetch_add(1, Ordering::AcqRel) >= self.config.max_clients {
            self.active.fetch_sub(1, Ordering::AcqRel);
            return; // dropping the stream refuses the connection before any parsing
        }
        let slot = Slot(self.active.clone());
        let (shared, token, addr) = (self.shared.clone(), self.token.clone(), self.addr);
        let (origins, write_timeout) = (self.config.allowed_origins.clone(), self.config.write_timeout);
        let spawned = std::thread::Builder::new().name("atomics-protocol-client".into()).spawn(move || {
            let _slot = slot;
            let _ = serve_stream(stream, &shared, &token, addr, &origins, write_timeout);
        });
        let _ = spawned;
    }
}

fn serve_stream(
    stream: TcpStream,
    shared: &session::Shared,
    token: &str,
    addr: SocketAddr,
    origins: &[String],
    write_timeout: Duration,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(shared.config.handshake_timeout))?;
    stream.set_write_timeout(Some(write_timeout))?;
    #[allow(clippy::result_large_err)] // the error type is tungstenite's callback contract
    let socket = tungstenite::accept_hdr_with_config(
        stream,
        |request: &Request, response: Response| authorise(request, response, token, addr, origins),
        Some(link::socket_config()),
    )
    .map_err(|e| io::Error::other(e.to_string()))?;
    let mut link = link::WsLink::new(socket, write_timeout)?;
    session::serve(&mut link, Role::Controller, shared);
    Ok(())
}

/// Equal contents in time that depends only on the length.
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn refuse(status: u16, why: &str) -> ErrorResponse {
    http::Response::builder().status(status).body(Some(why.to_string())).expect("static response")
}

#[allow(clippy::result_large_err)] // `ErrorResponse` is the type tungstenite's callback requires
/// The upgrade itself is authenticated: a loopback Host header (against DNS rebinding), an exact
/// Origin when one is sent, the bearer token, and the `atomics.v1` subprotocol.
fn authorise(
    request: &Request,
    mut response: Response,
    token: &str,
    addr: SocketAddr,
    origins: &[String],
) -> Result<Response, ErrorResponse> {
    let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
    let port = addr.port();
    let host_ok = header("host")
        .is_some_and(|h| [format!("127.0.0.1:{port}"), format!("localhost:{port}")].iter().any(|ok| h.eq_ignore_ascii_case(ok)));
    if !host_ok {
        return Err(refuse(403, "unexpected Host"));
    }
    if let Some(origin) = header("origin")
        && !origins.iter().any(|o| o == origin)
    {
        return Err(refuse(403, "Origin not allowed"));
    }
    let presented = header("authorization").and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    if !same_secret(presented.as_bytes(), token.as_bytes()) {
        return Err(refuse(401, "a bearer token is required"));
    }
    let offers = header("sec-websocket-protocol").is_some_and(|v| v.split(',').any(|p| p.trim() == wire::SUBPROTOCOL));
    if !offers {
        return Err(refuse(400, "subprotocol atomics.v1 is required"));
    }
    response
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", http::HeaderValue::from_static(wire::SUBPROTOCOL));
    Ok(response)
}
