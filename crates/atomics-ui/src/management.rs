//! Settings, remote devices and the host's folder picker, shared by every client.
use crate::{Backend, Client, Command};
use atomics_core::ops::HostAction;
use atomics_core::projection::{ManagementInfo, PairingInfo};
use atomics_core::selfconfig::Scope;
use egui::RichText;

mod remote;

#[derive(Default)]
pub struct State {
    pub settings: bool,
    pub remote: bool,
    pub project: bool,
    pub folder: Option<String>,
    pub folder_page: u32,
    pub filter: String,
    pub toml: String,
    pub remove: String,
    pub reason: String,
    pub theme: String,
    pub connection_name: String,
    pub connection_url: String,
    pub credential_kind: String,
    pub credential_host: String,
    pub credential_value: String,
    pub scope: Option<Scope>,
    pub revoke: Option<String>,
    pub proposal_seen: Option<(String, f64)>,
    pub proposal_note: String,
}

fn allowed(backend: &dyn Backend, action: &HostAction) -> bool {
    backend.available(&Command::Manage(action.clone()))
}

pub fn open_project(backend: &dyn Backend, client: &mut Client) {
    let Some(info) = backend.management() else { return };
    client.management.project = true;
    client.management.folder_page = 0;
    let path = client.management.folder.get_or_insert(info.folder_start).clone();
    client.send(Command::Manage(HostAction::BrowseFolder { path }));
}

pub fn buttons(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let Some(info) = backend.management() else { return };
    let remote = ui.button(if info.remote.problems.is_empty() { "Remote" } else { "Remote ●" });
    crate::topbar::record_item(ui, "Remote", &remote);
    if remote.clicked() {
        client.management.remote = !client.management.remote;
        ui.close();
    }
    let settings = ui.button("Settings");
    crate::topbar::record_item(ui, "Settings", &settings);
    if settings.clicked() {
        client.management.settings = !client.management.settings;
        ui.close();
    }
}

pub fn dialogs(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client) {
    let Some(info) = backend.management() else { return };
    let width = (ctx.content_rect().width() - 24.0).clamp(160.0, 780.0);
    if client.management.settings {
        let mut open = true;
        egui::Window::new("Settings")
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .default_width(width)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(ctx.content_rect().height() * 0.7)
                    .show(ui, |ui| settings(ui, backend, client, &info));
            });
        client.management.settings = open;
    }
    if client.management.remote {
        let mut open = true;
        egui::Window::new("Remote devices")
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .default_width(width.min(440.0))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(ctx.content_rect().height() * 0.75)
                    .show(ui, |ui| remote::draw(ui, backend, client, &info));
            });
        client.management.remote = open;
    }
    if client.management.project {
        let mut open = true;
        egui::Window::new("Open project on host")
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .default_width(width.min(600.0))
            .show(ctx, |ui| folder(ui, backend, client, &info));
        client.management.project &= open;
    }
    proposal(ctx, backend, client, &info);
}

