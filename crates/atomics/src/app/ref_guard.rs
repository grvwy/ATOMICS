use super::*;
use crate::ref_guard::Exempt;

impl State {
    /// Branches whose moves are legitimate work: those of live agents, unfinished tasks and
    /// worktrees being made. A done task's branch is finished, and moving it is restored. Refs ATOMICS
    /// writes itself, such as integration branches, go through [`crate::ref_guard::host_write`]
    /// instead, so an agent moving them is still restored. Runs no git: [`Exempt::resolve`] does that
    /// on the ref guard thread.
    pub(super) fn exempt_branches(&self) -> Exempt {
        let mut exempt = Exempt::default();
        for session in &self.sessions {
            if session.chief || !session.is_agent() || session.status == AgentStatus::Exited {
                continue;
            }
            match &session.ref_guard {
                Some(guard) => exempt.refs.extend(guard.branch.clone().map(|branch| (guard.common.clone(), branch))),
                None => exempt.folders.extend(session.branch().map(|b| (session.root(), format!("refs/heads/{b}")))),
            }
        }
        for task in self.plans.iter().flat_map(|plan| &plan.tasks).filter(|task| task.status != TaskStatus::Done) {
            if let (Some(worktree), Some(branch)) = (&task.worktree, &task.branch) {
                exempt.folders.push((worktree.clone(), format!("refs/heads/{branch}")));
            }
        }
        for (common, branch, _) in &self.creating {
            exempt.refs.push((common.clone(), format!("refs/heads/{branch}")));
        }
        exempt
    }

    /// Check the completed turn and save a fresh baseline before the next one, including
    /// turns typed directly into a tile. Exit checks also catch an agent too quick to show busy.
    pub(super) fn check_refs(&self, index: usize) {
        self.queue_ref_check(index, self.exempt_branches());
    }

    /// Checks every guard, with `exempt` gathered while the agents that just exited or closed
    /// still counted as live: their last commits are accepted before their branches stop being exempt.
    pub(super) fn check_all_refs(&self, exempt: &Exempt) {
        for index in 0..self.sessions.len() {
            self.queue_ref_check(index, exempt.clone());
        }
    }

    fn queue_ref_check(&self, index: usize, exempt: Exempt) {
        let Some(guard) = self.sessions[index].ref_guard.clone() else { return };
        let (id, actions) = (self.sessions[index].id.clone(), self.action_tx.clone());
        self.ref_checks.run(move || {
            let alerts =
                guard.check(&exempt).unwrap_or_else(|error| vec![format!("Ref guard check FAILED: {error:#}")]);
            if !alerts.is_empty() {
                let _ = actions.send(Box::new(move |state| state.ref_alert(&id, alerts.join("\n"))));
            }
        });
    }

    pub(super) fn ref_alert(&mut self, id: &str, details: String) {
        let summary = format!("SANDBOX REF GUARD: {details}");
        if let Some(index) = self.sessions.iter().position(|s| s.id == id) {
            self.sessions[index].last_message = Some(summary.clone());
        }
        let mut owners = Vec::new();
        for (index, plan) in self.plans.iter_mut().enumerate() {
            for task in plan.tasks.iter_mut().filter(|task| task.session.as_deref() == Some(id)) {
                task.ref_alert = Some(summary.clone());
                task.notes.push(summary.clone());
                owners.push(index);
            }
        }
        for index in owners {
            self.changed_plans.insert(self.plans[index].id.clone());
            self.save_plan(index);
        }
        // Reports wake the chief's normal atomics_wait loop, even after the tile was closed.
        self.report(Some(id), "blocked", &summary);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repository, session, with_app};
    use super::*;

