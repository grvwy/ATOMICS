//! These tests never run egui, call a client callback, or drive a runtime tick.
//! All progress after startup must come from the production host thread.

use super::tests::{repository, session, with_state};
use super::*;
use crate::host::Host;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

fn response(rx: Receiver<ControlOutcome>) -> ControlResponse {
    // Some commands run git in the background (an integration takes a dozen calls), and while the
    // whole suite runs one git call can take half a second. The bound only guards against a hang.
    let ControlOutcome::Done(response) =
        rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(30))).expect("host did not answer")
    else {
        panic!("expected completed command");
    };
    assert!(response.ok, "{:?}", response.error);
    response
}

fn command(host: &Host, value: serde_json::Value) -> ControlResponse {
    response(host.command(serde_json::from_value(value).unwrap()))
}

fn eventually(host: &Host, check: impl Fn(&State) -> bool) {
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
    loop {
        if check(&host.state()) {
            break;
        }
        assert!(Instant::now() < deadline, "host did not make progress without frames");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_terminal_text(terminal: &Terminal, text: &str) {
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(3));
    while !terminal.screen_text(60).contains(text) {
        assert!(Instant::now() < deadline, "terminal did not show {text}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn hook(name: &str, tool: Option<&str>) -> HookReply {
    let stream = UnixStream::connect(paths::socket_path()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    writeln!(
        &stream,
        "{}",
        json!({
            "pane":"worker", "ancestors":[],
            "payload":{
                "hook_event_name":name, "session_id":"worker", "cwd":paths::atomics_home(),
                "tool_name":tool, "tool_input":{"command":"git status"}
            }
        })
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn expired_approval(state: &mut State) -> Arc<ipc::ApprovalReply> {
    let reply = Arc::new(ipc::ApprovalReply::default());
    let mut worker = session("worker", AgentKind::Claude, AgentStatus::NeedsInput, &state.launch_dir);
    worker.approval_hold = true;
    worker.approvals.push(PendingApproval {
        id: uuid::Uuid::new_v4().to_string(),
        request: ApprovalRequest {
            tool: "Bash".into(),
            command: Some("touch pending".into()),
            cwd: state.launch_dir.clone(),
            reason: None,
            input: json!({}),
        },
        reply: Some(reply.clone()),
    });
    state.sessions = vec![worker];
    reply
}

#[test]
fn permission_hook_gets_its_reply_without_frames() {
    with_state("app::host_tests::permission_hook_gets_its_reply_without_frames", |mut state, _| {
        state.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Working, &state.launch_dir)];
        let host = state.into_host();
        let reply = hook("PermissionRequest", Some("Bash"));
        assert!(reply.stdout.unwrap().contains("allow"));
        assert_eq!(host.state().sessions.len(), 1);
    });
}

#[test]
fn approval_expires_without_frames() {
    with_state("app::host_tests::approval_expires_without_frames", |mut state, _| {
        let reply = expired_approval(&mut state);
        let host = state.into_host();
        // Simulate the hook's own timeout after the runtime is already running.
        assert!(reply.wait(Duration::ZERO).stdout.is_none());
        eventually(&host, |state| !state.sessions[0].approval_hold && state.sessions[0].approvals.is_empty());
        assert_eq!(host.state().sessions[0].status, AgentStatus::Working);
    });
}

#[test]
fn held_messages_release_without_frames() {
    with_state("app::host_tests::held_messages_release_without_frames", |mut state, _| {
        let reply = expired_approval(&mut state);
        state.sessions[0].held = vec!["held message".into()];
        let hub = state.hub.clone();
        let host = state.into_host();
        reply.wait(Duration::ZERO);
        eventually(&host, |state| state.sessions[0].held.is_empty());
        assert_eq!(hub.lock().unwrap().outbox["worker"], ["held message"]);
    });
}

#[test]
fn waiter_wakes_on_a_hook_status_change_without_frames() {
    with_state("app::host_tests::waiter_wakes_on_a_hook_status_change_without_frames", |mut state, _| {
        state.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Working, &state.launch_dir)];
        let host = state.into_host();
        let wait = host.command(ControlRequest::Wait {
            sessions: vec!["worker".into()],
            statuses: vec!["turn done".into()],
            after: Some(0),
            timeout_secs: Some(10),
        });
        // A following command is a FIFO barrier: the waiter has been registered.
        command(&host, json!({"cmd":"list"}));
        hook("Stop", None);
        let reply = response(wait);
        assert_eq!(reply.result["timed_out"], false);
        assert_eq!(reply.result["events"][0]["session"], "worker");
        assert_eq!(reply.result["events"][0]["status"], "turn done");
    });
}

#[test]
fn waiter_times_out_on_its_own_deadline_without_frames() {
    with_state("app::host_tests::waiter_times_out_on_its_own_deadline_without_frames", |state, _| {
        let host = state.into_host();
        let reply = command(&host, json!({"cmd":"wait", "after":0, "timeout_secs":1}));
        assert_eq!(reply.result["timed_out"], true);
        assert!(reply.result["events"].as_array().unwrap().is_empty());
    });
}

#[test]
fn heartbeat_fires_without_frames() {
    with_state("app::host_tests::heartbeat_fires_without_frames", |mut state, _| {
        state.config.chief.heartbeat_secs = 1;
        let host = state.into_host();
        let reply = command(&host, json!({"cmd":"wait", "statuses":["heartbeat"], "after":0, "timeout_secs":5}));
        assert_eq!(reply.result["timed_out"], false);
        assert_eq!(reply.result["events"][0]["status"], "heartbeat");
        assert!(host.state().heartbeat.last.is_some());
    });
}

#[test]
fn chief_wake_submits_one_line_without_frames_and_preserves_drafts() {
    with_state("app::host_tests::chief_wake_submits_one_line_without_frames_and_preserves_drafts", |mut state, _| {
        let dir = paths::atomics_home();
        let received = dir.join("wake.txt");
        let mut chief = session("chief", AgentKind::Claude, AgentStatus::Done, &dir);
        chief.chief = true;
        chief.activity.observe(AgentStatus::Done);
        chief.activity.wake_since = Some(Instant::now() - Duration::from_secs(180));
        chief.terminal = Some(
            Terminal::spawn(
                state.wake.clone(),
                Vec::new(),
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    "printf '❯ draft'; read -r line; printf '%s\\n' \"$line\" >> wake.txt; exec sleep 30".into(),
                ],
                &dir,
                HashMap::new(),
                100,
            )
            .unwrap(),
        );
        wait_for_terminal_text(chief.terminal.as_ref().unwrap(), "❯ draft");
        state.sessions = vec![chief, session("worker", AgentKind::Generic, AgentStatus::Working, &dir)];
        let host = state.into_host();
        eventually(&host, |state| state.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("❯ draft"));
        {
            let state = host.state();
            assert!(!received.exists(), "a draft must not receive a wake");
            assert_eq!(state.chief_wake_cursor, 0);
        }
        {
            let mut state = host.state();
            // Replace the fixture with an empty prompt; no client input buffer or frame is involved.
            state.sessions[0].terminal = Some(
                Terminal::spawn(
                    state.wake.clone(),
                    Vec::new(),
                    "/bin/sh".into(),
                    vec![
                        "-c".into(),
                        "printf '❯ '; while read -r line; do printf '%s\\n' \"$line\" >> wake.txt; done".into(),
                    ],
                    &dir,
                    HashMap::new(),
                    100,
                )
                .unwrap(),
            );
        }
        eventually(&host, |_| std::fs::read_to_string(&received).is_ok_and(|line| line.ends_with('\n')));
        let line = std::fs::read_to_string(&received).unwrap();
        assert_eq!(
            line,
            "ATOMICS wake: 1 new events, 0 heartbeat findings (none). Call atomics_wait and act; if nothing changed, post one short line.\n"
        );
        assert_eq!(line.lines().count(), 1);
        {
            let state = host.state();
            assert!(state.chief_wake_cursor > 0);
            assert!(state.sessions[0].activity.wake_since.unwrap().elapsed() < Duration::from_secs(10));
        }
        assert_eq!(std::fs::read_to_string(&received).unwrap(), line);
    });
}

fn chief_suggestion_case(name: &str, input: &[bool], expect_wake: bool) {
    with_state(name, |mut state, _| {
        let dir = paths::atomics_home();
        let received = dir.join("wake.txt");
        let now = Instant::now();
        let mut chief = session("chief", AgentKind::Claude, AgentStatus::Done, &dir);
        chief.chief = true;
        chief.activity.observe(AgentStatus::Done);
        chief.activity.turn_ended = Some(now - Duration::from_secs(180));
        chief.activity.wake_since = chief.activity.turn_ended;
        let terminal = Terminal::spawn(
            state.wake.clone(),
            Vec::new(),
            "/bin/sh".into(),
            vec!["-c".into(), "printf '❯ \\033[2;90mrun the full suite once more\\033[0m'; while read -r line; do printf '%s\\n' \"$line\" >> wake.txt; done".into()],
            &dir,
            HashMap::new(),
            100,
        ).unwrap();
        for (i, submitted) in input.iter().enumerate() {
            // Native input after the turn ended, aged past the independent 60-second cooldown.
            terminal.record_input(now - Duration::from_secs(62 - i as u64), *submitted);
        }
        wait_for_terminal_text(&terminal, "run the full suite once more");
        chief.terminal = Some(terminal);
        state.sessions = vec![chief, session("worker", AgentKind::Generic, AgentStatus::Working, &dir)];
        let host = state.into_host();
        eventually(&host, |state| {
            state.event_seq >= 2
                && state.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("run the full suite once more")
        });
        if expect_wake {
            eventually(&host, |_| std::fs::read_to_string(&received).is_ok_and(|line| line.ends_with('\n')));
            assert!(std::fs::read_to_string(&received).unwrap().starts_with("ATOMICS wake:"));
            assert!(host.state().chief_wake_cursor > 0);
        } else {
            assert!(!received.exists(), "native draft must suppress the wake even when the screen shows a suggestion");
            assert_eq!(host.state().chief_wake_cursor, 0);
        }
    });
}

#[test]
fn chief_suggestion_without_user_keystrokes_still_wakes() {
    chief_suggestion_case("app::host_tests::chief_suggestion_without_user_keystrokes_still_wakes", &[], true);
}

#[test]
fn chief_keystrokes_since_turn_end_without_enter_suppress_wake() {
    chief_suggestion_case(
        "app::host_tests::chief_keystrokes_since_turn_end_without_enter_suppress_wake",
        &[false],
        false,
    );
}

#[test]
fn chief_wakes_resume_after_user_submits() {
    chief_suggestion_case("app::host_tests::chief_wakes_resume_after_user_submits", &[false, true], true);
}

#[test]
fn plan_is_saved_without_frames() {
    with_state("app::host_tests::plan_is_saved_without_frames", |state, _| {
        let _host = state.into_host();
        let reply = ipc::request(
            &serde_json::from_value(json!({
                "cmd":"plan_set", "goal":"Saved without painting", "tasks":[{"id":"a"}]
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(reply.ok, "{:?}", reply.error);
        let (plans, errors) = plan::load_plans(&paths::atomics_home()).unwrap();
        assert!(errors.is_empty());
        let saved = plans.iter().find(|p| json!(p.id) == reply.result["plan"]).unwrap();
        assert_eq!(saved.goal, "Saved without painting");
        assert_eq!(saved.tasks[0].id, "a");
    });
}

#[test]
fn integration_finishes_and_persists_without_frames() {
    with_state("app::host_tests::integration_finishes_and_persists_without_frames", |mut state, _| {
        let (_dir, repo) = repository();
        let worktree = repo.with_file_name("worker");
        git::add_worktree(&repo, &worktree, "task-a", "main").unwrap();
        std::fs::write(worktree.join("result.txt"), "completed work").unwrap();
        state.plans = vec![Plan {
            id: "no-frames".into(),
            project: Some(repo.clone()),
            approved: true,
            tasks: vec![Task {
                id: "a".into(),
                branch: Some("task-a".into()),
                worktree: Some(worktree),
                status: TaskStatus::Review,
                ..Task::default()
            }],
            ..Plan::default()
        }];
        state.selected_plan = 0;
        state.config.worktree_path = repo.with_file_name("worktrees").to_string_lossy().into_owned();
        let into = state.plans[0].integration_branch();
        let host = state.into_host();
        let result = command(&host, json!({"cmd":"plan_integrate", "plan":"no-frames", "id":"a"}));
        assert_eq!(result.result["integration"], into);
        assert_eq!(git::git(&repo, &["show", &format!("{into}:result.txt")]).unwrap().trim(), "completed work");
        let saved = plan::load_plans(&paths::atomics_home()).unwrap().0;
        assert_eq!(saved[0].tasks[0].integrated.as_deref(), Some(into.as_str()));
    });
}

#[test]
fn agent_exit_updates_and_saves_its_plan_without_frames() {
    with_state("app::host_tests::agent_exit_updates_and_saves_its_plan_without_frames", |mut state, _ctx| {
        let terminal = Terminal::spawn(
            state.wake.clone(),
            state.theme.terminal_colors(),
            "/bin/sh".into(),
            vec!["-c".into(), "sleep 0.1".into()],
            &state.launch_dir,
            HashMap::new(),
            100,
        )
        .unwrap();
        state.sessions = vec![session("worker", AgentKind::Generic, AgentStatus::Working, &state.launch_dir)];
        state.sessions[0].terminal = Some(terminal);
        state.plans = vec![Plan {
            id: "agent-exit".into(),
            tasks: vec![Task {
                id: "a".into(),
                session: Some("worker".into()),
                status: TaskStatus::Running,
                ..Task::default()
            }],
            ..Plan::default()
        }];
        let host = state.into_host();
        eventually(&host, |state| state.plans[0].tasks[0].status == TaskStatus::Blocked);
        let saved = plan::load_plans(&paths::atomics_home()).unwrap().0;
        assert_eq!(saved[0].tasks[0].status, TaskStatus::Blocked);
    });
}

#[test]
fn delivery_verification_times_out_without_frames() {
    with_state("app::host_tests::delivery_verification_times_out_without_frames", |mut state, _ctx| {
        let terminal = Terminal::spawn(
            state.wake.clone(),
            state.theme.terminal_colors(),
            "/bin/sleep".into(),
            vec!["30".into()],
            &state.launch_dir,
            HashMap::new(),
            100,
        )
        .unwrap();
        let (reply, received) = channel();
        let mut worker = session("worker", AgentKind::Generic, AgentStatus::Idle, &state.launch_dir);
        worker.terminal = Some(terminal);
        worker.pending = Some(Pending {
            text: "unconfirmed message".into(),
            sent: Instant::now() - DELIVERY_TIMEOUT + Duration::from_millis(100),
            attempts: DELIVERY_ATTEMPTS,
            was_working: false,
            codex: false,
            screen_before: String::new(),
            paste_seen: false,
            reply: Some(reply),
        });
        state.sessions = vec![worker];
        let host = state.into_host();
        let ControlOutcome::Done(result) =
            received.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(3))).unwrap()
        else {
            panic!("delivery was not resolved");
        };
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("no receipt"));
        assert!(host.state().sessions[0].pending.is_none());
    });
}

#[test]
fn worktree_changes_publish_a_fresh_diff_without_frames() {
    with_state("app::host_tests::worktree_changes_publish_a_fresh_diff_without_frames", |mut state, _| {
        let (_dir, repo) = repository();
        state.config.base_ref = "main".into();
        state.repos = vec![repo.clone()];
        state.diff_root = Some(repo.clone());
        let host = state.into_host();
        eventually(&host, |state| state.diffs.get(&repo).is_some_and(|d| d.prepared.is_some()));
        // The watcher is installed and the first revision has already been published.
        // Only a real filesystem change may produce the next revision.
        let initial = host.state().diffs[&repo].prepared.clone().unwrap();
        std::fs::write(repo.join("new.rs"), "fn fresh_revision() {}\n").unwrap();
        eventually(&host, |state| {
            state.diffs[&repo]
                .prepared
                .as_ref()
                .is_some_and(|p| !Arc::ptr_eq(&initial, p) && p.diff.files.iter().any(|file| file.path() == "new.rs"))
        });
        let current = host.state().diffs[&repo].prepared.clone().unwrap();
        std::fs::write(repo.join("new.rs"), "fn changed_again() {}\nfn another_line() {}\n").unwrap();
        eventually(&host, |state| {
            state.diffs[&repo].prepared.as_ref().is_some_and(|p| !Arc::ptr_eq(&current, p) && p.additions == 2)
        });
    });
}

#[test]
fn held_terminal_input_gets_a_receipt_without_frames() {
    with_state("app::host_tests::held_terminal_input_gets_a_receipt_without_frames", |mut state, _| {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex");
        state.config.agents.push(atomics_core::config::AgentConfig {
            name: "fake-codex".into(),
            command: vec![fixture.to_string_lossy().into_owned()],
            kind: AgentKind::Generic,
            busy_text: None,
            isolation: None,
        });
        let reply = expired_approval(&mut state);
        let worker = &mut state.sessions[0];
        worker.agent = "fake-codex".into();
        worker.kind = AgentKind::Generic;
        worker.held = vec!["A message held until the approval expires".into()];
        worker.terminal = Some(
            Terminal::spawn(
                state.wake.clone(),
                state.theme.terminal_colors(),
                fixture.to_string_lossy().into_owned(),
                vec!["--mode".into(), "queue".into()],
                &state.launch_dir,
                HashMap::new(),
                100,
            )
            .unwrap(),
        );
        let host = state.into_host();
        eventually(&host, |state| {
            state.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("tab to queue")
        });
        reply.wait(Duration::ZERO);
        eventually(&host, |state| {
            let worker = &state.sessions[0];
            worker.held.is_empty()
                && worker.pending.is_none()
                && steer::message_in_transcript(
                    &worker.terminal.as_ref().unwrap().screen_text(60),
                    "A message held until the approval expires",
                )
        });
        assert!(host.state().sessions[0].last_message.is_none());
    });
}

#[test]
fn cleanup_previews_finish_without_painting_the_dialogs() {
    with_state("app::host_tests::cleanup_previews_finish_without_painting_the_dialogs", |mut state, _| {
        let (_dir, repo) = repository();
        state.config.base_ref = "main".into();
        state.plans[0].project = Some(repo.clone());
        state.plans[0].status = PlanStatus::Completed;
        state.open_cleanup(repo, None);
        state.open_plan_cleanup(0);
        let host = state.into_host();
        eventually(&host, |state| {
            state.cleanup.as_ref().unwrap().rows.is_some() && state.plan_cleanup.as_ref().unwrap().ready()
        });
    });
}

#[test]
fn desktop_actions_launch_and_deliver_on_the_host_without_frames() {
    with_state("app::host_tests::desktop_actions_launch_and_deliver_on_the_host_without_frames", |state, _| {
        let actions = state.action_tx.clone();
        let host = state.into_host();
        let (tx, rx) = channel();
        actions
            .send(Box::new(move |state| {
                assert_eq!(std::thread::current().name(), Some("atomics-runtime"));
                let id = state.start("shell", state.launch_dir.clone(), None, vec![], None).unwrap();
                tx.send(id).unwrap();
            }))
            .unwrap();
        let id = rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(5))).unwrap();
        let session = id.clone();
        actions
            .send(Box::new(move |state| {
                let index = state.find(&session).unwrap();
                state.deliver(index, "printf 'desktop-action-delivered\\n'");
            }))
            .unwrap();
        eventually(&host, |state| {
            state.sessions[state.find(&id).unwrap()]
                .terminal
                .as_ref()
                .unwrap()
                .screen_text(60)
                .lines()
                .any(|line| line.trim() == "desktop-action-delivered")
        });
    });
}

