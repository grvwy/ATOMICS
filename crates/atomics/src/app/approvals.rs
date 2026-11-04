use super::*;

impl State {
    pub(super) fn approval_poll_interval(&self) -> Option<Duration> {
        if self.sessions.iter().any(|session| session.approval_hold || !session.approvals.is_empty()) {
            Some(Duration::from_millis(400))
        } else if self
            .sessions
            .iter()
            .any(|session| session.status == AgentStatus::NeedsInput && session.terminal.is_some())
        {
            Some(NEEDS_INPUT_CHECK_INTERVAL)
        } else {
            None
        }
    }

    pub(super) fn release_held(&mut self, index: usize) {
        if !self.sessions[index].approval_hold
            && self.sessions[index].approvals.is_empty()
            && !self.sessions[index].pending.as_ref().is_some_and(|pending| pending.codex)
            && !self.sessions[index].held.is_empty()
        {
            self.sessions[index].activity.held_since = None;
            let text = std::mem::take(&mut self.sessions[index].held).join("\n\n");
            self.deliver(index, &text);
        }
    }

    pub(super) fn expire_approvals(&mut self, now: Instant) {
        for approval in self.relay.take_write_approvals() {
            let index = self.sessions.iter().position(|session| session.sandbox.as_deref() == Some(&approval.session));
            if let Some(index) = index.filter(|_| !approval.reply.expired()) {
                let session = &mut self.sessions[index];
                session.approvals.push(PendingApproval {
                    id: uuid::Uuid::new_v4().to_string(),
                    request: approval.request,
                    reply: Some(approval.reply),
                });
                session.status = AgentStatus::NeedsInput;
                self.last_status.remove(&session.id);
            } else {
                approval.reply.answer(HookReply::default());
            }
        }
        self.mirror_pending_permission();
        if now.duration_since(self.approvals_checked) < APPROVAL_CHECK_INTERVAL {
            return;
        }
        self.approvals_checked = now;
        let check_needs_input = now.duration_since(self.needs_input_checked) >= NEEDS_INPUT_CHECK_INTERVAL;
        if check_needs_input {
            self.needs_input_checked = now;
        }
        for index in 0..self.sessions.len() {
            let session = &mut self.sessions[index];
            if session.sandbox.is_some() {
                let had_approvals = !session.approvals.is_empty();
                session.approvals.retain(|pending| {
                    pending.request.tool == "Azure REST" && pending.reply.as_ref().is_some_and(|reply| !reply.expired())
                });
                if had_approvals && session.approvals.is_empty() {
                    session.status = if session.terminal.as_ref().is_some_and(Terminal::exited) {
                        AgentStatus::Exited
                    } else {
                        AgentStatus::Working
                    };
                }
                session.approval_hold = false;
                continue;
            }
            let pending = session.approval_hold || !session.approvals.is_empty();
            if !pending && !(check_needs_input && session.status == AgentStatus::NeedsInput) {
                continue;
            }
            let screen = session.terminal.as_ref().map(|terminal| terminal.screen_text(60)).unwrap_or_default();
            let menu = atomics_core::approvals::menu_visible(&screen);
            if menu {
                // A native menu means Codex has already stopped waiting for the hook.
                for approval in &mut session.approvals {
                    if let Some(reply) = approval.reply.take() {
                        reply.answer(HookReply::default());
                    }
                }
                session.approval_hold = true;
                session.menu_absent_since = None;
                continue;
            }
            if session.approvals.iter().any(|pending| pending.reply.as_ref().is_some_and(|reply| !reply.expired())) {
                session.menu_absent_since = None;
                continue;
            }
            if !pending
                && !self
                    .config
                    .agent(&session.agent)
                    .and_then(|agent| agent.busy_text.as_ref())
                    .is_some_and(|busy| screen.contains(busy))
            {
                session.menu_absent_since = None;
                continue;
            }
            let missing_since = session.menu_absent_since.get_or_insert(now);
            if now.duration_since(*missing_since) < APPROVAL_MENU_GRACE {
                continue;
            }
            session.approvals.clear();
            session.approval_hold = false;
            session.menu_absent_since = None;
            if session.status == AgentStatus::NeedsInput {
                session.status = if session.terminal.as_ref().is_some_and(Terminal::exited) {
                    AgentStatus::Exited
                } else {
                    AgentStatus::Working
                };
            }
            self.release_held(index);
        }
    }

    /// Answer the approval with this ID, in whichever session it waits. Its ID belongs to that one
    /// request, so a decision made for it never answers the next one. Protocol clients, the
    /// desktop and ctl all decide on this thread, one at a time: the first decision wins, and a
    /// later one finds it answered and hears how (`already_resolved`).
    pub(super) fn decide(
        &mut self,
        approval: &str,
        decision: Decision,
        message: Option<&str>,
    ) -> Result<Disposition, WireError> {
        self.expire_approvals(Instant::now());
        let waiting = self.sessions.iter().enumerate().find_map(|(index, session)| {
            session.approvals.iter().position(|pending| pending.id == approval).map(|at| (index, at))
        });
        match waiting {
            None => Err(match self.resolution(approval) {
                Some(disposition) => WireError::new(
                    ErrorCode::AlreadyResolved,
                    format!("that permission request was already answered: {}", disposition.label()),
                ),
                None => WireError::new(ErrorCode::NotFound, "that permission request is no longer pending"),
            }),
            Some((_, at)) if at > 0 => Err(WireError::new(
                ErrorCode::Rejected,
                "an earlier permission request in that session waits for an answer first",
            )),
            // How it ended, as `approve` recorded it: "Allow always" can end as a one-time allow.
            Some((index, _)) => self
                .approve(index, approval, decision, message)
                .map(|()| self.resolution(approval).unwrap_or(disposition(decision))),
        }
    }

