//! Host-owned settings, device management and filesystem browsing. No client filesystem APIs.
use super::*;
use crate::protocol::rendezvous::devices::{self, PairingView};
use atomics_core::ops::HostAction;
use atomics_core::projection::{ConfigProposalInfo, DeviceInfo, FolderInfo, ManagementInfo, PairingInfo, RemoteInfo};

impl State {
    pub(super) fn management_info(&self) -> ManagementInfo {
        let mut config = self.management_config.get_or_init(|| self.config_get(None, None).unwrap_or_default()).clone();
        config["theme_tokens"] = serde_json::to_value(&self.theme.tokens).unwrap_or_default();
        let proposals = self
            .proposals
            .iter()
            .take(1)
            .map(|pending| {
                let proposal = &pending.proposal;
                let mut layers = self.config_layers(None);
                // Include proposed secrets as well as the current ones when masking diff context.
                layers.push(atomics_core::selfconfig::Layer::new(
                    atomics_core::selfconfig::Source::Main,
                    &proposal.file,
                    Some(proposal.after.clone()),
                ));
                let cleaner = self.config_cleaner(&layers);
                ConfigProposalInfo {
                    id: pending.id.clone(),
                    file: proposal.file.clone(),
                    scope: proposal.scope.name().into(),
                    from: cleaner.text(&pending.from),
                    reason: pending.reason.as_deref().map(|s| cleaner.text(s)),
                    diff: atomics_core::selfconfig::public_diff(proposal, &cleaner),
                    risks: proposal
                        .risks()
                        .into_iter()
                        .map(|(risk, keys)| format!("⚠ {}: {}", risk.label(), keys.join(", ")))
                        .collect(),
                    warnings: proposal.warnings.iter().map(|s| cleaner.text(s)).collect(),
                    preview: pending
                        .preview
                        .as_ref()
                        .and_then(|theme| serde_json::to_value(&theme.tokens).ok())
                        .map(|value| cleaner.setting(value)),
                }
            })
            .collect();
        let mut remote = RemoteInfo {
            enabled: self.remote.enabled,
            status: self.rendezvous_status().map(|s| format!("{:?}", s.phase)).unwrap_or_else(|| {
                self.remote.addr.map(|addr| format!("Listening on {addr}")).unwrap_or_else(|| "Disabled".into())
            }),
            problems: self.remote.error.iter().cloned().chain(self.journal_problem()).collect(),
            ..Default::default()
        };
        if let Some(devices) = self.remote_devices() {
            remote.problems.extend(devices.problem());
            remote.pairing = Some(match devices.view() {
                PairingView::Idle { note } => PairingInfo::Idle { note: note.or_else(|| devices.unready()) },
                PairingView::Open { link, left, busy } => {
                    PairingInfo::Open { link: Some(link.to_string()), seconds_left: left.as_secs(), busy }
                }
                PairingView::Confirm { name, code, left } => {
                    PairingInfo::Confirm { name, code, seconds_left: left.as_secs() }
                }
            });
            remote.devices = devices
                .list()
                .into_iter()
                .map(|(d, connected)| DeviceInfo {
                    id: devices::digest(&d.key),
                    name: d.name,
                    controls: d.access == atomics_core::noise::Access::Control,
                    connected,
                    seen_ms: d.seen_ms,
                })
                .collect();
        }
        ManagementInfo {
            config,
            proposals,
            folder_start: paths::home().to_string_lossy().into_owned(),
            folders: self.folder_cache.clone(),
            recent: self.store.recent_projects.iter().take(8).map(|p| p.to_string_lossy().into_owned()).collect(),
            tools: json!({
                "hooks": global_hooks_installed(),
                "sandbox": self.sandbox_lines,
                "sandbox_busy": self.sandbox_check.is_some(),
                "connections": self.connections.management_info(),
                "credentials": self.credential_status,
                "credential_busy": self.credential_busy,
                "credential_kinds": crate::credentials::KINDS.iter().map(|(id, label, _)| json!({"id":id, "label":label})).collect::<Vec<_>>(),
            }),
            remote,
        }
    }