#[test]
fn blocked_project_picker_does_not_block_hooks_or_waits() {
    with_state("app::host_tests::blocked_project_picker_does_not_block_hooks_or_waits", |mut state, ctx| {
        let (_dir, repo) = repository();
        state.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Working, &state.launch_dir)];
        let client = Client::new(ctx, &mut Options::default(), &crate::load_icon());
        let mut app = App { client, host: state.into_host(), replies: Vec::new() };
        // The UI can keep this request pending across frames without invoking
        // native code or draining the runtime's action queue under its guard.
        for _ in 0..2 {
            ctx.run_ui(egui::RawInput::default(), |_| {
                let mut state = app.host.state();
                Desktop { runtime: &mut state, client: &mut app.client }.pick_project();
            })
            .drop_without_applying_deltas();
        }
        let wait = app.host.command(ControlRequest::Wait {
            sessions: vec!["worker".into()],
            statuses: vec!["turn done".into()],
            after: Some(0),
            timeout_secs: Some(5),
        });
        command(&app.host, json!({"cmd":"list"}));
        let (opened_tx, opened_rx) = channel();
        let (done_tx, done_rx) = channel();
        let worker = std::thread::spawn(move || {
            opened_rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(5))).unwrap();
            hook("Stop", None);
            let reply = response(wait);
            assert_eq!(reply.result["timed_out"], false);
            assert_eq!(reply.result["events"][0]["status"], "turn done");
            // This real deadline also has to fire while the fake picker blocks.
            let reply = ipc::request(
                &serde_json::from_value(json!({"cmd":"wait", "after":u64::MAX, "timeout_secs":1})).unwrap(),
            )
            .unwrap();
            assert_eq!(reply.result["timed_out"], true);
            done_tx.send(()).unwrap();
        });
        ctx.run_ui(egui::RawInput::default(), |ui| {
            app.ui_with_picker(ui, |_| {
                opened_tx.send(()).unwrap();
                done_rx
                    .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(10)))
                    .expect("runtime stalled while folder picker was open");
                Some(repo.clone())
            });
        })
        .drop_without_applying_deltas();
        worker.join().unwrap();
        eventually(&app.host, |state| state.open_projects.contains(&repo.canonicalize().unwrap()));
        assert!(app.client.project_picker.is_none());
    });
}

