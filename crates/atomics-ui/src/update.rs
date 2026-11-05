//! A new version in the top bar: a quiet Update button where this window can start one, and a hint where it cannot,
//! as a browser and `--connect` show the host's. It folds into the Actions menu with the bar's other items, which then
//! wears a small dot, opens nothing, and leaves the keyboard where typing goes. Settings has the rest
//! ([`crate::management`]).

use atomics_core::projection::{UpdateInfo, UpdatePhase};
use egui::RichText;

use crate::topbar::record_item;
use crate::{Backend, Client, Command};

/// What the top bar says about updates. Nothing, unless a new version is there or an update is under way: a check
/// that failed shows in Settings only.
pub fn top_bar_item(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    give_back_keyboard(ui, backend, client);
    let Some(update) = backend.update() else { return };
    match &update.phase {
        UpdatePhase::Working { step } => hint(ui, client, "Updating…", step),
        UpdatePhase::WaitingForAgents { blockers } => {
            hint(ui, client, "Restarts when agents finish", &blockers.join("\n"));
            if backend.available(&Command::CancelUpdate) {
                press(ui, client, "Cancel", "Stay on this version for now", Command::CancelUpdate);
            }
        }
        _ => match offer(backend, &update) {
            Some((text, tip)) => press(ui, client, text, &tip, Command::Update),
            None => {
                if let Some(latest) = &update.latest {
                    hint(ui, client, &format!("{latest} available"), &why_not(backend, &update));
                }
            }
        },
    }
}

/// The button this window has for the user to press, with its tooltip: nothing while an update runs or waits, or
/// where it could only fail.
fn offer(backend: &dyn Backend, update: &UpdateInfo) -> Option<(&'static str, String)> {
    match &update.phase {
        UpdatePhase::Working { .. } | UpdatePhase::WaitingForAgents { .. } => None,
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

/// Whether Update or Restart to update is there to press, so that the Actions menu says so while it holds them.
pub fn offered(backend: &dyn Backend) -> bool {
    backend.update().is_some_and(|update| offer(backend, &update).is_some())
}

/// The dot on a menu button that holds one of them. It sits inside the button's corner, so it moves nothing and needs
/// no room; painted, because a browser's fonts have no such glyph.
pub fn paint_dot(ui: &egui::Ui, button: &egui::Response, palette: &crate::theme::Palette) {
    let center = button.rect.right_top() + egui::vec2(-5.0, 5.0);
    ui.painter().circle_filled(center, 2.5, palette.accent);
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
        let holder = ui.memory(|memory| memory.focused());
        ui.ctx().data_mut(|data| data.insert_temp(keyboard_id(), holder));
        ui.ctx().request_repaint();
        ui.close();
    }
}

fn keyboard_id() -> egui::Id {
    egui::Id::NULL.with("update-item-keyboard")
}

/// A click anywhere takes the keyboard, on the frame it lands in, from whatever had it. Nothing here needs it, so
/// the frame after it goes back to that: the terminal, the diff or a text field, else the active terminal.
fn give_back_keyboard(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let Some(holder) = ui.ctx().data_mut(|data| data.remove_temp::<Option<egui::Id>>(keyboard_id())) else { return };
    match holder {
        Some(id) => ui.memory_mut(|memory| memory.request_focus(id)),
        None => client.focus_terminal = backend.view_hints().active,
    }
}