    /// What an approval of `request` from the session at `index` warns about when its command drops
    /// shared stash entries: those entries as git lists them now, in the repository the command runs
    /// git in, and whose they are.
    pub(super) fn stash_warning(&self, index: usize, request: &ApprovalRequest) -> Option<String> {
        use atomics_core::stash;
        let drops = request.stash_drops();
        if drops.is_empty() {
            return None;
        }
        let base = if request.cwd.as_os_str().is_empty() { self.sessions[index].root() } else { request.cwd.clone() };
        let dirs: Vec<PathBuf> =
            drops.iter().map(|drop| drop.dirs.iter().fold(base.clone(), |dir, d| dir.join(d))).collect();
        let mut listed: Vec<(&PathBuf, Result<Vec<stash::Entry>, String>)> = Vec::new();
        for dir in &dirs {
            if !listed.iter().any(|(listed, _)| *listed == dir) {
                listed.push((dir, stash::list(dir).map_err(|error| format!("{error:#}"))));
            }
        }
        let entries = |dir: &PathBuf| &listed.iter().find(|(listed, _)| *listed == dir).expect("listed").1;
        let drops = drops.iter().zip(&dirs).map(|(drop, dir)| (drop, entries(dir).as_deref().map_err(String::as_str)));
        Some(stash::warning(drops, |entry| self.stash_owner(index, entry)))
    }

    /// Whose stash `entry` is: the session whose worktree has its branch checked out, or else whose
    /// task its message names (`git stash push -m <task id>`); a task whose session has ended, by its
    /// branch or id; `requester` itself when it is that session.
    fn stash_owner(&self, requester: usize, entry: &atomics_core::stash::Entry) -> atomics_core::stash::Owner {
        use atomics_core::stash::Owner;
        let branch = entry.branch();
        let tag = entry.message().and_then(|message| message.split_whitespace().next());
        let tasks = || self.plans.iter().flat_map(|plan| &plan.tasks);
        let task_of = |session: &Session| tasks().find(|task| task.session.as_deref() == Some(&session.id));
        let by_branch: Vec<usize> =
            (0..self.sessions.len()).filter(|&i| branch.is_some() && self.sessions[i].branch() == branch).collect();
        let owners = if by_branch.is_empty() {
            (0..self.sessions.len())
                .filter(|&i| tag.is_some_and(|tag| task_of(&self.sessions[i]).is_some_and(|task| task.id == tag)))
                .collect()
        } else {
            by_branch
        };
        if owners.contains(&requester) {
            return Owner::Requester;
        }
        if let Some(&owner) = owners.first() {
            let session = &self.sessions[owner];
            return Owner::Known(match task_of(session) {
                _ if session.chief => "the main checkout, the chief's or yours".into(),
                Some(task) => format!("task {} (session {})", task.id, session.id),
                None => format!("session {}", session.id),
            });
        }
        let ended: Vec<_> = tasks()
            .filter(|task| {
                (branch.is_some() && task.branch.as_deref() == branch) || tag.is_some_and(|tag| task.id == tag)
            })
            .collect();
        match ended[..] {
            [task] => Owner::Known(format!("task {}", task.id)),
            _ => Owner::Unknown,
        }
    }

    /// How the approval `id` ended, if it was answered lately.
    pub(super) fn resolution(&self, id: &str) -> Option<Disposition> {
        self.resolved.iter().rev().find(|(i, _)| i == id).map(|(_, d)| *d)
    }

    fn record_resolution(&mut self, id: String, disposition: Disposition) {
        self.resolved.push_back((id, disposition));
        while self.resolved.len() > atomics_core::projection::MAX_RESOLUTIONS {
            self.resolved.pop_front();
        }
    }

    pub(super) fn approve(
        &mut self,
        index: usize,
        approval: &str,
        decision: Decision,
        message: Option<&str>,
    ) -> Result<(), WireError> {
        let rejected = |message: &str| WireError::new(ErrorCode::Rejected, message);
        self.expire_approvals(Instant::now());
        let session = &mut self.sessions[index];
        let Some(pending) = session.approvals.first_mut() else {
            return Err(WireError::new(ErrorCode::NotFound, "no pending permission request"));
        };
        if pending.id != approval {
            return Err(WireError::new(
                ErrorCode::StaleRevision,
                "stale permission decision: review the current request",
            ));
        }
        // "Allow always" answers once a request ATOMICS must not remember, such as a stash drop.
        let decision = match decision {
            Decision::AllowAlways if !pending.request.may_allow_always() => Decision::Allow,
            decision => decision,
        };
        let reply = HookReply { stdout: Some(decision.output(message)) };
        let azure = pending.request.tool == "Azure REST";
        let answered = pending.reply.take().is_some_and(|sender| sender.answer(reply));
        if !answered {
            if azure {
                let expired = session.approvals.remove(0);
                self.record_resolution(expired.id, Disposition::Expired);
                return Err(rejected("the Azure request expired; retry the request"));
            }
            let Some(terminal) = &session.terminal else {
                return Err(rejected("the permission hook has returned; answer in the agent terminal"));
            };
            let Some(key) = atomics_core::approvals::menu_key(
                &terminal.screen_text(60),
                pending.request.command.as_deref(),
                decision,
            ) else {
                return Err(rejected("the permission menu is not visible; retry or answer in the terminal"));
            };
            terminal.write(key.as_bytes().to_vec());
            if key == "\x01" {
                return Err(rejected("expanded the permission command; retry approval once it is visible"));
            }
        }
        let pending = session.approvals.remove(0);
        let id = pending.id.clone();
        if decision == Decision::AllowAlways {
            session.allowed.push(pending.request);
        }
        session.status = if session.approvals.is_empty() { AgentStatus::Working } else { AgentStatus::NeedsInput };
        if azure && !session.approvals.is_empty() {
            self.last_status.remove(&session.id);
        }
        self.record_resolution(id, disposition(decision));
        Ok(())
    }
}