#[test]
fn delayed_file_open_keeps_the_hosts_resolved_target() {
    with_state("app::host_tests::delayed_file_open_keeps_the_hosts_resolved_target", |mut state, ctx| {
        let (_dir, repo) = repository();
        let path = "review.txt";
        std::fs::write(repo.join(path), "original\n").unwrap();
        git::git(&repo, &["add", path]).unwrap();
        git::git(&repo, &["commit", "-qm", "tracked file"]).unwrap();
        std::fs::write(repo.join(path), "changed\n").unwrap();
        state.config.base_ref = "main".into();
        state.repos = vec![repo.clone()];
        state.diff_root = Some(repo.clone());
        let mut client = Client::new(ctx, &mut Options::default(), &crate::load_icon());
        let host = state.into_host();
        eventually(&host, |state| {
            state.diffs.get(&repo).and_then(|d| d.prepared.as_ref()).is_some_and(|p| !p.diff.files.is_empty())
        });
        for whole_file in [false, true] {
            {
                let mut state = host.state();
                let mut app = Desktop { runtime: &mut state, client: &mut client };
                app.open_file(&repo, path);
                // Do not drain the action or consume events across these frames.
                for _ in 0..2 {
                    ctx.run_ui(egui::RawInput::default(), |_| app.sync_diffs()).drop_without_applying_deltas();
                }
            }
            eventually(&host, |state| {
                state.client_events.iter().any(|event| matches!(event, ClientEvent::OpenFile { .. }))
            });
            assert_eq!(host.state().diffs[&repo].viewing.is_some(), whole_file);
            let before = host.state().diffs[&repo].revision;
            let contents = if whole_file { "changed again\n" } else { "original\n" };
            std::fs::write(repo.join(path), contents).unwrap();
            eventually(&host, |state| {
                let data = &state.diffs[&repo];
                data.revision > before && data.prepared.as_ref().is_some_and(|p| p.diff.files.is_empty() != whole_file)
            });
            if whole_file {
                eventually(&host, |state| {
                    state.diffs[&repo]
                        .file
                        .as_ref()
                        .is_some_and(|file| file.lines.first().is_some_and(|line| line == "changed again"))
                });
            }
            let mut state = host.state();
            let mut app = Desktop { runtime: &mut state, client: &mut client };
            for _ in 0..2 {
                ctx.run_ui(egui::RawInput::default(), |_| app.sync_diffs()).drop_without_applying_deltas();
            }
            app.apply_client_events(ctx);
            app.sync_diffs();
            let view = &app.client.diff_views[&repo];
            assert_eq!(
                view.viewing, app.runtime.diffs[&repo].viewing,
                "late events must not choose a different target"
            );
            assert_eq!(view.viewing.is_some(), whole_file);
            assert_eq!(view.focused.as_deref(), Some(path));
            if whole_file {
                assert_eq!(view.file.as_ref().unwrap().lines[0], "changed again");
            } else {
                assert!(!view.loading);
            }
        }
    });
}

