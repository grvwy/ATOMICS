//! One channel of the tunnel, on a thread of its own: the Noise handshake on its first frames
//! (`atomics_core::noise`), then one `atomics.v1` session inside the encrypted channel, with the
//! role the device's allowlist entry gives it (`devices.rs`).
//!
//! - **Sessions** (`Noise_IK`): the device's key must be on the allowlist, or the channel is closed
//!   with `not_paired` (4003) before the host answers. A view-only device is a viewer, any other a
//!   controller, so H4's grants (`welcome.may`) follow from what the user chose when pairing. Its
//!   paired name, not the `client_id` its `hello` chose, is the lease holder others see and the
//!   sender the operation journal records (`protocol::Peer`).
//! - **Pairing** (`Noise_IKpsk2`): only while the desktop offers a link, and one device at a time
//!   (otherwise `no_pairing`, 4004). The device's first message proves the psk and names it; the
//!   desktop shows its name and the code and asks; the answer goes back encrypted
//!   ([`PairAnswer`]). A paired device's channel goes on as its session.
//! - Before the handshake the host reads one bounded frame and parses nothing else. A message
//!   that fails to decrypt ends the channel with `refused` (4000); so does a first frame that is no
//!   handshake, and one that does not arrive in time ends it with `idle` (4001).
//! - Revoking the device from the desktop ends its sessions with `not_paired`.
//! - The host rekeys what it sends every hour (`Settings::rekey_every`); both ends also rekey
//!   every 1 GiB (`noise::Channel`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atomics_core::noise::{self, Established, Kind, NoiseError, PairAnswer, PairRequest, Responder};
use atomics_core::tunnel::{self, close};

use super::super::Sessions;
use super::super::link::{Link, LinkError, Wait, Waker};
use super::devices::{Answer, Devices, Live};
use super::keys::DeviceKeys;
use super::tunnel::Outbound;

/// What every channel of one tunnel needs.
pub struct Gate {
    /// This host's id at the service: the prologue binds it.
    pub host_id: String,
    pub keys: Arc<DeviceKeys>,
    pub devices: Arc<Devices>,
    pub sessions: Sessions,
    /// From `joined` to the first frame.
    pub handshake_timeout: Duration,
    /// From the host's answer to a pairing device's first message.
    pub pairing_timeout: Duration,
    pub send_timeout: Duration,
    pub rekey_every: Duration,
}

/// The frames that arrived for one channel and not yet read. The tunnel's thread fills it; the
/// channel's thread empties it.
pub struct Inbox {
    state: Mutex<InboxState>,
    waker: Arc<Waker>,
}

#[derive(Default)]
struct InboxState {
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    /// The channel left: after its backlog, the channel's reads fail.
    left: bool,
}

impl Inbox {
    /// Hand the channel a frame. False when its backlog is already over `limit`: its thread has
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

/// The channel's bytes: bodies in from its inbox, bodies out on the tunnel's queue.
struct Raw {
    channel: u32,
    inbox: Arc<Inbox>,
    wait: Wait,
    outbound: Arc<Outbound>,
    send_timeout: Duration,
    closed: bool,
}

impl Raw {
    fn send(&mut self, body: &[u8]) -> Result<(), LinkError> {
        if self.closed || self.inbox.has_left() {
            return Err(LinkError::Closed);
        }
        self.outbound.push(self.channel, tunnel::data(self.channel, body), self.send_timeout)
    }

    /// The next body, waiting up to `wait` for one.
    fn recv(&mut self, wait: Duration) -> Result<Option<Vec<u8>>, LinkError> {
        if let Some(body) = self.inbox.take()? {
            return Ok(Some(body));
        }
        self.wait.wait(wait);
        self.inbox.take()
    }

    /// Ask the service to close the channel, after what was queued for it. A channel that already
    /// left needs nothing: the service has forgotten it.
    fn close(&mut self, code: u16) {
        if !self.closed && !self.inbox.has_left() {
            let code = if close::host_may_send(code) { code } else { close::NORMAL };
            self.outbound.control(tunnel::close_channel(self.channel, code));
        }
        self.closed = true;
    }

