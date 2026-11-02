//! One client's conversation with the host, over any `Link`.
//!
//! The first frame must be a `hello`. After the `welcome` the loop alternates between draining the
//! client's outbound queue and reading one frame, so a client that stops reading stalls only its
//! own thread, and that thread gives up after the link's write timeout.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use atomics_core::diff::FileDiff;
use atomics_core::projection::DiffFileBody;
use atomics_core::wire::{
    self, CAPABILITIES, Envelope, ErrorCode, Limits, MAX_SUBSCRIPTIONS, Message, Role, Seq, Topic, VERSION, WireError,
};

use super::hub::{Hub, Taken};
use super::link::{Link, LinkError};

/// Where `diff_get` finds hunks. Implemented over data the runtime thread published, never `State`.
pub trait DiffSource: Send + Sync {
    /// The diff revision and the file whose new or old path is `path`.
    fn file(&self, worktree: &str, path: &str) -> Option<(u64, FileDiff)>;
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub handshake_timeout: Duration,
    pub tick: Duration,
    pub ping_interval: Duration,
    pub idle_timeout: Duration,
    /// Errors a client may cause before it is disconnected.
    pub max_errors: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(5),
            tick: Duration::from_millis(25),
            ping_interval: Duration::from_secs(25),
            idle_timeout: Duration::from_secs(60),
            max_errors: 8,
        }
    }
}

pub struct Shared {
    pub hub: Arc<Mutex<Hub>>,
    pub diffs: Arc<dyn DiffSource>,
    pub stop: Arc<AtomicBool>,
    pub config: SessionConfig,
    pub queue_frames: usize,
}

fn lock(hub: &Mutex<Hub>) -> MutexGuard<'_, Hub> {
    hub.lock().unwrap_or_else(|e| e.into_inner())
}

/// Why a session ended; for tests and logs.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    Stopped,
    Closed,
    /// Refused in the handshake, or disconnected for misbehaving.
    Rejected(ErrorCode),
    Slow,
    Idle,
}

struct Out<'a> {
    link: &'a mut dyn Link,
    epoch: String,
    last_sent: Instant,
}

impl Out<'_> {
    fn raw(&mut self, frame: &str) -> Result<(), LinkError> {
        self.last_sent = Instant::now();
        self.link.send(frame)
    }

    fn envelope(&mut self, envelope: &Envelope) -> Result<(), LinkError> {
        match wire::encode(envelope) {
            Ok(frame) => self.raw(&frame),
            Err(e) => self.error(e, envelope.request_id.clone()),
        }
    }

    fn error(&mut self, error: WireError, request_id: Option<String>) -> Result<(), LinkError> {
        let envelope = error.into_envelope(request_id, Some(self.epoch.clone()));
        let frame = wire::encode(&envelope).expect("an error frame is small");
        self.raw(&frame)
    }
}

fn link_ended(error: LinkError) -> Ended {
    match error {
        LinkError::Stalled => Ended::Slow,
        _ => Ended::Closed,
    }
}

pub fn serve(link: &mut dyn Link, role: Role, shared: &Shared) -> Ended {
    let epoch = lock(&shared.hub).epoch().to_string();
    let mut out = Out { link, epoch, last_sent: Instant::now() };
    let ended = run(&mut out, role, shared);
    let reason = match &ended {
        Ended::Stopped => "host is shutting down",
        Ended::Rejected(_) => "protocol error",
        Ended::Slow => "client is too slow",
        Ended::Idle => "idle",
        Ended::Closed => "closed",
    };
    out.link.close(reason);
    ended
}