#[test]
fn the_protocol_server_is_off_by_default() {
    with_state("app::host_tests::the_protocol_server_is_off_by_default", |state, _| {
        assert!(!state.config.remote.enabled);
        let host = state.into_host();
        assert!(host.protocol_addr().is_none(), "no flag, no listener");
        assert!(!paths::atomics_home().join("protocol.json").exists(), "and nothing advertised");
        assert!(!host.streams_terminals(), "and no terminal is published, copied or streamed");
    });
}

#[test]
fn the_protocol_streams_a_session_terminal_while_the_state_lock_is_held() {
    with_state(
        "app::host_tests::the_protocol_streams_a_session_terminal_while_the_state_lock_is_held",
        |mut state, _| {
            use crate::protocol::tests::TestClient;
            use atomics_core::screen::ScreenMirror;
            use atomics_core::wire::{Envelope, Message, Topic};
            state.config.remote.enabled = true;
            let script = "printf 'first line\\r\\n'; read -r go; printf 'second line\\r\\n'; exec sleep 30";
            let terminal = Terminal::spawn(
                state.wake.clone(),
                state.theme.terminal_colors(),
                "/bin/sh".into(),
                vec!["-c".into(), script.into()],
                &state.launch_dir,
                HashMap::new(),
                100,
            )
            .unwrap();
            state.sessions = vec![session("worker", AgentKind::Generic, AgentStatus::Working, &state.launch_dir)];
            state.sessions[0].terminal = Some(terminal);
            let host = state.into_host();
            assert!(host.streams_terminals());
            let addr = host.protocol_addr().expect("the flag starts the server");
            let advert: serde_json::Value =
                serde_json::from_slice(&std::fs::read(paths::atomics_home().join("protocol.json")).unwrap()).unwrap();
            let mut client = TestClient::connect(addr, advert["token"].as_str().unwrap());
            client.hello();
            let subscribe = Envelope::new(Message::Subscribe {
                topics: vec![Topic::Screen { session: "worker".into(), focused: true }],
            });
            // Terminals are published before the board: a session on the board can be subscribed to.
            client.sync_until(|m| m.sessions.contains_key("worker"));
            client.send(&subscribe);
            let mut mirror = ScreenMirror::default();
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
            let follow = |client: &mut TestClient, mirror: &mut ScreenMirror, text: &str| {
                while !mirror.screen.as_ref().is_some_and(|s| s.text().contains(text)) {
                    assert!(Instant::now() < deadline, "the screen never showed {text}");
                    match client.next().expect("connection open").message {
                        Message::ScreenSnapshot(s) => drop(mirror.apply_snapshot(&s).unwrap()),
                        Message::ScreenDelta(d) => drop(mirror.apply_delta(&d).unwrap()),
                        Message::Error { code, message } => panic!("{code:?}: {message}"),
                        _ => {}
                    }
                }
            };
            follow(&mut client, &mut mirror, "first line");

            // With the model locked, as for a whole frame of drawing, output still streams.
            let guard = host.state();
            guard.sessions[0].terminal.as_ref().unwrap().write(b"go\r".to_vec());
            follow(&mut client, &mut mirror, "second line");
            drop(guard);
            let screen = mirror.screen.unwrap();
            assert_eq!(screen.lines[0].text(), "first line");
            assert_eq!((screen.cols, screen.rows), (80, 24));
        },
    );
}

