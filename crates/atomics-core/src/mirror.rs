//! A client's copy of the host's board, kept current by applying `atomics.v1` messages in order.
//!
//! Browser-safe and I/O free. Events carry consecutive cursors; a snapshot replaces everything.
//! A gap (a missing event, or an epoch that changed under a replay) is reported, never guessed
//! over: the caller sends `resync` and applies the snapshot that answers.

use std::collections::BTreeMap;

use crate::projection::{ApprovalInfo, DiffSummary, PlanInfo, SessionInfo};
use crate::wire::{Envelope, Message, Resume};

/// Why the mirror cannot follow the stream and needs a fresh snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gap {
    /// An event skipped from `have` to `got`.
    Missing { have: u64, got: u64 },
    /// The host's epoch is not the one this copy was built from.
    Epoch,
    /// An event arrived before the snapshot that a reset promised.
    NoSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Changed,
    /// An event already applied (a replay overlapping a snapshot).
    Duplicate,
    /// A message that does not change the board.
    Ignored,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Mirror {
    pub epoch: Option<String>,
    pub cursor: u64,
    pub sessions: BTreeMap<String, SessionInfo>,
    pub plans: BTreeMap<String, PlanInfo>,
    pub approvals: BTreeMap<String, ApprovalInfo>,
    /// By worktree.
    pub diffs: BTreeMap<String, DiffSummary>,
    awaiting_snapshot: bool,
}

impl Mirror {
    /// What to put in `hello` to resume: the epoch and cursor applied so far.
    pub fn resume_point(&self) -> (Option<String>, Option<u64>) {
        match (&self.epoch, self.awaiting_snapshot) {
            (Some(e), false) => (Some(e.clone()), Some(self.cursor)),
            _ => (None, None),
        }
    }

