//! GitHub and Azure pull requests read by the host, with client-local diff navigation.
use crate::diff_view::{self, DiffAction, DiffState, Header};
use crate::{Backend, Client, Command, PanelMode};
use atomics_core::layout::DiffSplit;
use egui::Rect;
use std::path::Path;

pub fn menu(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let pulls = backend.pull_requests();
    if pulls.is_empty() {
        return;
    }
    ui.menu_button(format!("Pull requests · {}", pulls.len()), |ui| {
        for pull in pulls {
            let load = Command::LoadPullRequest(pull.key.clone());
            let may_load = backend.available(&load);
            if ui
                .add_enabled(
                    may_load || pull.head_sha.is_some(),
                    egui::Button::new(format!("{}#{} {}", pull.repo, pull.number, pull.title)),
                )
                .clicked()
            {
                client.pr_target = None;
                client.pr_view = Some((pull.key.clone(), DiffState::default()));
                if may_load {
                    client.send(load);
                }
                client.send(Command::SetPanel(PanelMode::Diff));
                client.send(Command::OpenDiff);
                ui.close();
            }
        }
    });
}

/// True while this client has a PR selected. Comments and editor actions are absent: PR files do not
/// belong to a local worktree, and reviewing one must never type into an agent or open an editor.
pub fn panel(ui: &mut egui::Ui, rect: Rect, backend: &dyn Backend, client: &mut Client) -> bool {
    let hints = backend.view_hints();
    let target = (hints.active, hints.diff_target);
    if client.pr_target.as_ref().is_some_and(|t| *t != target) {
        client.pr_view = None;
        client.pr_target = None;
    }
    if client.pr_view.is_some() {
        client.pr_target = Some(target);
    }
    let Some((key, state)) = client.pr_view.as_mut() else { return false };
    let Some(pull) = backend.pull_requests().into_iter().find(|p| p.key == *key) else {
        client.pr_view = None;
        return false;
    };
    let data = backend.diff(Path::new(key));
    state.error = data.as_ref().and_then(|d| d.error.clone()).or(pull.error);
    state.prepared = data.and_then(|d| d.prepared);
    state.loading = state.prepared.is_none() && state.error.is_none();
    state.comments.clear();
    let title = format!("{}#{} {}", pull.repo, pull.number, pull.title);
    let branch = format!(
        "{} → {}{}",
        pull.head,
        pull.base,
        pull.head_sha.map(|s| format!(" · {}", s.chars().take(8).collect::<String>())).unwrap_or_default()
    );
    let p = client.palette.clone();
    let config = backend.ui_config();
    let rect = if rect.width() >= 760.0 {
        let column = rect.with_max_x(rect.left() + 250.0);
        let current = state.focused.clone();
        if let Some((path, _)) = diff_view::changed_files(ui, column, state, current.as_deref(), &p) {
            state.open(&path);
        }
        rect.with_min_x(column.right())
    } else {
        rect
    };
    let header = Header {
        refresh: backend.available(&Command::LoadPullRequest(key.clone())),
        title: &title,
        branch: Some(&branch),
        can_send: false,
        split: client.diff_split.unwrap_or(config.layout.diff_split),
        editor_shortcut: &config.keys.toggle_editor,
        touch: client.touch,
        review: false,
        edit: false,
    };
    let font = p.mono(p.fonts.diff_size.unwrap_or(config.font.diff_size));
    let key = key.clone();
    match diff_view::show(ui, rect, state, header, &p, &font) {
        DiffAction::Close => client.pr_view = None,
        DiffAction::Refresh => client.send(Command::LoadPullRequest(key.clone())),
        DiffAction::Split(on) => client.diff_split = Some(if on { DiffSplit::Always } else { DiffSplit::Never }),
        _ => {}
    }
    true
}