#[test]
fn a_remote_client_types_into_a_session_shell_through_its_lease_while_the_state_lock_is_held() {
    with_state(
        "app::host_tests::a_remote_client_types_into_a_session_shell_through_its_lease_while_the_state_lock_is_held",
        |mut state, _| {
            use crate::protocol::tests::TestClient;
            use atomics_core::lease::Typist;
            use atomics_core::screen::ScreenMirror;
            use atomics_core::wire::{Envelope, Message, Topic};
            state.config.remote.enabled = true;
            let script = "read -r line; printf 'got %s\\r\\n' \"$line\"; exec sleep 30";
            let terminal = Terminal::spawn(
                state.wake.clone(),
                state.theme.terminal_colors(),
                "/bin/sh".into(),
                vec!["-c".into(), script.into()],
                &state.launch_dir,
                HashMap::new(),
                100,
            )
            .unwrap();
            state.sessions = vec![session("worker", AgentKind::Generic, AgentStatus::Working, &state.launch_dir)];
            state.sessions[0].terminal = Some(terminal);
            let host = state.into_host();
            let addr = host.protocol_addr().expect("the flag starts the server");
            let advert: serde_json::Value =
                serde_json::from_slice(&std::fs::read(paths::atomics_home().join("protocol.json")).unwrap()).unwrap();
            let mut client = TestClient::connect(addr, advert["token"].as_str().unwrap());
            client.ws.get_ref().set_nodelay(true).unwrap();
            client.hello();
            client.sync_until(|m| m.sessions.contains_key("worker"));
            client.send(&Envelope::new(Message::Subscribe {
                topics: vec![Topic::Screen { session: "worker".into(), focused: true }],
            }));
            let mut typist = Typist::new("worker");
            client.send(&Envelope::new(typist.acquire(false)));
            let (mut mirror, deadline) = (
                ScreenMirror::default(),
                Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10)),
            );
            let follow = |client: &mut TestClient,
                          typist: &mut Typist,
                          mirror: &mut ScreenMirror,
                          done: &dyn Fn(&Typist, &ScreenMirror) -> bool| {
                while !done(typist, mirror) {
                    assert!(Instant::now() < deadline, "the remote client never got there");
                    match client.next().expect("connection open").message {
                        Message::ScreenSnapshot(s) => drop(mirror.apply_snapshot(&s).unwrap()),
                        Message::ScreenDelta(d) => drop(mirror.apply_delta(&d).unwrap()),
                        Message::LeaseState(l) => typist.apply(&l),
                        Message::Error { code, message } => panic!("{code:?}: {message}"),
                        _ => {}
                    }
                }
            };
            follow(&mut client, &mut typist, &mut mirror, &|t, m| t.may_type() && m.screen.is_some());

            // With the model locked, as for a whole frame of drawing, typing still reaches the shell.
            let guard = host.state();
            for frame in typist.input("hello\r") {
                client.send(&Envelope::new(frame));
            }
            follow(&mut client, &mut typist, &mut mirror, &|_, m| {
                m.screen.as_ref().is_some_and(|s| s.text().contains("got hello"))
            });
            drop(guard);
            assert_eq!(mirror.screen.unwrap().lines[0].text(), "hello", "the PTY echoed what was typed");
        },
    );
}