fn settings(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    ui.heading("Effective configuration");
    ui.label("Changes are proposed first. Review the diff, then Apply to save them on the host.");
    if let Some(problem) = info.config["load_error"].as_str() {
        ui.colored_label(client.palette.warning, problem);
    }
    ui.add(egui::TextEdit::singleline(&mut client.management.filter).hint_text("Find a setting"));
    if let Some(values) = info.config["values"].as_object() {
        egui::ScrollArea::vertical().id_salt("effective-settings").max_height(230.0).show(ui, |ui| {
            for (key, entry) in values {
                if !key.to_lowercase().contains(&client.management.filter.to_lowercase()) {
                    continue;
                }
                ui.label(
                    RichText::new(format!(
                        "{key} = {}  ({})",
                        entry["value"],
                        entry["source"].as_str().unwrap_or("default")
                    ))
                    .monospace(),
                );
            }
        });
    }
    ui.separator();
    ui.heading("Propose a change");
    let enabled = allowed(
        backend,
        &HostAction::ConfigPropose {
            scope: Scope::Main,
            toml: String::new(),
            remove: vec![],
            theme: None,
            reason: None,
        },
    );
    if !enabled {
        ui.label("View only: settings changes require the host grant.");
    }
    ui.add_enabled_ui(enabled, |ui| {
        let state = &mut client.management;
        let mut scope = state.scope.unwrap_or(Scope::Main);
        egui::ComboBox::from_label("Save to").selected_text(scope.name()).show_ui(ui, |ui| {
            for target in [Scope::Main, Scope::Host, Scope::Workspace, Scope::Theme] {
                ui.selectable_value(&mut scope, target, target.name());
            }
        });
        state.scope = Some(scope);
        if scope == Scope::Theme {
            ui.add(egui::TextEdit::singleline(&mut state.theme).hint_text("Theme name"));
        }
        ui.add(
            egui::TextEdit::multiline(&mut state.toml)
                .code_editor()
                .desired_rows(5)
                .desired_width(ui.available_width())
                .hint_text("[layout]\nmin_tile_width = 400.0"),
        );
        ui.add(egui::TextEdit::singleline(&mut state.remove).hint_text("Keys to remove, separated by commas"));
        ui.add(egui::TextEdit::singleline(&mut state.reason).hint_text("Reason (optional)"));
        if ui
            .add_enabled(
                !state.toml.trim().is_empty() || !state.remove.trim().is_empty(),
                egui::Button::new("Propose change"),
            )
            .clicked()
        {
            let action = HostAction::ConfigPropose {
                scope,
                toml: state.toml.clone(),
                remove: state.remove.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
                theme: (scope == Scope::Theme).then(|| state.theme.clone()),
                reason: Some(state.reason.clone()),
            };
            client.send(Command::Manage(action));
        }
    });
    tools(ui, backend, client, info);
    // The VM panel can be inserted as another Settings section without changing the shell.
}

fn folder(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    ui.label("Choose a folder on the host.");
    let path = client.management.folder.clone().unwrap_or_else(|| info.folder_start.clone());
    ui.label(RichText::new(&path).monospace());
    let enabled = allowed(backend, &HostAction::BrowseFolder { path: path.clone() });
    ui.add_enabled_ui(enabled, |ui| {
        let found = info.folders.iter().find(|f| {
            (f.page.worktree == path || f.requested == path) && f.page.offset == client.management.folder_page
        });
        let mut browse = None;
        let mut next_page = None;
        ui.horizontal(|ui| {
            if ui.button("Home").clicked() {
                browse = Some(info.folder_start.clone());
            }
            if ui.button("Refresh").clicked() {
                browse = Some(path.clone());
            }
        });
        if let Some(folder) = found {
            client.management.folder = Some(folder.page.worktree.clone());
            if let Some(parent) = &folder.parent
                && ui.button("↑ Parent folder").clicked()
            {
                browse = Some(parent.clone());
            }
            if ui.add_enabled(folder.project, egui::Button::new("Open this project or worktree")).clicked() {
                client.send(Command::OpenProject(folder.worktree.as_deref().unwrap_or(&path).into()));
                client.management.project = false;
            }
            ui.horizontal(|ui| {
                if let Some(next) = folder.page.next_offset
                    && ui.button("Next folders").clicked()
                {
                    next_page = Some(next);
                }
                if folder.page.offset > 0 && ui.button("First folders").clicked() {
                    next_page = Some(0);
                }
            });
            egui::ScrollArea::vertical().max_height(ui.ctx().content_rect().height() * 0.55).show(ui, |ui| {
                for entry in &folder.page.entries {
                    let child = format!("{}/{}", folder.page.worktree.trim_end_matches('/'), entry.path);
                    if ui.button(format!("▸ {}", entry.path)).clicked() {
                        browse = Some(child);
                    }
                }
            });
        } else {
            ui.label("Waiting for the host folder listing…");
        }
        if let Some(offset) = next_page {
            client.management.folder_page = offset;
            client.send(Command::Manage(HostAction::BrowseFolderPage { path: path.clone(), offset }));
        }
        if let Some(path) = browse {
            client.management.folder_page = 0;
            client.management.folder = Some(path.clone());
            client.send(Command::Manage(HostAction::BrowseFolder { path }));
        }
    });
}