    pub(super) fn manage(&mut self, action: HostAction) -> Result<serde_json::Value, WireError> {
        let rejected = |s: String| WireError::new(ErrorCode::Rejected, s);
        match action {
            HostAction::SettingsTool { tool } => return self.settings_tool(tool),
            HostAction::BrowseFolder { path } => return self.manage(HostAction::BrowseFolderPage { path, offset: 0 }),
            HostAction::BrowseFolderPage { path, offset } => {
                let folder = self.browse_folder(&path, offset).map_err(rejected)?;
                self.folder_cache.retain(|f| f.page.worktree != folder.page.worktree || f.page.offset != offset);
                if self.folder_cache.len() >= 8 {
                    self.folder_cache.remove(0);
                }
                self.folder_cache.push(folder);
            }
            HostAction::OpenProject { path } => {
                let root = workspace::project_root(&paths::expand(&path)).map_err(|e| rejected(format!("{e:#}")))?;
                self.open_project(&root);
                if let Some(worktree) = git::worktree_of(&paths::expand(&path)) {
                    self.diff_root = Some(worktree.root);
                }
            }
            HostAction::CloseProject { path } => {
                let root = PathBuf::from(path);
                if !self.open_projects.contains(&root) {
                    return Err(WireError::new(ErrorCode::NotFound, "project is not open"));
                }
                self.close_project(&root);
            }
            HostAction::SaveWorkspace { name } => {
                // Validate before the existing save flow, which reports failures through the notice line.
                Workspace::file(&name).map_err(|e| rejected(format!("{e:#}")))?;
                self.save_workspace_checked(&name).map_err(rejected)?;
            }
            HostAction::ConfigPropose { scope, toml, remove, theme, reason } => {
                return self
                    .propose_config(
                        super::selfconfig::ProposedChange {
                            scope,
                            toml: &toml,
                            remove: &remove,
                            theme: theme.as_deref(),
                        },
                        reason,
                        None,
                    )
                    .map_err(rejected);
            }
            HostAction::ConfigDecide { id, apply, note } => {
                if !self.proposals.iter().any(|p| p.id == id) {
                    return Err(WireError::new(ErrorCode::NotFound, "proposal is no longer pending"));
                }
                if apply {
                    self.apply_proposal(&id, note);
                } else {
                    self.reject_proposal(&id, note);
                }
                if let Some(config) = self.events.last().and_then(|event| event.config.as_ref())
                    && config["outcome"] == "failed"
                {
                    return Err(rejected(
                        self.config_cleaner(&self.config_layers(None))
                            .text(config["error"].as_str().unwrap_or("Could not apply configuration")),
                    ));
                }
            }
            action => {
                let devices = self
                    .remote_devices()
                    .ok_or_else(|| rejected("Device pairing is not available on this host".into()))?;
                match action {
                    HostAction::PairDevice => {
                        devices.offer(&self.remote.pair_url).map_err(|e| rejected(e.to_string()))?
                    }
                    HostAction::CancelPairing => devices.cancel(),
                    HostAction::DecidePairing { code, controls } => {
                        if !devices.decide_code(
                            &code,
                            controls.map(|control| {
                                if control {
                                    atomics_core::noise::Access::Control
                                } else {
                                    atomics_core::noise::Access::View
                                }
                            }),
                        ) {
                            return Err(WireError::new(
                                ErrorCode::StaleRevision,
                                "pairing changed; compare the current code",
                            ));
                        }
                    }
                    HostAction::RevokeDevice { id } => {
                        let key = devices
                            .list()
                            .into_iter()
                            .find(|(d, _)| devices::digest(&d.key) == id)
                            .map(|(d, _)| d.key)
                            .ok_or_else(|| WireError::new(ErrorCode::NotFound, "device is no longer paired"))?;
                        devices.revoke(&key);
                    }
                    _ => unreachable!("management action"),
                }
            }
        }
        Ok(json!({ "done": true }))
    }

    fn browse_folder(&self, path: &str, offset: u32) -> Result<FolderInfo, String> {
        let folder = paths::expand(path).canonicalize().map_err(|e| format!("Cannot open host folder: {e}"))?;
        let entries = std::fs::read_dir(&folder).map_err(|e| format!("Cannot read host folder: {e}"))?;
        // Unlike a prepared worktree listing, this reads the host filesystem. Bound that read too.
        const MAX_ENTRIES: usize = 16_384;
        let mut directories = Vec::new();
        for (index, entry) in entries.enumerate() {
            if index >= MAX_ENTRIES {
                return Err("Host folder has too many entries to browse".into());
            }
            let entry = entry.map_err(|e| format!("Cannot read host folder: {e}"))?;
            if entry.path().is_dir() {
                directories.push(atomics_core::folder::Entry {
                    path: entry.file_name().to_string_lossy().into_owned(),
                    directory: true,
                });
            }
        }
        let page = atomics_core::folder::entries_page(directories, &folder.to_string_lossy(), "", 0, offset, 8192)
            .map_err(|error| error.message)?;
        Ok(FolderInfo {
            requested: path.into(),
            parent: folder.parent().map(|p| p.to_string_lossy().into_owned()),
            project: workspace::project_root(&folder).is_ok(),
            worktree: git::worktree_of(&folder).map(|w| w.root.to_string_lossy().into_owned()),
            page,
        })
    }
}