#[test]
fn the_protocol_serves_the_board_while_the_state_lock_is_held() {
    with_state("app::host_tests::the_protocol_serves_the_board_while_the_state_lock_is_held", |mut state, _| {
        use crate::protocol::tests::TestClient;
        state.config.remote.enabled = true;
        state.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Working, &state.launch_dir)];
        let host = state.into_host();
        let addr = host.protocol_addr().expect("the flag starts the server");
        let advert: serde_json::Value =
            serde_json::from_slice(&std::fs::read(paths::atomics_home().join("protocol.json")).unwrap()).unwrap();
        let token = advert["token"].as_str().unwrap().to_string();

        let mut first = TestClient::connect(addr, &token);
        first.hello();
        first.sync_until(|m| m.sessions.contains_key("worker"));

        // With the model locked, as it is for a whole frame of drawing, the protocol thread still
        // connects a new client and sends the board: it never asks for this lock.
        let guard = host.state();
        let mut second = TestClient::connect(addr, &token);
        second.hello();
        second.sync_until(|m| m.sessions.contains_key("worker"));
        drop(guard);

        // And the host still answers its own clients with a client attached.
        assert_eq!(host.state().sessions.len(), 1);
    });
}

#[test]
fn a_headless_host_refuses_window_requests_and_drops_client_events() {
    with_state("app::host_tests::a_headless_host_refuses_window_requests_and_drops_client_events", |mut state, _| {
        // The desktop's model has its window attached; `atomics host` builds one with none.
        assert!(state.window.is_attached());
        state.window = crate::host::Window::default();
        state.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Working, &state.launch_dir)];
        let host = state.into_host();
        for request in [
            json!({"cmd":"screenshot", "path":"/tmp/atomics-never.png"}),
            json!({"cmd":"resize", "width":800.0, "height":600.0}),
            json!({"cmd":"show_settings", "open":true}),
            json!({"cmd":"restart"}),
        ] {
            let outcome = host.command(serde_json::from_value(request.clone()).unwrap());
            let Ok(ControlOutcome::Done(refused)) =
                outcome.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(10)))
            else {
                panic!("{request} was not answered");
            };
            assert!(
                !refused.ok && refused.error.as_deref().unwrap().contains("headless host"),
                "{request}: {refused:?}"
            );
        }
        let list = command(&host, json!({"cmd":"list"}));
        assert!(list.result["window"].is_null() && list.result["monitor"].is_null(), "{}", list.result);
        // Work that would show something in a window queues nothing for a client that never comes.
        command(&host, json!({"cmd":"open", "session":"worker", "path":"README.md"}));
        eventually(&host, |state| state.client_events.is_empty() && state.client_commands.is_empty());
    });
}

/// A protocol client of `host`, greeted with every capability, as client `test`.
fn remote_client(host: &Host) -> crate::protocol::tests::TestClient {
    let addr = host.protocol_addr().expect("the flag starts the server");
    let advert: serde_json::Value =
        serde_json::from_slice(&std::fs::read(paths::atomics_home().join("protocol.json")).unwrap()).unwrap();
    let mut client = crate::protocol::tests::TestClient::connect(addr, advert["token"].as_str().unwrap());
    client.hello();
    client
}

/// The final outcome of `op`, applying the board frames that come first.
fn outcome_of(client: &mut crate::protocol::tests::TestClient, op: &str) -> atomics_core::ops::Outcome {
    use atomics_core::wire::Message;
    loop {
        let envelope = client.step().expect("connection open");
        if let Message::CommandResult(result) = envelope.message
            && result.op == op
            && result.outcome.is_final()
        {
            return result.outcome;
        }
    }
}

/// How the client was told approval `id` ended.
fn resolved_as(client: &mut crate::protocol::tests::TestClient, id: &str) -> atomics_core::wire::Disposition {
    use atomics_core::wire::Message;
    loop {
        if let Message::ApprovalResolved { id: resolved, disposition, .. } = client.step().expect("open").message
            && resolved == id
        {
            return disposition;
        }
    }
}

fn decide(op: &str, approval: &str, decision: Decision) -> atomics_core::wire::Envelope {
    let command =
        atomics_core::ops::ApprovalDecide { op: op.into(), approval: approval.into(), decision, message: None };
    atomics_core::wire::Envelope::new(atomics_core::wire::Message::ApprovalDecide(command))
}

fn steer(op: &str, session: &str, text: &str) -> atomics_core::ops::Command {
    use atomics_core::ops::{Command, SessionAction, SessionCommand};
    Command::SessionCommand(SessionCommand {
        op: op.into(),
        action: SessionAction::Steer { session: session.into(), text: text.into() },
    })
}

/// A permission request waiting in `session`, its hook waiting for the answer.
fn waiting_approval(session: &mut Session, id: &str) -> Arc<ipc::ApprovalReply> {
    let reply = Arc::new(ipc::ApprovalReply::default());
    session.approvals.push(PendingApproval {
        id: id.into(),
        request: ApprovalRequest {
            tool: "Bash".into(),
            command: Some(format!("touch {id}")),
            cwd: session.cwd.clone(),
            reason: None,
            input: json!({}),
        },
        reply: Some(reply.clone()),
    });
    session.status = AgentStatus::NeedsInput;
    reply
}

#[test]
fn two_clients_racing_an_approval_get_exactly_one_winner_and_both_see_how_it_ended() {
    with_state(
        "app::host_tests::two_clients_racing_an_approval_get_exactly_one_winner_and_both_see_how_it_ended",
        |mut state, _| {
            use atomics_core::ops::Outcome;
            use atomics_core::wire::{Disposition, ErrorCode};
            state.config.remote.enabled = true;
            state.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Working, &state.launch_dir)];
            let hook = waiting_approval(&mut state.sessions[0], "ask-1");
            let host = state.into_host();
            let (mut a, mut b) = (remote_client(&host), remote_client(&host));
            a.sync_until(|m| m.approvals.contains_key("ask-1"));
            b.sync_until(|m| m.approvals.contains_key("ask-1"));

            // Both decisions are in the runtime's queue before either runs: the model is locked.
            let guard = host.state();
            a.send(&decide("a-1", "ask-1", Decision::Allow));
            b.send(&decide("b-1", "ask-1", Decision::Deny));
            std::thread::sleep(Duration::from_millis(100));
            drop(guard);
            let (by_a, by_b) = (outcome_of(&mut a, "a-1"), outcome_of(&mut b, "b-1"));
            let a_won = matches!(by_a, Outcome::Done { .. });
            let (won, lost) = if a_won { (&by_a, &by_b) } else { (&by_b, &by_a) };
            assert!(matches!(won, Outcome::Done { .. }), "one wins: {by_a:?} / {by_b:?}");
            assert!(
                matches!(lost, Outcome::Refused { code: ErrorCode::AlreadyResolved, message } if message.contains(if a_won { "allowed" } else { "denied" })),
                "the other hears how it ended: {lost:?}"
            );
            let answer = hook.wait(Duration::ZERO).stdout.expect("the hook was answered");
            assert!(answer.contains(if a_won { "allow" } else { "deny" }), "with the winner's decision: {answer}");
            let disposition = if a_won { Disposition::Allowed } else { Disposition::Denied };
            assert_eq!(resolved_as(&mut a, "ask-1"), disposition, "every client is told how it ended");
            assert_eq!(resolved_as(&mut b, "ask-1"), disposition);
            assert!(host.state().sessions[0].approvals.is_empty());
        },
    );
}

