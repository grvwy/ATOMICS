use super::*;
use atomics_core::heartbeat::{Finding, Kind};

struct WakeConditions {
    wake_secs: u64,
    status: Option<AgentStatus>,
    work_open: bool,
    approval_pending: bool,
    input_age: Option<Duration>,
    input_empty: bool,
    elapsed: Option<Duration>,
}

impl WakeConditions {
    fn should_wake(&self) -> bool {
        self.wake_secs != 0
            && matches!(self.status, Some(AgentStatus::Idle | AgentStatus::Done))
            && self.work_open
            && !self.approval_pending
            && self.input_age.is_none_or(|age| age >= Duration::from_secs(60))
            && self.input_empty
            && self.elapsed.is_some_and(|elapsed| elapsed >= Duration::from_secs(self.wake_secs))
    }
}

fn wake_message(events: usize, findings: &[Finding], cleanup: &[String]) -> String {
    let kinds: Vec<_> = [
        (Kind::AgentGone, "agent_gone"),
        (Kind::Unreported, "unreported"),
        (Kind::Approval, "approval"),
        (Kind::HeldMessages, "held_messages"),
        (Kind::StuckWorking, "stuck_working"),
    ]
    .into_iter()
    .filter_map(|(kind, label)| findings.iter().any(|finding| finding.kind == kind).then_some(label))
    .collect();
    let kinds = if kinds.is_empty() { "none".into() } else { kinds.join(", ") };
    let mut message = format!(
        "ATOMICS wake: {events} new events, {} heartbeat findings ({kinds}). Call atomics_wait and act; if nothing changed, post one short line.",
        findings.len()
    );
    if !cleanup.is_empty() {
        let listed = cleanup.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        let more = if cleanup.len() > 5 { format!(" (+{} more)", cleanup.len() - 5) } else { String::new() };
        message.push_str(&format!(
            " Cleanup: {listed}{more}. Close each listed agent with atomics_close or say why it should stay open."
        ));
    }
    message
}

fn chief_work_open(plans: &[Plan], sessions: &[Session]) -> bool {
    plans.iter().any(|plan| {
        plan.is_open() && plan.tasks.iter().any(|task| matches!(task.status, TaskStatus::Running | TaskStatus::Review))
    }) || sessions.iter().any(|s| !s.chief && s.is_agent())
}

/// A transcript belongs to the provider that created it, even after chief config changes.
pub(super) fn provider_identity(
    provider: Option<atomics_core::config::AgentProvider>,
) -> atomics_core::config::ChiefProvider {
    use atomics_core::config::{AgentProvider, ChiefProvider};
    match provider {
        Some(AgentProvider::Copilot) => ChiefProvider::Copilot,
        Some(AgentProvider::Cursor) => ChiefProvider::Cursor,
        None => ChiefProvider::Claude,
    }
}

impl State {
    pub(super) fn check_chief_resume(&self, record: &SessionRecord) -> Result<(), String> {
        use atomics_core::config::ChiefProvider;
        let saved = record.chief_provider.unwrap_or(ChiefProvider::Claude);
        let selected = provider_identity(self.config.chief.provider());
        if saved != selected {
            return Err(format!(
                "Could not resume chief: its saved conversation belongs to {saved:?}, but [chief] now selects {selected:?}. \
                 Restore that provider to resume it, or start a new chief conversation."
            ));
        }
        Ok(())
    }

