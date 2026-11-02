//! The byte-moving half of the protocol server. A `Link` carries whole text frames between the
//! server and one client; everything above it (handshake, queues, replay) is transport-agnostic, so
//! the loopback WebSocket and the later rendezvous transport serve the same session code.

use std::io;
use std::net::TcpStream;
use std::time::Duration;

use atomics_core::wire::MAX_FRAME_BYTES;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

#[derive(Debug, PartialEq, Eq)]
pub enum LinkError {
    Closed,
    /// The peer sent a frame over the size limit. The link refused it before buffering it all.
    Oversize,
    /// The peer sent a binary frame; `atomics.v1` is JSON text.
    Binary,
    /// A send did not finish in time: the peer is not reading.
    Stalled,
    Io(String),
}

pub trait Link: Send {
    /// Send one frame. May block, but only for this link's own bounded write time.
    fn send(&mut self, frame: &str) -> Result<(), LinkError>;
    /// Wait up to `wait` for one frame. `Ok(None)` means nothing arrived.
    fn recv(&mut self, wait: Duration) -> Result<Option<String>, LinkError>;
    fn close(&mut self, reason: &str);
}

pub fn socket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES))
        .max_write_buffer_size(2 * MAX_FRAME_BYTES)
}

/// A WebSocket over an accepted loopback TCP stream.
pub struct WsLink {
    socket: WebSocket<TcpStream>,
    read_wait: Option<Duration>,
}

fn would_block(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

fn map(error: tungstenite::Error) -> LinkError {
    use tungstenite::Error as E;
    use tungstenite::error::CapacityError;
    match error {
        E::ConnectionClosed | E::AlreadyClosed => LinkError::Closed,
        E::Capacity(CapacityError::MessageTooLong { .. }) => LinkError::Oversize,
        E::Io(e) if would_block(&e) => LinkError::Stalled,
        E::WriteBufferFull(_) => LinkError::Stalled,
        E::Io(e) => LinkError::Io(e.to_string()),
        other => LinkError::Io(other.to_string()),
    }
}

impl WsLink {
    pub fn new(socket: WebSocket<TcpStream>, write_timeout: Duration) -> io::Result<Self> {
        socket.get_ref().set_write_timeout(Some(write_timeout))?;
        socket.get_ref().set_nodelay(true)?;
        Ok(Self { socket, read_wait: None })
    }
}

impl Link for WsLink {
    fn send(&mut self, frame: &str) -> Result<(), LinkError> {
        self.socket.send(Message::text(frame)).map_err(map)
    }

    fn recv(&mut self, wait: Duration) -> Result<Option<String>, LinkError> {
        if self.read_wait != Some(wait) {
            self.socket.get_ref().set_read_timeout(Some(wait.max(Duration::from_millis(1)))).map_err(|e| LinkError::Io(e.to_string()))?;
            self.read_wait = Some(wait);
        }
        let read = self.socket.read();
        // Answering a ping queued a pong; push it out without waiting on a slow peer.
        let _ = self.socket.flush();
        match read {
            Ok(Message::Text(text)) => Ok(Some(text.as_str().to_owned())),
            Ok(Message::Binary(_)) => Err(LinkError::Binary),
            Ok(Message::Close(_)) => Err(LinkError::Closed),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e)) if would_block(&e) => Ok(None),
            Err(e) => Err(map(e)),
        }
    }

    fn close(&mut self, reason: &str) {
        use tungstenite::protocol::CloseFrame;
        use tungstenite::protocol::frame::coding::CloseCode;
        let frame = CloseFrame { code: CloseCode::Policy, reason: reason.chars().take(100).collect::<String>().into() };
        let _ = self.socket.close(Some(frame));
        let _ = self.socket.flush();
        let _ = self.socket.get_ref().shutdown(std::net::Shutdown::Both);
    }
}