#[test]
fn the_desktop_and_a_client_racing_an_approval_get_exactly_one_winner() {
    with_state(
        "app::host_tests::the_desktop_and_a_client_racing_an_approval_get_exactly_one_winner",
        |mut state, _| {
            use atomics_core::ops::{ApprovalDecide, Command, Outcome};
            use atomics_core::wire::ErrorCode;
            state.config.remote.enabled = true;
            state.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Working, &state.launch_dir)];
            let hook = waiting_approval(&mut state.sessions[0], "ask-1");
            let host = state.into_host();
            let mut client = remote_client(&host);
            client.sync_until(|m| m.approvals.contains_key("ask-1"));
            let guard = host.state();
            client.send(&decide("c-1", "ask-1", Decision::Allow));
            std::thread::sleep(Duration::from_millis(100));
            // What the desktop's Deny button posts (`Desktop::approve_request`).
            let (tx, desktop) = channel();
            let _ = guard.action_tx.send(Box::new(move |state: &mut State| {
                let outcome = state.mutate_desktop(|op| {
                    Command::ApprovalDecide(ApprovalDecide {
                        op,
                        approval: "ask-1".into(),
                        decision: Decision::Deny,
                        message: None,
                    })
                });
                tx.send(outcome).unwrap();
            }));
            drop(guard);
            let by_desktop =
                desktop.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(10))).unwrap();
            let by_client = outcome_of(&mut client, "c-1");
            // The runtime takes the desktop's actions first in a pass; either way exactly one wins.
            let wins = [&by_desktop, &by_client].iter().filter(|o| matches!(o, Outcome::Done { .. })).count();
            assert_eq!(wins, 1, "{by_desktop:?} / {by_client:?}");
            let lost = if matches!(by_desktop, Outcome::Done { .. }) { &by_client } else { &by_desktop };
            assert!(matches!(lost, Outcome::Refused { code: ErrorCode::AlreadyResolved, .. }), "{lost:?}");
            assert!(hook.wait(Duration::ZERO).stdout.is_some());
        },
    );
}

#[test]
fn a_decision_for_an_answered_request_never_answers_the_next_one() {
    with_state("app::host_tests::a_decision_for_an_answered_request_never_answers_the_next_one", |mut state, _| {
        use atomics_core::ops::Outcome;
        use atomics_core::wire::ErrorCode;
        state.config.remote.enabled = true;
        state.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Working, &state.launch_dir)];
        let first = waiting_approval(&mut state.sessions[0], "ask-1");
        let host = state.into_host();
        let (mut slow, mut quick) = (remote_client(&host), remote_client(&host));
        slow.sync_until(|m| m.approvals.contains_key("ask-1"));
        quick.sync_until(|m| m.approvals.contains_key("ask-1"));
        quick.send(&decide("q-1", "ask-1", Decision::Allow));
        assert!(matches!(outcome_of(&mut quick, "q-1"), Outcome::Done { .. }));
        assert!(first.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
        // The agent asks again; `slow` still shows the first request and denies it.
        let (next, behind) = {
            let mut guard = host.state();
            (waiting_approval(&mut guard.sessions[0], "ask-2"), waiting_approval(&mut guard.sessions[0], "ask-3"))
        };
        slow.send(&decide("s-1", "ask-1", Decision::Deny));
        let stale = outcome_of(&mut slow, "s-1");
        assert!(matches!(stale, Outcome::Refused { code: ErrorCode::AlreadyResolved, .. }), "{stale:?}");
        // A request queued behind another is not answered before it either.
        slow.send(&decide("s-2", "ask-3", Decision::Allow));
        let early = outcome_of(&mut slow, "s-2");
        assert!(matches!(early, Outcome::Refused { code: ErrorCode::Rejected, .. }), "{early:?}");
        // And one that never existed is not found.
        slow.send(&decide("s-3", "ask-9", Decision::Allow));
        assert!(matches!(outcome_of(&mut slow, "s-3"), Outcome::Refused { code: ErrorCode::NotFound, .. }));
        let ids: Vec<String> = host.state().sessions[0].approvals.iter().map(|a| a.id.clone()).collect();
        assert_eq!(ids, ["ask-2", "ask-3"], "both later requests still wait");
        assert!(next.answer(HookReply::default()) && behind.answer(HookReply::default()), "and nothing answered them");
    });
}

#[test]
fn a_retried_operation_returns_its_known_outcome_and_runs_once() {
    with_state("app::host_tests::a_retried_operation_returns_its_known_outcome_and_runs_once", |mut state, _| {
        use atomics_core::ops::Outcome;
        use atomics_core::wire::{Envelope, ErrorCode};
        state.config.remote.enabled = true;
        // Outside ATOMICS: a message waits in the hub for the session's next hook.
        state.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Working, &state.launch_dir)];
        let hub = state.hub.clone();
        let host = state.into_host();
        let mut client = remote_client(&host);
        client.sync_until(|m| m.sessions.contains_key("worker"));
        client.send(&Envelope::new(steer("s-1", "worker", "check the tests").into_message()));
        let first = outcome_of(&mut client, "s-1");
        assert!(matches!(&first, Outcome::Done { result } if result["queued_until_next_hook"] == true), "{first:?}");
        client.send(&Envelope::new(steer("s-1", "worker", "check the tests").into_message()));
        assert_eq!(outcome_of(&mut client, "s-1"), first, "a retry is answered from the journal");
        // So is one from a new connection of the same client, as after a reconnect.
        drop(client);
        let mut again = remote_client(&host);
        again.send(&Envelope::new(steer("s-1", "worker", "check the tests").into_message()));
        assert_eq!(outcome_of(&mut again, "s-1"), first);
        assert_eq!(hub.lock().unwrap().outbox["worker"], ["check the tests"], "delivered once");
        // The same ID for another command is a conflict, not the old outcome.
        again.send(&Envelope::new(steer("s-1", "worker", "something else").into_message()));
        let reused = outcome_of(&mut again, "s-1");
        assert!(matches!(reused, Outcome::Refused { code: ErrorCode::Conflict, .. }), "{reused:?}");
        assert_eq!(hub.lock().unwrap().outbox["worker"].len(), 1);
    });
}

