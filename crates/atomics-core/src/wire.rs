//! The `atomics.v1` client protocol: JSON text frames, one `Envelope` per frame.
//!
//! Browser-safe: serde types and pure functions only, so the same code builds the host's server
//! and the wasm client. Ordering and tags follow `docs/daemon.md` (Client protocol). This module
//! carries the read-only slice (board, plans, diffs, approvals, cursors); terminal and mutation
//! messages join it in later slices, and until then they fail as `unknown_kind`.
//!
//! Rules every decoder here applies:
//! - A frame over `MAX_FRAME_BYTES` is rejected before it is parsed.
//! - A major version other than ours is rejected before the body is read (except `hello`, whose
//!   `versions` list is negotiated). Minor versions are additive: unknown fields are ignored.
//! - An unknown `kind` fails explicitly; it is never ignored.
//! - Cursors and revisions are decimal strings, so browser number precision cannot corrupt them.

use std::fmt;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};

use crate::projection::{ApprovalInfo, DiffFileBody, DiffSummary, Loaded, PlanInfo, ReportInfo, SessionInfo};

/// The WebSocket subprotocol name.
pub const SUBPROTOCOL: &str = "atomics.v1";
pub const VERSION: Version = Version { major: 1, minor: 0 };
/// Frames are capped before encryption; larger payloads are chunked by the transfer layer.
pub const MAX_FRAME_BYTES: usize = 256 * 1024;
pub const MAX_SUBSCRIPTIONS: usize = 256;
pub const MAX_PENDING_OPS: usize = 64;
pub const MAX_QUEUED_FRAMES: usize = 256;
pub const MAX_VERSIONS: usize = 16;
pub const MAX_CAPABILITIES: usize = 64;
/// The longest identifier, path or request ID accepted in a frame field.
pub const MAX_ID_BYTES: usize = 1024;

/// Capabilities this implementation can offer in a `welcome`.
pub const CAPABILITIES: &[&str] = &["board", "plans", "approvals", "diffs"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
}

/// An opaque, ordered 64-bit counter that travels as a decimal string.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(pub u64);

/// A position in the host's event stream (per boot epoch).
pub type Cursor = Seq;
/// A per-collection or per-entity content revision.
pub type Revision = Seq;

impl Serialize for Seq {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Seq {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<str>>::deserialize(d)?;
        if text.is_empty() || text.len() > 20 || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(de::Error::custom("a cursor is a decimal string"));
        }
        text.parse().map(Seq).map_err(|_| de::Error::custom("cursor out of range"))
    }
}

/// Stable error codes. The client maps these to messages and to retry or re-pair actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Malformed,
    Oversize,
    UnsupportedVersion,
    UnknownKind,
    TooMany,
    NotFound,
    StaleRevision,
    Unauthorised,
    SlowClient,
    Internal,
}

/// Why a frame was refused. Convert with `into_envelope` to answer the sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError {
    pub code: ErrorCode,
    pub message: String,
}

impl WireError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub fn into_envelope(self, request_id: Option<String>, epoch: Option<String>) -> Envelope {
        Envelope { v: VERSION, epoch, request_id, message: Message::Error { code: self.code, message: self.message } }
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for WireError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_frame_bytes: u32,
    pub max_subscriptions: u32,
    pub max_pending_ops: u32,
    pub max_queued_frames: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame_bytes: MAX_FRAME_BYTES as u32,
            max_subscriptions: MAX_SUBSCRIPTIONS as u32,
            max_pending_ops: MAX_PENDING_OPS as u32,
            max_queued_frames: MAX_QUEUED_FRAMES as u32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// May answer approvals and send commands (when those slices ship).
    Controller,
    Viewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetReason {
    NewClient,
    EpochChanged,
    CursorTooOld,
    CursorAhead,
}

/// The host's answer to "where was I": replay retained events after the cursor, or reset with a
/// `state_snapshot` at `cursor` (events after it follow).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Resume {
    Replay { after: Cursor },
    Reset { cursor: Cursor, reason: ResetReason },
}

/// Decide replay or reset from what the client last applied. `oldest_retained` is the lowest
/// cursor still in the replay ring (`head + 1` when it is empty); replay needs every event after
/// `prior_cursor`, so the ring must reach back to `prior_cursor + 1`.
pub fn decide_resume(
    epoch: &str,
    head: Cursor,
    oldest_retained: Cursor,
    prior_epoch: Option<&str>,
    prior_cursor: Option<Cursor>,
) -> Resume {
    let reset = |reason| Resume::Reset { cursor: head, reason };
    let (Some(prior_epoch), Some(after)) = (prior_epoch, prior_cursor) else {
        return reset(ResetReason::NewClient);
    };
    if prior_epoch != epoch {
        return reset(ResetReason::EpochChanged);
    }
    if after > head {
        return reset(ResetReason::CursorAhead);
    }
    if after.0.saturating_add(1) < oldest_retained.0 {
        return reset(ResetReason::CursorTooOld);
    }
    Resume::Replay { after }
}