    /// Runs on the host's background tick, including while the chief is hidden or no client is drawing.
    pub(super) fn wake_chief(&mut self) {
        if self.config.chief.wake_secs == 0 {
            return;
        }
        let Some(index) = self.sessions.iter().position(|s| s.chief) else { return };
        let chief = &self.sessions[index];
        if !matches!(chief.status, AgentStatus::Idle | AgentStatus::Done) {
            return;
        }
        let Some(terminal) = chief.terminal.as_ref().filter(|t| !t.exited()) else { return };
        // Another message must be picked up before adding an automatic one.
        if chief.pending.is_some() || !chief.held.is_empty() {
            return;
        }
        let screen = terminal.screen_text(60);
        let conditions = WakeConditions {
            wake_secs: self.config.chief.wake_secs,
            status: Some(chief.status),
            work_open: chief_work_open(&self.plans, &self.sessions),
            approval_pending: chief.approval_hold
                || !chief.approvals.is_empty()
                || chief.provider.is_none() && atomics_core::approvals::menu_visible(&screen),
            input_age: terminal.last_typed().map(|at| at.elapsed()),
            input_empty: terminal.input_empty_since(chief.activity.turn_ended),
            elapsed: chief.activity.wake_since.map(|at| at.elapsed()),
        };
        if !conditions.should_wake() {
            return;
        }
        let events =
            self.events.iter().filter(|event| event.seq > self.chief_wake_cursor && event.session != chief.id).count();
        let findings = self.heartbeat.last.as_ref().map_or(&[][..], |(_, findings)| findings.as_slice());
        let message = wake_message(events, findings, &self.agent_cleanup_candidates());
        if matches!(self.deliver(index, &message), Delivery::Sent | Delivery::Pending) {
            self.sessions[index].activity.wake_since = Some(Instant::now());
            self.chief_wake_cursor = self.event_seq;
        }
    }

    /// Starts the chief agent in `[chief] cwd`, else in the main checkout of the project on screen,
    /// continuing the `[chief] resume` conversation when one is named. The workspace file's values
    /// win over the main and host config's.
    pub(super) fn start_chief(&mut self, prompt: Option<String>) -> Result<String, String> {
        let cwd = self.chief_cwd().unwrap_or_else(|| {
            self.view.clone().or_else(|| self.repos.first().cloned()).unwrap_or_else(|| self.launch_dir.clone())
        });
        let launch = self.tickets.workspace_chief.clone().unwrap_or_default();
        let resume = setting(launch.resume).or_else(|| setting(self.config.chief.resume.clone()));
        let args = self.config.chief.args.clone();
        self.start_as("chief", cwd, resume, args, prompt, Role::Chief)
    }

    /// `[chief] cwd`, the workspace file's over the main and host config's. A restart resumes the
    /// chief there too, and without one in the folder it was started in.
    pub(super) fn chief_cwd(&self) -> Option<PathBuf> {
        let launch = self.tickets.workspace_chief.as_ref().and_then(|chief| chief.cwd.clone());
        setting(launch).or_else(|| setting(self.config.chief.cwd.clone())).map(|dir| paths::expand(&dir))
    }

    /// `[chief] autostart`, the workspace file's over the main and host config's.
    pub(super) fn chief_autostarts(&self) -> bool {
        self.tickets.workspace_chief.as_ref().and_then(|chief| chief.autostart).unwrap_or(self.config.chief.autostart)
    }

    pub(super) fn chief_dock(&self) -> ChiefDock {
        self.store.chief_dock.unwrap_or(self.config.chief.dock)
    }

    pub(super) fn save_chief_preferences(&mut self) {
        if let Err(err) = self.store.save(&paths::store_file()) {
            self.toast(format!("Could not save chief preferences: {err:#}"));
        }
    }

    pub(super) fn set_chief_dock(&mut self, dock: ChiefDock) {
        self.store.chief_dock = Some(dock);
        self.save_chief_preferences();
        if self.chief_open {
            self.focus_terminal = self.sessions.iter().find(|s| s.chief).map(|s| s.id.clone());
        }
    }

    /// Sends `text` to the chief, starting it with `text` as its first prompt when it is not
    /// running, and opens its window with the terminal focused.
    pub(super) fn send_to_chief(&mut self, text: &str) -> Result<(), String> {
        self.chief_delivery(text)?;
        self.chief_open = true;
        self.focus_terminal = self.sessions.iter().find(|s| s.chief).map(|s| s.id.clone());
        Ok(())
    }

    /// Delivers `text` to the chief, starting it with `text` as its first prompt when it is not
    /// running, without opening its window. Unavailable is an error; the other outcomes are returned.
    pub(super) fn chief_delivery(&mut self, text: &str) -> Result<Delivery, String> {
        match self.sessions.iter().position(|s| s.chief && s.can_receive()) {
            Some(index) => match self.deliver(index, text) {
                Delivery::Unavailable => Err("The chief cannot receive messages right now".into()),
                delivery => Ok(delivery),
            },
            None => {
                self.close_old_chiefs();
                self.start_chief(Some(text.to_string()))?;
                Ok(Delivery::Sent)
            }
        }
    }