#[test]
fn an_operation_the_host_stopped_during_is_uncertain_and_not_run_again() {
    with_state(
        "app::host_tests::an_operation_the_host_stopped_during_is_uncertain_and_not_run_again",
        |mut state, _| {
            use crate::protocol::journal::{self, Begun, Journal, Key};
            use atomics_core::ops::Outcome;
            use atomics_core::wire::Envelope;
            state.config.remote.enabled = true;
            state.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Working, &state.launch_dir)];
            let hub = state.hub.clone();
            let command = steer("s-1", "worker", "merge it");
            {
                // An earlier host began the operation and died before it ended.
                let path = paths::atomics_home().join("protocol-ops.jsonl");
                let mut earlier = Journal::open(&path, atomics_core::store::now()).unwrap();
                let began = earlier.begin(
                    &Key::client("test", "s-1"),
                    &journal::fingerprint(&command),
                    atomics_core::store::now(),
                );
                assert_eq!(began, Begun::Fresh);
            }
            let host = state.into_host();
            let mut client = remote_client(&host);
            client.send(&Envelope::new(command.into_message()));
            let outcome = outcome_of(&mut client, "s-1");
            assert!(
                matches!(&outcome, Outcome::Uncertain { message } if message.contains("check the board")),
                "{outcome:?}"
            );
            assert!(!hub.lock().unwrap().outbox.contains_key("worker"), "never redelivered on its own");
        },
    );
}

#[test]
fn approving_a_plan_names_the_scope_the_user_saw() {
    with_state("app::host_tests::approving_a_plan_names_the_scope_the_user_saw", |mut state, _| {
        use atomics_core::ops::{Command, Outcome, PlanAction, PlanCommand};
        use atomics_core::wire::{Envelope, ErrorCode};
        state.config.remote.enabled = true;
        let task = Task { id: "t1".into(), title: "fix the parser".into(), ..Task::default() };
        state.plans = vec![Plan { id: "p1".into(), goal: "ship".into(), tasks: vec![task], ..Plan::default() }];
        state.selected_plan = 0;
        let host = state.into_host();
        let mut client = remote_client(&host);
        client.sync_until(|m| m.plans.get("p1").is_some_and(|p| !p.digest.is_empty()));
        let seen = client.mirror.plans["p1"].digest.clone();
        let approve = |op: &str, digest: Option<&str>| {
            let command = PlanCommand {
                op: op.into(),
                plan: Some("p1".into()),
                digest: digest.map(str::to_string),
                action: PlanAction::Approve,
            };
            Envelope::new(Command::PlanCommand(command).into_message())
        };
        client.send(&approve("p-0", None));
        assert!(matches!(outcome_of(&mut client, "p-0"), Outcome::Refused { code: ErrorCode::Malformed, .. }));
        // The chief rewrites the plan while the user reads it.
        host.state().plans[0].tasks[0].brief = "and the lexer".into();
        client.send(&approve("p-1", Some(&seen)));
        let stale = outcome_of(&mut client, "p-1");
        assert!(matches!(stale, Outcome::Refused { code: ErrorCode::StaleRevision, .. }), "{stale:?}");
        assert!(!host.state().plans[0].approved);
        client.sync_until(|m| m.plans["p1"].digest != seen);
        let current = client.mirror.plans["p1"].digest.clone();
        client.send(&approve("p-2", Some(&current)));
        assert!(matches!(outcome_of(&mut client, "p-2"), Outcome::Done { .. }));
        assert!(host.state().plans[0].approved);
    });
}

#[test]
fn without_remote_there_is_no_journal_file_and_the_desktop_approves_and_steers_as_before() {
    with_state(
        "app::host_tests::without_remote_there_is_no_journal_file_and_the_desktop_approves_and_steers_as_before",
        |mut state, _| {
            use atomics_core::ops::{ApprovalDecide, Command, Outcome, SessionAction, SessionCommand};
            assert!(!state.config.remote.enabled, "[remote] unset");
            state.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Working, &state.launch_dir)];
            let hook = waiting_approval(&mut state.sessions[0], "ask-1");
            let hub = state.hub.clone();
            let host = state.into_host();
            // What the desktop's Allow button and steer bar post (`Desktop::approve_request`, `send_steer`).
            let (tx, outcomes) = channel();
            let _ = host.state().action_tx.send(Box::new(move |state: &mut State| {
                let approved = state.mutate_desktop(|op| {
                    Command::ApprovalDecide(ApprovalDecide {
                        op,
                        approval: "ask-1".into(),
                        decision: Decision::Allow,
                        message: None,
                    })
                });
                let steered = state.mutate_desktop(|op| {
                    let action = SessionAction::Steer { session: "worker".into(), text: "carry on".into() };
                    Command::SessionCommand(SessionCommand { op, action })
                });
                tx.send((approved, steered)).unwrap();
            }));
            let (approved, steered) =
                outcomes.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(10))).unwrap();
            assert_eq!(approved, Outcome::done(json!({ "disposition": "allowed" })));
            assert!(hook.wait(Duration::ZERO).stdout.unwrap().contains("allow"), "the hook got the answer");
            assert_eq!(
                steered,
                Outcome::done(json!({ "sent": true, "queued_until_next_hook": true })),
                "the steer bar's own delivery"
            );
            assert_eq!(hub.lock().unwrap().outbox["worker"], ["carry on"]);
            let worker = host.state();
            assert!(worker.sessions[0].approvals.is_empty());
            assert_eq!(worker.sessions[0].status, AgentStatus::Working);
            assert!(!paths::atomics_home().join("protocol-ops.jsonl").exists(), "no journal file");
            assert!(!paths::atomics_home().join("protocol.json").exists(), "and no server");
        },
    );
}