impl State {
    fn settings_tool(&mut self, tool: atomics_core::ops::SettingsTool) -> Result<serde_json::Value, WireError> {
        use atomics_core::ops::SettingsTool;
        match tool {
            SettingsTool::CheckSandbox => self.check_sandbox(),
            SettingsTool::RefreshConnections => self.refresh_connections(),
            SettingsTool::ImportConnections => self.import_connections(),
            SettingsTool::Connect { name, store_env } => self.connect_server(&name, store_env, None),
            SettingsTool::Disconnect { name } => self.disconnect_server(&name),
            SettingsTool::AddConnection { name, url } => {
                self.add_connection(&name, &url).map_err(|e| WireError::new(ErrorCode::Rejected, e))?;
            }
            SettingsTool::InstallHooks => {
                let actions = self.action_tx.clone();
                std::thread::spawn(move || {
                    let message = match install_global_hooks() {
                        Ok(_) => "Hooks installed".to_string(),
                        Err(error) => format!("Installing hooks failed: {error:#}"),
                    };
                    let _ = actions.send(Box::new(move |state| state.toast(message)));
                });
            }
            tool => {
                if self.credential_busy {
                    return Err(WireError::new(ErrorCode::Conflict, "A credential operation is already running"));
                }
                let kind = match &tool {
                    SettingsTool::PutCredential { kind, .. } | SettingsTool::RemoveCredential { kind, .. } => {
                        Some(kind)
                    }
                    _ => None,
                };
                if kind.is_some_and(|id| !crate::credentials::KINDS.iter().any(|(known, _, _)| known == id)) {
                    return Err(WireError::new(ErrorCode::Malformed, "Unknown credential kind"));
                }
                self.credential_busy = true;
                let (relay, actions) = (self.relay.clone(), self.action_tx.clone());
                std::thread::spawn(move || {
                    let result = credential_tool(&relay, tool);
                    let _ = actions.send(Box::new(move |state| {
                        state.credential_busy = false;
                        state.credential_status = result.unwrap_or_else(|error| json!({ "lines": [error] }));
                    }));
                });
            }
        }
        Ok(json!({ "accepted": true }))
    }
}

