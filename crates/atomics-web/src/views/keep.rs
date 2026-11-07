//! The passkey's two screens (docs/decisions/2026-11-07-passkey-sealed-device-key.md, `crate::keep`): keeping a new
//! pairing on a browser whose frame storage ends with it, and opening the kept copy on a cold start
//! ([`Stop::Locked`]). Their one tap is a real button on the page (`web/passkey.js`): a passkey needs that tap's own
//! activation, which egui's frame comes too late for. The screen leaves the button's place empty and says where it
//! is ([`BUTTON`]); the glue lays the button there.

use atomics_ui::Client;
use egui::{Id, RichText, Sense, vec2};

use super::{centred, padded};
use crate::driver::{Driver, KeepPrompt, Status, Stop};

/// Where this frame's passkey button goes and what it says: `Some((Rect, String))` in egui's memory under this
/// name ([`button_id`]), which the glue takes after each frame. No entry, no button.
pub const BUTTON: &str = "atomics-passkey-button";

pub fn button_id() -> Id {
    Id::NULL.with(BUTTON)
}
const BUTTON_SIZE: egui::Vec2 = vec2(240.0, 44.0);

/// The keep or the locked screen, when one is due; false when neither is.
pub(super) fn screen(ui: &mut egui::Ui, driver: &mut Driver, client: &Client, gutter: f32) -> bool {
    if *driver.status() == Status::Stopped(Stop::Locked) {
        padded(ui, gutter, |ui| centred(ui, |ui| locked(ui, driver, client)));
        return true;
    }
    let Some(prompt) = driver.keep_prompt() else { return false };
    padded(ui, gutter, |ui| centred(ui, |ui| keeping(ui, driver, client, &prompt)));
    true
}

fn button(ui: &mut egui::Ui, label: &str) {
    let (rect, _) = ui.allocate_exact_size(BUTTON_SIZE, Sense::hover());
    ui.ctx().data_mut(|d| d.insert_temp(button_id(), Some((rect, label.to_string()))));
}

fn locked(ui: &mut egui::Ui, driver: &Driver, client: &Client) {
    let p = &client.palette;
    ui.label(RichText::new("Unlock this pairing").size(18.0));
    ui.add_space(8.0);
    ui.label(Stop::Locked.message());
    ui.add_space(16.0);
    button(ui, "Unlock with passkey");
    if let Some(why) = driver.unlock_failed() {
        ui.add_space(8.0);
        ui.label(RichText::new(format!("Not unlocked: {why}.")).color(p.warning));
        ui.label(
            RichText::new("Try again, or pair this browser again from Remote devices on the computer.").color(p.muted),
        );
    }
    ui.add_space(12.0);
    for line in [driver.pairing_reason().map(str::to_string), driver.storage_reason()].into_iter().flatten() {
        ui.label(RichText::new(line).small().color(p.muted));
    }
}

fn keeping(ui: &mut egui::Ui, driver: &mut Driver, client: &Client, prompt: &KeepPrompt) {
    let p = client.palette.clone();
    match prompt {
        KeepPrompt::Ask { first, failed, another } => {
            ui.label(RichText::new("Keep this pairing on this browser").size(18.0));
            ui.add_space(8.0);
            ui.label(
                "This browser forgets its pairings when it closes. A passkey keeps this one: the next time, Face ID, \
                 Touch ID or this device's lock opens it again.",
            );
            if *first {
                ui.label(RichText::new("The first tap makes the passkey, in a window of its own.").color(p.muted));
            }
            ui.add_space(16.0);
            button(ui, if *first { "Make a passkey" } else { "Keep with passkey" });
            if let Some(why) = failed {
                ui.add_space(8.0);
                ui.label(RichText::new(format!("Not kept: {why}.")).color(p.warning));
            }
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("Not now").clicked() {
                    driver.keep_declined();
                }
                if *another && ui.button("Make another passkey").clicked() {
                    driver.use_another_passkey();
                }
            });
        }
        KeepPrompt::Enrolling => {
            ui.label(RichText::new("Make the passkey in the window that opened").size(18.0));
            ui.add_space(8.0);
            ui.label("When it is made, come back here and keep the pairing with it.");
            ui.add_space(16.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("It is made").clicked() {
                    driver.enrolled_elsewhere(true);
                }
                if ui.button("Cancel").clicked() {
                    driver.enrolled_elsewhere(false);
                }
            });
        }
        KeepPrompt::Handing => {
            ui.label(RichText::new("Keeping this pairing…").size(18.0));
        }
    }
    ui.add_space(12.0);
    ui.label(RichText::new(driver.key_line()).small().color(p.muted));
}
