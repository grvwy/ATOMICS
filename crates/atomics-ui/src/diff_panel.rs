//! A worktree's diff in the diff panel: the header the host fills in, the diff view, and what its buttons
//! ask of the host.

use std::path::Path;

use atomics_core::layout::DiffSplit;
use egui::Rect;

use crate::diff_view::{self, DiffAction, Header};
use crate::{Backend, Client, Command};

/// The diff of the worktree at `root` under its header: its session's title and branch, whether an agent
/// there can take a review, and the diff view. Review comments, Send review, Refresh, Close and Edit record
/// commands; the side-by-side toggle stays with the client.
pub fn draw(ui: &mut egui::Ui, rect: Rect, root: &Path, backend: &dyn Backend, client: &mut Client) {
    let heading = backend.diff_heading(root);
    let config = backend.ui_config();
    let palette = client.palette.clone();
    let font = palette.mono(palette.fonts.diff_size.unwrap_or(config.font.diff_size));
    let split = client.diff_split.unwrap_or(config.layout.diff_split);
    let state = client.diff_views.entry(root.to_path_buf()).or_default();
    let header = Header {
        title: &heading.title,
        branch: heading.branch.as_deref(),
        can_send: heading.can_send,
        split,
        editor_shortcut: &config.keys.toggle_editor,
    };
    let before = state.comments.clone();
    let action = diff_view::show(ui, rect, state, header, &palette, &font);
    let after = state.comments.clone();
    if before != after {
        client.send(Command::EditComments { root: root.to_path_buf(), before, after });
    }
    match action {
        DiffAction::None => {}
        DiffAction::SendReview => client.send(Command::SendReview(root.to_path_buf())),
        DiffAction::Refresh => client.send(Command::RefreshDiff(root.to_path_buf())),
        DiffAction::Close => client.send(Command::CloseDiff),
        DiffAction::Split(on) => client.diff_split = Some(if on { DiffSplit::Always } else { DiffSplit::Never }),
        DiffAction::Edit { path, line } => client.send(Command::Edit { root: root.to_path_buf(), path, line }),
    }
}