    fn close_old_chiefs(&mut self) {
        let chiefs: Vec<_> =
            self.sessions.iter().filter(|session| session.chief).map(|session| session.id.clone()).collect();
        for chief in chiefs {
            self.close(&chief);
        }
    }

    pub(super) fn toggle_chief(&mut self) {
        if !self.sessions.iter().any(|s| s.chief && s.terminal.as_ref().is_some_and(|t| !t.exited())) {
            self.close_old_chiefs();
            self.traces.retain(|id, _| self.sessions.iter().any(|session| session.id == *id && session.chief));
            if let Err(err) = self.start_chief(None) {
                self.toast(err);
                return;
            }
            self.chief_open = true;
        } else {
            self.chief_open = !self.chief_open;
        }
        if self.chief_open
            && let Some(chief) = self.sessions.iter().find(|s| s.chief)
        {
            self.focus_terminal = Some(chief.id.clone());
        }
    }
}

/// Tools the chief may use without asking: reading, planning, reviewing, and running the
/// plan the user approved. Starting agents, typing into terminals and closing sessions ask
/// first. Config tools only read or propose: a proposed change waits for the user to apply it.
/// Checks with atomics_exec run in an agent's sandbox, which reaches no more than its agent does.
pub(super) const CHIEF_ALLOWED_TOOLS: &[&str] = &[
    "mcp__atomics__atomics_list",
    "mcp__atomics__atomics_trace_get",
    "mcp__atomics__atomics_wait",
    "mcp__atomics__atomics_restart",
    "mcp__atomics__atomics_approve",
    "mcp__atomics__atomics_result",
    "mcp__atomics__atomics_read",
    "mcp__atomics__atomics_diff",
    "mcp__atomics__atomics_exec",
    "mcp__atomics__atomics_plan_get",
    "mcp__atomics__atomics_plan_close",
    "mcp__atomics__atomics_plan_set",
    "mcp__atomics__atomics_plan_update",
    "mcp__atomics__atomics_plan_dispatch",
    "mcp__atomics__atomics_plan_integrate",
    "mcp__atomics__atomics_plan_stage",
    "mcp__atomics__atomics_stage_event",
    "mcp__atomics__atomics_comment",
    "mcp__atomics__atomics_send_review",
    "mcp__atomics__atomics_focus",
    "mcp__atomics__atomics_open",
    "mcp__atomics__atomics_ticket_link",
    "mcp__atomics__atomics_ticket_sync",
    "mcp__atomics__atomics_ticket_comment",
    "mcp__atomics__atomics_ticket_search",
    "mcp__atomics__atomics_ticket_read",
    "mcp__atomics__atomics_ticket_children",
    "mcp__atomics__atomics_ticket_related",
    "mcp__atomics__atomics_config_get",
    "mcp__atomics__atomics_config_propose",
    "mcp__atomics__atomics_doctor",
];

#[cfg(test)]
impl Desktop<'_> {
    pub(super) fn chief_content(&mut self, ui: &mut egui::Ui) {
        self.draw_terminals(|backend, client, terminals| atomics_ui::chief::draw(ui, backend, client, terminals));
    }
}

