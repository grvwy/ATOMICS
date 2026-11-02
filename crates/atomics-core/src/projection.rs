//! Read-only projections of the host's model, published as immutable snapshots.
//!
//! The runtime thread builds a projection from plain data and publishes it; any other thread loads
//! the current `Arc` and encodes or sends it with no `State` lock held. A collection's revision
//! moves only when its content does, so a reader can tell "nothing changed" by comparing numbers.
//! Revisions and the cursor are opaque strings on the wire (`wire::Cursor`); here they are `u64`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::diff::{FileDiff, FileStatus, WorktreeDiff};
use crate::plan::{PlanStatus, TaskStatus};
use crate::session::AgentStatus;

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
        }
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
    cursor: AtomicU64,
}

/// Everything at one moment, as sent in a `state_snapshot` and as the protocol thread reads it.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub cursor: u64,
    pub sessions: Arc<Snapshot<Vec<SessionInfo>>>,
    pub plans: Arc<Snapshot<Vec<PlanInfo>>>,
    pub approvals: Arc<Snapshot<Vec<ApprovalInfo>>>,
    pub diffs: Arc<Snapshot<Vec<DiffSummary>>>,
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