/// Pick the highest version both sides speak: the client's best minor for a major the host
/// supports, clamped to the host's minor. Fails with `unsupported_version` listing ours.
pub fn negotiate(client: &[Version], host: &[Version]) -> Result<Version, WireError> {
    host.iter()
        .filter_map(|h| {
            let c = client.iter().filter(|c| c.major == h.major).map(|c| c.minor).max()?;
            Some(Version { major: h.major, minor: c.min(h.minor) })
        })
        .max()
        .ok_or_else(|| {
            let ours: Vec<_> = host.iter().map(|v| format!("{}.{}", v.major, v.minor)).collect();
            WireError::new(ErrorCode::UnsupportedVersion, format!("this host speaks {}", ours.join(", ")))
        })
}

/// The capabilities both sides have, in the host's order.
pub fn common_capabilities(client: &[String], host: &[&str]) -> Vec<String> {
    host.iter().filter(|h| client.iter().any(|c| c == *h)).map(|h| h.to_string()).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Allowed,
    AllowedAlways,
    Denied,
    Expired,
    Withdrawn,
}

/// What a client may subscribe to. Diffs are per worktree; the board collections are always sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "topic", rename_all = "snake_case")]
pub enum Topic {
    Diff { worktree: String },
    Plan { id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revisions {
    pub sessions: Revision,
    pub plans: Revision,
    pub approvals: Revision,
    pub diffs: Revision,
}

/// Every message, tagged by `kind`. Fields beyond those listed are ignored on read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Message {
    Hello {
        client_id: String,
        versions: Vec<Version>,
        #[serde(default)]
        capabilities: Vec<String>,
        /// The epoch and cursor last applied, to resume; both absent for a first connection.
        #[serde(default)]
        prior_epoch: Option<String>,
        #[serde(default)]
        prior_cursor: Option<Cursor>,
    },
    Welcome {
        version: Version,
        capabilities: Vec<String>,
        limits: Limits,
        role: Role,
        resume: Resume,
    },
    Subscribe {
        topics: Vec<Topic>,
    },
    Unsubscribe {
        topics: Vec<Topic>,
    },
    StateSnapshot {
        cursor: Cursor,
        revisions: Revisions,
        sessions: Vec<SessionInfo>,
        plans: Vec<PlanInfo>,
        approvals: Vec<ApprovalInfo>,
        diffs: Vec<DiffSummary>,
    },
    SessionUpsert {
        cursor: Cursor,
        session: SessionInfo,
    },
    SessionRemoved {
        cursor: Cursor,
        id: String,
    },
    PlanUpsert {
        cursor: Cursor,
        plan: PlanInfo,
    },
    PlanRemoved {
        cursor: Cursor,
        id: String,
    },
    TaskReport {
        cursor: Cursor,
        plan: String,
        task: String,
        report: ReportInfo,
    },
    ApprovalPending {
        cursor: Cursor,
        approval: ApprovalInfo,
    },
    ApprovalResolved {
        cursor: Cursor,
        id: String,
        disposition: Disposition,
    },
    DiffSummary {
        cursor: Cursor,
        summary: DiffSummary,
    },
    DiffGet {
        worktree: String,
        path: String,
        /// The revision the client holds, so the host can say it is stale.
        #[serde(default)]
        revision: Option<Revision>,
    },
    DiffFile {
        body: DiffFileBody,
    },
    FileGet {
        worktree: String,
        path: String,
    },
    FileContent {
        worktree: String,
        path: String,
        content: String,
        truncated: bool,
    },
    Ack {
        cursor: Cursor,
    },
    Resync {
        #[serde(default)]
        reason: Option<String>,
    },
    Ping {
        #[serde(default)]
        nonce: Option<String>,
    },
    Pong {
        #[serde(default)]
        nonce: Option<String>,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

const KINDS: &[&str] = &[
    "hello",
    "welcome",
    "subscribe",
    "unsubscribe",
    "state_snapshot",
    "session_upsert",
    "session_removed",
    "plan_upsert",
    "plan_removed",
    "task_report",
    "approval_pending",
    "approval_resolved",
    "diff_summary",
    "diff_get",
    "diff_file",
    "file_get",
    "file_content",
    "ack",
    "resync",
    "ping",
    "pong",
    "error",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: Version,
    /// The host's boot epoch, on every host message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    /// Echoed on the reply to a request that carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(flatten)]
    pub message: Message,
}

impl Envelope {
    pub fn new(message: Message) -> Self {
        Self { v: VERSION, epoch: None, request_id: None, message }
    }

    pub fn host(epoch: &str, message: Message) -> Self {
        Self { v: VERSION, epoch: Some(epoch.into()), request_id: None, message }
    }

    pub fn reply_to(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }

    /// A `state_snapshot` at the cursor the projections were loaded at.
    pub fn snapshot(epoch: &str, loaded: &Loaded) -> Self {
        Self::host(
            epoch,
            Message::StateSnapshot {
                cursor: Seq(loaded.cursor),
                revisions: Revisions {
                    sessions: Seq(loaded.sessions.revision),
                    plans: Seq(loaded.plans.revision),
                    approvals: Seq(loaded.approvals.revision),
                    diffs: Seq(loaded.diffs.revision),
                },
                sessions: loaded.sessions.data.clone(),
                plans: loaded.plans.data.clone(),
                approvals: loaded.approvals.data.clone(),
                diffs: loaded.diffs.data.clone(),
            },
        )
    }
}

impl Message {
    /// Bounds a decoded message must keep: collection sizes and identifier lengths.
    pub fn validate(&self) -> Result<(), WireError> {
        let too_many = |what: &str, n: usize, max: usize| {
            (n > max).then(|| WireError::new(ErrorCode::TooMany, format!("{what}: {n} exceeds {max}")))
        };
        let long = |what: &str, s: &str| {
            (s.len() > MAX_ID_BYTES).then(|| WireError::new(ErrorCode::Oversize, format!("{what} is too long")))
        };
        let failure = match self {
            Message::Hello { client_id, versions, capabilities, prior_epoch, .. } => {
                too_many("versions", versions.len(), MAX_VERSIONS)
                    .or_else(|| too_many("capabilities", capabilities.len(), MAX_CAPABILITIES))
                    .or_else(|| long("client_id", client_id))
                    .or_else(|| prior_epoch.as_deref().and_then(|e| long("prior_epoch", e)))
                    .or_else(|| {
                        versions.is_empty().then(|| WireError::new(ErrorCode::Malformed, "hello offers no versions"))
                    })
            }
            Message::Subscribe { topics } | Message::Unsubscribe { topics } => {
                too_many("topics", topics.len(), MAX_SUBSCRIPTIONS).or_else(|| {
                    topics.iter().find_map(|t| match t {
                        Topic::Diff { worktree } => long("worktree", worktree),
                        Topic::Plan { id } => long("plan id", id),
                    })
                })
            }
            Message::DiffGet { worktree, path, .. } | Message::FileGet { worktree, path } => {
                long("worktree", worktree).or_else(|| long("path", path))
            }
            _ => None,
        };
        failure.map_or(Ok(()), Err)
    }
}

/// Parse one frame. See the module docs for the order of the checks.
pub fn decode(frame: &str) -> Result<Envelope, WireError> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(WireError::new(
            ErrorCode::Oversize,
            format!("frame is {} bytes; the limit is {MAX_FRAME_BYTES}", frame.len()),
        ));
    }
    let malformed = |e: &dyn fmt::Display| WireError::new(ErrorCode::Malformed, e.to_string());
    let value: serde_json::Value = serde_json::from_str(frame).map_err(|e| malformed(&e))?;
    let object = value.as_object().ok_or_else(|| malformed(&"a frame is a JSON object"))?;
    let kind = object.get("kind").and_then(|k| k.as_str()).ok_or_else(|| malformed(&"a frame has a string `kind`"))?;
    let v: Version =
        serde_json::from_value(object.get("v").cloned().ok_or_else(|| malformed(&"a frame has a version `v`"))?)
            .map_err(|e| malformed(&e))?;
    if kind != "hello" && v.major != VERSION.major {
        return Err(WireError::new(
            ErrorCode::UnsupportedVersion,
            format!(
                "version {}.{} is not supported; this host speaks {}.{}",
                v.major, v.minor, VERSION.major, VERSION.minor
            ),
        ));
    }
    if !KINDS.contains(&kind) {
        return Err(WireError::new(ErrorCode::UnknownKind, format!("unknown kind `{kind}`")));
    }
    let envelope: Envelope = serde_json::from_value(value).map_err(|e| malformed(&e))?;
    if let Some(id) = &envelope.request_id
        && id.len() > MAX_ID_BYTES
    {
        return Err(WireError::new(ErrorCode::Oversize, "request_id is too long"));
    }
    envelope.message.validate()?;
    Ok(envelope)
}

/// Serialize one frame, refusing one the peer would reject as oversize.
pub fn encode(envelope: &Envelope) -> Result<String, WireError> {
    let frame = serde_json::to_string(envelope).map_err(|e| WireError::new(ErrorCode::Internal, e.to_string()))?;
    if frame.len() > MAX_FRAME_BYTES {
        return Err(WireError::new(
            ErrorCode::Oversize,
            format!("frame is {} bytes; the limit is {MAX_FRAME_BYTES}", frame.len()),
        ));
    }
    Ok(frame)
}

#[cfg(test)]
mod tests;