    fn waker(&self) -> Arc<Waker> {
        self.wait.waker()
    }
}

/// Serve `channel` on a thread of its own: its handshake, then its session. Returns the inbox the
/// tunnel fills for it.
pub fn open(channel: u32, outbound: Arc<Outbound>, gate: Arc<Gate>) -> std::io::Result<Arc<Inbox>> {
    let wait = Wait::new()?;
    let inbox = Arc::new(Inbox { state: Mutex::default(), waker: wait.waker() });
    let raw = Raw { channel, inbox: inbox.clone(), wait, outbound, send_timeout: gate.send_timeout, closed: false };
    std::thread::Builder::new().name("atomics-protocol-channel".into()).spawn(move || serve(raw, &gate))?;
    Ok(inbox)
}

fn serve(mut raw: Raw, gate: &Gate) {
    let first = match read_raw(&mut raw, gate.handshake_timeout) {
        Ok(first) => first,
        Err(code) => return raw.close(code),
    };
    match Kind::of(&first) {
        Ok(Kind::Session) => session(raw, gate, &first),
        Ok(Kind::Pair) => pair(raw, gate, &first),
        Err(_) => raw.close(close::REFUSED),
    }
}

/// The channel's next body within `timeout`; the close code to end it with otherwise (normal
/// when it left).
fn read_raw(raw: &mut Raw, timeout: Duration) -> Result<Vec<u8>, u16> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(close::IDLE);
        }
        match raw.recv(left) {
            Ok(Some(body)) => return Ok(body),
            Ok(None) => {}
            Err(_) => return Err(close::NORMAL),
        }
    }
}

/// A known device's session.
fn session(mut raw: Raw, gate: &Gate, first: &[u8]) {
    let Ok(responder) = Responder::read(first, &gate.host_id, gate.keys.noise_secret(), None) else {
        return raw.close(close::REFUSED);
    };
    let key = responder.device();
    let Some((device, live)) = gate.devices.attach(&key, raw.waker()) else {
        return raw.close(close::NOT_PAIRED);
    };
    match responder.answer() {
        Ok((answer, Established { channel, .. })) if raw.send(&answer).is_ok() => {
            let mut link = SecureLink::new(raw, channel, gate.rekey_every);
            link.live = Some(live);
            gate.sessions.run(&mut link, &device.peer());
        }
        _ => raw.close(close::REFUSED),
    }
    gate.devices.detach(&key);
}

/// A device answering the pairing link.
fn pair(mut raw: Raw, gate: &Gate, first: &[u8]) {
    let Some((attempt, psk)) = gate.devices.begin() else { return raw.close(close::NO_PAIRING) };
    // Until the device has proved the psk, a failure gives the link back for another try.
    let failed = |mut raw: Raw, code: u16| {
        gate.devices.abandon(attempt);
        raw.close(code);
    };
    let read = Responder::read(first, &gate.host_id, gate.keys.noise_secret(), Some(&psk));
    drop(psk);
    let Ok(responder) = read else { return failed(raw, close::REFUSED) };
    let key = responder.device();
    let Ok((answer, Established { channel, code })) = responder.answer() else {
        return failed(raw, close::REFUSED);
    };
    if raw.send(&answer).is_err() {
        return failed(raw, close::NORMAL);
    }
    let mut link = SecureLink::new(raw, channel, gate.rekey_every);
    // Its first message names it; that it decrypts proves the psk.
    let request = match link.frame(gate.pairing_timeout) {
        Ok(frame) => frame,
        Err(code) => {
            gate.devices.abandon(attempt);
            return link.raw.close(code);
        }
    };
    let request = (request.len() <= noise::MAX_PAIR_REQUEST_BYTES)
        .then(|| serde_json::from_slice::<PairRequest>(&request).ok())
        .flatten();
    let Some(request) = request else {
        gate.devices.abandon(attempt);
        return link.raw.close(close::REFUSED);
    };
    if !gate.devices.claim(attempt, noise::device_name(&request.name), code, key, link.raw.waker()) {
        return link.raw.close(close::NO_PAIRING);
    }
    let Some(answer) = await_answer(&mut link.raw, &gate.devices, attempt) else {
        return gate.devices.abandon(attempt);
    };
    let access = match answer {
        Answer::Paired(access) => Some(access),
        _ => None,
    };
    let reply = serde_json::to_string(&PairAnswer { paired: access.is_some(), may: access }).expect("a small answer");
    if link.send(&reply).is_err() {
        return;
    }
    if access.is_none() {
        return link.raw.close(close::NOT_PAIRED);
    }
    // Paired: the channel goes on as the device's session (unless it was revoked already).
    match gate.devices.attach(&key, link.raw.waker()) {
        Some((device, live)) => {
            link.live = Some(live);
            gate.sessions.run(&mut link, &device.peer());
            gate.devices.detach(&key);
        }
        None => link.raw.close(close::NOT_PAIRED),
    }
}