fn credential_tool(
    relay: &crate::relay::Service,
    tool: atomics_core::ops::SettingsTool,
) -> Result<serde_json::Value, String> {
    use atomics_core::ops::SettingsTool;
    let store = crate::credentials::Store::new(relay.keychain().clone());
    match tool {
        SettingsTool::RefreshCredentials => {}
        SettingsTool::PutCredential { kind, value, host } => {
            let label = if kind == "registry" {
                format!("Private registry ({})", host.as_deref().unwrap_or_default())
            } else {
                crate::credentials::KINDS.iter().find(|(id, _, _)| *id == kind).unwrap().1.into()
            };
            relay
                .edit(&label, |store| store.put(&kind, &value, host.as_deref()))
                .map_err(|_| "Could not store credential".to_string())?;
        }
        SettingsTool::RemoveCredential { kind, host } => {
            let key = match kind.as_str() {
                "registry" => format!("registry:{}", host.as_deref().unwrap_or_default()),
                crate::credentials::GITHUB_APP => {
                    crate::credentials::github_app_id(host.as_deref().unwrap_or_default())
                }
                _ => kind.clone(),
            };
            let label = if kind == "registry" {
                format!("Private registry ({})", host.as_deref().unwrap_or_default())
            } else {
                crate::credentials::KINDS.iter().find(|(id, _, _)| *id == kind).unwrap().1.into()
            };
            relay.edit(&label, |store| store.remove(&key)).map_err(|_| "Could not remove credential".to_string())?;
        }
        SettingsTool::CleanCredentials => {
            store.remove_invalid().map_err(|_| "Could not clean credentials".to_string())?
        }
        _ => unreachable!("credential tool"),
    }
    let snapshot = store.snapshot().map_err(|_| "Cannot read OS credential store".to_string())?;
    let mut lines = snapshot.lines();
    for entry in &snapshot.entries {
        let (last, sessions) = relay.usage(&entry.label);
        lines.push(format!(
            "{} — last used: {}; sessions: {}",
            entry.label,
            last.map(|s| crate::relay::utc(std::time::UNIX_EPOCH + Duration::from_secs(s)))
                .unwrap_or_else(|| "never".into()),
            if sessions.is_empty() { "none".into() } else { sessions.join(", ") }
        ));
    }
    let status = crate::relay::azure::status(if cfg!(test) { None } else { crate::setup::find_program("az") });
    Ok(
        json!({ "lines": lines, "invalid": snapshot.invalid, "azure": format!("az found: {}; logged in: {}", status.found, status.logged_in) }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{repository, with_state};

    #[test]
    fn host_projects_and_proposals_are_visible_and_only_apply_writes() {
        with_state(
            "app::management::tests::host_projects_and_proposals_are_visible_and_only_apply_writes",
            |mut state, _| {
                let (_dir, repo) = repository();
                let repo = repo.canonicalize().unwrap();
                let path = repo.to_string_lossy().into_owned();
                state.manage(HostAction::BrowseFolder { path: path.clone() }).unwrap();
                let info = state.management_info();
                assert!(info.folders.iter().any(|f| f.project && f.worktree.as_deref() == Some(path.as_str())));
                state.manage(HostAction::OpenProject { path }).unwrap();
                assert!(state.host_info().projects.iter().any(|p| Path::new(&p.root) == repo));
                let before = std::fs::read_to_string(&state.config_path).unwrap_or_default();
                let result = state
                    .manage(HostAction::ConfigPropose {
                        scope: atomics_core::selfconfig::Scope::Main,
                        toml: "[layout]\nmin_tile_width = 407.0\n[voice]\napi_key = 'hidden-test-value'".into(),
                        remove: vec![],
                        theme: None,
                        reason: None,
                    })
                    .unwrap();
                let id = result["proposal"].as_str().unwrap().to_string();
                assert_eq!(std::fs::read_to_string(&state.config_path).unwrap_or_default(), before);
                let shown = serde_json::to_string(&state.management_info()).unwrap();
                assert!(!shown.contains("hidden-test-value"), "the config and diff must mask secrets: {shown}");
                state.manage(HostAction::ConfigDecide { id: id.clone(), apply: true, note: String::new() }).unwrap();
                assert_eq!(state.config.layout.min_tile_width, 407.0);
                assert!(state.management_info().proposals.is_empty());
                assert!(state.manage(HostAction::ConfigDecide { id, apply: true, note: String::new() }).is_err());
                assert!(state.manage(HostAction::BrowseFolder { path: "/missing/host/folder".into() }).is_err());
            },
        );
    }

    #[test]
    fn host_folder_pages_are_bounded_and_reach_every_child() {
        with_state("app::management::tests::host_folder_pages_are_bounded_and_reach_every_child", |mut state, _| {
            let folder = paths::home().join("many-folders");
            std::fs::create_dir_all(&folder).unwrap();
            for number in 0..300 {
                std::fs::create_dir(folder.join(format!("project-{number:04}"))).unwrap();
            }
            let path = folder.to_string_lossy().into_owned();
            let mut offset = 0;
            let mut seen = std::collections::BTreeSet::new();
            loop {
                state.manage(HostAction::BrowseFolderPage { path: path.clone(), offset }).unwrap();
                let page = state.folder_cache.last().unwrap();
                assert!(atomics_core::wire::json_len(&page.page) <= 8192);
                seen.extend(page.page.entries.iter().map(|e| e.path.clone()));
                let Some(next) = page.page.next_offset else { break };
                assert!(next > offset);
                offset = next;
            }
            assert_eq!(seen.len(), 300);
        });
    }

    #[test]
    fn host_credential_tools_never_return_stored_or_new_values() {
        use atomics_core::ops::SettingsTool;
        for mac in [true, false] {
            let (dir, keychain, store) = crate::credentials::tests::machine(mac);
            let relay = crate::relay::Service::transient(dir.path().join("state"), keychain);
            let value = "sk-ant-oat01-PRIVATE-NEW-VALUE";
            let status = credential_tool(
                &relay,
                SettingsTool::PutCredential { kind: "claude".into(), value: value.into(), host: None },
            )
            .unwrap();
            assert_eq!(store.token("claude").unwrap().as_deref(), Some(value));
            assert!(!status.to_string().contains(value));
            assert!(status.to_string().contains("Claude subscription: present"));
            credential_tool(&relay, SettingsTool::RemoveCredential { kind: "claude".into(), host: None }).unwrap();
            assert!(store.token("claude").unwrap().is_none());
        }
    }

    #[test]
    fn host_pairing_refuses_stale_codes_and_unavailable_devices() {
        with_state(
            "app::management::tests::host_pairing_refuses_stale_codes_and_unavailable_devices",
            |mut state, _| {
                assert!(state.manage(HostAction::PairDevice).is_err());
                let devices = devices::Devices::open(paths::atomics_home().join("devices.json"), || {});
                state.remote.devices = Some(devices);
                assert_eq!(
                    state
                        .manage(HostAction::DecidePairing { code: "123456".into(), controls: Some(true) })
                        .unwrap_err()
                        .code,
                    ErrorCode::StaleRevision
                );
                assert_eq!(
                    state.manage(HostAction::RevokeDevice { id: "missing".into() }).unwrap_err().code,
                    ErrorCode::NotFound
                );
            },
        );
    }
}
