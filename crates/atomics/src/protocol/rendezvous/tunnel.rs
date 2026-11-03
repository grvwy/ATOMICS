//! The pump: the one thread that owns the service socket. It writes what the channels' sessions
//! queued (`Outbound`), reads the service's frames and hands each channel its own (`Inbox`),
//! opens a session for every `joined` and ends it on `left`, and renews the pass at half its
//! life through the issuer thread, so a slow MCP call never holds the frames up. Its waits end
//! early on a wake: anything queued, an issuer answer, or a stop.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use atomics_core::tunnel::{self, FromService, close};
use tungstenite::protocol::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::{Message, WebSocket};

use super::super::link::{LinkError, Waker};
use super::channel::{self, Gate, Inbox};
use super::issuer::{Ask, Desk, IssueKind, Reply};
use super::socket::Conn;
use super::{Backoff, Context, Phase};

/// What the channels' sessions queued for the service, in order. Each channel may have at most
/// `budget` bytes waiting (always at least one frame); a session that would pass it waits for
/// room up to its send timeout, then its link reports `Stalled` and the session ends as slow.
pub struct Outbound {
    queue: Mutex<Queue>,
    room: Condvar,
    tunnel: Arc<Waker>,
    budget: usize,
}

#[derive(Default)]
struct Queue {
    /// The channel a data frame counts against; control frames count against none.
    frames: VecDeque<(Option<u32>, Vec<u8>)>,
    queued: HashMap<u32, usize>,
    shut: bool,
}

impl Outbound {
    pub fn new(tunnel: Arc<Waker>, budget: usize) -> Self {
        Self { queue: Mutex::default(), room: Condvar::new(), tunnel, budget }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push(&self, channel: u32, frame: Vec<u8>, timeout: Duration) -> Result<(), LinkError> {
        let deadline = Instant::now() + timeout;
        let mut queue = self.lock();
        loop {
            if queue.shut {
                return Err(LinkError::Closed);
            }
            let queued = queue.queued.get(&channel).copied().unwrap_or(0);
            if queued == 0 || queued + frame.len() <= self.budget {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(LinkError::Stalled);
            }
            queue = self.room.wait_timeout(queue, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        *queue.queued.entry(channel).or_default() += frame.len();
        queue.frames.push_back((Some(channel), frame));
        drop(queue);
        self.tunnel.wake();
        Ok(())
    }

    /// A control frame: never waits, and goes out in order with the data before it.
    pub fn control(&self, frame: Vec<u8>) {
        let mut queue = self.lock();
        if !queue.shut {
            queue.frames.push_back((None, frame));
        }
        drop(queue);
        self.tunnel.wake();
    }

    fn take(&self) -> Vec<Vec<u8>> {
        let mut queue = self.lock();
        let frames = queue.frames.drain(..).map(|(_, frame)| frame).collect();
        queue.queued.clear();
        drop(queue);
        self.room.notify_all();
        frames
    }

    /// The socket is gone: every waiting and later send fails.
    fn shut(&self) {
        let mut queue = self.lock();
        queue.shut = true;
        queue.frames.clear();
        queue.queued.clear();
        drop(queue);
        self.room.notify_all();
    }
}

/// Why the pump stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    Stopped,
    /// The service closed the socket, with its close code if it gave one.
    Closed(Option<u16>),
    /// Nothing came from the service, not even a ping, for the idle timeout.
    Idle,
    Broken(String),
    /// A renewal answered that the host is revoked.
    Revoked(String),
}

fn would_block(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

struct Pump<'a> {
    socket: WebSocket<Conn>,
    outbound: Arc<Outbound>,
    channels: HashMap<u32, Arc<Inbox>>,
    ctx: &'a Context,
    desk: &'a mut Desk,
    /// What each channel's thread needs, with this tunnel's host id.
    gate: Arc<Gate>,
    renew_at: Option<Instant>,
    last_heard: Instant,
    since: Instant,
    stable: bool,
}

/// Pump `socket` until it ends. Every channel ends with it.
pub fn run(
    mut socket: WebSocket<Conn>,
    ctx: &Context,
    desk: &mut Desk,
    gate: Arc<Gate>,
    backoff: &mut Backoff,
) -> Ended {
    let waker = socket.get_mut().stream().waker();
    ctx.signal.set_tunnel(Some(waker.clone()));
    let outbound = Arc::new(Outbound::new(waker, ctx.settings.channel_buffer));
    let now = Instant::now();
    let mut pump = Pump {
        socket,
        outbound,
        channels: HashMap::new(),
        ctx,
        desk,
        gate,
        renew_at: None,
        last_heard: now,
        since: now,
        stable: false,
    };
    ctx.status.update(|s| {
        s.phase = Phase::Online;
        s.online = true;
        s.channels = 0;
        s.note = None;
    });
    let ended = pump.run(backoff);
    pump.outbound.shut();
    for inbox in pump.channels.values() {
        inbox.leave();
    }
    ctx.signal.set_tunnel(None);
    ctx.status.update(|s| {
        s.online = false;
        s.channels = 0;
    });
    ended
}

impl Pump<'_> {
    fn run(&mut self, backoff: &mut Backoff) -> Ended {
        loop {
            if self.ctx.signal.stopped() {
                self.goodbye();
                return Ended::Stopped;
            }
            if let Err(ended) = self.send_queued() {
                return ended;
            }
            self.sweep();
            if let Some(ended) = self.renewal() {
                return ended;
            }
            if !self.stable && self.since.elapsed() >= self.ctx.settings.stable {
                self.stable = true;
                backoff.reset();
            }
            let wait = self.wait();
            self.socket.get_mut().stream().set_wait(Some(wait));
            match self.socket.read() {
                Ok(Message::Binary(bytes)) => {
                    self.last_heard = Instant::now();
                    self.frame(&bytes);
                }
                Ok(Message::Close(frame)) => {
                    let _ = self.socket.flush();
                    return Ended::Closed(frame.map(|f| u16::from(f.code)));
                }
                // A ping (answered by tungstenite), a pong, or text, which this protocol never
                // uses: the service is there.
                Ok(_) => self.last_heard = Instant::now(),
                Err(tungstenite::Error::Io(error)) if would_block(&error) => {}
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Ended::Closed(None);
                }
                Err(error) => return Ended::Broken(format!("the tunnel broke: {error}")),
            }
            if self.last_heard.elapsed() > self.ctx.settings.idle_timeout {
                return Ended::Idle;
            }
        }
    }