    #[test]
    fn checks_run_off_the_state_lock_and_restore_branches_of_done_tasks_and_exited_agents() {
        with_app(
            "app::ref_guard::tests::checks_run_off_the_state_lock_and_restore_branches_of_done_tasks_and_exited_agents",
            |app, _| {
                let (dir, repo) = repository();
                let worktree = |name: &str| dir.path().join(name);
                for name in ["guarded", "live", "gone", "running", "done"] {
                    git::add_worktree(&repo, &worktree(name), name, "main").unwrap();
                }
                let guarded = worktree("guarded");
                let file = dir.path().join("snapshot.json");
                let guard = crate::ref_guard::RefGuard::open(&guarded, file).unwrap().unwrap();
                let mut sandboxed = session("sandboxed", AgentKind::Generic, AgentStatus::Working, &guarded);
                sandboxed.ref_guard = Some(crate::ref_guard::Shared::new(guard));
                let peer = |id: &str, status| {
                    let mut peer = session(id, AgentKind::Generic, status, &worktree(id));
                    peer.worktree = git::worktree_of(&peer.cwd);
                    peer
                };
                app.runtime.sessions =
                    vec![sandboxed, peer("live", AgentStatus::Idle), peer("gone", AgentStatus::Exited)];
                let task = |id: &str, status| Task {
                    id: id.into(),
                    worktree: Some(worktree(id)),
                    branch: Some(id.into()),
                    status,
                    ..Default::default()
                };
                let tasks = vec![task("running", TaskStatus::Running), task("done", TaskStatus::Done)];
                app.runtime.plans = vec![Plan { id: "guard-plan".into(), tasks, ..Default::default() }];
                let original = git::git(&repo, &["rev-parse", "main"]).unwrap();
                // A new commit, and every other branch moved to it, as an agent in the sandbox could.
                git::git(&guarded, &["commit", "--allow-empty", "-qm", "work"]).unwrap();
                let moved = git::git(&guarded, &["rev-parse", "HEAD"]).unwrap();
                for branch in ["running", "done", "live", "gone"] {
                    git::git(&repo, &["update-ref", &format!("refs/heads/{branch}"), moved.trim()]).unwrap();
                }

                // With the ref guard thread busy and git unusable meanwhile, a check is only queued: any git
                // the state thread ran for it would fail, and leave exempt branches out or fail the check.
                let (release, wait) = std::sync::mpsc::channel::<()>();
                app.runtime.ref_checks.run(move || {
                    let _ = wait.recv();
                });
                std::fs::rename(repo.join(".git"), repo.join(".git-away")).unwrap();
                app.runtime.check_refs(0);
                std::fs::rename(repo.join(".git-away"), repo.join(".git")).unwrap();
                release.send(()).unwrap();

                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !app.runtime.sessions[0].last_message.as_deref().is_some_and(|m| m.contains("SANDBOX REF GUARD"))
                {
                    assert!(Instant::now() < deadline, "the check never reported");
                    app.runtime.drain_actions();
                    std::thread::sleep(Duration::from_millis(10));
                }
                let alert = app.runtime.sessions[0].last_message.clone().unwrap();
                assert!(
                    alert.contains("Restored refs/heads/done") && alert.contains("Restored refs/heads/gone"),
                    "{alert}"
                );
                for kept in ["running", "live", "guarded"] {
                    assert!(!alert.contains(&format!("refs/heads/{kept}")), "{alert}");
                }
                assert_eq!(git::git(&repo, &["rev-parse", "done"]).unwrap(), original);
                assert_eq!(git::git(&repo, &["rev-parse", "gone"]).unwrap(), original);
                assert_eq!(git::git(&repo, &["rev-parse", "running"]).unwrap(), moved);
                assert_eq!(git::git(&repo, &["rev-parse", "live"]).unwrap(), moved);
                app.runtime.sessions.clear();
            },
        );
    }
    #[test]
    fn integration_branches_atomics_makes_survive_older_guards_and_agent_branches_do_not() {
        with_app(
            "app::ref_guard::tests::integration_branches_atomics_makes_survive_older_guards_and_agent_branches_do_not",
            |app, _| {
                let (dir, repo) = repository();
                let guarded = dir.path().join("guarded");
                git::add_worktree(&repo, &guarded, "guarded", "main").unwrap();
                let guard =
                    crate::ref_guard::RefGuard::open(&guarded, dir.path().join("snapshot.json")).unwrap().unwrap();
                let mut sandboxed = session("sandboxed", AgentKind::Generic, AgentStatus::Working, &guarded);
                sandboxed.ref_guard = Some(crate::ref_guard::Shared::new(guard));
                app.runtime.sessions = vec![sandboxed];
                let task = |id: &str, branch: &str, status| Task {
                    id: id.into(),
                    repo: Some(repo.to_string_lossy().into_owned()),
                    branch: Some(branch.into()),
                    status,
                    ..Default::default()
                };
                let running = Task { worktree: Some(guarded.clone()), ..task("a", "guarded", TaskStatus::Running) };
                app.runtime.plans = vec![Plan { id: "first".into(), tasks: vec![running], ..Default::default() }];

                // A check queued before the next plan exists runs only after ATOMICS made that plan's
                // integration branch: the race that deleted an integration branch end to end.
                let (release, wait) = std::sync::mpsc::channel::<()>();
                app.runtime.ref_checks.run(move || {
                    let _ = wait.recv();
                });
                app.runtime.check_refs(0);
                git::git(&repo, &["branch", "next-task"]).unwrap();
                let tasks = vec![task("b", "next-task", TaskStatus::Review)];
                app.runtime.plans.push(Plan { id: "second".into(), approved: true, tasks, ..Default::default() });
                let into = app.runtime.plans[1].integration_branch();
                app.runtime.integrate(1, "b", None);
                let integrated = app
                    .runtime
                    .integrate_rx
                    .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(30)))
                    .unwrap();
                assert!(integrated.result.is_ok(), "{:?}", integrated.result);
                app.runtime.finish_integration(integrated);
                // Meanwhile the agent makes a branch of its own.
                git::git(&guarded, &["branch", "agent-made"]).unwrap();
                release.send(()).unwrap();
                // And the next check, with both plans in place.
                app.runtime.check_refs(0);
                app.runtime.ref_checks.finish();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !app.runtime.sessions[0].last_message.as_deref().is_some_and(|m| m.contains("SANDBOX REF GUARD"))
                {
                    assert!(Instant::now() < deadline, "the check never reported");
                    app.runtime.drain_actions();
                    std::thread::sleep(Duration::from_millis(10));
                }

                assert!(git::git(&repo, &["rev-parse", "--verify", &format!("refs/heads/{into}")]).is_ok());
                assert!(git::git(&repo, &["rev-parse", "--verify", "refs/heads/agent-made"]).is_err());
                let alert = app.runtime.sessions[0].last_message.clone().unwrap();
                // Never deleted: the agent's branch is kept aside.
                let kept = "refs/atomics/quarantine/snapshot/refs/heads/agent-made@";
                assert!(alert.contains(&format!("Restored refs/heads/agent-made: moved it to {kept}")), "{alert}");
                let quarantined = git::git(&repo, &["for-each-ref", "--format=%(refname)", "refs/atomics/quarantine/"]);
                assert!(quarantined.unwrap().contains(kept), "{alert}");
                assert!(!alert.contains(&into) && !alert.contains("next-task"), "no alert for ATOMICS's refs: {alert}");
                assert!(!alert.contains("Object("), "plain text: {alert}");
                app.runtime.sessions.clear();
            },
        );
    }

    #[test]
    fn an_agent_moving_an_open_plans_integration_branch_is_restored_to_what_atomics_merged() {
        with_app(
            "app::ref_guard::tests::an_agent_moving_an_open_plans_integration_branch_is_restored_to_what_atomics_merged",
            |app, _| {
                use crate::ref_guard::{RefGuard, Shared};
                let (dir, repo) = repository();
                let worktree = |name: &str| dir.path().join(name);
                for name in ["guarded", "closed"] {
                    git::add_worktree(&repo, &worktree(name), name, "main").unwrap();
                }
                git::git(&repo, &["branch", "next-task"]).unwrap();
                let guard = RefGuard::open(&worktree("guarded"), dir.path().join("snapshot.json")).unwrap().unwrap();
                let mut sandboxed =
                    session("sandboxed", AgentKind::Generic, AgentStatus::Working, &worktree("guarded"));
                sandboxed.ref_guard = Some(Shared::new(guard));
                app.runtime.sessions = vec![sandboxed];
                // A sandboxed session that is not running: its snapshot waits on disk to reattach with.
                let closed = paths::atomics_home().join("ref-guards/atomics-closed.json");
                drop(RefGuard::open(&worktree("closed"), closed.clone()).unwrap().unwrap());
                let task = |id: &str, branch: &str, status| Task {
                    id: id.into(),
                    repo: Some(repo.to_string_lossy().into_owned()),
                    branch: Some(branch.into()),
                    status,
                    ..Default::default()
                };
                let running = Task { worktree: Some(worktree("guarded")), ..task("a", "guarded", TaskStatus::Running) };
                app.runtime.plans = vec![
                    Plan { id: "first".into(), tasks: vec![running], ..Default::default() },
                    Plan {
                        id: "second".into(),
                        approved: true,
                        tasks: vec![task("b", "next-task", TaskStatus::Review)],
                        ..Default::default()
                    },
                ];
                // ATOMICS makes the open plan's integration branch after both snapshots were taken.
                let into = app.runtime.plans[1].integration_branch();
                app.runtime.integrate(1, "b", None);
                let integrated = app
                    .runtime
                    .integrate_rx
                    .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(30)))
                    .unwrap();
                assert!(integrated.result.is_ok(), "{:?}", integrated.result);
                app.runtime.finish_integration(integrated);
                let merged = git::git(&repo, &["rev-parse", &into]).unwrap().trim().to_string();

                // The agent points the integration branch, what everything ships from, at a commit of its own.
                let guarded = worktree("guarded");
                let injected = git::git(&guarded, &["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "injected"])
                    .unwrap()
                    .trim()
                    .to_string();
                git::git(&guarded, &["update-ref", &format!("refs/heads/{into}"), &injected]).unwrap();
                app.runtime.check_refs(0);
                app.runtime.ref_checks.finish();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !app.runtime.sessions[0].last_message.as_deref().is_some_and(|m| m.contains("SANDBOX REF GUARD"))
                {
                    assert!(Instant::now() < deadline, "the check never reported");
                    app.runtime.drain_actions();
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert_eq!(git::git(&repo, &["rev-parse", &into]).unwrap().trim(), merged, "back to ATOMICS's merge");
                let alert = app.runtime.sessions[0].last_message.clone().unwrap();
                let restored =
                    format!("Restored refs/heads/{into}: from {} back to {}", &injected[..12], &merged[..12]);
                assert!(alert.contains(&restored), "{alert}");

                // The session that was not running reattaches with ATOMICS's merge in its baseline.
                let mut reattached = RefGuard::open(&worktree("closed"), closed).unwrap().unwrap();
                let alerts = reattached.check(&Default::default(), &Default::default()).unwrap();
                assert!(alerts.is_empty(), "{alerts:?}");
                assert_eq!(git::git(&repo, &["rev-parse", &into]).unwrap().trim(), merged);
                app.runtime.sessions.clear();
            },
        );
    }

    /// A repository with a guarded sandbox worktree, a host worktree `other`, and `ahead`, a commit past
    /// `other`'s branch; the guard is open before anything moves.
    fn guarded_with_an_ahead_branch()
    -> (crate::test_support::TempDir, PathBuf, PathBuf, PathBuf, String, crate::ref_guard::Shared) {
        let (dir, repo) = repository();
        let (guarded, other) = (dir.path().join("guarded"), dir.path().join("other"));
        git::add_worktree(&repo, &guarded, "guarded", "main").unwrap();
        git::add_worktree(&repo, &other, "other", "main").unwrap();
        let ahead = git::git(&repo, &["commit-tree", "main^{tree}", "-p", "main", "-m", "ahead"]).unwrap();
        let ahead = ahead.trim().to_string();
        git::git(&repo, &["branch", "ahead", &ahead]).unwrap();
        let guard = crate::ref_guard::RefGuard::open(&guarded, dir.path().join("snapshot.json")).unwrap().unwrap();
        (dir, repo, guarded, other, ahead, crate::ref_guard::Shared::new(guard))
    }

    #[test]
    fn a_host_fast_forward_in_a_worktree_no_sandbox_works_in_is_kept() {
        let (_dir, repo, _guarded, other, ahead, shared) = guarded_with_an_ahead_branch();
        git::git(&other, &["merge", "--ff-only", "ahead"]).unwrap();
        let alerts = shared.check(&Default::default()).unwrap();
        assert!(alerts.is_empty(), "{alerts:?}");
        assert_eq!(git::git(&repo, &["rev-parse", "other"]).unwrap().trim(), ahead);
        let quarantined = git::git(&repo, &["for-each-ref", "refs/atomics/quarantine/"]).unwrap();
        assert!(quarantined.trim().is_empty(), "{quarantined}");
    }

    #[test]
    fn a_host_fast_forward_in_a_sandboxed_worktree_is_restored_saying_how_to_keep_it() {
        let (_dir, repo, _guarded, other, ahead, shared) = guarded_with_an_ahead_branch();
        let original = git::git(&repo, &["rev-parse", "other"]).unwrap();
        // Another sandbox works in `other`: its HEAD log is no trace of this machine's, since that agent writes it.
        let pins = git::resolve_pins(&other).unwrap();
        git::pin(pins.clone());
        // Run by hand, as in a terminal on this machine.
        let merged = std::process::Command::new("git")
            .args(["merge", "-q", "--ff-only", "ahead"])
            .current_dir(&other)
            .status()
            .unwrap();
        let alerts = shared.check(&Default::default());
        git::unpin(&pins.work_tree);
        assert!(merged.success());
        let alerts = alerts.unwrap();
        let alert = alerts.iter().find(|alert| alert.starts_with("Restored refs/heads/other:")).expect("restored");
        assert!(alert.contains("It may have been written on this machine, not in the sandbox"), "{alert}");
        let keep = format!("git update-ref refs/heads/other {ahead}");
        assert!(alert.contains(&format!("close this repository's sandboxed sessions, then run `{keep}`")), "{alert}");
        assert_eq!(git::git(&repo, &["rev-parse", "other"]).unwrap(), original);
        let kept = format!("refs/atomics/quarantine/snapshot/refs/heads/other@{}", &ahead[..12]);
        assert_eq!(git::git(&repo, &["rev-parse", &kept]).unwrap().trim(), ahead);

        // The command it gives keeps the change once no guard is open.
        drop(shared);
        let command = alert.rsplit('`').nth(1).unwrap();
        git::git(&repo, &command.split(' ').skip(1).collect::<Vec<_>>()).unwrap();
        assert_eq!(git::git(&repo, &["rev-parse", "other"]).unwrap().trim(), ahead);
    }

    /// Puts `text` on session 0's terminal screen, as its agent would draw it there.
    fn show(state: &mut State, cwd: &Path, text: &str) {
        let terminal = crate::terminal::Terminal::spawn(
            state.wake.clone(),
            state.theme.terminal_colors(),
            "/bin/sh".into(),
            vec!["-c".into(), format!("printf '%s' '{text}'; exec sleep 600")],
            cwd,
            std::collections::HashMap::new(),
            100,
        )
        .unwrap();
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(60));
        while !terminal.screen_text(60).contains(text) {
            assert!(Instant::now() < deadline, "screen did not appear");
            state.wake.wait_until(deadline.min(Instant::now() + Duration::from_millis(100)));
        }
        state.sessions[0].terminal = Some(terminal);
    }

    /// Runs the queued checks and what they report.
    fn checked(state: &mut State) -> Option<String> {
        state.ref_checks.finish();
        state.drain_actions();
        state.sessions[0].last_message.clone().filter(|message| message.contains("SANDBOX REF GUARD"))
    }

    #[test]
    fn a_ref_the_sandbox_writes_during_a_turn_is_restored_when_the_poll_loop_ends_it() {
        with_app(
            "app::ref_guard::tests::a_ref_the_sandbox_writes_during_a_turn_is_restored_when_the_poll_loop_ends_it",
            |app, _| {
                let (_dir, repo, guarded, _other, ahead, shared) = guarded_with_an_ahead_branch();
                let original = git::git(&repo, &["rev-parse", "other"]).unwrap();
                let mut sandboxed = session("sandboxed", AgentKind::Generic, AgentStatus::Idle, &guarded);
                sandboxed.sandbox = Some("atomics-sandboxed".into());
                sandboxed.ref_guard = Some(shared);
                app.runtime.sessions = vec![sandboxed];
                let track = |state: &mut State| {
                    state.busy_checked = Instant::now() - Duration::from_secs(1);
                    state.track_busy();
                };
                show(app.runtime, &guarded, "Working (esc to interrupt)");
                track(app.runtime);
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                assert_eq!(checked(app.runtime), None, "the turn starts from a clean baseline");

                // The agent moves a branch it does not own, then its turn ends: the status is Done before
                // the check that ending queued runs.
                git::git(&guarded, &["update-ref", "refs/heads/other", &ahead]).unwrap();
                show(app.runtime, &guarded, "Ready");
                app.runtime.sessions[0].busy_absent_since = Some(Instant::now() - Duration::from_secs(1));
                track(app.runtime);
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Done);
                let alert = checked(app.runtime).expect("the turn's write was reported");
                assert!(alert.contains("Restored refs/heads/other"), "{alert}");
                assert_eq!(git::git(&repo, &["rev-parse", "other"]).unwrap(), original);
                app.runtime.sessions.clear();
            },
        );
    }

    #[test]
    fn a_ref_written_while_the_session_is_idle_is_restored() {
        with_app("app::ref_guard::tests::a_ref_written_while_the_session_is_idle_is_restored", |app, _| {
            let (_dir, repo, guarded, _other, ahead, shared) = guarded_with_an_ahead_branch();
            let original = git::git(&repo, &["rev-parse", "other"]).unwrap();
            let mut sandboxed = session("sandboxed", AgentKind::Generic, AgentStatus::Idle, &guarded);
            sandboxed.sandbox = Some("atomics-sandboxed".into());
            sandboxed.ref_guard = Some(shared);
            app.runtime.sessions = vec![sandboxed];
            app.runtime.check_refs(0);
            assert_eq!(checked(app.runtime), None);
            // No turn runs, but a process the agent left running in its sandbox still can write.
            git::git(&guarded, &["update-ref", "refs/heads/other", &ahead]).unwrap();
            app.runtime.check_refs(0);
            let alert = checked(app.runtime).expect("the idle write was reported");
            assert!(alert.contains("Restored refs/heads/other"), "{alert}");
            assert_eq!(git::git(&repo, &["rev-parse", "other"]).unwrap(), original);
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn a_progress_report_clears_the_ref_alert_and_the_blocked_task() {
        with_app("app::ref_guard::tests::a_progress_report_clears_the_ref_alert_and_the_blocked_task", |app, _| {
            let (dir, _repo) = repository();
            let mut sandboxed = session("s", AgentKind::Generic, AgentStatus::NeedsInput, dir.path());
            sandboxed.sandbox = Some("atomics-s".into());
            app.runtime.sessions = vec![sandboxed];
            let task = Task { id: "t".into(), session: Some("s".into()), ..Default::default() };
            app.runtime.plans = vec![Plan { id: "p".into(), tasks: vec![task], ..Default::default() }];
            for status in ["progress", "done"] {
                app.runtime.ref_alert("s", "moved".into());
                assert_eq!(app.runtime.plans[0].tasks[0].status, TaskStatus::Blocked);
                assert!(app.runtime.plans[0].tasks[0].ref_alert.is_some());
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
                app.runtime.report(Some("s"), status, "carrying on");
                let task = &app.runtime.plans[0].tasks[0];
                assert!(task.ref_alert.is_none(), "{status}");
                assert_ne!(task.status, TaskStatus::Blocked, "{status}");
                let session = &app.runtime.sessions[0];
                assert_ne!(session.status, AgentStatus::NeedsInput, "{status}");
                // The flip goes through the activity record: progress starts the working clock, done ends the turn.
                let activity = &session.activity;
                match status {
                    "progress" => assert!(activity.working_since.is_some() && activity.wake_since.is_none()),
                    _ => assert!(activity.turn_ended.is_some() && activity.wake_since.is_some()),
                }
            }
            app.runtime.sessions.clear();
        });
    }
}
