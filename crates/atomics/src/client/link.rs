//! One WebSocket to a host's protocol server, on a thread of its own: the native client's transport. It
//! connects with the host's bearer token, then carries `atomics.v1` text frames both ways until either
//! side closes. What happens is reported as [`Event`]s, each followed by a wake, so the window draws it.

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use atomics_core::tunnel::close;
use atomics_core::wire;
use tungstenite::client::IntoClientRequest;
use tungstenite::{HandshakeError, Message, WebSocket};
use zeroize::Zeroizing;

/// Where a host's server is and how to get in.
pub struct Endpoint {
    /// The address to dial: the server's, or a forward's that leads to it.
    pub addr: SocketAddr,
    /// The `Host` the upgrade names: always a loopback name, whatever port the forward has.
    pub host: String,
    pub token: Zeroizing<String>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("addr", &self.addr).field("host", &self.host).finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Open { socket: u64, protocol: String },
    Data { socket: u64, bytes: Vec<u8> },
    Closed { socket: u64, code: u16 },
}

/// How long a read waits before the thread looks for frames to send; a keystroke waits at most this.
const POLL: Duration = Duration::from_millis(4);
const CONNECT: Duration = Duration::from_secs(5);

pub struct Link {
    outgoing: Sender<String>,
    stop: Arc<AtomicBool>,
}

impl Link {
    /// Find the host with `resolve` and connect, on a thread of its own; `wake` is called after every
    /// event. A host that cannot be found is a drop (1006), so the driver tries again later.
    pub fn open(
        socket: u64,
        resolve: super::Resolve,
        events: SyncSender<Event>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Link {
        let (outgoing, frames) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let running = stop.clone();
        let spawned = std::thread::Builder::new().name("atomics-client-link".into()).spawn(move || {
            let send = |event| {
                let _ = events.send(event);
                wake();
            };
            let code = match resolve().map_err(|_| close::ABNORMAL).and_then(|endpoint| connect(&endpoint)) {
                Ok((mut ws, protocol)) => {
                    send(Event::Open { socket, protocol });
                    pump(&mut ws, &frames, &running, &mut |bytes| send(Event::Data { socket, bytes }))
                }
                Err(code) => code,
            };
            send(Event::Closed { socket, code });
        });
        if spawned.is_err() {
            eprintln!("atomics: could not start the connection's thread");
        }
        Link { outgoing, stop }
    }

    pub fn send(&self, frame: String) {
        let _ = self.outgoing.send(frame);
    }

    /// Close it normally; no event follows that matters (the socket is no longer the driver's).
    pub fn close(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.close();
    }
}

/// The upgrade, with the token. A refused token is [`close::UNAUTHORISED`]; anything else that fails is
/// 1006, a drop.
fn connect(endpoint: &Endpoint) -> Result<(WebSocket<TcpStream>, String), u16> {
    let stream = TcpStream::connect_timeout(&endpoint.addr, CONNECT).map_err(|_| close::ABNORMAL)?;
    let _ = stream.set_nodelay(true);
    stream.set_read_timeout(Some(CONNECT)).map_err(|_| close::ABNORMAL)?;
    stream.set_write_timeout(Some(CONNECT)).map_err(|_| close::ABNORMAL)?;
    let mut request = format!("ws://{}", endpoint.host).into_client_request().map_err(|_| close::ABNORMAL)?;
    let headers = request.headers_mut();
    let bearer = Zeroizing::new(format!("Bearer {}", endpoint.token.as_str()));
    headers.insert("Authorization", bearer.parse().map_err(|_| close::ABNORMAL)?);
    headers.insert("Sec-WebSocket-Protocol", wire::SUBPROTOCOL.parse().map_err(|_| close::ABNORMAL)?);
    match tungstenite::client::client_with_config(request, stream, Some(socket_config())) {
        Ok((ws, response)) => {
            let protocol = response
                .headers()
                .get("Sec-WebSocket-Protocol")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            ws.get_ref().set_read_timeout(Some(POLL)).map_err(|_| close::ABNORMAL)?;
            Ok((ws, protocol))
        }
        Err(HandshakeError::Failure(tungstenite::Error::Http(response))) if response.status() == 401 => {
            Err(close::UNAUTHORISED)
        }
        Err(_) => Err(close::ABNORMAL),
    }
}

fn socket_config() -> tungstenite::protocol::WebSocketConfig {
    tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(wire::MAX_FRAME_BYTES))
        .max_frame_size(Some(wire::MAX_FRAME_BYTES))
}

/// Frames both ways until the host closes, the link drops, or `stop`. The close code it ended with.
fn pump(
    ws: &mut WebSocket<TcpStream>,
    frames: &Receiver<String>,
    stop: &AtomicBool,
    received: &mut dyn FnMut(Vec<u8>),
) -> u16 {
    loop {
        if stop.load(Ordering::Acquire) {
            let _ = ws.close(None);
            let _ = ws.flush();
            return close::NORMAL;
        }
        loop {
            match frames.try_recv() {
                Ok(frame) => {
                    trace(">", &frame);
                    if ws.write(Message::text(frame)).is_err() {
                        return close::ABNORMAL;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = ws.close(None);
                    return close::NORMAL;
                }
            }
        }
        match ws.flush() {
            Ok(()) => {}
            Err(tungstenite::Error::Io(e)) if would_block(&e) => {}
            Err(_) => return close::ABNORMAL,
        }
        match ws.read() {
            Ok(Message::Text(text)) => {
                trace("<", text.as_str());
                received(text.as_bytes().to_vec())
            }
            Ok(Message::Close(frame)) => return frame.map_or(close::ABNORMAL, |f| u16::from(f.code)),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if would_block(&e) => {}
            Err(tungstenite::Error::ConnectionClosed) => return close::NORMAL,
            Err(_) => return close::ABNORMAL,
        }
    }
}

/// `ATOMICS_CLIENT_TRACE=1`: each frame's kind and size on stderr, for diagnosing a connection. Never
/// its content, which may hold terminal text.
fn trace(direction: &str, frame: &str) {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("ATOMICS_CLIENT_TRACE").is_some_and(|v| v == "1"));
    if *ON {
        let kind = serde_json::from_str::<serde_json::Value>(frame)
            .ok()
            .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
            .unwrap_or_default();
        eprintln!("atomics client {direction} {kind} ({} bytes)", frame.len());
    }
}

fn would_block(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}
