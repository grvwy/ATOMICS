//! Builds the protocol's projections from the native model. Runs on the runtime thread, under
//! the `State` lock, with plain data only: it does no I/O and never blocks on a client. Readers
//! load the published `Arc` snapshots and never touch `State`.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use atomics_core::diff::FileDiff;
use atomics_core::diff_data::Prepared;

use atomics_core::projection::{
    ApprovalInfo, DiffSummary, PlanInfo, Projections, ReportInfo, SessionInfo, TaskInfo, TaskLink,
};

use super::State;
use crate::protocol::{Departed, TerminalEntry, Terminals};

/// What the last publish was built from. Every runtime pass publishes under the `State` lock and
/// most change nothing a client sees, so each collection keeps a fingerprint of its source and is
/// rebuilt only when that moves. (`State` is changed from too many places for a counter bumped at
/// each to stay right; a fingerprint of what the projection reads cannot go stale, and finding
/// that nothing changed allocates nothing.)
#[derive(Default)]
pub(crate) struct DiffSummaries {
    /// Diff summaries already built, by worktree and the diff revision they were built from, so
    /// an unchanged diff is not walked again.
    summaries: HashMap<PathBuf, (u64, DiffSummary)>,
    sessions: Option<u64>,
    plans: Option<u64>,
    approvals: Option<u64>,
    diffs: Option<u64>,
}

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
    /// only when its content does, and a collection whose source is unchanged is not rebuilt.
    pub(crate) fn publish_projections(&self, out: &Projections, cache: &mut DiffSummaries, bodies: &DiffBodies) {
        let plans = self.plans_fingerprint();
        // A session's task link comes from the plans, so they are part of its source.
        let sessions = fingerprint(|h| (self.sessions_fingerprint(), plans).hash(h));
        let approvals = self.approvals_fingerprint();
        let diffs = self.diffs_fingerprint();
        if cache.sessions != Some(sessions) {
            out.publish_sessions(self.session_infos());
            cache.sessions = Some(sessions);
        }
        if cache.plans != Some(plans) {
            out.publish_plans(self.plan_infos());
            cache.plans = Some(plans);
        }
        if cache.approvals != Some(approvals) {
            out.publish_approvals(self.approval_infos());
            cache.approvals = Some(approvals);
        }
        if cache.diffs != Some(diffs) {
            self.publish_diffs(out, cache, bodies);
            cache.diffs = Some(diffs);
        }
    }

    /// Rebuild and publish everything whether or not it changed, as every pass used to; kept to
    /// measure `publish_projections` against.
    #[cfg(test)]
    fn publish_projections_unconditionally(&self, out: &Projections, cache: &mut DiffSummaries, bodies: &DiffBodies) {
        out.publish_sessions(self.session_infos());
        out.publish_plans(self.plan_infos());
        out.publish_approvals(self.approval_infos());
        self.publish_diffs(out, cache, bodies);
    }

    /// Everything `session_infos` reads from the sessions (the plans are fingerprinted apart).
    fn sessions_fingerprint(&self) -> u64 {
        fingerprint(|h| {
            for s in &self.sessions {
                (&s.id, &s.agent, &s.title, &s.cwd, s.worktree.as_ref().map(|w| &w.root), s.branch()).hash(h);
                (s.status, s.chief, s.approvals.len()).hash(h);
            }
        })
    }

    /// Everything `plan_infos` reads.
    fn plans_fingerprint(&self) -> u64 {
        fingerprint(|h| {
            for p in &self.plans {
                (&p.id, p.status as u8, &p.goal, &p.project, p.approved, &p.integration, p.tasks.len()).hash(h);
                for t in &p.tasks {
                    (&t.id, &t.title, t.status, &t.agent, &t.depends_on, &t.session, &t.branch).hash(h);
                    (&t.integrated, t.additions, t.deletions).hash(h);
                    t.report.as_ref().map(|r| (&r.status, &r.summary, r.at)).hash(h);
                }
            }
        })
    }

    /// Everything `approval_infos` reads.
    fn approvals_fingerprint(&self) -> u64 {
        fingerprint(|h| {
            for s in &self.sessions {
                for a in &s.approvals {
                    (&a.id, &s.id, &a.request.tool, &a.request.command, &a.request.cwd, &a.request.reason).hash(h);
                }
            }
        })
    }

    /// Order-independent, as the diffs are a map. The content pointer is in it because the bodies
    /// published for `diff_get` are replaced when it changes, even at an unchanged revision.
    fn diffs_fingerprint(&self) -> u64 {
        self.diffs
            .iter()
            .filter_map(|(root, data)| Some((root, data.revision, Arc::as_ptr(data.prepared.as_ref()?) as usize)))
            .map(|entry| fingerprint(|h| entry.hash(h)))
            .fold(0u64, u64::wrapping_add)
    }

    fn session_infos(&self) -> Vec<SessionInfo> {
        let task_of = |session: &str| {
            self.plans.iter().find_map(|plan| {
                plan.tasks
                    .iter()
                    .find(|t| t.session.as_deref() == Some(session))
                    .map(|t| TaskLink { plan: plan.id.clone(), task: t.id.clone() })
            })
        };
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
            .collect()
    }

    fn plan_infos(&self) -> Vec<PlanInfo> {
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
            .collect()
    }

    fn approval_infos(&self) -> Vec<ApprovalInfo> {
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
            .collect()
    }

    fn publish_diffs(&self, out: &Projections, cache: &mut DiffSummaries, bodies: &DiffBodies) {
        cache.summaries.retain(|root, _| self.diffs.contains_key(root));
        let mut diffs = Vec::new();
        let mut contents = HashMap::new();
        for (root, data) in &self.diffs {
            let Some(prepared) = &data.prepared else { continue };
            contents.insert(text(root), (data.revision, prepared.clone()));
            let entry = match cache.summaries.get(root) {
                Some((revision, summary)) if *revision == data.revision => summary.clone(),
                _ => {
                    let summary = DiffSummary::new(&text(root), data.revision, &prepared.diff);
                    cache.summaries.insert(root.clone(), (data.revision, summary.clone()));
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

fn fingerprint(feed: impl FnOnce(&mut DefaultHasher)) -> u64 {
    let mut hasher = DefaultHasher::new();
    feed(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{session, with_state};
    use super::super::{
        AgentKind, AgentStatus, ApprovalRequest, PendingApproval, Plan, PlanStatus, Report, State, Task, TaskStatus,
    };
    use super::{Arc, DiffBodies, DiffSummaries, Prepared, Projections};
    use crate::protocol::DiffSource;
    use atomics_core::diff::{FileDiff, FileStatus, WorktreeDiff};
    use atomics_core::projection::{ApprovalInfo, PlanInfo, SessionInfo};

    fn approval(id: &str) -> PendingApproval {
        PendingApproval {
            id: id.into(),
            request: ApprovalRequest {
                tool: "Bash".into(),
                command: Some("git status".into()),
                cwd: "/w".into(),
                reason: None,
                input: serde_json::json!({}),
            },
            reply: None,
        }
    }

    fn task(id: &str, session: Option<&str>) -> Task {
        Task { id: id.into(), title: format!("task {id}"), session: session.map(str::to_string), ..Task::default() }
    }

    fn prepared(files: usize) -> Arc<Prepared> {
        let file = |n: usize| FileDiff {
            old_path: None,
            new_path: Some(format!("src/file{n}.rs")),
            status: FileStatus::Added,
            binary: false,
            hunks: vec![],
            omitted: 0,
        };
        Arc::new(Prepared {
            diff: WorktreeDiff { files: (0..files).map(file).collect(), ..WorktreeDiff::default() },
            highlights: vec![],
            longest_line: 0,
            additions: 0,
            deletions: 0,
            all_files: Arc::default(),
        })
    }

    /// Everything a client would be told, as it stands in `out`.
    fn published(out: &Projections) -> (Vec<SessionInfo>, Vec<PlanInfo>, Vec<ApprovalInfo>) {
        let l = out.load();
        (l.sessions.data.clone(), l.plans.data.clone(), l.approvals.data.clone())
    }

    fn built(state: &State) -> (Vec<SessionInfo>, Vec<PlanInfo>, Vec<ApprovalInfo>) {
        (state.session_infos(), state.plan_infos(), state.approval_infos())
    }

    type Change = Box<dyn Fn(&mut State)>;

    fn change(f: impl Fn(&mut State) + 'static) -> Change {
        Box::new(f)
    }

    #[test]
    fn an_unchanged_board_is_not_rebuilt_and_every_projected_field_still_republishes() {
        with_state(
            "app::projection::tests::an_unchanged_board_is_not_rebuilt_and_every_projected_field_still_republishes",
            |mut state, _| {
                let root = state.launch_dir.clone();
                state.sessions = vec![
                    session("a", AgentKind::Claude, AgentStatus::Working, &root),
                    session("b", AgentKind::Claude, AgentStatus::Idle, &root),
                ];
                state.sessions[0].approvals.push(approval("x1"));
                let mut plan = Plan { id: "p1".into(), goal: "ship".into(), ..Plan::default() };
                plan.tasks = vec![task("t1", Some("a")), task("t2", None)];
                state.plans = vec![plan];
                let (out, bodies) = (Projections::default(), DiffBodies::default());
                let mut cache = DiffSummaries::default();

                state.publish_projections(&out, &mut cache, &bodies);
                assert_eq!(published(&out), built(&state));
                assert_eq!(published(&out).0[0].task.as_ref().map(|t| t.task.as_str()), Some("t1"));
                let cursor = out.cursor();

                // Nothing changed, so nothing is rebuilt: a marker planted in the published slot stays.
                out.publish_sessions(Vec::new());
                let planted = out.cursor();
                state.publish_projections(&out, &mut cache, &bodies);
                assert!(out.sessions.load().data.is_empty(), "an unchanged board was rebuilt");
                assert_eq!(out.cursor(), planted);
                assert!(planted > cursor);
                // Put the real sessions back for what follows.
                out.publish_sessions(state.session_infos());

                // Each change to something a client sees is published, and only then.
                let changes: Vec<(&str, Change)> = vec![
                    ("session title", change(|s| s.sessions[1].title = "renamed".into())),
                    ("session status", change(|s| s.sessions[1].status = AgentStatus::Done)),
                    ("session agent", change(|s| s.sessions[1].agent = "other".into())),
                    ("session chief", change(|s| s.sessions[1].chief = true)),
                    ("session cwd", change(|s| s.sessions[1].cwd = "/elsewhere".into())),
                    ("approval added", change(|s| s.sessions[1].approvals.push(approval("x2")))),
                    ("approval removed", change(|s| s.sessions[1].approvals.clear())),
                    ("session removed", change(|s| drop(s.sessions.pop()))),
                    ("plan goal", change(|s| s.plans[0].goal = "ship it".into())),
                    ("plan approved", change(|s| s.plans[0].approved = true)),
                    ("plan status", change(|s| s.plans[0].status = PlanStatus::Completed)),
                    ("plan integration", change(|s| s.plans[0].integration = Some("int".into()))),
                    ("task title", change(|s| s.plans[0].tasks[1].title = "new".into())),
                    ("task status", change(|s| s.plans[0].tasks[1].status = TaskStatus::Done)),
                    ("task agent", change(|s| s.plans[0].tasks[1].agent = "codex".into())),
                    ("task depends_on", change(|s| s.plans[0].tasks[1].depends_on = vec!["t1".into()])),
                    ("task branch", change(|s| s.plans[0].tasks[1].branch = Some("b".into()))),
                    ("task integrated", change(|s| s.plans[0].tasks[1].integrated = Some("main".into()))),
                    ("task counts", change(|s| s.plans[0].tasks[1].additions = 9)),
                    (
                        "task report",
                        change(|s| {
                            s.plans[0].tasks[1].report =
                                Some(Report { status: "done".into(), summary: "ok".into(), at: 1 })
                        }),
                    ),
                    ("task session (a session's task link)", change(|s| s.plans[0].tasks[0].session = None)),
                    ("task added", change(|s| s.plans[0].tasks.push(task("t3", None)))),
                    ("plan removed", change(|s| s.plans.clear())),
                ];
                for (what, change) in changes {
                    let was = built(&state);
                    change(&mut state);
                    assert_ne!(built(&state), was, "{what}: the change is not visible in the projection");
                    state.publish_projections(&out, &mut cache, &bodies);
                    assert_eq!(published(&out), built(&state), "{what}: not republished");
                    let cursor = out.cursor();
                    state.publish_projections(&out, &mut cache, &bodies);
                    assert_eq!(out.cursor(), cursor, "{what}: republished without a change");
                }

                // A diff is republished when its revision moves, and its bodies whenever its content does.
                let root = std::path::PathBuf::from("/work/tree");
                state.diffs.entry(root.clone()).or_default().prepared = Some(prepared(1));
                state.publish_projections(&out, &mut cache, &bodies);
                assert_eq!(out.diffs.load().data[0].files.len(), 1);
                assert!(bodies.file("/work/tree", "src/file1.rs").is_none());
                state.diffs.get_mut(&root).unwrap().prepared = Some(prepared(2));
                state.publish_projections(&out, &mut cache, &bodies);
                assert!(bodies.file("/work/tree", "src/file1.rs").is_some(), "new content at the same revision");
                state.diffs.get_mut(&root).unwrap().revision = 1;
                state.publish_projections(&out, &mut cache, &bodies);
                assert_eq!(out.diffs.load().data[0].files.len(), 2);
                state.diffs.clear();
                state.publish_projections(&out, &mut cache, &bodies);
                assert!(out.diffs.load().data.is_empty());
            },
        );
    }

    /// The cost of a runtime pass on a large board: `ATOMICS_BENCH_PROJECTION=<file>` writes the numbers.
    #[test]
    fn publishing_cost_on_a_large_board() {
        let Some(report) = std::env::var_os("ATOMICS_BENCH_PROJECTION") else { return };
        with_state("app::projection::tests::publishing_cost_on_a_large_board", move |mut state, _| {
            let root = state.launch_dir.clone();
            state.sessions = (0..400)
                .map(|i| {
                    let mut s = session(&format!("s{i}"), AgentKind::Claude, AgentStatus::Working, &root);
                    s.title = format!("a fairly long session title number {i} to make strings real");
                    if i % 4 == 0 {
                        s.approvals.push(approval(&format!("x{i}")));
                    }
                    s
                })
                .collect();
            state.plans = (0..100)
                .map(|p| Plan {
                    id: format!("plan{p}"),
                    goal: "a plan goal of a typical length for a real board".into(),
                    tasks: (0..20)
                        .map(|t| {
                            let mut task = task(&format!("t{t}"), (t == 0).then(|| format!("s{p}")).as_deref());
                            task.brief = "x".repeat(400);
                            task.report = Some(Report { status: "progress".into(), summary: "y".repeat(200), at: 1 });
                            task
                        })
                        .collect(),
                    ..Plan::default()
                })
                .collect();
            for i in 0..100 {
                let data = state.diffs.entry(format!("/work/tree{i}").into()).or_default();
                data.revision = 1;
                data.prepared = Some(prepared(50));
            }
            let (out, bodies) = (Projections::default(), DiffBodies::default());
            let (mut before, mut after) = (DiffSummaries::default(), DiffSummaries::default());
            state.publish_projections_unconditionally(&out, &mut before, &bodies);
            state.publish_projections(&out, &mut after, &bodies);
            const PASSES: u32 = 300;
            let time = |f: &mut dyn FnMut()| {
                let started = std::time::Instant::now();
                for _ in 0..PASSES {
                    f();
                }
                started.elapsed() / PASSES
            };
            let idle_before = time(&mut || state.publish_projections_unconditionally(&out, &mut before, &bodies));
            let idle_after = time(&mut || state.publish_projections(&out, &mut after, &bodies));
            // One session changes on every pass (its title, as a running agent's would).
            let mut n = 0;
            let busy_before = time(&mut || {
                n += 1;
                state.sessions[0].title = format!("t{n}");
                state.publish_projections_unconditionally(&out, &mut before, &bodies);
            });
            let busy_after = time(&mut || {
                n += 1;
                state.sessions[0].title = format!("t{n}");
                state.publish_projections(&out, &mut after, &bodies);
            });
            std::fs::write(
                report,
                format!(
                    "board: 400 sessions, 100 plans x 20 tasks, 100 approvals, 100 diffs x 50 files\n\
                     per pass, unchanged board: before {idle_before:?}, after {idle_after:?}\n\
                     per pass, one session title changed: before {busy_before:?}, after {busy_after:?}\n"
                ),
            )
            .unwrap();
        });
    }
}