fn run(out: &mut Out<'_>, role: Role, shared: &Shared) -> Ended {
    let config = &shared.config;
    // The handshake: the first frame is a hello.
    let started = Instant::now();
    let hello = loop {
        if shared.stop.load(Ordering::Acquire) {
            return Ended::Stopped;
        }
        if started.elapsed() > config.handshake_timeout {
            let _ = out.error(WireError::new(ErrorCode::Malformed, "no hello"), None);
            return Ended::Rejected(ErrorCode::Malformed);
        }
        match out.link.recv(config.tick) {
            Ok(Some(frame)) => break frame,
            Ok(None) => {}
            Err(e) => return refuse_link(out, e),
        }
    };
    let envelope = match wire::decode(&hello) {
        Ok(e) => e,
        Err(e) => {
            let code = e.code;
            let _ = out.error(e, None);
            return Ended::Rejected(code);
        }
    };
    let Message::Hello { versions, capabilities, prior_epoch, prior_cursor, .. } = &envelope.message else {
        let _ = out.error(WireError::new(ErrorCode::Malformed, "the first message must be hello"), envelope.request_id);
        return Ended::Rejected(ErrorCode::Malformed);
    };
    let version = match wire::negotiate(versions, &[VERSION]) {
        Ok(v) => v,
        Err(e) => {
            let _ = out.error(e, envelope.request_id.clone());
            return Ended::Rejected(ErrorCode::UnsupportedVersion);
        }
    };
    let attached = match lock(&shared.hub).attach(prior_epoch.as_deref(), *prior_cursor) {
        Ok(a) => a,
        Err(e) => {
            let _ = out.error(e, envelope.request_id.clone());
            return Ended::Rejected(ErrorCode::Oversize);
        }
    };
    let outbox = attached.outbox;
    let welcome = Envelope::host(
        &attached.epoch,
        Message::Welcome {
            version,
            capabilities: wire::common_capabilities(capabilities, CAPABILITIES),
            limits: Limits { max_queued_frames: shared.queue_frames as u32, ..Limits::default() },
            role,
            resume: attached.resume,
        },
    )
    .reply_to(envelope.request_id.clone());
    let result = (|| {
        out.envelope(&welcome).map_err(link_ended)?;
        conversation(out, shared, &outbox)
    })();
    outbox.close();
    match result {
        Ok(()) => Ended::Closed,
        Err(ended) => ended,
    }
}

fn refuse_link(out: &mut Out<'_>, error: LinkError) -> Ended {
    let code = match error {
        LinkError::Oversize => ErrorCode::Oversize,
        LinkError::Binary => ErrorCode::Malformed,
        other => return link_ended(other),
    };
    let message = if code == ErrorCode::Oversize { "frame is over the size limit" } else { "frames are JSON text" };
    let _ = out.error(WireError::new(code, message), None);
    Ended::Rejected(code)
}

fn conversation(out: &mut Out<'_>, shared: &Shared, outbox: &Arc<super::hub::Outbox>) -> Result<(), Ended> {
    let config = &shared.config;
    let mut errors = 0u32;
    let mut subscriptions: HashSet<String> = HashSet::new();
    let mut last_heard = Instant::now();
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return Err(Ended::Stopped);
        }
        match outbox.take() {
            Taken::Frames(frames) => {
                for frame in frames {
                    out.raw(&frame).map_err(link_ended)?;
                }
            }
            Taken::Lagged => resync(out, shared, outbox)?,
        }
        match out.link.recv(config.tick) {
            Ok(Some(frame)) => {
                last_heard = Instant::now();
                let envelope = match wire::decode(&frame) {
                    Ok(e) => e,
                    Err(e) => {
                        errors += 1;
                        let code = e.code;
                        out.error(e, None).map_err(link_ended)?;
                        if code == ErrorCode::Oversize || errors > config.max_errors {
                            return Err(Ended::Rejected(code));
                        }
                        continue;
                    }
                };
                if let Err(e) = handle(out, shared, outbox, &mut subscriptions, envelope) {
                    match e {
                        Handled::Link(ended) => return Err(ended),
                        Handled::Refused(error, request_id) => {
                            errors += 1;
                            let code = error.code;
                            out.error(error, request_id).map_err(link_ended)?;
                            if errors > config.max_errors {
                                return Err(Ended::Rejected(code));
                            }
                        }
                    }
                }
            }
            Ok(None) => {
                if last_heard.elapsed() > config.idle_timeout {
                    return Err(Ended::Idle);
                }
                if out.last_sent.elapsed() > config.ping_interval {
                    let ping = Envelope::host(&out.epoch, Message::Ping { nonce: None });
                    out.envelope(&ping).map_err(link_ended)?;
                }
            }
            Err(e) => {
                return Err(refuse_link(out, e));
            }
        }
    }
}

