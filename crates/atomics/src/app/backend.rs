//! The in-process [`Backend`]: reads the host model directly. The desktop's draw code reads through it,
//! so a loopback or browser client can read the same things over the protocol.
use atomics_ui::{
    ApprovalRow, Backend, BoardView, DiffHeading, DiffSnapshot, PlanRow, RecentSession, SessionRow, UiConfig, ViewHints,
    WorktreeRow,
};

use super::*;

/// A short-lived read of the model; build it with [`State::backend`].
pub(super) struct Local<'a>(&'a State);

impl State {
    pub(super) fn backend(&self) -> Local<'_> {
        Local(self)
    }
}

impl Local<'_> {
    fn row(&self, index: usize, plan: &Plan) -> PlanRow {
        PlanRow {
            id: plan.id.clone(),
            goal: plan.goal.clone(),
            tab_name: super::sidebar::plan_tab_name(plan),
            status: plan.status,
            state: self.0.plan_state(index),
            changed: self.0.changed_plans.contains(&plan.id),
            done: plan.count(TaskStatus::Done),
            total: plan.tasks.len(),
            attention: plan.count(TaskStatus::Review) + plan.count(TaskStatus::Blocked),
        }
    }

    fn session(&self, session: &Session) -> SessionRow {
        let root = session.root();
        SessionRow {
            id: session.id.clone(),
            title: session.title.clone(),
            agent: session.agent.clone(),
            kind: session.kind,
            status: session.status,
            chief: session.chief,
            worktree: session.worktree.as_ref().map(|worktree| worktree.root.clone()),
            branch: session.branch().map(str::to_string),
            project: self.0.project_of(&root),
            ungrouped: self.0.ungrouped_session(session),
            scratch: session.scratch.is_some(),
            sandbox: session.sandbox.clone(),
            outside: session.terminal.is_none(),
            pending_approvals: session.approvals.len(),
            held_messages: session.held.len(),
            changes: self
                .0
                .diffs
                .get(&root)
                .and_then(|data| data.prepared.as_ref())
                .map(|diff| (diff.additions, diff.deletions)),
            root,
        }
    }
}

