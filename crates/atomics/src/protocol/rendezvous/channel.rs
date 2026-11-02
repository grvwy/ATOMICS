//! One channel of the tunnel as a [`Link`]: the `atomics.v1` session it carries reads the frames
//! the tunnel's thread hands it (`Inbox`) and queues its own on the tunnel's outbound queue.
//!
//! **H5b seam.** Today a body is one `atomics.v1` text frame in plaintext (`PLAINTEXT`), so the
//! service could read and forge it, and every channel is served as a viewer (`ROLE`). H5b adds the
//! Noise session here: the handshake on the first frames (before any JSON is parsed), decryption
//! in `recv`, encryption in `send`, and the role of the paired device instead of `ROLE`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use atomics_core::tunnel::{self, close};
use atomics_core::wire::MAX_FRAME_BYTES;

use super::super::link::{Link, LinkError, Wait, Waker};
use super::tunnel::Outbound;

/// Channel bodies are `atomics.v1` as it is, with no Noise layer yet (H5b).
#[allow(dead_code)] // A marker the tests read; H5b removes it.
pub const PLAINTEXT: bool = true;

/// What a channel's client may do. A plaintext channel may be read or forged by the service, so it
/// only ever watches: no lease, no input, no decisions. H5b replaces this with the paired device's role.
pub const ROLE: atomics_core::wire::Role = atomics_core::wire::Role::Viewer;

/// The frames that arrived for one channel and not yet read. The tunnel's thread fills it; the
/// session's thread empties it.
pub struct Inbox {
    state: Mutex<InboxState>,
    waker: Arc<Waker>,
}

#[derive(Default)]
struct InboxState {
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    /// The channel left: after its backlog, the session's link is closed.
    left: bool,
}

impl Inbox {
    /// Hand the channel a frame. False when its backlog is already over `limit`: its session has
    /// stopped reading, and the tunnel closes the channel rather than hold more.
    pub fn push(&self, body: Vec<u8>, limit: usize) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.left {
            return true;
        }
        if state.bytes > 0 && state.bytes + body.len() > limit {
            return false;
        }
        state.bytes += body.len();
        state.frames.push_back(body);
        drop(state);
        self.waker.wake();
        true
    }

    /// The channel ended: nothing more arrives, and nothing more may be sent.
    pub fn leave(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).left = true;
        self.waker.wake();
    }

    fn take(&self) -> Result<Option<Vec<u8>>, LinkError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.frames.pop_front() {
            Some(frame) => {
                state.bytes -= frame.len();
                Ok(Some(frame))
            }
            None if state.left => Err(LinkError::Closed),
            None => Ok(None),
        }
    }

    fn has_left(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).left
    }
}

pub struct ChannelLink {
    channel: u32,
    inbox: Arc<Inbox>,
    wait: Wait,
    outbound: Arc<Outbound>,
    send_timeout: Duration,
    closed: bool,
}

impl ChannelLink {
    /// A link for `channel` and the inbox the tunnel fills for it.
    pub fn new(channel: u32, outbound: Arc<Outbound>, send_timeout: Duration) -> std::io::Result<(Self, Arc<Inbox>)> {
        let wait = Wait::new()?;
        let inbox = Arc::new(Inbox { state: Mutex::default(), waker: wait.waker() });
        Ok((Self { channel, inbox: inbox.clone(), wait, outbound, send_timeout, closed: false }, inbox))
    }

    /// H5b: the Noise transport message is decrypted here, before this.
    fn text(frame: Vec<u8>) -> Result<Option<String>, LinkError> {
        if frame.len() > MAX_FRAME_BYTES {
            return Err(LinkError::Oversize);
        }
        String::from_utf8(frame).map(Some).map_err(|_| LinkError::Binary)
    }
}

impl Link for ChannelLink {
    fn send(&mut self, frame: &str) -> Result<(), LinkError> {
        if self.closed || self.inbox.has_left() {
            return Err(LinkError::Closed);
        }
        // H5b: encrypted into a Noise transport message here.
        self.outbound.push(self.channel, tunnel::data(self.channel, frame.as_bytes()), self.send_timeout)
    }

    fn recv(&mut self, wait: Duration) -> Result<Option<String>, LinkError> {
        if let Some(frame) = self.inbox.take()? {
            return Self::text(frame);
        }
        self.wait.wait(wait);
        match self.inbox.take()? {
            Some(frame) => Self::text(frame),
            None => Ok(None),
        }
    }

    fn close(&mut self, reason: &str) {
        self.close_with(close::NORMAL, reason);
    }

    /// Ask the service to close the channel, after what was queued for it. A channel that already
    /// left needs nothing: the service has forgotten it.
    fn close_with(&mut self, code: u16, _reason: &str) {
        if !self.closed && !self.inbox.has_left() {
            let code = if close::host_may_send(code) { code } else { close::NORMAL };
            self.outbound.control(tunnel::close_channel(self.channel, code));
        }
        self.closed = true;
    }

    fn waker(&self) -> Option<Arc<Waker>> {
        Some(self.wait.waker())
    }
}