    /// How long the next read may wait: until the next timer, at most a tick.
    fn wait(&self) -> Duration {
        let now = Instant::now();
        let mut wait = self.ctx.settings.tick;
        if let Some(at) = self.renew_at {
            wait = wait.min(at.saturating_duration_since(now));
        }
        wait = wait.min((self.last_heard + self.ctx.settings.idle_timeout).saturating_duration_since(now));
        wait.max(Duration::from_millis(1))
    }

    fn send_queued(&mut self) -> Result<(), Ended> {
        for frame in self.outbound.take() {
            match self.socket.send(Message::binary(frame)) {
                Ok(()) => {}
                Err(tungstenite::Error::Io(error)) if would_block(&error) => {
                    return Err(Ended::Broken(format!(
                        "the service read nothing for {} s",
                        self.ctx.settings.write_timeout.as_secs()
                    )));
                }
                Err(error) => return Err(Ended::Broken(format!("the tunnel broke: {error}"))),
            }
        }
        Ok(())
    }

    fn frame(&mut self, message: &[u8]) {
        match tunnel::parse(message) {
            Some(FromService::Joined(channel)) => self.join(channel),
            Some(FromService::Left { channel, .. }) => {
                if let Some(inbox) = self.channels.remove(&channel) {
                    inbox.leave();
                    self.count();
                }
            }
            Some(FromService::Data { channel, body }) => {
                let Some(inbox) = self.channels.get(&channel) else { return };
                if !inbox.push(body, self.ctx.settings.channel_buffer) {
                    // Its session stopped reading: close the channel rather than hold more.
                    inbox.leave();
                    self.channels.remove(&channel);
                    self.outbound.control(tunnel::close_channel(channel, close::SLOW));
                    self.count();
                }
            }
            Some(FromService::Expiry(until)) => self.schedule(until),
            // From a newer service, or malformed: neither names anything this host serves.
            Some(FromService::Unknown) | None => {}
        }
    }

    /// A new channel: its handshake and then its session, on a thread of its own. A `joined` is
    /// only a hint: a channel that cannot complete the handshake ends there.
    fn join(&mut self, channel: u32) {
        if self.channels.contains_key(&channel) {
            return;
        }
        self.sweep();
        let settings = &self.ctx.settings;
        if self.channels.len() >= settings.max_channels {
            self.outbound.control(tunnel::close_channel(channel, close::QUOTA));
            return;
        }
        match channel::open(channel, self.outbound.clone(), self.gate.clone()) {
            Ok(inbox) => {
                self.channels.insert(channel, inbox);
                self.count();
            }
            Err(_) => self.outbound.control(tunnel::close_channel(channel, close::UNAVAILABLE)),
        }
    }

    /// Forget the channels the host closed: their slots are free before the service's `left`,
    /// which then finds nothing.
    fn sweep(&mut self) {
        let before = self.channels.len();
        self.channels.retain(|_, inbox| !inbox.has_left());
        if self.channels.len() != before {
            self.count();
        }
    }

    fn count(&self) {
        let open = self.channels.len();
        self.ctx.status.update(|s| s.channels = open);
    }

    /// Renew at half of what is left of the admission, within the configured bounds: a host whose
    /// clock is off still renews in time, and never in a tight loop.
    fn schedule(&mut self, until_ms: i64) {
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        let half = Duration::from_millis(u64::try_from(until_ms.saturating_sub(now_ms) / 2).unwrap_or(0));
        let (least, most) = self.ctx.settings.renew;
        self.renew_at = Some(Instant::now() + half.clamp(least, most));
    }

    fn renewal(&mut self) -> Option<Ended> {
        if !self.desk.waiting() && self.renew_at.is_some_and(|at| Instant::now() >= at) {
            self.renew_at = None;
            self.desk.send(Ask::Pass { host_id: self.gate.host_id.clone() });
        }
        match self.desk.poll()? {
            Ok(Reply::Pass(pass)) => {
                self.outbound.control(tunnel::renew(&pass.0));
                self.ctx.status.update(|s| s.note = None);
            }
            Ok(Reply::Registered(_)) => {}
            Err(error) if error.kind == IssueKind::Revoked => {
                self.goodbye();
                return Some(Ended::Revoked(error.message));
            }
            Err(error) => {
                let settings = &self.ctx.settings;
                let retry = if error.kind == IssueKind::Throttled { settings.throttle_wait } else { settings.renew.0 };
                self.renew_at = Some(Instant::now() + retry);
                let note =
                    format!("renewing the pass failed: {}; trying again in {} s", error.message, retry.as_secs());
                self.ctx.status.update(|s| s.note = Some(note));
            }
        }
        None
    }

    fn goodbye(&mut self) {
        let _ = self.socket.close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() }));
        let _ = self.socket.flush();
    }
}
