//! Read-only projections of the host's model, published as immutable snapshots.
//!
//! The runtime thread builds a projection from plain data and publishes it; any other thread loads
//! the current `Arc` and encodes or sends it with no `State` lock held. A collection's revision
//! moves only when its content does, so a reader can tell "nothing changed" by comparing numbers.
//! Revisions and the cursor are opaque strings on the wire (`wire::Cursor`); here they are `u64`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};

use crate::diff::{FileDiff, FileStatus, WorktreeDiff};
use crate::plan::{PlanStatus, TaskStatus};
use crate::session::AgentStatus;
use crate::steer::ReviewComment;
use crate::wire::Disposition;

/// One published value and the revision it was published at. Never mutated after publication.
#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot<T> {
    pub revision: u64,
    pub data: T,
}

/// A slot holding the latest snapshot. `load` takes a lock only to clone an `Arc`; nothing
/// is computed or sent under it.
#[derive(Debug)]
pub struct Published<T> {
    slot: RwLock<Arc<Snapshot<T>>>,
}

impl<T: PartialEq + Default> Default for Published<T> {
    fn default() -> Self {
        Self { slot: RwLock::new(Arc::new(Snapshot { revision: 0, data: T::default() })) }
    }
}

impl<T: PartialEq> Published<T> {
    pub fn load(&self) -> Arc<Snapshot<T>> {
        self.slot.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Publish `data` unless it equals the current snapshot. Returns whether the revision moved.
    pub fn publish(&self, data: T) -> bool {
        let mut slot = self.slot.write().unwrap_or_else(|e| e.into_inner());
        if slot.data == data {
            return false;
        }
        *slot = Arc::new(Snapshot { revision: slot.revision + 1, data });
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLink {
    pub plan: String,
    pub task: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub agent: String,
    pub title: String,
    /// A host path, never a browser path.
    pub cwd: String,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub status: AgentStatus,
    pub chief: bool,
    pub task: Option<TaskLink>,
    pub pending_approvals: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportInfo {
    pub status: String,
    pub summary: String,
    pub at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInfo {
    pub id: String,
    pub title: String,
    pub status: TaskStatus,
    pub agent: String,
    pub depends_on: Vec<String>,
    pub session: Option<String>,
    pub branch: Option<String>,
    pub integrated: Option<String>,
    pub additions: usize,
    pub deletions: usize,
    pub report: Option<ReportInfo>,
    /// The key of the ticket saying what the task is for (its id where it has no key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanInfo {
    pub id: String,
    pub status: PlanStatus,
    pub goal: String,
    pub project: Option<String>,
    pub approved: bool,
    pub integration: Option<String>,
    pub tasks: Vec<TaskInfo>,
    /// The digest of the plan's approval scope (goal and planned tasks), as the remote
    /// conversation's `/approve` uses it. `plan_command` `approve` and `set` name it, so the
    /// user approves or replaces exactly the plan they saw. Empty for a closed plan.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
    /// The key of the ticket (ticket mode) or board (board mode) the plan follows, its id where it has
    /// no key. A client opens the plan a ticket names by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
}

/// What the host says about itself beside the board: the workspace open there and the ticket system's
/// sign-in. It never holds a token or a client secret; while a sign-in waits it holds the address to
/// open in a browser, as the desktop's banner shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    /// The workspace open on the host, by name; `None` when none is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The saved workspaces, by name, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<String>,
    /// The configured ticket system's sign-in, when it signs in with OAuth from ATOMICS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_in: Option<SignInInfo>,
}

/// Where the ticket system's sign-in stands, as the Sign in and Reconnect buttons show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignInInfo {
    /// The ticket system, as `[tickets] system` names it.
    pub system: String,
    /// The server's host, never its path or query.
    pub server: String,
    pub state: SignInState,
    /// Why it needs a sign-in, or why the last one did not finish; free of tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// While a sign-in waits: the address to open in a browser, on any machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// While a sign-in waits: why the address pasted last was not used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected: Option<String>,
    /// While a sign-in waits: the host could not open its own browser, so the address has to be opened by hand.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub browser_failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInState {
    /// Signed in; the ticket tools work.
    SignedIn,
    /// Never signed in on the host, or signed out: "Sign in to <system>".
    SignedOut,
    /// It was signed in, but the server refused the saved sign-in or it ran out: "Reconnect".
    Expired,
    /// A sign-in is under way; `url` and `rejected` say how far.
    SigningIn,
    /// The host's keychain could not be read; `reason` says why.
    Unavailable,
    /// A state from a newer host.
    #[serde(other)]
    Unknown,
}

impl SignInInfo {
    /// Whether the ticket tools need the person: anything but signed in (or under way).
    pub fn needs_sign_in(&self) -> bool {
        matches!(self.state, SignInState::SignedOut | SignInState::Expired)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalInfo {
    pub id: String,
    pub session: String,
    pub tool: String,
    pub command: Option<String>,
    pub cwd: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFileSummary {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    pub additions: usize,
    pub deletions: usize,
    /// Lines of an untracked file whose content was left out.
    pub omitted: usize,
}

/// A worktree's diff without hunks: fetch a file's hunks with `diff_get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSummary {
    pub worktree: String,
    /// The host's content revision for this worktree's diff.
    pub revision: u64,
    pub base_ref: String,
    pub base: String,
    pub partial: bool,
    pub files: Vec<DiffFileSummary>,
    /// Review comments on this worktree waiting to be sent to its agent, shared by every client.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<ReviewComment>,
}

impl DiffSummary {
    pub fn new(worktree: &str, revision: u64, diff: &WorktreeDiff) -> Self {
        Self {
            worktree: worktree.into(),
            revision,
            base_ref: diff.base_ref.clone(),
            base: diff.base.clone(),
            partial: diff.partial,
            files: diff
                .files
                .iter()
                .map(|f| DiffFileSummary {
                    old_path: f.old_path.clone(),
                    new_path: f.new_path.clone(),
                    status: f.status,
                    binary: f.binary,
                    additions: f.additions(),
                    deletions: f.deletions(),
                    omitted: f.omitted,
                })
                .collect(),
            comments: Vec::new(),
        }
    }

    /// The event for a worktree whose diff is gone: no files and no base, at the last revision + 1.
    /// `wire` has no removal message for diffs, so this marks it.
    pub fn tombstone(worktree: &str, revision: u64) -> Self {
        Self {
            worktree: worktree.into(),
            revision,
            base_ref: String::new(),
            base: String::new(),
            partial: false,
            files: Vec::new(),
            comments: Vec::new(),
        }
    }

    pub fn is_tombstone(&self) -> bool {
        self.base.is_empty() && self.base_ref.is_empty() && self.files.is_empty()
    }

    pub fn additions(&self) -> usize {
        self.files.iter().map(|f| f.additions).sum()
    }

    pub fn deletions(&self) -> usize {
        self.files.iter().map(|f| f.deletions).sum()
    }
}

/// One file's hunks at a diff revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffFileBody {
    pub worktree: String,
    pub revision: u64,
    pub file: FileDiff,
}

/// All projections the protocol serves. Shared between the runtime thread (the only publisher)
/// and the protocol thread (a reader).
#[derive(Debug, Default)]
pub struct Projections {
    pub sessions: Published<Vec<SessionInfo>>,
    pub plans: Published<Vec<PlanInfo>>,
    pub approvals: Published<Vec<ApprovalInfo>>,
    pub diffs: Published<Vec<DiffSummary>>,
    pub host: Published<HostInfo>,
    cursor: AtomicU64,
    /// How approvals that left the board ended, newest last, so `approval_resolved` can say.
    resolutions: Mutex<VecDeque<(String, Disposition)>>,
}

/// Approval outcomes kept for `approval_resolved`; an older one is reported as withdrawn.
pub const MAX_RESOLUTIONS: usize = 256;

/// Everything at one moment, as sent in a `state_snapshot` and as the protocol thread reads it.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub cursor: u64,
    pub sessions: Arc<Snapshot<Vec<SessionInfo>>>,
    pub plans: Arc<Snapshot<Vec<PlanInfo>>>,
    pub approvals: Arc<Snapshot<Vec<ApprovalInfo>>>,
    pub diffs: Arc<Snapshot<Vec<DiffSummary>>>,
    pub host: Arc<Snapshot<HostInfo>>,
}

impl Projections {
    /// The event cursor: it moves by one for each collection that changed.
    pub fn cursor(&self) -> u64 {
        self.cursor.load(Ordering::Acquire)
    }

    fn moved(&self, changed: bool) -> bool {
        if changed {
            self.cursor.fetch_add(1, Ordering::AcqRel);
        }
        changed
    }

    pub fn publish_sessions(&self, data: Vec<SessionInfo>) -> bool {
        self.moved(self.sessions.publish(data))
    }

    pub fn publish_plans(&self, data: Vec<PlanInfo>) -> bool {
        self.moved(self.plans.publish(data))
    }

    pub fn publish_approvals(&self, data: Vec<ApprovalInfo>) -> bool {
        self.moved(self.approvals.publish(data))
    }

    pub fn publish_diffs(&self, data: Vec<DiffSummary>) -> bool {
        self.moved(self.diffs.publish(data))
    }

    pub fn publish_host(&self, data: HostInfo) -> bool {
        self.moved(self.host.publish(data))
    }

    /// Record how approvals ended (the newest `MAX_RESOLUTIONS`). Publish them before the
    /// approvals that no longer list them, so a reader that sees one gone finds how it ended.
    pub fn publish_resolutions(&self, resolved: &VecDeque<(String, Disposition)>) {
        let mut kept = self.resolutions.lock().unwrap_or_else(|e| e.into_inner());
        if *kept != *resolved {
            kept.clone_from(resolved);
            while kept.len() > MAX_RESOLUTIONS {
                kept.pop_front();
            }
        }
    }

    /// How the approval `id` ended, if it was answered lately.
    pub fn resolution(&self, id: &str) -> Option<Disposition> {
        let kept = self.resolutions.lock().unwrap_or_else(|e| e.into_inner());
        kept.iter().rev().find(|(i, _)| i == id).map(|(_, d)| *d)
    }

    /// Load every collection. The cursor is read first, so a publish racing the loads can only
    /// make the data newer than the cursor: the client then sees an event it already has, which
    /// replay treats as a no-op, never a gap.
    pub fn load(&self) -> Loaded {
        let cursor = self.cursor();
        Loaded {
            cursor,
            sessions: self.sessions.load(),
            plans: self.plans.load(),
            approvals: self.approvals.load(),
            diffs: self.diffs.load(),
            host: self.host.load(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, status: AgentStatus) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            agent: "claude".into(),
            title: id.into(),
            cwd: "/w".into(),
            worktree: None,
            branch: None,
            status,
            chief: false,
            task: None,
            pending_approvals: 0,
        }
    }

    #[test]
    fn revision_moves_only_when_content_changes() {
        let p = Projections::default();
        assert_eq!(p.sessions.load().revision, 0);
        assert!(!p.publish_sessions(vec![]), "publishing the empty default is not a change");
        assert!(p.publish_sessions(vec![session("a", AgentStatus::Idle)]));
        assert!(!p.publish_sessions(vec![session("a", AgentStatus::Idle)]));
        assert!(p.publish_sessions(vec![session("a", AgentStatus::Working)]));
        assert_eq!(p.sessions.load().revision, 2);
        assert_eq!(p.cursor(), 2);
        assert!(p.publish_approvals(vec![ApprovalInfo {
            id: "x".into(),
            session: "a".into(),
            tool: "Bash".into(),
            command: None,
            cwd: "/w".into(),
            reason: None,
        }]));
        assert_eq!((p.approvals.load().revision, p.cursor()), (1, 3));
    }

    #[test]
    fn a_loaded_snapshot_is_immutable_across_later_publishes() {
        let p = Projections::default();
        p.publish_sessions(vec![session("a", AgentStatus::Idle)]);
        let held = p.load();
        p.publish_sessions(vec![]);
        assert_eq!(held.sessions.data.len(), 1);
        assert_eq!((held.sessions.revision, held.cursor), (1, 1));
        assert!(p.load().sessions.data.is_empty());
    }

    #[test]
    fn readers_proceed_while_another_thread_publishes() {
        let p = Arc::new(Projections::default());
        let writer = {
            let p = p.clone();
            std::thread::spawn(move || {
                for i in 0..200 {
                    p.publish_sessions(vec![
                        session("a", AgentStatus::Idle),
                        session(&i.to_string(), AgentStatus::Done),
                    ]);
                }
            })
        };
        let mut last = 0;
        while !writer.is_finished() {
            let s = p.sessions.load();
            assert!(s.revision >= last, "revisions never go backwards");
            last = s.revision;
        }
        writer.join().unwrap();
        assert_eq!(p.sessions.load().revision, 200);
    }
}
