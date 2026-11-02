//! Builds the protocol's projections from the native model. Runs on the runtime thread, under
//! the `State` lock, with plain data only: it does no I/O and never blocks on a client. Readers
//! load the published `Arc` snapshots and never touch `State`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use atomics_core::diff::FileDiff;
use atomics_core::diff_data::Prepared;

use atomics_core::projection::{
    ApprovalInfo, DiffSummary, PlanInfo, Projections, ReportInfo, SessionInfo, TaskInfo, TaskLink,
};

use super::State;
use crate::protocol::{Departed, TerminalEntry, Terminals};

/// Diff summaries already built, by worktree and the diff revision they were built from, so an
/// unchanged diff is not walked again on every runtime pass.
#[derive(Default)]
pub(crate) struct DiffSummaries(HashMap<PathBuf, (u64, DiffSummary)>);

/// The accepted diff content per worktree, published so the protocol thread can answer `diff_get`
/// without the `State` lock. Holds `Arc`s of what the diff worker already built, and remembers
/// worktrees whose diff went away lately.
#[derive(Default)]
pub(crate) struct DiffBodies(RwLock<HashMap<String, (u64, Arc<Prepared>)>>, Departed);

impl DiffBodies {
    fn replace(&self, entries: HashMap<String, (u64, Arc<Prepared>)>) {
        let same = {
            let current = self.0.read().unwrap_or_else(|e| e.into_inner());
            current.len() == entries.len()
                && entries.iter().all(|(k, (r, p))| current.get(k).is_some_and(|(cr, cp)| cr == r && Arc::ptr_eq(cp, p)))
        };
        if !same {
            let before = std::mem::replace(&mut *self.0.write().unwrap_or_else(|e| e.into_inner()), entries);
            let now = self.0.read().unwrap_or_else(|e| e.into_inner());
            self.1.record(before.keys().map(String::as_str), |w| now.contains_key(w));
        }
    }
}

impl crate::protocol::DiffSource for DiffBodies {
    fn file(&self, worktree: &str, path: &str) -> Option<(u64, FileDiff)> {
        let (revision, prepared) = self.0.read().unwrap_or_else(|e| e.into_inner()).get(worktree)?.clone();
        let file = prepared
            .diff
            .files
            .iter()
            .find(|f| f.new_path.as_deref() == Some(path) || f.old_path.as_deref() == Some(path))?;
        Some((revision, file.clone()))
    }

    fn moved_on(&self, worktree: &str, revision: u64) -> bool {
        match self.0.read().unwrap_or_else(|e| e.into_inner()).get(worktree) {
            Some((current, _)) => *current != revision,
            None => self.1.contains(worktree),
        }
    }
}

fn text(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

impl State {
    /// Publish which terminals exist, by session, for the protocol thread to stream. Called only
    /// while the protocol server runs; builds handles only when the set of terminals changed.
    pub(crate) fn publish_terminals(&self, out: &Terminals) {
        let live = || self.sessions.iter().filter_map(|s| Some((s, s.terminal.as_ref()?)));
        if out.matches(live().map(|(s, t)| (s.id.as_str(), t.screen_identity()))) {
            return;
        }
        out.publish(
            live()
                .map(|(s, t)| {
                    let handle = Arc::new(t.screen_handle());
                    TerminalEntry {
                        session: s.id.clone(),
                        identity: t.screen_identity(),
                        source: handle.clone(),
                        control: handle,
                    }
                })
                .collect(),
        );
    }

    /// The `[remote]` settings, for the host to decide whether to start the protocol server.
    pub(crate) fn remote_access(&self) -> atomics_core::config::RemoteAccessConfig {
        self.config.remote.clone()
    }

    /// Publish sessions, plans, approvals and diff summaries. Each collection's revision moves
    /// only when its content does.
    pub(crate) fn publish_projections(&self, out: &Projections, cache: &mut DiffSummaries, bodies: &DiffBodies) {
        let task_of = |session: &str| {
            self.plans.iter().find_map(|plan| {
                plan.tasks
                    .iter()
                    .find(|t| t.session.as_deref() == Some(session))
                    .map(|t| TaskLink { plan: plan.id.clone(), task: t.id.clone() })
            })
        };
        out.publish_sessions(
            self.sessions
                .iter()
                .map(|s| SessionInfo {
                    id: s.id.clone(),
                    agent: s.agent.clone(),
                    title: s.title.clone(),
                    cwd: text(&s.cwd),
                    worktree: s.worktree.as_ref().map(|w| text(&w.root)),
                    branch: s.branch().map(str::to_string),
                    status: s.status,
                    chief: s.chief,
                    task: task_of(&s.id),
                    pending_approvals: s.approvals.len() as u32,
                })
                .collect(),
        );
        out.publish_plans(
            self.plans
                .iter()
                .map(|p| PlanInfo {
                    id: p.id.clone(),
                    status: p.status,
                    goal: p.goal.clone(),
                    project: p.project.as_deref().map(text),
                    approved: p.approved,
                    integration: p.integration.clone(),
                    tasks: p
                        .tasks
                        .iter()
                        .map(|t| TaskInfo {
                            id: t.id.clone(),
                            title: t.title.clone(),
                            status: t.status,
                            agent: t.agent.clone(),
                            depends_on: t.depends_on.clone(),
                            session: t.session.clone(),
                            branch: t.branch.clone(),
                            integrated: t.integrated.clone(),
                            additions: t.additions,
                            deletions: t.deletions,
                            report: t.report.as_ref().map(|r| ReportInfo {
                                status: r.status.clone(),
                                summary: r.summary.clone(),
                                at: r.at,
                            }),
                        })
                        .collect(),
                })
                .collect(),
        );
        out.publish_approvals(
            self.sessions
                .iter()
                .flat_map(|s| {
                    s.approvals.iter().map(|a| ApprovalInfo {
                        id: a.id.clone(),
                        session: s.id.clone(),
                        tool: a.request.tool.clone(),
                        command: a.request.command.clone(),
                        cwd: text(&a.request.cwd),
                        reason: a.request.reason.clone(),
                    })
                })
                .collect(),
        );
        cache.0.retain(|root, _| self.diffs.contains_key(root));
        let mut diffs = Vec::new();
        let mut contents = HashMap::new();
        for (root, data) in &self.diffs {
            let Some(prepared) = &data.prepared else { continue };
            contents.insert(text(root), (data.revision, prepared.clone()));
            let entry = match cache.0.get(root) {
                Some((revision, summary)) if *revision == data.revision => summary.clone(),
                _ => {
                    let summary = DiffSummary::new(&text(root), data.revision, &prepared.diff);
                    cache.0.insert(root.clone(), (data.revision, summary.clone()));
                    summary
                }
            };
            diffs.push(entry);
        }
        diffs.sort_by(|a, b| a.worktree.cmp(&b.worktree));
        // Bodies first: a client told about a revision can always fetch it.
        bodies.replace(contents);
        out.publish_diffs(diffs);
    }
}
