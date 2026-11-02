//! The banner over a session's tile while the session waits on a permission request.

use atomics_core::approvals::Decision;
use egui::{Rect, RichText};

use crate::{Backend, Client, Command};

/// Draws the first permission request the session at `index` of [`Backend::session_list`] waits on at the
/// top of its tile's `body`, and returns the part of `body` below it; with no request waiting, `body` as it
/// is. Allow, Allow always (never for Azure REST) and Deny record an [`Command::Decide`]; the buttons stay
/// disabled until the host has decided.
pub fn banner(ui: &mut egui::Ui, body: Rect, index: usize, backend: &dyn Backend, client: &mut Client) -> Rect {
    let Some(pending) = backend.session_approval(index) else { return body };
    let p = client.palette.clone();
    let request = &pending.request;
    let deciding = client.approval_decisions.contains(&(pending.session.clone(), pending.id.clone()));
    let height = (body.height() - 220.0).clamp(110.0, 210.0).min(body.height());
    let banner = body.with_max_y(body.top() + height);
    ui.painter().rect_filled(banner, 0.0, p.panel);
    let mut child =
        ui.new_child(egui::UiBuilder::new().id_salt(("approval", &pending.session)).max_rect(banner.shrink(6.0)));
    child.set_clip_rect(banner);
    child.label(RichText::new("Permission requested").color(p.warning).strong());
    let mut decided = None;
    child.horizontal_wrapped(|ui| {
        for (label, decision) in
            [("Allow", Decision::Allow), ("Allow always", Decision::AllowAlways), ("Deny", Decision::Deny)]
        {
            if request.tool == "Azure REST" && decision == Decision::AllowAlways {
                continue;
            }
            let response = ui.add_enabled(!deciding, egui::Button::new(label));
            #[cfg(feature = "test-support")]
            ui.ctx().data_mut(|data| {
                data.insert_temp(egui::Id::unique(("approval-button", label)), response.enabled());
            });
            if response.clicked() {
                decided = Some(decision);
            }
        }
    });
    if deciding {
        child.label("Decision pending…");
    }
    egui::ScrollArea::vertical().max_height(child.available_height()).show(&mut child, |ui| {
        ui.label(RichText::new(request.command.as_deref().unwrap_or(&request.tool)).monospace());
        if !request.cwd.as_os_str().is_empty() {
            ui.label(format!("cwd: {}", request.cwd.display()));
        }
        ui.label(request.reason.as_deref().unwrap_or("Permission requested"));
    });
    if let Some(decision) = decided {
        client.send(Command::Decide { session: index, approval: pending.id.clone(), decision });
    }
    body.with_min_y(banner.bottom())
}
