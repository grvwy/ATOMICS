//! Builds the protocol's projections from the native model. Runs on the runtime thread, under
//! the `State` lock, with plain data only: it does no I/O and never blocks on a client. Readers
//! load the published `Arc` snapshots and never touch `State`.

use std::collections::HashMap;
use std::path::PathBuf;

use atomics_core::projection::{
    ApprovalInfo, DiffSummary, PlanInfo, Projections, ReportInfo, SessionInfo, TaskInfo, TaskLink,
};

use super::State;

/// Diff summaries already built, by worktree and the diff revision they were built from, so an
/// unchanged diff is not walked again on every runtime pass.
#[derive(Default)]
pub(crate) struct DiffSummaries(HashMap<PathBuf, (u64, DiffSummary)>);

fn text(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

impl State {
    /// Publish sessions, plans, approvals and diff summaries. Each collection's revision moves
    /// only when its content does.
    pub(crate) fn publish_projections(&self, out: &Projections, cache: &mut DiffSummaries) {
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
        for (root, data) in &self.diffs {
            let Some(prepared) = &data.prepared else { continue };
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
        out.publish_diffs(diffs);
    }
}
