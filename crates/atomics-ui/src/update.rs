//! A new version in the top bar: a quiet Update button where this window can start one, and a hint where it cannot,
//! as a browser and `--connect` show the host's. It folds into the Actions menu with the bar's other items, which then
//! wears a small dot. An update under way stays in sight: beside the folded menu where it fits, and otherwise as a
//! dot of another colour with the state on hover. The countdown before the window closes floats over everything, at
//! any width. None of this opens anything or moves the keyboard. Settings has the rest ([`crate::management`]).

use atomics_core::projection::{UpdateInfo, UpdatePhase};
use egui::RichText;

use crate::topbar::record_item;
use crate::{Backend, Client, Command};

/// What the top bar says about updates. Nothing, unless a new version is there or an update is under way: a check
/// that failed shows in Settings only.
pub fn top_bar_item(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let Some(update) = backend.update() else { return };
    if under_way(ui, backend, client, &update) || matches!(update.phase, UpdatePhase::RestartsIn { .. }) {
        return;
    }
    match offer(backend, &update) {
        Some((text, tip)) => press(ui, client, text, &tip, Command::Update),
        None => {
            if let Some(latest) = &update.latest {
                hint(ui, client, &format!("{latest} available"), &why_not(backend, &update));
            }
        }
    }
}

/// Draws an update's state while one runs or waits, and says whether it did.
fn under_way(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, update: &UpdateInfo) -> bool {
    match &update.phase {
        UpdatePhase::Working { step } => hint(ui, client, "Updating…", step),
        UpdatePhase::WaitingForAgents { blockers } => {
            hint(ui, client, "Restarts when agents finish", &blockers.join("\n"));
            if backend.available(&Command::CancelUpdate) {
                press(ui, client, "Cancel", "Stay on this version for now", Command::CancelUpdate);
            }
        }
        _ => return false,
    }
    true
}

/// An update under way, for the bar to keep beside the folded Actions menu where there is room: nothing otherwise.
pub fn status(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    if let Some(update) = backend.update() {
        under_way(ui, backend, client, &update);
    }
}

/// The button this window has for the user to press, with its tooltip: nothing while an update runs or waits, or
/// where it could only fail.
fn offer(backend: &dyn Backend, update: &UpdateInfo) -> Option<(&'static str, String)> {
    match &update.phase {
        UpdatePhase::Working { .. } | UpdatePhase::WaitingForAgents { .. } | UpdatePhase::RestartsIn { .. } => None,
        UpdatePhase::Installed { version } if backend.available(&Command::Update) => {
            let tip = format!("{version} is installed. This restarts into it when agents finish.");
            Some(("Restart to update", tip))
        }
        _ => {
            let latest = update.latest.as_ref()?;
            let tip = format!("ATOMICS {latest} is available; this is {}.", update.current);
            can_update(backend, update).then_some(("Update", tip))
        }
    }
}

/// Whether Update or Restart to update is there to press.
pub fn offered(backend: &dyn Backend) -> bool {
    backend.update().is_some_and(|update| offer(backend, &update).is_some())
}

/// Marks the folded Actions menu for what is inside it: the accent dot while Update or Restart to update waits there,
/// the warning dot, with the state on hover, while an update runs or waits to restart. The dot sits inside the
/// button's corner, so it moves nothing and needs no room; painted, because a browser's fonts have no such glyph.
pub fn mark_menu(ui: &egui::Ui, button: &egui::Response, backend: &dyn Backend, palette: &crate::theme::Palette) {
    let Some(update) = backend.update() else { return };
    let state = match &update.phase {
        UpdatePhase::Working { step } => Some(format!("Updating…\n{step}")),
        UpdatePhase::WaitingForAgents { blockers } => {
            Some(format!("Restarts when agents finish\n{}", blockers.join("\n")))
        }
        _ => None,
    };
    let color = if state.is_some() {
        palette.warning
    } else if offer(backend, &update).is_some() {
        palette.accent
    } else {
        return;
    };
    ui.painter().circle_filled(button.rect.right_top() + egui::vec2(-5.0, 5.0), 2.5, color);
    if let Some(state) = state {
        button.clone().on_hover_text(state);
    }
}