fn disposition(decision: Decision) -> Disposition {
    match decision {
        Decision::Allow => Disposition::Allowed,
        Decision::AllowAlways => Disposition::AllowedAlways,
        Decision::Deny => Disposition::Denied,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{request, session, with_app};
    use super::*;

    #[test]
    fn approval_errors_keep_their_specific_guidance() {
        with_app("app::approvals::tests::approval_errors_keep_their_specific_guidance", |app, _| {
            for (tool, terminal, expected) in [
                ("Azure REST", false, "the Azure request expired; retry the request"),
                ("Bash", false, "the permission hook has returned; answer in the agent terminal"),
                ("Bash", true, "the permission menu is not visible; retry or answer in the terminal"),
            ] {
                let mut worker =
                    session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir);
                if terminal {
                    worker.terminal = Some(
                        Terminal::spawn(
                            app.runtime.wake.clone(),
                            app.runtime.theme.terminal_colors(),
                            "/bin/cat".into(),
                            vec![],
                            &app.runtime.launch_dir,
                            HashMap::new(),
                            100,
                        )
                        .unwrap(),
                    );
                }
                worker.approvals.push(PendingApproval {
                    id: "pending".into(),
                    request: ApprovalRequest {
                        tool: tool.into(),
                        command: Some("touch fixture".into()),
                        cwd: app.runtime.launch_dir.clone(),
                        reason: None,
                        input: json!({}),
                        warning: None,
                    },
                    reply: None,
                });
                app.runtime.sessions = vec![worker];
                app.runtime.approvals_checked = Instant::now();
                let error = app.runtime.decide("pending", Decision::Allow, None).unwrap_err();
                assert_eq!(error.message, expected);
            }
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn azure_writes_from_sandboxes_use_the_permission_banner_and_never_persist_allow_always() {
        with_app(
            "app::approvals::tests::azure_writes_from_sandboxes_use_the_permission_banner_and_never_persist_allow_always",
            |app, ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                let upstream = crate::relay::fake::start();
                let config = atomics_core::isolation::AzureConfig { enabled: true, ..Default::default() };
                let azure =
                    crate::relay::azure::Azure::new(crate::relay::azure::tests::fake(dir.path()), config.clone());
                app.runtime.relay = Arc::new(
                    crate::relay::Service::with_upstreams(
                        dir.path().to_path_buf(),
                        crate::relay::Keychain::at(Path::new("/no-keychain"), false),
                        [("management.azure.com".into(), upstream.url)].into(),
                    )
                    .with_azure(azure),
                );
                let granted = app.runtime.relay.grant_session("atomics-worker", false, &config).unwrap().unwrap();
                app.runtime.sessions =
                    vec![session("worker", AgentKind::Generic, AgentStatus::Working, &app.runtime.launch_dir)];
                app.runtime.sessions[0].sandbox = Some("atomics-worker".into());
                for decision in [Decision::Deny, Decision::AllowAlways] {
                    let port = granted.port;
                    let placeholder = granted.azure.as_ref().unwrap();
                    let request = format!(
                        "PUT /management.azure.com/v1/models?api-version=2026 HTTP/1.1\r\nauthorization: Bearer {placeholder}\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
                    );
                    let client = std::thread::spawn(move || crate::relay::fake::send(port, &request).0);
                    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                    while app.runtime.sessions[0].approvals.is_empty() {
                        assert!(Instant::now() < deadline);
                        app.runtime.expire_approvals(Instant::now());
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
                    let pending = &app.runtime.sessions[0].approvals[0];
                    let id = pending.id.clone();
                    assert_eq!(pending.request.tool, "Azure REST");
                    assert!(pending.request.command.as_ref().unwrap().contains("api-version=2026"));
                    // Normal sandbox hook permissions are ignored, but a live relay approval must survive polling.
                    app.runtime.expire_approvals(Instant::now() + Duration::from_secs(1));
                    assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
                            ..Default::default()
                        },
                        |ui| {
                            egui::CentralPanel::default().show(ui, |ui| {
                                let layout =
                                    layout::compute(1400.0, 900.0, 1, 0, false, 0.0, &app.runtime.config.layout);
                                app.tiles(ui, ui.max_rect(), &layout, &[0]);
                            });
                        },
                    );
                    let text: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|shape| match &shape.shape {
                            egui::epaint::Shape::Text(text) => Some(text.galley.text().to_string()),
                            _ => None,
                        })
                        .collect();
                    for label in ["Permission requested", "Allow", "Deny"] {
                        assert!(text.iter().any(|text| text == label), "{text:?}");
                    }
                    assert!(!text.iter().any(|text| text == "Allow always"));
                    output.drop_without_applying_deltas();
                    app.runtime.approve(0, &id, decision, None).unwrap();
                    let expected = if decision == Decision::Deny { 403 } else { 200 };
                    assert!(client.join().unwrap().starts_with(&format!("HTTP/1.1 {expected}")));
                    assert!(app.runtime.sessions[0].allowed.is_empty());
                    assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                }
                app.runtime.relay.revoke(&granted);
            },
        );
    }

    #[test]
    fn delayed_duplicate_decisions_cannot_approve_the_next_request() {
        with_app("app::approvals::tests::delayed_duplicate_decisions_cannot_approve_the_next_request", |app, ctx| {
            app.runtime.sessions =
                vec![session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir)];
            let reviewed = Arc::new(ipc::ApprovalReply::default());
            let unreviewed = Arc::new(ipc::ApprovalReply::default());
            for (id, reply) in [("reviewed", reviewed.clone()), ("unreviewed", unreviewed.clone())] {
                app.runtime.sessions[0].approvals.push(PendingApproval {
                    id: id.into(),
                    request: ApprovalRequest {
                        tool: "Bash".into(),
                        command: Some(format!("touch {id}")),
                        cwd: app.runtime.launch_dir.clone(),
                        reason: None,
                        input: json!({}),
                        warning: None,
                    },
                    reply: Some(reply),
                });
            }
            let mut time = 0.0;
            // Deliberately leave host actions AND client acknowledgements undrained.
            let mut frame = |app: &mut Desktop<'_>, events| {
                time += 0.05;
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
                        time: Some(time),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        egui::CentralPanel::default().show(ui, |ui| {
                            let layout = layout::compute(1400.0, 900.0, 1, 0, false, 0.0, &app.runtime.config.layout);
                            app.tiles(ui, ui.max_rect(), &layout, &[0]);
                        });
                    },
                );
                let text: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|shape| match &shape.shape {
                        egui::epaint::Shape::Text(t) => {
                            Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2()))
                        }
                        _ => None,
                    })
                    .collect();
                output.drop_without_applying_deltas();
                text
            };
            let pos = frame(app, vec![]).into_iter().find(|(text, _)| text == "Allow").unwrap().1;
            for _ in 0..2 {
                for pressed in [true, false] {
                    frame(
                        app,
                        vec![
                            egui::Event::PointerMoved(pos),
                            egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                    );
                }
            }
            let text = frame(app, vec![]);
            assert!(text.iter().any(|(text, _)| text == "Decision pending…"));
            assert!(text.iter().any(|(text, _)| text == "touch reviewed"));
            for label in ["Allow", "Allow always", "Deny"] {
                assert_eq!(
                    ctx.data(|data| data.get_temp::<bool>(egui::Id::unique(("approval-button", label)))),
                    Some(false)
                );
            }
            assert_eq!(app.runtime.sessions[0].approvals.len(), 2);
            // Even a duplicate already in transit must be rejected by the host.
            let (tx, rx) = std::sync::mpsc::channel();
            app.post(move |state| {
                tx.send(state.approve(0, "reviewed", Decision::Allow, None)).unwrap();
            });
            app.runtime.drain_actions();
            assert!(rx.recv().unwrap().unwrap_err().message.contains("stale"));
            assert!(reviewed.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
            assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
            assert_eq!(app.runtime.sessions[0].approvals[0].id, "unreviewed");
            assert!(unreviewed.answer(HookReply::default()), "the unseen request was already decided");
            app.apply_client_events(ctx);
            frame(app, vec![]);
            assert!(app.client.approval_decisions.is_empty());
            assert_eq!(
                ctx.data(|data| data.get_temp::<bool>(egui::Id::unique(("approval-button", "Allow")))),
                Some(true)
            );
        });
    }

    #[test]
    fn permission_banner_buttons_answer_hooks_and_chief_liveness_selects_the_wait() {
        with_app(
            "app::approvals::tests::permission_banner_buttons_answer_hooks_and_chief_liveness_selects_the_wait",
            |app, ctx| {
                let terminal = Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.theme.terminal_colors(),
                    "/bin/sleep".into(),
                    vec!["30".into()],
                    &app.runtime.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                app.runtime.sessions =
                    vec![session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir)];
                app.runtime.sessions[0].terminal = Some(terminal);
                app.runtime.record_events();
                assert!(!app.runtime.hub.lock().unwrap().chief_running);
                let mut time = 0.0;
                let mut frame = |app: &mut Desktop<'_>, events| {
                    time += 0.05;
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
                            time: Some(time),
                            events,
                            ..Default::default()
                        },
                        |ui| {
                            egui::CentralPanel::default().show(ui, |ui| {
                                let layout =
                                    layout::compute(1400.0, 900.0, 1, 0, false, 0.0, &app.runtime.config.layout);
                                app.tiles(ui, ui.max_rect(), &layout, &[0]);
                            });
                        },
                    );
                    app.runtime.drain_actions();
                    let text: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|shape| match &shape.shape {
                            egui::epaint::Shape::Text(t) => {
                                Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2()))
                            }
                            _ => None,
                        })
                        .collect();
                    output.drop_without_applying_deltas();
                    text
                };
                for (label, decision) in
                    [("Allow", Decision::Allow), ("Allow always", Decision::AllowAlways), ("Deny", Decision::Deny)]
                {
                    let reply = Arc::new(ipc::ApprovalReply::default());
                    app.runtime.sessions[0].status = AgentStatus::NeedsInput;
                    app.runtime.sessions[0].approval_hold = true;
                    app.runtime.sessions[0].approvals.push(PendingApproval {
                        id: uuid::Uuid::new_v4().to_string(),
                        request: ApprovalRequest {
                            tool: "Bash".into(),
                            command: Some("touch banner-fixture".into()),
                            cwd: app.runtime.launch_dir.clone(),
                            reason: Some("Verify permission controls".into()),
                            input: json!({}),
                            warning: None,
                        },
                        reply: Some(reply.clone()),
                    });
                    let text = frame(app, vec![]);
                    for expected in ["Permission requested", "touch banner-fixture", "Verify permission controls"] {
                        assert!(text.iter().any(|(text, _)| text == expected), "{expected}");
                    }
                    assert!(text.iter().any(|(text, _)| text.starts_with("cwd: ")));
                    let pos = text.iter().find(|(text, _)| text == label).unwrap().1;
                    for pressed in [true, false] {
                        frame(
                            app,
                            vec![
                                egui::Event::PointerMoved(pos),
                                egui::Event::PointerButton {
                                    pos,
                                    button: egui::PointerButton::Primary,
                                    pressed,
                                    modifiers: egui::Modifiers::NONE,
                                },
                            ],
                        );
                    }
                    let result: serde_json::Value =
                        serde_json::from_str(&reply.wait(Duration::ZERO).stdout.unwrap()).unwrap();
                    assert_eq!(
                        result["hookSpecificOutput"]["decision"]["behavior"],
                        if decision == Decision::Deny { "deny" } else { "allow" }
                    );
                    assert!(app.runtime.sessions[0].approvals.is_empty());
                    assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                    assert!(!frame(app, vec![]).iter().any(|(text, _)| text == "Permission requested"));
                }
                assert_eq!(app.runtime.sessions[0].allowed.len(), 1);
                app.runtime.sessions[0].chief = true;
                app.runtime.record_events();
                assert!(app.runtime.hub.lock().unwrap().chief_running);
                app.runtime.sessions[0].status = AgentStatus::Exited;
                app.runtime.record_events();
                assert!(!app.runtime.hub.lock().unwrap().chief_running);
                app.runtime.sessions[0].status = AgentStatus::Working;
                app.runtime.record_events();
                app.runtime.close("worker");
                assert!(!app.runtime.hub.lock().unwrap().chief_running);
            },
        );
    }

    #[test]
    fn hidden_permission_commands_expand_before_approval_and_running_clears_needs_input() {
        with_app(
            "app::approvals::tests::hidden_permission_commands_expand_before_approval_and_running_clears_needs_input",
            |app, _ctx| {
                let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex");
                app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                    name: "test".into(),
                    command: vec![fixture.to_string_lossy().into_owned()],
                    kind: AgentKind::Generic,
                    busy_text: Some("esc to interrupt".into()),
                    isolation: None,
                });
                let terminal = Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.theme.terminal_colors(),
                    fixture.to_string_lossy().into_owned(),
                    vec!["--mode".into(), "hidden".into()],
                    &app.runtime.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
                while !terminal.screen_text(60).contains("view all") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.runtime.sessions =
                    vec![session("hidden", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir)];
                app.runtime.sessions[0].terminal = Some(terminal);
                app.runtime.sessions[0].hooks_active = true;
                let reply = Arc::new(ipc::ApprovalReply::default());
                let permission = atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":"PermissionRequest", "session_id":"hidden", "tool_name":"Bash",
                    "tool_input":{"command":"touch fixture"}
                }))
                .unwrap();
                app.runtime.sessions[0].approval_hold = true;
                app.runtime.sessions[0].approvals.push(PendingApproval {
                    id: uuid::Uuid::new_v4().to_string(),
                    request: ApprovalRequest::from_event(&permission),
                    reply: Some(reply.clone()),
                });
                assert!(
                    app.runtime
                        .approve(
                            0,
                            &app.runtime.sessions[0]
                                .approvals
                                .first()
                                .map(|pending| pending.id.clone())
                                .unwrap_or_default(),
                            Decision::Allow,
                            None
                        )
                        .unwrap_err()
                        .message
                        .contains("expanded")
                );
                assert!(reply.expired() || reply.wait(Duration::ZERO).stdout.is_none());
                assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
                while !app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("$ touch fixture") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.runtime
                    .approve(
                        0,
                        &app.runtime.sessions[0]
                            .approvals
                            .first()
                            .map(|pending| pending.id.clone())
                            .unwrap_or_default(),
                        Decision::Allow,
                        None,
                    )
                    .unwrap();
                while !app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("Approved") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.runtime.sessions[0].approval_hold = false;
                app.runtime.sessions[0].status = AgentStatus::NeedsInput;
                app.runtime.sessions[0].menu_absent_since = Some(Instant::now() - APPROVAL_MENU_GRACE);
                app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
                app.runtime.needs_input_checked = Instant::now() - NEEDS_INPUT_CHECK_INTERVAL;
                app.runtime.expire_approvals(Instant::now());
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                assert!(app.runtime.sessions[0].approvals.is_empty());
                assert!(!app.runtime.sessions[0].approval_hold);
                let reply = Arc::new(ipc::ApprovalReply::default());
                app.runtime.sessions[0].approvals.push(PendingApproval {
                    id: uuid::Uuid::new_v4().to_string(),
                    request: ApprovalRequest::from_event(&permission),
                    reply: Some(reply.clone()),
                });
                app.runtime.sessions[0].status = AgentStatus::NeedsInput;
                let event = atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":"PreToolUse", "session_id":"hidden", "tool_name":"Bash",
                    "tool_input":{"command":"touch fixture"}
                }))
                .unwrap();
                app.runtime.on_hook(HookMessage {
                    pane: None,
                    ancestors: vec![],
                    event,
                    delivered: 0,
                    approval_reply: None,
                });
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                assert!(app.runtime.sessions[0].approvals.is_empty());
                assert!(reply.wait(Duration::ZERO).stdout.is_none());
            },
        );
    }

    #[test]
    fn expired_approvals_wait_for_menu_quiet_or_lifecycle() {
        with_app("app::approvals::tests::expired_approvals_wait_for_menu_quiet_or_lifecycle", |app, _ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            app.runtime.sessions = vec![session("stale", AgentKind::Generic, AgentStatus::NeedsInput, dir.path())];
            let permission = atomics_core::hook::HookEvent::from_payload(&json!({
                "hook_event_name":"PermissionRequest", "session_id":"stale", "cwd":dir.path(),
                "tool_name":"Bash", "tool_input":{"command":"touch fixture", "description":"test"}
            }))
            .unwrap();
            let reply = Arc::new(ipc::ApprovalReply::default());
            app.runtime.sessions[0].approval_hold = true;
            app.runtime.sessions[0].approvals.push(PendingApproval {
                id: uuid::Uuid::new_v4().to_string(),
                request: ApprovalRequest::from_event(&permission),
                reply: Some(reply.clone()),
            });
            let raw = request(app, json!({"cmd":"send", "session":"stale", "raw":true, "text":"y"}));
            assert!(!raw.ok);
            assert!(raw.error.unwrap().contains("atomics_approve"));
            assert_eq!(app.runtime.deliver(0, "held plain message"), Delivery::Held);
            let now = Instant::now();
            app.runtime.approvals_checked = now - APPROVAL_CHECK_INTERVAL;
            app.runtime.expire_approvals(now);
            assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
            reply.wait(Duration::ZERO);
            let first_check = now + APPROVAL_CHECK_INTERVAL;
            app.runtime.expire_approvals(first_check);
            assert_eq!(app.runtime.sessions[0].approvals.len(), 1, "an expired hook may precede its menu");
            assert_eq!(app.runtime.sessions[0].held, ["held plain message"]);
            let second_check = first_check + APPROVAL_CHECK_INTERVAL;
            app.runtime.expire_approvals(second_check);
            assert_eq!(app.runtime.sessions[0].approvals.len(), 1, "the menu quiet period must elapse");
            app.runtime.sessions[0].menu_absent_since = Some(first_check - APPROVAL_MENU_GRACE);
            app.runtime.expire_approvals(second_check);
            assert_eq!(app.runtime.sessions[0].approvals.len(), 1, "screen checks must be throttled");
            app.runtime.expire_approvals(second_check + APPROVAL_CHECK_INTERVAL);
            assert!(app.runtime.sessions[0].approvals.is_empty());
            assert!(app.runtime.sessions[0].held.is_empty());
            assert_eq!(app.runtime.hub.lock().unwrap().outbox["stale"], ["held plain message"]);
            let menu = "Allow these file edits?\n› 1. Yes, proceed\n  2. No\nPress enter to confirm or esc to cancel";
            assert!(atomics_core::approvals::menu_key(menu, Some("touch fixture"), Decision::Allow).is_none());
            let terminal = Terminal::spawn(
                app.runtime.wake.clone(),
                app.runtime.theme.terminal_colors(),
                "/bin/sh".into(),
                vec!["-c".into(), "printf '%s' \"$1\"; exec sleep 10".into(), "fixture".into(), menu.into()],
                dir.path(),
                HashMap::new(),
                100,
            )
            .unwrap();
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(2));
            while !terminal.screen_text(60).contains("esc to cancel") {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            app.runtime.sessions[0].terminal = Some(terminal);
            app.runtime.sessions[0].status = AgentStatus::NeedsInput;
            app.runtime.sessions[0].approval_hold = true;
            app.runtime.sessions[0].menu_absent_since = Some(Instant::now() - APPROVAL_MENU_GRACE);
            app.runtime.sessions[0].approvals.push(PendingApproval {
                id: uuid::Uuid::new_v4().to_string(),
                request: ApprovalRequest::from_event(&permission),
                reply: Some(reply),
            });
            assert_eq!(app.runtime.deliver(0, "held while editing"), Delivery::Held);
            let before = app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60);
            app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
            app.runtime.expire_approvals(Instant::now());
            assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
            assert!(app.runtime.sessions[0].menu_absent_since.is_none(), "a menu drawn late resets the quiet period");
            assert_eq!(app.runtime.sessions[0].held, ["held while editing"]);
            let event = atomics_core::hook::HookEvent::from_payload(&json!({"hook_event_name":"PreToolUse", "session_id":"stale", "cwd":dir.path(), "tool_name":"Bash", "tool_input":{"command":"cat fixture"}})).unwrap();
            app.runtime.on_hook(HookMessage {
                pane: None,
                ancestors: vec![],
                event,
                delivered: 0,
                approval_reply: None,
            });
            assert_eq!(
                app.runtime.sessions[0].approvals.len(),
                1,
                "an unrelated tool must leave the permission pending"
            );
            assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
            assert!(app.runtime.sessions[0].approval_hold, "PreToolUse alone must not release input into a menu");
            assert_eq!(app.runtime.deliver(0, "still held"), Delivery::Held);
            app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
            app.runtime.expire_approvals(Instant::now());
            assert_eq!(app.runtime.sessions[0].held.len(), 2);
            assert_eq!(app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60), before);
            app.runtime.sessions[0].terminal = None;
            let event = atomics_core::hook::HookEvent::from_payload(
                &json!({"hook_event_name":"Interrupt", "session_id":"stale", "cwd":dir.path()}),
            )
            .unwrap();
            app.runtime.on_hook(HookMessage {
                pane: None,
                ancestors: vec![],
                event,
                delivered: 0,
                approval_reply: None,
            });
            assert!(!app.runtime.sessions[0].approval_hold);
            assert!(app.runtime.sessions[0].held.is_empty());
            assert!(
                app.runtime.hub.lock().unwrap().outbox["stale"]
                    .last()
                    .unwrap()
                    .contains("held while editing\n\nstill held")
            );
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn approvals_and_held_sends_stay_with_their_plan_across_tab_switches() {
        with_app(
            "app::approvals::tests::approvals_and_held_sends_stay_with_their_plan_across_tab_switches",
            |app, _ctx| {
                app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                    name: "test".into(),
                    command: vec![],
                    kind: AgentKind::Generic,
                    busy_text: Some("esc to interrupt".into()),
                    isolation: None,
                });
                let dir = crate::test_support::tempdir().unwrap();
                let mut indices = Vec::new();
                for (goal, id) in [("One", "worker-one"), ("Two", "worker-two")] {
                    assert!(
                        request(app, json!({"cmd":"plan_set", "goal":goal, "tasks":[{"id":"a", "title":goal}]})).ok
                    );
                    let index = app.runtime.selected_plan;
                    app.runtime.plans[index].tasks[0].session = Some(id.into());
                    app.runtime.plans[index].tasks[0].status = TaskStatus::Running;
                    indices.push(index);
                    let mut worker = session(id, AgentKind::Generic, AgentStatus::Working, dir.path());
                    worker.claude_id = Some(format!("codex-{id}"));
                    worker.terminal = Some(
                        Terminal::spawn(
                            app.runtime.wake.clone(),
                            app.runtime.theme.terminal_colors(),
                            "/bin/cat".into(),
                            vec![],
                            dir.path(),
                            HashMap::new(),
                            100,
                        )
                        .unwrap(),
                    );
                    app.runtime.sessions.push(worker);
                }
                let (one, two) = (indices[0], indices[1]);
                let hook = |session: &str, name: &str| {
                    HookMessage {
                pane: Some(session.into()), ancestors: Vec::new(), delivered: 0, approval_reply: None,
                event: atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":name, "session_id":format!("codex-{session}"), "cwd":dir.path(),
                    "tool_name":"Bash", "tool_input":{"command":"touch permission-fixture", "description":format!("Request from {session}")}
                })).unwrap(),
            }
                };
                let permission = |session: &str| {
                    let reply = Arc::new(ipc::ApprovalReply::default());
                    let mut event = hook(session, "PermissionRequest");
                    event.approval_reply = Some(reply.clone());
                    (event, reply)
                };
                app.runtime.record_events();
                let (wait_tx, wait_rx) = channel();
                app.runtime.control(
                    ControlRequest::Wait {
                        sessions: vec!["worker-one".into()],
                        statuses: vec!["needs you".into()],
                        after: Some(app.runtime.event_seq),
                        timeout_secs: Some(30),
                    },
                    wait_tx,
                );
                let (event, second_reply) = permission("worker-two");
                app.runtime.on_hook(event);
                assert!(wait_rx.try_recv().is_err(), "another plan's approval must not satisfy this waiter");
                let (event, first_reply) = permission("worker-one");
                app.runtime.on_hook(event);
                let ControlOutcome::Done(waited) =
                    wait_rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(2))).unwrap()
                else {
                    panic!("wait response")
                };
                let event = &waited.result["events"][0];
                assert_eq!(event["plan"], app.runtime.plans[one].id);
                assert_eq!(event["task"], "a");
                assert_eq!(event["session"], "worker-one");
                assert_eq!(event["approval"]["command"], "touch permission-fixture");
                assert_eq!(event["approval"]["reason"], "Request from worker-one");
                assert_eq!(event["approval"]["cwd"], json!(dir.path()));
                let other = app
                    .runtime
                    .events
                    .iter()
                    .find(|e| e.session == "worker-two" && e.approval.is_some())
                    .unwrap()
                    .to_json();
                assert_eq!(other["plan"], app.runtime.plans[two].id);
                assert_eq!(other["task"], "a");
                assert_eq!(other["approval"]["reason"], "Request from worker-two");
                app.runtime.last_status.remove("worker-one");
                app.runtime.record_events();
                let event = app.runtime.events.last().unwrap().to_json();
                assert_eq!(event["plan"], app.runtime.plans[one].id);
                assert_eq!(event["task"], "a");
                assert_eq!(event["approval"]["command"], "touch permission-fixture");
                for (index, id) in [(0, "worker-one"), (1, "worker-two")] {
                    let response = request(app, json!({"cmd":"send", "session":id, "text":format!("steer-{id}")}));
                    assert!(response.ok);
                    assert_eq!(response.result["held"], true);
                    assert_eq!(response.result["sent"], false);
                    let raw =
                        request(app, json!({"cmd":"send", "session":id, "text":format!("raw-{id}\\r"), "raw":true}));
                    assert!(!raw.ok);
                    assert!(raw.error.unwrap().contains("atomics_approve"));
                    assert_eq!(app.runtime.sessions[index].held.len(), 1);
                    assert!(app.runtime.sessions[index].pending.is_none());
                }
                assert_eq!(app.runtime.selected_plan, two);
                assert!(request(app, json!({"cmd":"approve", "session":"worker-one", "decision":"allow_always", "message":"First plan approved"})).ok);
                let output: serde_json::Value =
                    serde_json::from_str(&first_reply.wait(Duration::ZERO).stdout.unwrap()).unwrap();
                assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "allow");
                assert_eq!(output["hookSpecificOutput"]["decision"]["message"], "First plan approved");
                assert!(app.runtime.sessions[0].approvals.is_empty());
                assert_eq!(app.runtime.sessions[1].approvals.len(), 1);
                assert_eq!(app.runtime.sessions[1].status, AgentStatus::NeedsInput);
                assert_eq!(
                    app.runtime.sessions[0].held.len(),
                    1,
                    "approval alone must not flush sends into the permission menu"
                );
                app.runtime.on_hook(hook("worker-one", "PostToolUse"));
                assert!(app.runtime.sessions[0].held.is_empty());
                assert_eq!(app.runtime.sessions[0].pending.as_ref().unwrap().text, "steer-worker-one");
                assert_eq!(app.runtime.sessions[1].held.len(), 1);
                assert!(app.runtime.sessions[1].pending.is_none());
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(2));
                while !app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(20).contains("steer-worker-one") {
                    assert!(Instant::now() < deadline, "first plan's plain message was not released");
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(!app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(20).contains("raw-worker-one"));
                assert!(!app.runtime.sessions[1].terminal.as_ref().unwrap().screen_text(20).contains("worker-one"));
                app.runtime.selected_plan = one;
                app.runtime.show_plan_fleet();
                assert!(
                request(
                    app,
                    json!({"cmd":"approve", "session":"worker-two", "decision":"deny", "message":"Second plan denied"})
                )
                .ok
            );
                let output: serde_json::Value =
                    serde_json::from_str(&second_reply.wait(Duration::ZERO).stdout.unwrap()).unwrap();
                assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "deny");
                assert_eq!(output["hookSpecificOutput"]["decision"]["message"], "Second plan denied");
                assert_eq!(app.runtime.sessions[1].held.len(), 1);
                app.runtime.on_hook(hook("worker-two", "Interrupt"));
                assert!(app.runtime.sessions[1].held.is_empty());
                assert_eq!(app.runtime.sessions[1].pending.as_ref().unwrap().text, "steer-worker-two");
                assert_eq!(app.runtime.sessions[0].pending.as_ref().unwrap().text, "steer-worker-one");
                let (event, reply) = permission("worker-two");
                app.runtime.on_hook(event);
                assert_eq!(
                    app.runtime.sessions[1].approvals.len(),
                    1,
                    "allow_always must not leak to another plan's session"
                );
                assert!(request(app, json!({"cmd":"approve", "session":"worker-two", "decision":"allow"})).ok);
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                let (event, reply) = permission("worker-one");
                app.runtime.on_hook(event);
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                assert!(app.runtime.sessions[0].approvals.is_empty());
                assert_eq!(app.runtime.plans[one].tasks[0].status, TaskStatus::Running);
                assert_eq!(app.runtime.plans[two].tasks[0].status, TaskStatus::Running);
            },
        );
    }

    #[test]
    fn the_banner_shows_a_stash_warning_and_offers_no_allow_always() {
        with_app("app::approvals::tests::the_banner_shows_a_stash_warning_and_offers_no_allow_always", |app, ctx| {
            let mut worker = session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir);
            let warning = "stash@{1} (On atomics/t2: t2), which belongs to task t2 (session worker-two)";
            worker.approvals.push(PendingApproval {
                id: "pending".into(),
                request: ApprovalRequest {
                    tool: "Bash".into(),
                    command: Some("git stash pop 1".into()),
                    cwd: app.runtime.launch_dir.clone(),
                    reason: None,
                    input: json!({}),
                    warning: Some(warning.into()),
                },
                reply: Some(Arc::new(ipc::ApprovalReply::default())),
            });
            app.runtime.sessions = vec![worker];
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
                    ..Default::default()
                },
                |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let layout = layout::compute(1400.0, 900.0, 1, 0, false, 0.0, &app.runtime.config.layout);
                        app.tiles(ui, ui.max_rect(), &layout, &[0]);
                    });
                },
            );
            let text: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Text(text) => Some(text.galley.text().to_string()),
                    _ => None,
                })
                .collect();
            output.drop_without_applying_deltas();
            for label in ["Permission requested", "Allow", "Deny", warning, "git stash pop 1"] {
                assert!(text.iter().any(|text| text == label), "{label}: {text:?}");
            }
            assert!(!text.iter().any(|text| text == "Allow always"), "{text:?}");
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn a_stash_drop_waits_names_the_entrys_owner_and_is_never_allowed_always() {
        with_app(
            "app::approvals::tests::a_stash_drop_waits_names_the_entrys_owner_and_is_never_allowed_always",
            |app, _| {
                let dir = crate::test_support::tempdir().unwrap();
                let repo = dir.path().canonicalize().unwrap().join("app");
                std::fs::create_dir(&repo).unwrap();
                let git = |dir: &Path, args: &[&str]| git::git(dir, args).unwrap();
                git(&repo, &["init", "-q", "-b", "main"]);
                git(&repo, &["config", "user.name", "test"]);
                git(&repo, &["config", "user.email", "test@example.com"]);
                std::fs::write(repo.join("a.txt"), "base\n").unwrap();
                git(&repo, &["add", "a.txt"]);
                git(&repo, &["commit", "-q", "-m", "base"]);
                let (one, two) = (repo.with_file_name("one"), repo.with_file_name("two"));
                git::add_worktree(&repo, &two, "atomics/two", "main").unwrap();
                git::add_worktree(&repo, &one, "atomics/one", "main").unwrap();
                // Oldest first: task t2's entry, then task t1's, then one made in the main checkout.
                for (checkout, message) in [(&two, Some("t2")), (&one, Some("t1 before rebase")), (&repo, None)] {
                    std::fs::write(checkout.join("a.txt"), format!("{message:?}\n")).unwrap();
                    match message {
                        Some(message) => git(checkout, &["stash", "push", "-q", "-m", message]),
                        None => git(checkout, &["stash", "push", "-q"]),
                    };
                }
                app.runtime.config.approvals.allow = vec!["git".into()];
                app.runtime.sessions = ["one", "two"]
                    .into_iter()
                    .map(|name| {
                        let root = repo.with_file_name(name);
                        let mut worker =
                            session(&format!("worker-{name}"), AgentKind::Generic, AgentStatus::Working, &root);
                        worker.worktree = Some(Worktree { root, branch: Some(format!("atomics/{name}")) });
                        worker
                    })
                    .collect();
                let task =
                    |id: &str, session: &str| Task { id: id.into(), session: Some(session.into()), ..Task::default() };
                app.runtime.plans =
                    vec![Plan { tasks: vec![task("t1", "worker-one"), task("t2", "worker-two")], ..Plan::default() }];
                let permission = |command: &str| {
                    let reply = Arc::new(ipc::ApprovalReply::default());
                    let message = HookMessage {
                        pane: Some("worker-one".into()),
                        ancestors: vec![],
                        delivered: 0,
                        approval_reply: Some(reply.clone()),
                        event: atomics_core::hook::HookEvent::from_payload(&json!({
                            "hook_event_name":"PermissionRequest", "session_id":"worker-one", "cwd":one,
                            "tool_name":"Bash", "tool_input":{"command":command}
                        }))
                        .unwrap(),
                    };
                    (message, reply)
                };

                // Allowed by the list, a drop still waits, and says the entry is task t2's.
                let (message, reply) = permission("git stash drop stash@{2}");
                app.runtime.on_hook(message);
                assert!(!reply.expired(), "the drop must wait for a decision");
                let pending = &app.runtime.sessions[0].approvals[0];
                let warning = pending.request.warning.clone().unwrap();
                assert!(warning.starts_with(atomics_core::stash::SHARED), "{warning}");
                assert!(
                warning.ends_with(
                    "git stash drop drops stash@{2} (On atomics/two: t2), which belongs to task t2 (session worker-two), \
                     not to this agent."
                ),
                "{warning}"
            );
                // The chief's event and every client see it too.
                let event = app.runtime.events.iter().rev().find(|event| event.session == "worker-one").unwrap();
                assert_eq!(event.approval.as_ref().unwrap().warning.as_ref(), Some(&warning));
                assert_eq!(app.runtime.approval_infos()[0].warning.as_ref(), Some(&warning));

                // "Allow always" answers it once, and is not remembered.
                let id = pending.id.clone();
                assert_eq!(app.runtime.decide(&id, Decision::AllowAlways, None).unwrap(), Disposition::Allowed);
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                assert!(app.runtime.sessions[0].allowed.is_empty());
                let (message, reply) = permission("git stash drop stash@{2}");
                app.runtime.on_hook(message);
                assert!(!reply.expired() && app.runtime.sessions[0].approvals.len() == 1, "asked again");
                let id = app.runtime.sessions[0].approvals[0].id.clone();
                app.runtime.decide(&id, Decision::Deny, None).unwrap();

                // A clear names every entry's owner: the main checkout's no session accounts for.
                let (message, reply) = permission("git stash list && git stash clear");
                app.runtime.on_hook(message);
                let warning = app.runtime.sessions[0].approvals[0].request.warning.clone().unwrap();
                assert!(warning.contains("git stash clear drops all 3 entries: stash@{0} (WIP on main: "), "{warning}");
                assert!(
                warning.contains(
                    "), whose owner ATOMICS cannot tell; stash@{1} (On atomics/one: t1 before rebase), this agent's own; \
                     stash@{2} (On atomics/two: t2), which belongs to task t2 (session worker-two), not to this agent."
                ),
                "{warning}"
            );
                let id = app.runtime.sessions[0].approvals[0].id.clone();
                app.runtime.decide(&id, Decision::Deny, None).unwrap();
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("deny"));

                // Other commands carry no warning, and the allow list answers them.
                let (message, reply) = permission("git stash list");
                app.runtime.on_hook(message);
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                assert!(app.runtime.sessions[0].approvals.is_empty());
                app.runtime.sessions.clear();
                app.runtime.plans.clear();
            },
        );
    }
}