/// Wait for the desktop's answer to a claimed pairing. `None` when the device left first. What it
/// sends meanwhile stays queued for its session.
fn await_answer(raw: &mut Raw, devices: &Devices, attempt: u64) -> Option<Answer> {
    loop {
        match devices.answer(attempt) {
            Answer::Pending => {}
            answer => return Some(answer),
        }
        if raw.inbox.has_left() {
            return None;
        }
        // The answer and the device's leaving both wake this; the second bounds the wait for
        // the pairing's expiry.
        raw.wait.wait(Duration::from_secs(1));
    }
}

/// A channel after its handshake: `atomics.v1` frames, each sealed into Noise transport messages.
pub struct SecureLink {
    raw: Raw,
    channel: noise::Channel,
    /// The paired device's session: revoking it ends this link.
    live: Option<Arc<Live>>,
    rekey_every: Duration,
    rekey_at: Instant,
    /// Why the link ended, when the session did not choose it: the close code then.
    end: Option<u16>,
}

impl SecureLink {
    fn new(raw: Raw, channel: noise::Channel, rekey_every: Duration) -> Self {
        Self { raw, channel, live: None, rekey_every, rekey_at: Instant::now() + rekey_every, end: None }
    }

    fn check(&mut self) -> Result<(), LinkError> {
        if self.live.as_ref().is_some_and(|live| live.revoked()) {
            self.end = Some(close::NOT_PAIRED);
            return Err(LinkError::Closed);
        }
        Ok(())
    }

    /// The next whole frame within `timeout`, or the close code to end the channel with.
    fn frame(&mut self, timeout: Duration) -> Result<Vec<u8>, u16> {
        let deadline = Instant::now() + timeout;
        loop {
            let message = read_raw(&mut self.raw, deadline.saturating_duration_since(Instant::now()))?;
            match self.channel.open(&message) {
                Ok(Some(frame)) => return Ok(frame),
                Ok(None) => {}
                Err(_) => return Err(close::REFUSED),
            }
        }
    }
}

impl Link for SecureLink {
    fn send(&mut self, frame: &str) -> Result<(), LinkError> {
        self.check()?;
        if Instant::now() >= self.rekey_at {
            self.channel.rekey_soon();
            self.rekey_at = Instant::now() + self.rekey_every;
        }
        let messages = self.channel.seal(frame.as_bytes()).map_err(|error| match error {
            NoiseError::TooBig => LinkError::Oversize,
            other => LinkError::Io(other.to_string()),
        })?;
        messages.iter().try_for_each(|message| self.raw.send(message))
    }

    fn recv(&mut self, wait: Duration) -> Result<Option<String>, LinkError> {
        let deadline = Instant::now() + wait;
        loop {
            self.check()?;
            let Some(message) = self.raw.recv(deadline.saturating_duration_since(Instant::now()))? else {
                return Ok(None);
            };
            match self.channel.open(&message) {
                Ok(Some(frame)) => return String::from_utf8(frame).map(Some).map_err(|_| LinkError::Binary),
                Ok(None) => {}
                Err(NoiseError::TooBig) => return Err(LinkError::Oversize),
                Err(_) => {
                    // Forged, replayed, reordered or cut: nothing after it can be read.
                    self.end = Some(close::REFUSED);
                    return Err(LinkError::Closed);
                }
            }
        }
    }

    fn close(&mut self, reason: &str) {
        self.close_with(close::NORMAL, reason);
    }

    fn close_with(&mut self, code: u16, _reason: &str) {
        self.raw.close(self.end.unwrap_or(code));
    }

    fn waker(&self) -> Option<Arc<Waker>> {
        Some(self.raw.waker())
    }
}