impl Backend for Local<'_> {
    fn plan_list(&self) -> Vec<PlanRow> {
        self.0.plans.iter().enumerate().map(|(index, plan)| self.row(index, plan)).collect()
    }

    fn plan_row(&self, index: usize) -> Option<PlanRow> {
        self.0.plans.get(index).map(|plan| self.row(index, plan))
    }

    fn board(&self, index: usize) -> Option<BoardView> {
        let state = self.0;
        let plan = state.plans.get(index)?;
        let integration_notes = plan
            .tasks
            .iter()
            .filter_map(|task| {
                let note = state.integration_check_note(index, task)?;
                let checking = state.integration_checks.contains_key(&(plan.id.clone(), task.id.clone()));
                Some((task.id.clone(), (note, checking)))
            })
            .collect();
        let board_counts = state.tickets.boards.get(&plan.id).map(|items| {
            let planned = |item: &atomics_core::tickets::TicketLink| {
                plan.tasks.iter().any(|task| task.ticket.as_ref().is_some_and(|link| link.same_ticket(item)))
            };
            (items.len(), items.iter().filter(|item| !planned(item)).count())
        });
        Some(BoardView {
            state: state.plan_state(index),
            ready: plan.ready().iter().map(|task| task.id.clone()).collect(),
            sessions: state.sessions.iter().map(|s| (s.id.clone(), (s.title.clone(), s.status))).collect(),
            integration_notes,
            checking: state.integration_checks.keys().any(|(id, _)| id == &plan.id),
            integrating: state
                .integrating
                .iter()
                .filter(|(id, _)| id == &plan.id)
                .map(|(_, task)| task.clone())
                .collect(),
            ticket_config: state.ticket_config(),
            board_counts,
            worktrees: plan
                .tasks
                .iter()
                .filter(|task| task.finished_at.is_some() && task.worktree.as_ref().is_some_and(|path| path.is_dir()))
                .map(|task| task.id.clone())
                .collect(),
            codex: plan
                .tasks
                .iter()
                .filter(|task| state.config.agent(&task.agent).is_some_and(|agent| agent.is_codex()))
                .map(|task| task.id.clone())
                .collect(),
            now: atomics_core::store::now(),
            plan: plan.clone(),
        })
    }

    fn session_list(&self) -> Vec<SessionRow> {
        self.0.sessions.iter().map(|session| self.session(session)).collect()
    }

    fn session_row(&self, index: usize) -> Option<SessionRow> {
        self.0.sessions.get(index).map(|session| self.session(session))
    }

    fn session_approval(&self, index: usize) -> Option<ApprovalRow> {
        let session = self.0.sessions.get(index)?;
        let pending = session.approvals.first()?;
        Some(ApprovalRow { session: session.id.clone(), id: pending.id.clone(), request: pending.request.clone() })
    }

    fn diff_roots(&self) -> Vec<PathBuf> {
        self.0.diffs.keys().cloned().collect()
    }

    fn diff(&self, root: &Path) -> Option<DiffSnapshot> {
        let data = self.0.diffs.get(root)?;
        Some(DiffSnapshot {
            prepared: data.prepared.clone(),
            error: data.error.clone(),
            loading: data.loading,
            comments: data.comments.clone(),
            viewing: data.viewing.clone(),
            file: data.file.clone(),
            file_error: data.file_error.clone(),
        })
    }

    fn diff_heading(&self, root: &Path) -> DiffHeading {
        let state = self.0;
        let session = state.active_index().map(|i| &state.sessions[i]);
        let title = session
            .map(|s| s.title.clone())
            .unwrap_or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        let branch = session
            .and_then(|s| s.branch().map(str::to_string))
            .or_else(|| state.worktrees.iter().find(|w| w.path == root).and_then(|w| w.branch.clone()))
            .filter(|b| *b != title);
        let can_send = state.sessions.iter().any(|s| s.root() == root && s.is_agent() && s.can_receive());
        DiffHeading { title, branch, can_send }
    }

    fn ui_config(&self) -> UiConfig {
        let config = &self.0.config;
        UiConfig {
            keys: config.keys.clone(),
            font: config.font.clone(),
            layout: config.layout.clone(),
            agents: config.agents.iter().map(|agent| agent.name.clone()).collect(),
        }
    }

    fn view_hints(&self) -> ViewHints {
        ViewHints { active: self.0.active.clone(), diff_target: self.0.diff_target() }
    }

    fn visible_sessions(&self) -> Vec<usize> {
        self.0.visible_sessions()
    }

    fn checkouts(&self) -> Vec<String> {
        self.0.creating.iter().map(|(_, branch, _)| branch.clone()).collect()
    }

    fn recent_sessions(&self) -> Vec<RecentSession> {
        let state = self.0;
        let live_ids: HashSet<&str> = state.sessions.iter().filter_map(|s| s.claude_id.as_deref()).collect();
        state
            .store
            .sessions
            .iter()
            .filter(|r| !live_ids.contains(r.claude_session_id.as_str()) && r.cwd.is_dir() && r.agent != "chief")
            .filter(|r| state.project_of(&r.cwd).is_some())
            .filter({
                let mut seen = HashSet::new();
                move |r| seen.insert((r.title.clone(), r.cwd.clone()))
            })
            .take(8)
            .map(|record| RecentSession { folder: paths::display(&record.cwd), record: record.clone() })
            .collect()
    }

    fn repos(&self) -> Vec<PathBuf> {
        self.0.repos.clone()
    }

    fn project_of(&self, dir: &Path) -> Option<PathBuf> {
        self.0.project_of(dir)
    }

    fn repo_workflow(&self, project: &Path) -> Option<atomics_core::repo_workflow::RepoWorkflow> {
        self.0.repo_workflows.get(project).cloned()
    }

    fn worktrees(&self, project: &Path) -> Vec<WorktreeRow> {
        let state = self.0;
        state
            .worktrees
            .iter()
            .filter(|w| w.path != project && state.worktree_project.get(&w.path).map(PathBuf::as_path) == Some(project))
            .map(|w| WorktreeRow { path: w.path.clone(), branch: w.branch.clone() })
            .collect()
    }

    fn display(&self, path: &Path) -> String {
        paths::display(path)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{session, with_app};
    use super::*;

    #[test]
    fn session_rows_follow_the_model() {
        with_app("app::backend::tests::session_rows_follow_the_model", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let (repo, worktree, scratch) = (dir.path().join("repo"), dir.path().join("task"), dir.path().join("note"));
            app.runtime.repos = vec![repo.clone()];
            app.runtime.worktree_project.insert(worktree.clone(), repo.clone());
            let mut planned = session("planned", AgentKind::Claude, AgentStatus::Working, &worktree);
            planned.worktree = Some(Worktree { root: worktree.clone(), branch: Some("task".into()) });
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &repo);
            chief.chief = true;
            let mut loose = session("loose", AgentKind::Generic, AgentStatus::NeedsInput, &scratch);
            loose.scratch = Some(scratch.clone());
            loose.sandbox = Some("vm".into());
            loose.held.push("after the decision".into());
            app.runtime.sessions = vec![planned, chief, loose];
            app.runtime.plans = vec![Plan {
                id: "plan".into(),
                tasks: vec![Task { session: Some("planned".into()), ..Task::default() }],
                ..Plan::default()
            }];
            app.runtime.diffs.entry(worktree.clone()).or_default().prepared = Some(Arc::new(diff_view::Prepared {
                diff: Default::default(),
                highlights: vec![],
                longest_line: 0,
                additions: 12,
                deletions: 3,
                all_files: Arc::default(),
            }));

            let backend = app.runtime.backend();
            let rows = backend.session_list();
            assert_eq!(rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["planned", "chief", "loose"]);
            assert_eq!(backend.session_row(2).as_ref(), Some(&rows[2]));
            assert_eq!(backend.session_row(3), None);
            let [planned, chief, loose] = &rows[..] else { unreachable!() };
            assert_eq!(planned.root, worktree);
            assert_eq!(planned.worktree.as_ref(), Some(&worktree));
            assert_eq!(planned.branch.as_deref(), Some("task"));
            assert_eq!(planned.project.as_ref(), Some(&repo));
            assert_eq!(planned.changes, Some((12, 3)));
            assert!(!planned.ungrouped);
            assert!(chief.chief && !chief.ungrouped && chief.project.as_ref() == Some(&repo));
            assert_eq!((chief.root.as_path(), chief.worktree.as_ref(), chief.changes), (repo.as_path(), None, None));
            assert!(loose.ungrouped && loose.scratch && loose.project.is_none());
            assert_eq!(
                (loose.kind, loose.status, loose.agent.as_str()),
                (AgentKind::Generic, AgentStatus::NeedsInput, "test")
            );
            assert_eq!((loose.sandbox.as_deref(), loose.pending_approvals, loose.held_messages), (Some("vm"), 0, 1));
            assert!(rows.iter().all(|row| row.outside), "the test's sessions have no terminal in ATOMICS");
        });
    }

    #[test]
    fn boards_and_approvals_follow_the_model() {
        with_app("app::backend::tests::boards_and_approvals_follow_the_model", |app, _| {
            use atomics_core::tickets::{TicketConfig, TicketLink};
            let dir = crate::test_support::tempdir().unwrap();
            let ticket = |id: &str| TicketLink { system: "relay".into(), id: id.into(), ..TicketLink::default() };
            let pending = |id: &str, command: &str| PendingApproval {
                id: id.into(),
                request: ApprovalRequest {
                    tool: "Bash".into(),
                    command: Some(command.into()),
                    cwd: dir.path().into(),
                    reason: None,
                    input: json!({}),
                },
                reply: None,
            };
            let mut asking = session("asking", AgentKind::Claude, AgentStatus::NeedsInput, dir.path());
            asking.title = "Asking".into();
            asking.approvals = vec![pending("first", "touch first"), pending("second", "touch second")];
            let quiet = session("quiet", AgentKind::Claude, AgentStatus::Working, dir.path());
            app.runtime.sessions = vec![asking, quiet];
            let task = |id: &str, status: TaskStatus| Task { id: id.into(), status, ..Task::default() };
            app.runtime.plans = vec![
                Plan { id: "other".into(), ..Plan::default() },
                Plan {
                    id: "plan".into(),
                    tasks: vec![
                        Task {
                            session: Some("asking".into()),
                            ticket: Some(ticket("T-1")),
                            ..task("a", TaskStatus::Done)
                        },
                        Task { depends_on: vec!["a".into()], ..task("b", TaskStatus::Planned) },
                        Task { depends_on: vec!["c".into()], ..task("blocked", TaskStatus::Planned) },
                        task("c", TaskStatus::Running),
                    ],
                    ..Plan::default()
                },
            ];
            let config = Arc::new(TicketConfig { system: "relay".into(), ..TicketConfig::default() });
            app.runtime.tickets.config = Some(config.clone());
            app.runtime.tickets.boards.insert("plan".into(), vec![ticket("T-1"), ticket("T-2"), ticket("T-3")]);
            app.runtime.integration_checks.insert(("plan".into(), "a".into()), (0, Instant::now()));
            app.runtime.integrating.insert(("plan".into(), "a".into()));
            app.runtime.integrating.insert(("other".into(), "c".into()));

            let backend = app.runtime.backend();
            let board = backend.board(1).unwrap();
            assert_eq!(board.plan, app.runtime.plans[1]);
            assert_eq!(board.state, app.runtime.plan_state(1));
            assert_eq!(board.ready, ["b"]);
            assert_eq!(board.sessions["asking"], ("Asking".to_string(), AgentStatus::NeedsInput));
            assert_eq!(board.sessions.len(), 2);
            let note = format!("checking whether it is in {}", app.runtime.plans[1].integration_branch());
            assert_eq!(board.integration_notes.len(), 1);
            assert_eq!(board.integration_notes["a"], (note, true));
            assert!(board.checking);
            assert_eq!(board.integrating, HashSet::from(["a".to_string()]));
            assert_eq!(board.ticket_config, Some(config));
            assert_eq!(board.board_counts, Some((3, 2)));
            let other = backend.board(0).unwrap();
            assert!(!other.checking && other.integration_notes.is_empty());
            assert_eq!((other.integrating, other.board_counts), (HashSet::from(["c".to_string()]), None));
            assert_eq!(backend.board(2), None);

            let approval = backend.session_approval(0).unwrap();
            assert_eq!((approval.session.as_str(), approval.id.as_str()), ("asking", "first"));
            assert_eq!(approval.request, app.runtime.sessions[0].approvals[0].request);
            assert_eq!(backend.session_approval(1), None);
            assert_eq!(backend.session_approval(2), None);
        });
    }
}