fn proposal(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    let Some(pending) = info.proposals.first() else {
        client.management.proposal_seen = None;
        return;
    };
    let now = ctx.input(|i| i.time);
    if client.management.proposal_seen.as_ref().is_none_or(|(id, _)| id != &pending.id) {
        client.management.proposal_seen = Some((pending.id.clone(), now));
        client.management.proposal_note.clear();
    }
    let ready = now - client.management.proposal_seen.as_ref().unwrap().1 >= 0.8;
    if !ready {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    let enabled =
        backend.available(&Command::DecideProposal { id: pending.id.clone(), apply: true, note: String::new() });
    egui::Window::new("Proposed config change")
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .id(egui::Id::NULL.with("config-proposal"))
        .default_width((ctx.content_rect().width() - 24.0).min(820.0))
        .show(ctx, |ui| {
            ui.label(RichText::new(format!("{} · {} scope", pending.file, pending.scope)).strong());
            ui.label(match &pending.reason {
                Some(reason) => format!("From {}: {reason}", pending.from),
                None => format!("From {}", pending.from),
            });
            for risk in &pending.risks {
                ui.colored_label(client.palette.warning, risk);
            }
            for warning in &pending.warnings {
                ui.colored_label(client.palette.warning, warning);
            }
            if let Some(preview) = &pending.preview
                && let Ok(tokens) = serde_json::from_value::<atomics_core::theme::ThemeTokens>(preview.clone())
            {
                ui.label("Preview");
                crate::theme::Palette::new(&tokens, crate::theme::Fonts::default()).show_preview(ui);
            }
            egui::ScrollArea::both().max_height(ctx.content_rect().height() * 0.45).show(ui, |ui| {
                for line in pending.diff.lines() {
                    ui.label(RichText::new(line).monospace().size(12.0));
                }
            });
            if !enabled {
                ui.label("View only: applying settings requires the host grant.");
            }
            ui.horizontal(|ui| {
                for (label, apply) in [("Apply", true), ("Reject", false)] {
                    if ui.add_enabled(enabled && (!apply || ready), egui::Button::new(label)).clicked() {
                        let note = std::mem::take(&mut client.management.proposal_note);
                        client.send(Command::DecideProposal { id: pending.id.clone(), apply, note });
                    }
                }
            });
            ui.add(
                egui::TextEdit::singleline(&mut client.management.proposal_note)
                    .hint_text("Note for the chief (optional)"),
            );
        });
}

fn tool(client: &mut Client, tool: atomics_core::ops::SettingsTool) {
    client.send(Command::Manage(HostAction::SettingsTool { tool }));
}

fn tools(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    use atomics_core::ops::SettingsTool;
    let enabled = allowed(backend, &HostAction::SettingsTool { tool: SettingsTool::CheckSandbox });
    ui.separator();
    ui.collapsing("Themes", |ui| {
        ui.label("Selecting a theme proposes the setting; Apply saves it.");
        for source in ["built_in", "files"] {
            if let Some(names) = info.config["themes"][source].as_array() {
                ui.horizontal_wrapped(|ui| {
                    for name in names.iter().filter_map(|n| n.as_str()) {
                        if ui.add_enabled(enabled, egui::Button::new(name)).clicked() {
                            client.send(Command::Manage(HostAction::ConfigPropose {
                                scope: client.management.scope.filter(|s| *s == Scope::Host).unwrap_or(Scope::Main),
                                toml: format!("[theme]\nname = {name:?}"),
                                remove: vec![],
                                theme: None,
                                reason: Some("Select theme".into()),
                            }));
                        }
                    }
                });
            }
        }
    });
    ui.collapsing("Host setup", |ui| {
        if info.tools["hooks"] == true {
            ui.label("Hooks for all Claude sessions are installed");
        } else if ui.add_enabled(enabled, egui::Button::new("Install hooks for all Claude sessions")).clicked() {
            tool(client, SettingsTool::InstallHooks);
        }
        if let Some(lines) = info.tools["sandbox"].as_array() {
            for line in lines.iter().filter_map(|s| s.as_str()) {
                ui.label(line);
            }
        }
        if ui
            .add_enabled(enabled && info.tools["sandbox_busy"] != true, egui::Button::new("Refresh sandbox checks"))
            .clicked()
        {
            tool(client, SettingsTool::CheckSandbox);
        }
        if ui.button("Copy: atomics setup").clicked() {
            ui.ctx().copy_text("atomics setup".into());
        }
    });
    ui.collapsing("Connections", |ui| {
        let connections = &info.tools["connections"];
        ui.label("MCP servers for sandboxed agents");
        if let Some(error) = connections["error"].as_str() {
            ui.label(error);
        }
        if let Some(rows) = connections["rows"].as_array() {
            for row in rows {
                let name = row["name"].as_str().unwrap_or_default();
                ui.label(format!("{name} · {}", row["status"].as_str().unwrap_or("not connected")));
                if let Some(detail) = row["detail"].as_str() {
                    ui.label(detail);
                }
                if let Some(error) = row["error"].as_str() {
                    ui.label(error);
                }
                ui.add_enabled_ui(enabled && row["busy"] != true, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Connect").clicked() {
                            tool(client, SettingsTool::Connect { name: name.into(), store_env: false });
                        }
                        if row["env"].as_u64().unwrap_or(0) > 0 && ui.button("Connect and store env values").clicked() {
                            tool(client, SettingsTool::Connect { name: name.into(), store_env: true });
                        }
                        if row["status"].is_string() && ui.button("Disconnect").clicked() {
                            tool(client, SettingsTool::Disconnect { name: name.into() });
                        }
                    });
                });
            }
        }
        ui.add_enabled_ui(enabled, |ui| {
            if ui.button("Import from Claude Code / Codex").clicked() {
                tool(client, SettingsTool::ImportConnections);
            }
            if ui.button("Refresh connections").clicked() {
                tool(client, SettingsTool::RefreshConnections);
            }
            ui.add(egui::TextEdit::singleline(&mut client.management.connection_name).hint_text("Server name"));
            ui.add(
                egui::TextEdit::singleline(&mut client.management.connection_url)
                    .hint_text("https://mcp.example.com/mcp"),
            );
            if ui.button("Add connection").clicked() {
                tool(
                    client,
                    SettingsTool::AddConnection {
                        name: client.management.connection_name.clone(),
                        url: client.management.connection_url.clone(),
                    },
                );
            }
        });
    });
    ui.collapsing("Credentials", |ui| {
        ui.label("Stored values are never displayed. Replacing or removing a credential revokes existing session placeholders.");
        let credentials = &info.tools["credentials"];
        if let Some(lines) = credentials["lines"].as_array() { for line in lines.iter().filter_map(|s| s.as_str()) { ui.label(line); } }
        if let Some(azure) = credentials["azure"].as_str() { ui.label(azure); }
        ui.add_enabled_ui(enabled && info.tools["credential_busy"] != true, |ui| {
            if ui.button("Refresh credentials").clicked() { tool(client, SettingsTool::RefreshCredentials); }
            let state = &mut client.management;
            if state.credential_kind.is_empty() { state.credential_kind = "claude".into(); }
            egui::ComboBox::from_label("Credential type").selected_text(&state.credential_kind).show_ui(ui, |ui| {
                if let Some(kinds) = info.tools["credential_kinds"].as_array() {
                    for kind in kinds {
                        let id = kind["id"].as_str().unwrap_or_default();
                        if ui.selectable_value(&mut state.credential_kind, id.into(), kind["label"].as_str().unwrap_or(id)).changed() { state.credential_value.clear(); state.credential_host.clear(); }
                    }
                }
            });
            let hosted = matches!(state.credential_kind.as_str(), "registry" | "github-app");
            if hosted { ui.add(egui::TextEdit::singleline(&mut state.credential_host).hint_text("Host (optional for github.com)")); }
            ui.add(egui::TextEdit::singleline(&mut state.credential_value).password(true).hint_text("New value (hidden)"));
            let host = hosted.then(|| state.credential_host.trim().to_string());
            let kind = state.credential_kind.clone();
            if ui.add_enabled(!state.credential_value.is_empty(), egui::Button::new("Add / replace credential")).clicked() {
                let value = std::mem::take(&mut state.credential_value);
                tool(client, SettingsTool::PutCredential { kind: kind.clone(), value, host: host.clone() });
            }
            if ui.button("Remove credential").clicked() { client.management.credential_value.clear(); tool(client, SettingsTool::RemoveCredential { kind, host }); }
            if credentials["invalid"].as_u64().unwrap_or(0) > 0 && ui.button("Remove unknown / misfiled entries").clicked() { tool(client, SettingsTool::CleanCredentials); }
            let azure = info.config["values"]["isolation.sandbox.azure.enabled"]["value"].as_bool().unwrap_or(false);
            let mut changed = azure;
            if ui.checkbox(&mut changed, "Enable Azure CLI access").changed() {
                client.send(Command::Manage(HostAction::ConfigPropose { scope: Scope::Main, toml: format!("[isolation.sandbox.azure]\nenabled = {changed}"), remove: vec![], theme: None, reason: Some("Change Azure CLI access".into()) }));
            }
        });
    });
}