/// A set config value: trimmed, and unset when empty.
fn setting(value: Option<String>) -> Option<String> {
    value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eligible() -> WakeConditions {
        WakeConditions {
            wake_secs: 180,
            status: Some(AgentStatus::Idle),
            work_open: true,
            approval_pending: false,
            input_age: None,
            input_empty: true,
            elapsed: Some(Duration::from_secs(180)),
        }
    }

    #[test]
    fn wakes_idle_or_turn_done_even_without_new_events_or_findings() {
        for status in [AgentStatus::Idle, AgentStatus::Done] {
            assert!(WakeConditions { status: Some(status), ..eligible() }.should_wake());
        }
    }

    #[test]
    fn wake_disabled() {
        assert!(!WakeConditions { wake_secs: 0, ..eligible() }.should_wake());
    }

    #[test]
    fn wake_requires_chief_session() {
        assert!(!WakeConditions { status: None, ..eligible() }.should_wake());
    }

    #[test]
    fn wake_never_interrupts_working_chief() {
        assert!(!WakeConditions { status: Some(AgentStatus::Working), ..eligible() }.should_wake());
    }

    #[test]
    fn wake_suppressed_while_starting_exited_or_needing_input() {
        for status in [AgentStatus::Starting, AgentStatus::Exited, AgentStatus::NeedsInput] {
            assert!(!WakeConditions { status: Some(status), ..eligible() }.should_wake());
        }
    }

    #[test]
    fn wake_requires_open_work() {
        assert!(!WakeConditions { work_open: false, ..eligible() }.should_wake());
    }

    #[test]
    fn wake_suppressed_by_pending_permission_or_approval() {
        assert!(!WakeConditions { approval_pending: true, ..eligible() }.should_wake());
    }

    #[test]
    fn wake_suppressed_for_sixty_seconds_after_typing() {
        assert!(!WakeConditions { input_age: Some(Duration::from_millis(59_999)), ..eligible() }.should_wake());
        assert!(WakeConditions { input_age: Some(Duration::from_secs(60)), ..eligible() }.should_wake());
    }

    #[test]
    fn wake_suppressed_by_unsubmitted_input() {
        assert!(!WakeConditions { input_empty: false, ..eligible() }.should_wake());
    }

    #[test]
    fn wake_waits_full_interval_after_turn_end_or_last_wake() {
        assert!(!WakeConditions { elapsed: Some(Duration::from_millis(179_999)), ..eligible() }.should_wake());
        assert!(!WakeConditions { elapsed: None, ..eligible() }.should_wake());
        assert!(eligible().should_wake());
    }

    #[test]
    fn wake_work_includes_running_review_or_non_chief_agents() {
        use super::super::tests::session;
        assert!(!chief_work_open(&[], &[]));
        for status in
            [TaskStatus::Planned, TaskStatus::Blocked, TaskStatus::Done, TaskStatus::Running, TaskStatus::Review]
        {
            let mut plan =
                Plan { id: "open".into(), tasks: vec![Task { status, ..Task::default() }], ..Plan::default() };
            assert_eq!(
                chief_work_open(std::slice::from_ref(&plan), &[]),
                matches!(status, TaskStatus::Running | TaskStatus::Review)
            );
            plan.status = PlanStatus::Completed;
            assert!(!chief_work_open(&[plan], &[]));
        }
        let mut agent = session("agent", AgentKind::Generic, AgentStatus::Idle, Path::new("/tmp"));
        assert!(chief_work_open(&[], std::slice::from_ref(&agent)));
        agent.chief = true;
        assert!(!chief_work_open(&[], std::slice::from_ref(&agent)));
        agent.chief = false;
        agent.kind = AgentKind::Shell;
        assert!(!chief_work_open(&[], &[agent]));
    }

    #[test]
    fn wake_line_is_exact_and_summarizes_unique_short_kinds() {
        assert_eq!(
            wake_message(0, &[], &[]),
            "ATOMICS wake: 0 new events, 0 heartbeat findings (none). Call atomics_wait and act; if nothing changed, post one short line."
        );
        let findings: Vec<_> = [
            Kind::StuckWorking,
            Kind::AgentGone,
            Kind::AgentGone,
            Kind::Approval,
            Kind::Unreported,
            Kind::HeldMessages,
        ]
        .into_iter()
        .map(|kind| Finding { plan: "plan".into(), task: "task".into(), session: None, kind })
        .collect();
        assert_eq!(
            wake_message(3, &findings, &[]),
            "ATOMICS wake: 3 new events, 6 heartbeat findings (agent_gone, unreported, approval, held_messages, stuck_working). Call atomics_wait and act; if nothing changed, post one short line."
        );
    }

    #[test]
    fn wake_names_cleanup_candidates_and_asks_for_a_decision_on_each() {
        let cleanup: Vec<_> = (0..7).map(|i| format!("agent-{i} (merged branch)")).collect();
        let message = wake_message(0, &[], &cleanup);
        assert!(message.contains("Cleanup: agent-0 (merged branch), agent-1 (merged branch)"));
        assert!(message.contains("agent-4 (merged branch) (+2 more)."));
        assert!(!message.contains("agent-5"));
        assert!(message.ends_with("Close each listed agent with atomics_close or say why it should stay open."));
        assert_eq!(message.lines().count(), 1);
    }

    #[test]
    fn native_chiefs_launch_orchestrate_and_resume_with_their_own_provider() {
        use super::super::tests::{request, with_app};
        use atomics_core::config::{AgentProvider, ChiefProvider};
        with_app("app::chief::tests::native_chiefs_launch_orchestrate_and_resume_with_their_own_provider", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            for provider in [AgentProvider::Copilot, AgentProvider::Cursor] {
                let executable = dir.path().join(match provider {
                    AgentProvider::Copilot => "copilot",
                    AgentProvider::Cursor => "cursor-agent",
                });
                crate::test_support::write_program(
                    &executable,
                    r#"#!/bin/sh
printf '%s\n' "$@" > chief-argv
printf '%s\n' "${COPILOT_CUSTOM_INSTRUCTIONS_DIRS-}" > chief-instructions-env
if [ -t 0 ] && [ -t 1 ]; then printf 'chief-interactive-pty\n'; fi
exec /bin/sleep 600
"#,
                );
                app.runtime.config.chief.command = vec![executable.to_string_lossy().into_owned()];
                app.runtime.config.chief.provider = Some(provider_identity(Some(provider)));
                app.runtime.config.chief.args = vec!["--model".into(), "chief-model".into()];
                app.runtime.config.chief.cwd = Some(dir.path().to_string_lossy().into_owned());
                app.runtime.config.chief.resume = None;
                app.runtime.config.chief.system_prompt =
                    "CHIEF_INSTRUCTIONS: coordinate, plan, review {projects}".into();
                let id = app.runtime.start_chief(Some("Plan the release".into())).unwrap();
                let index = app.runtime.find(&id).unwrap();
                assert!(app.runtime.sessions[index].chief);
                assert_eq!(app.runtime.sessions[index].kind, AgentKind::Generic);
                assert_eq!(app.runtime.sessions[index].provider, Some(provider));
                assert!(app.runtime.sessions[index].sandbox.is_none());
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !app.runtime.sessions[index]
                    .terminal
                    .as_ref()
                    .unwrap()
                    .screen_text(60)
                    .contains("chief-interactive-pty")
                {
                    assert!(Instant::now() < deadline, "chief did not get an interactive PTY");
                    std::thread::sleep(Duration::from_millis(10));
                }
                let args: Vec<String> = std::fs::read_to_string(dir.path().join("chief-argv"))
                    .unwrap()
                    .lines()
                    .map(str::to_owned)
                    .collect();
                assert!(args.windows(2).any(|pair| pair == ["--model", "chief-model"]));
                assert!(!args.iter().any(|arg| matches!(
                    arg.as_str(),
                    "--settings" | "--mcp-config" | "--append-system-prompt" | "--allowedTools"
                )));
                let plugin = crate::provider_launch::directory(&paths::atomics_home(), &id);
                let mcp: serde_json::Value = match provider {
                    AgentProvider::Copilot => {
                        assert!(args.windows(2).any(|pair| pair == ["-i", "Plan the release"]));
                        assert!(args.iter().any(|arg| arg == "--allow-tool=atomics(atomics_plan_set)"));
                        assert!(!args.iter().any(|arg| arg == "--allow-tool=atomics(atomics_start)"));
                        let at = args.iter().position(|arg| arg == "--additional-mcp-config").unwrap();
                        serde_json::from_str(&args[at + 1]).unwrap()
                    }
                    AgentProvider::Cursor => {
                        assert_eq!(args.last().map(String::as_str), Some("Plan the release"));
                        serde_json::from_slice(&std::fs::read(plugin.join("mcp.json")).unwrap()).unwrap()
                    }
                };
                assert_eq!(mcp["mcpServers"]["atomics"]["args"], json!(["mcp", "--chief"]));
                assert_eq!(mcp["mcpServers"]["atomics"]["env"]["ATOMICS_PANE"], id);
                let instructions = match provider {
                    AgentProvider::Copilot => {
                        assert!(
                            std::fs::read_to_string(dir.path().join("chief-instructions-env"))
                                .unwrap()
                                .contains(plugin.join("instructions").to_str().unwrap())
                        );
                        std::fs::read_to_string(plugin.join("instructions/AGENTS.md")).unwrap()
                    }
                    AgentProvider::Cursor => std::fs::read_to_string(plugin.join("rules/atomics-chief.mdc")).unwrap(),
                };
                assert!(instructions.contains("CHIEF_INSTRUCTIONS: coordinate, plan, review"));
                let conversation =
                    app.runtime.sessions[index].claude_id.clone().unwrap_or_else(|| "cursor-chief-conversation".into());
                let emit = |app: &mut Desktop<'_>, name: &str, fields: serde_json::Value| {
                    let mut payload = fields;
                    match provider {
                        AgentProvider::Copilot => payload["sessionId"] = json!(conversation),
                        AgentProvider::Cursor => payload["conversation_id"] = json!(conversation),
                    }
                    let payload = atomics_core::hook::normalize_native_payload(&payload, provider, name).unwrap();
                    app.runtime.on_hook(HookMessage {
                        pane: Some(id.clone()),
                        ancestors: vec![],
                        event: atomics_core::hook::HookEvent::from_payload(&payload).unwrap(),
                        delivered: 0,
                        approval_reply: None,
                    });
                };
                emit(app, "sessionStart", json!({}));
                emit(
                    app,
                    match provider {
                        AgentProvider::Copilot => "userPromptSubmitted",
                        AgentProvider::Cursor => "beforeSubmitPrompt",
                    },
                    json!({"prompt":"Plan the release"}),
                );
                assert!(app.runtime.sessions[index].hooks_active);
                assert_eq!(app.runtime.sessions[index].status, AgentStatus::Working);
                let plan = request(
                    app,
                    json!({"cmd":"plan_set", "goal":format!("Release via {}", provider.name()), "tasks":[{"id":"work", "title":"Implement the release"}]}),
                );
                assert!(plan.ok, "{plan:?}");
                let read = request(app, json!({"cmd":"plan_get", "plan":plan.result["plan"]}));
                assert!(read.ok);
                assert_eq!(read.result["tasks"][0]["id"], "work");
                // Config changes affect the next chief; hooks and persistence keep following this one.
                app.runtime.config.chief.provider = Some(ChiefProvider::Claude);
                emit(
                    app,
                    match provider {
                        AgentProvider::Copilot => "agentStop",
                        AgentProvider::Cursor => "stop",
                    },
                    json!({"status":"completed", "loop_count":0}),
                );
                assert_eq!(app.runtime.sessions[index].status, AgentStatus::Done);
                app.runtime.remember(&id);
                let saved = app
                    .runtime
                    .store
                    .sessions
                    .iter()
                    .find(|record| record.claude_session_id == conversation)
                    .unwrap()
                    .clone();
                assert_eq!(saved.chief_provider, Some(provider_identity(Some(provider))));
                app.runtime.close(&id);
                assert!(!plugin.exists());
                let before = app.runtime.sessions.len();
                app.runtime.resume_record(saved.clone());
                assert_eq!(
                    app.runtime.sessions.len(),
                    before,
                    "provider switch must not pass the old id to another CLI"
                );
                assert!(app.runtime.toast.as_ref().unwrap().0.contains("saved conversation belongs to"));
                app.runtime.config.chief.provider = Some(provider_identity(Some(provider)));
                app.runtime.resume_record(saved);
                let resumed = app.runtime.sessions.iter().find(|session| session.chief).unwrap();
                assert_eq!(resumed.provider, Some(provider));
                assert_eq!(resumed.claude_id.as_deref(), Some(conversation.as_str()));
                let resumed_id = resumed.id.clone();
                assert!(crate::provider_launch::directory(&paths::atomics_home(), &resumed_id).exists());
                app.runtime.close(&resumed_id);
            }
        });
    }

    #[test]
    fn ticket_tools_are_preapproved_for_the_chief() {
        for tool in ["search", "read", "children", "related"] {
            assert!(CHIEF_ALLOWED_TOOLS.contains(&format!("mcp__atomics__atomics_ticket_{tool}").as_str()));
        }
    }

    #[test]
    fn chief_approval_access_and_read_only_defaults_are_documented() {
        assert!(CHIEF_ALLOWED_TOOLS.contains(&"mcp__atomics__atomics_approve"));
        let readme = include_str!("../../../../README.md");
        assert!(readme.contains("pre-approved `atomics_approve`"));
        assert!(readme.contains("`allow_always` for the rest"));
        assert!(!readme.contains("or answer other agents'"));
        assert!(readme.contains("outside Codex's sandbox"));
        assert!(CHIEF_ALLOWED_TOOLS.contains(&"mcp__atomics__atomics_exec"));
        assert!(readme.contains("pre-approved `atomics_exec`"));
    }
}