fn resync(out: &mut Out<'_>, shared: &Shared, outbox: &super::hub::Outbox) -> Result<(), Ended> {
    if let Err(e) = lock(&shared.hub).resync(outbox) {
        // The board no longer fits in one frame; say so rather than leave the client stale.
        let code = e.code;
        let _ = out.error(e, None);
        return Err(Ended::Rejected(code));
    }
    Ok(())
}

enum Handled {
    Link(Ended),
    Refused(WireError, Option<String>),
}

impl From<LinkError> for Handled {
    fn from(e: LinkError) -> Self {
        Handled::Link(link_ended(e))
    }
}

fn handle(
    out: &mut Out<'_>,
    shared: &Shared,
    outbox: &super::hub::Outbox,
    subscriptions: &mut HashSet<String>,
    envelope: Envelope,
) -> Result<(), Handled> {
    let id = envelope.request_id.clone();
    let refuse = |code: ErrorCode, message: &str| Handled::Refused(WireError::new(code, message), id.clone());
    match envelope.message {
        Message::Subscribe { topics } => {
            for topic in topics {
                let key = match &topic {
                    Topic::Diff { worktree } => format!("diff:{worktree}"),
                    Topic::Plan { id } => format!("plan:{id}"),
                };
                if !subscriptions.contains(&key) && subscriptions.len() >= MAX_SUBSCRIPTIONS {
                    return Err(refuse(ErrorCode::TooMany, "too many subscriptions"));
                }
                subscriptions.insert(key);
            }
            Ok(())
        }
        Message::Unsubscribe { topics } => {
            for topic in topics {
                subscriptions.remove(&match &topic {
                    Topic::Diff { worktree } => format!("diff:{worktree}"),
                    Topic::Plan { id } => format!("plan:{id}"),
                });
            }
            Ok(())
        }
        Message::DiffGet { worktree, path, revision } => {
            let Some((current, file)) = shared.diffs.file(&worktree, &path) else {
                return Err(refuse(ErrorCode::NotFound, "no such diff file"));
            };
            if revision.is_some_and(|r| r != Seq(current)) {
                return Err(refuse(ErrorCode::StaleRevision, &format!("the diff is at revision {current}")));
            }
            let body = DiffFileBody { worktree, revision: current, file };
            let reply = Envelope::host(&out.epoch, Message::DiffFile { body }).reply_to(id);
            match wire::encode(&reply) {
                Ok(frame) => Ok(out.raw(&frame)?),
                Err(e) => Err(Handled::Refused(e, reply.request_id)),
            }
        }
        Message::FileGet { .. } => Err(refuse(ErrorCode::NotFound, "this host does not serve file contents yet")),
        Message::Ack { cursor } => {
            let head = lock(&shared.hub).head();
            if cursor.0 > head {
                return Err(refuse(ErrorCode::StaleRevision, "ack is ahead of the host's cursor"));
            }
            Ok(())
        }
        Message::Resync { .. } => {
            outbox_resync(shared, outbox).map_err(|e| Handled::Refused(e, id.clone()))
        }
        Message::Ping { nonce } => {
            let pong = Envelope::host(&out.epoch, Message::Pong { nonce }).reply_to(id);
            Ok(out.envelope(&pong)?)
        }
        Message::Pong { .. } => Ok(()),
        Message::Hello { .. } => Err(refuse(ErrorCode::Malformed, "already greeted")),
        _ => Err(refuse(ErrorCode::Malformed, "that message is sent by the host, not to it")),
    }
}

fn outbox_resync(shared: &Shared, outbox: &super::hub::Outbox) -> Result<(), WireError> {
    lock(&shared.hub).resync(outbox)
}
