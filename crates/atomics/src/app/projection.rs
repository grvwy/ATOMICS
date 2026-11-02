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

    /// Publish sessions, plans, approvals and diff summaries. Every pass rebuilds them; each
    /// collection's revision, and so the event cursor, moves only when its content does.
    pub(crate) fn publish_projections(&self, out: &Projections, cache: &mut DiffSummaries, bodies: &DiffBodies) {
        out.publish_sessions(self.session_infos());
        out.publish_plans(self.plan_infos());
        out.publish_approvals(self.approval_infos());
        self.publish_diffs(out, cache, bodies);
    }

    fn session_infos(&self) -> Vec<SessionInfo> {
        // Each session's task, found in one walk over the plans rather than one per session: the
        // first plan, in order, with a task on the session, and that plan's first such task.
        let mut links: HashMap<&str, TaskLink> = HashMap::new();
        for plan in &self.plans {
            for task in &plan.tasks {
                if let Some(session) = task.session.as_deref() {
                    links.entry(session).or_insert_with(|| TaskLink { plan: plan.id.clone(), task: task.id.clone() });
                }
            }
        }
        let task_of = |session: &str| links.get(session).cloned();
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

    /// The event cursor and each collection's revision, as they stand in `out`.
    fn revisions(out: &Projections) -> (u64, [u64; 4]) {
        let l = out.load();
        (l.cursor, [l.sessions.revision, l.plans.revision, l.approvals.revision, l.diffs.revision])
    }

    #[test]
    fn every_pass_rebuilds_but_revisions_move_only_on_a_real_change() {
        with_state(
            "app::projection::tests::every_pass_rebuilds_but_revisions_move_only_on_a_real_change",
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
                // A session's task is the first on it in plan order: a later task in the same plan
                // or in a later plan does not take its place.
                let mut later = Plan { id: "p2".into(), ..Plan::default() };
                later.tasks = vec![task("t9", Some("a"))];
                state.plans.push(later);
                state.plans[0].tasks.push(task("t8", Some("a")));
                let link = state.session_infos()[0].task.clone().map(|t| (t.plan, t.task));
                assert_eq!(link, Some(("p1".to_string(), "t1".to_string())));
                state.plans.pop();
                state.plans[0].tasks.pop();
                let (out, bodies) = (Projections::default(), DiffBodies::default());
                let mut cache = DiffSummaries::default();

                state.publish_projections(&out, &mut cache, &bodies);
                assert_eq!(published(&out), built(&state));
                assert_eq!(published(&out).0[0].task.as_ref().map(|t| t.task.as_str()), Some("t1"));

                // Passes that change nothing a client sees move no revision, so clients get no
                // event: neither does a change to something the projection leaves out.
                let quiet = revisions(&out);
                for _ in 0..3 {
                    state.publish_projections(&out, &mut cache, &bodies);
                }
                state.plans[0].tasks[0].brief = "not on the wire".into();
                state.plans[0].tasks[0].notes.push("nor this".into());
                state.publish_projections(&out, &mut cache, &bodies);
                assert_eq!(revisions(&out), quiet, "a pass with nothing to tell moved a revision");

                // Each change to something a client sees is published and moves the cursor; the
                // pass after it moves nothing.
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
                    let before = revisions(&out);
                    state.publish_projections(&out, &mut cache, &bodies);
                    assert_eq!(published(&out), built(&state), "{what}: not republished");
                    assert!(revisions(&out).0 > before.0, "{what}: no revision moved");
                    let after = revisions(&out);
                    state.publish_projections(&out, &mut cache, &bodies);
                    assert_eq!(revisions(&out), after, "{what}: a revision moved without a change");
                }

                // A diff's summary moves with its revision; new content at the same revision only
                // replaces the bodies `diff_get` reads.
                let root = std::path::PathBuf::from("/work/tree");
                state.diffs.entry(root.clone()).or_default().prepared = Some(prepared(1));
                let before = revisions(&out);
                state.publish_projections(&out, &mut cache, &bodies);
                assert_eq!(out.diffs.load().data[0].files.len(), 1);
                assert!(revisions(&out).0 > before.0);
                assert!(bodies.file("/work/tree", "src/file1.rs").is_none());
                state.diffs.get_mut(&root).unwrap().prepared = Some(prepared(2));
                let quiet = revisions(&out);
                state.publish_projections(&out, &mut cache, &bodies);
                assert!(bodies.file("/work/tree", "src/file1.rs").is_some(), "new content at the same revision");
                assert_eq!(revisions(&out), quiet);
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
            let mut cache = DiffSummaries::default();
            state.publish_projections(&out, &mut cache, &bodies);
            const PASSES: u32 = 300;
            let time = |f: &mut dyn FnMut()| {
                let started = std::time::Instant::now();
                for _ in 0..PASSES {
                    f();
                }
                started.elapsed() / PASSES
            };
            let idle = time(&mut || state.publish_projections(&out, &mut cache, &bodies));
            // One session changes on every pass (its title, as a running agent's would).
            let mut n = 0;
            let busy = time(&mut || {
                n += 1;
                state.sessions[0].title = format!("t{n}");
                state.publish_projections(&out, &mut cache, &bodies);
            });
            std::fs::write(
                report,
                format!(
                    "board: 400 sessions, 100 plans x 20 tasks, 100 approvals, 100 diffs x 50 files\n\
                     per pass, unchanged board: {idle:?}\n\
                     per pass, one session title changed: {busy:?}\n"
                ),
            )
            .unwrap();
        });
    }
}