/// The last seconds before the window closes, over everything and at any width, with the only way to stop it. A
/// client cannot stop it and is told what is about to happen to its connection.
pub fn countdown(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client) {
    let Some(update) = backend.update() else { return };
    let UpdatePhase::RestartsIn { seconds } = update.phase else { return };
    let cancels = backend.available(&Command::CancelUpdate);
    let palette = client.palette.clone();
    egui::Area::new(egui::Id::NULL.with("update-countdown"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 40.0))
        .show(ctx, |ui| {
            let frame = egui::Frame::popup(ui.style()).stroke(egui::Stroke::new(1.0, palette.warning));
            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    let text = if cancels {
                        format!("Restarting in {seconds} s to update")
                    } else {
                        format!("The host restarts in {seconds} s to update")
                    };
                    let label = ui.label(RichText::new(text).color(palette.foreground));
                    record_item(ui, "countdown", &label);
                    if cancels {
                        press(ui, client, "Cancel", "Stay on this version for now", Command::CancelUpdate);
                    }
                });
            });
        });
}

/// Whether this window can install the version found.
pub fn can_update(backend: &dyn Backend, update: &UpdateInfo) -> bool {
    update.unavailable.is_none() && backend.available(&Command::Update)
}

/// Why this window offers no Update button.
pub fn why_not(backend: &dyn Backend, update: &UpdateInfo) -> String {
    if backend.available(&Command::Update) {
        return update.unavailable.clone().unwrap_or_default();
    }
    format!("The host runs {}. Update it on the host: its window's Update button, or atomics update.", update.current)
}

fn hint(ui: &mut egui::Ui, client: &Client, text: &str, tip: &str) {
    let response = ui.label(RichText::new(text).color(client.palette.muted));
    record_item(ui, "Update", &response);
    if !tip.is_empty() {
        response.on_hover_text(tip);
    }
}

fn press(ui: &mut egui::Ui, client: &mut Client, text: &str, tip: &str, command: Command) {
    let response =
        ui.add(crate::touch::sized(egui::Button::new(RichText::new(text).color(client.palette.muted)), client.touch));
    record_item(ui, text, &response);
    if response.on_hover_text(tip).clicked() {
        client.send(command);
        let holder = ui.ctx().data(|data| data.get_temp::<Option<egui::Id>>(holder_id())).flatten();
        ui.ctx().data_mut(|data| data.insert_temp(give_back_id(), holder));
        ui.ctx().request_repaint();
        ui.close();
    }
}

/// What had the keyboard as this frame began, or as the open menu was opened.
fn holder_id() -> egui::Id {
    egui::Id::NULL.with("update-item-holder")
}

/// A press's holder, for the frame after it.
fn give_back_id() -> egui::Id {
    egui::Id::NULL.with("update-item-keyboard")
}

/// The buttons here take nothing from the keyboard. A click takes it from whatever had it, as that widget draws on
/// the frame the click lands in, and in a folded bar the click on Actions already did. So every frame starts by noting
/// who has it, except under an open menu, and the frame after a press gives it back: the terminal, the diff or a text
/// field, or nothing at all if nothing had it. A key that has moved the keyboard by then, Ctrl+1 to 9 for one, keeps it.
/// Call this before anything that can hold the keyboard is drawn.
pub fn keep_keyboard(ctx: &egui::Context) {
    if let Some(Some(id)) = ctx.data_mut(|data| data.remove_temp::<Option<egui::Id>>(give_back_id()))
        && ctx.memory(|memory| memory.focused()).is_none()
    {
        ctx.memory_mut(|memory| memory.request_focus(id));
    }
    if !egui::Popup::is_any_open(ctx) {
        let holder = ctx.memory(|memory| memory.focused());
        ctx.data_mut(|data| data.insert_temp(holder_id(), holder));
    }
}