    pub fn apply(&mut self, envelope: &Envelope) -> Result<Applied, Gap> {
        if let Message::Welcome { resume, .. } = &envelope.message {
            match resume {
                Resume::Reset { .. } => {
                    *self = Mirror { epoch: envelope.epoch.clone(), awaiting_snapshot: true, ..Mirror::default() };
                }
                Resume::Replay { after } => {
                    if self.epoch != envelope.epoch || *after != crate::wire::Seq(self.cursor) {
                        return Err(Gap::Epoch);
                    }
                }
            }
            return Ok(Applied::Ignored);
        }
        if let (Some(have), Some(got)) = (&self.epoch, &envelope.epoch)
            && have != got
        {
            return Err(Gap::Epoch);
        }
        let cursor = match &envelope.message {
            Message::StateSnapshot { cursor, sessions, plans, approvals, diffs, .. } => {
                self.epoch = envelope.epoch.clone().or_else(|| self.epoch.take());
                self.cursor = cursor.0;
                self.awaiting_snapshot = false;
                self.sessions = sessions.iter().map(|s| (s.id.clone(), s.clone())).collect();
                self.plans = plans.iter().map(|p| (p.id.clone(), p.clone())).collect();
                self.approvals = approvals.iter().map(|a| (a.id.clone(), a.clone())).collect();
                self.diffs = diffs.iter().map(|d| (d.worktree.clone(), d.clone())).collect();
                return Ok(Applied::Changed);
            }
            Message::SessionUpsert { cursor, .. }
            | Message::SessionRemoved { cursor, .. }
            | Message::PlanUpsert { cursor, .. }
            | Message::PlanRemoved { cursor, .. }
            | Message::TaskReport { cursor, .. }
            | Message::ApprovalPending { cursor, .. }
            | Message::ApprovalResolved { cursor, .. }
            | Message::DiffSummary { cursor, .. } => cursor.0,
            _ => return Ok(Applied::Ignored),
        };
        if self.awaiting_snapshot {
            return Err(Gap::NoSnapshot);
        }
        if cursor <= self.cursor {
            return Ok(Applied::Duplicate);
        }
        if cursor != self.cursor + 1 {
            return Err(Gap::Missing { have: self.cursor, got: cursor });
        }
        match &envelope.message {
            Message::SessionUpsert { session, .. } => {
                self.sessions.insert(session.id.clone(), session.clone());
            }
            Message::SessionRemoved { id, .. } => {
                self.sessions.remove(id);
            }
            Message::PlanUpsert { plan, .. } => {
                self.plans.insert(plan.id.clone(), plan.clone());
            }
            Message::PlanRemoved { id, .. } => {
                self.plans.remove(id);
            }
            Message::ApprovalPending { approval, .. } => {
                self.approvals.insert(approval.id.clone(), approval.clone());
            }
            Message::ApprovalResolved { id, .. } => {
                self.approvals.remove(id);
            }
            Message::DiffSummary { summary, .. } => {
                if summary.is_tombstone() {
                    self.diffs.remove(&summary.worktree);
                } else {
                    self.diffs.insert(summary.worktree.clone(), summary.clone());
                }
            }
            // A report is already inside the plan it belongs to; it only advances the cursor.
            _ => {}
        }
        self.cursor = cursor;
        Ok(Applied::Changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::Projections;
    use crate::session::AgentStatus;
    use crate::wire::{Limits, ResetReason, Role, Seq, VERSION};

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

    fn upsert(cursor: u64, id: &str) -> Envelope {
        Envelope::host("e", Message::SessionUpsert { cursor: Seq(cursor), session: session(id, AgentStatus::Idle) })
    }

    fn snapshot() -> Envelope {
        let p = Projections::default();
        p.publish_sessions(vec![session("a", AgentStatus::Idle)]);
        Envelope::snapshot("e", &p.load())
    }

    fn welcome(resume: Resume) -> Envelope {
        Envelope::host(
            "e",
            Message::Welcome { version: VERSION, capabilities: vec![], limits: Limits::default(), role: Role::Viewer, resume },
        )
    }

    #[test]
    fn events_apply_in_order_and_duplicates_are_ignored() {
        let mut m = Mirror::default();
        assert_eq!(m.apply(&snapshot()), Ok(Applied::Changed));
        assert_eq!(m.resume_point(), (Some("e".into()), Some(1)));
        assert_eq!(m.apply(&upsert(1, "a")), Ok(Applied::Duplicate));
        assert_eq!(m.apply(&upsert(2, "b")), Ok(Applied::Changed));
        assert_eq!(m.sessions.len(), 2);
        assert_eq!(m.apply(&upsert(4, "c")), Err(Gap::Missing { have: 2, got: 4 }));
        assert_eq!(m.sessions.len(), 2, "a gap changes nothing");
    }

    #[test]
    fn another_epoch_is_a_gap_and_a_reset_waits_for_its_snapshot() {
        let mut m = Mirror::default();
        m.apply(&snapshot()).unwrap();
        let mut other = upsert(2, "b");
        other.epoch = Some("other".into());
        assert_eq!(m.apply(&other), Err(Gap::Epoch));

        m.apply(&welcome(Resume::Reset { cursor: Seq(7), reason: ResetReason::CursorTooOld })).unwrap();
        assert_eq!(m.resume_point(), (None, None), "nothing to resume from until the snapshot lands");
        assert_eq!(m.apply(&upsert(8, "b")), Err(Gap::NoSnapshot));
        m.apply(&snapshot()).unwrap();
        assert_eq!(m.apply(&upsert(2, "b")), Ok(Applied::Changed));
    }

    #[test]
    fn a_replay_must_continue_from_the_cursor_held() {
        let mut m = Mirror::default();
        m.apply(&snapshot()).unwrap();
        assert_eq!(m.apply(&welcome(Resume::Replay { after: Seq(1) })), Ok(Applied::Ignored));
        assert_eq!(m.apply(&welcome(Resume::Replay { after: Seq(5) })), Err(Gap::Epoch));
    }
}
