//! "Remote devices": pairing a phone or browser with this host and revoking it (docs/daemon.md,
//! "Rendezvous"). Opened from the top bar's Remote indicator while `[remote.rendezvous]` runs.
//!
//! "Pair a device" shows a QR code and the link (both carry the single-use secret, only in the
//! link's fragment), then, once a device proved it, the device's name and the six-digit code it
//! also shows. The user pairs it if the codes match, as able to type and approve or view only,
//! or refuses it. Paired devices are listed with what they may do and when they were last seen;
//! revoking one ends its sessions at once.

use super::*;
use crate::protocol::rendezvous::devices::{self, Devices, PairingView};
use atomics_core::noise::{self, Access, PublicKey};

/// The QR code's side at most, in points, with its quiet zone.
const QR_SIDE: f32 = 260.0;

impl Desktop<'_> {
    pub(super) fn remote_devices_dialog(&mut self, ctx: &egui::Context) {
        if !self.client.remote_devices {
            return;
        }
        let Some(devices) = self.runtime.remote_devices() else {
            self.client.remote_devices = false;
            return;
        };
        let mut open = true;
        egui::Window::new("Remote devices")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                if let Some(problem) = devices.problem() {
                    ui.colored_label(self.palette().warning, problem);
                    ui.add_space(6.0);
                }
                self.pairing_section(ui, &devices);
                ui.separator();
                self.device_list(ui, &devices);
            });
        if !open {
            self.client.remote_devices = false;
            self.client.revoke_confirm = None;
        }
    }

    fn pairing_section(&mut self, ui: &mut egui::Ui, devices: &Devices) {
        let palette = self.palette();
        match devices.view() {
            PairingView::Idle { note } => {
                if let Some(note) = note {
                    ui.label(note);
                }
                let template = self.runtime.remote_view().pair_url.clone();
                let blocked = devices.unready().or_else(|| noise::pair_url_problem(&template));
                let response = ui.add_enabled(blocked.is_none(), egui::Button::new("Pair a device"));
                if let Some(why) = &blocked {
                    ui.colored_label(palette.muted, why.as_str());
                }
                if response.clicked()
                    && let Err(error) = devices.offer(&template)
                {
                    self.runtime.toast(format!("Could not start pairing: {error}"));
                }
            }
            PairingView::Open { link, left, busy } => {
                ui.label("Scan this code with the phone's camera, or open the link on the device:");
                ui.add_space(4.0);
                qr_code(ui, &link);
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(link.as_str()).monospace().size(11.0)).wrap());
                ui.horizontal(|ui| {
                    if ui.button("Copy link").clicked() {
                        ui.ctx().copy_text(link.to_string());
                    }
                    if ui.button("Cancel").clicked() {
                        devices.cancel();
                    }
                    ui.colored_label(palette.muted, format!("Expires in {}", minutes(left)));
                });
                if busy {
                    ui.colored_label(palette.muted, "A device is answering the link…");
                }
                ui.label(
                    RichText::new("The link works once, for five minutes. Then both screens show a code to compare.")
                        .color(palette.muted)
                        .size(11.0),
                );
                ui.ctx().request_repaint_after(Duration::from_secs(1));
            }
            PairingView::Confirm { name, code, left } => {
                ui.label(RichText::new(format!("Pair “{name}”?")).strong().size(16.0));
                ui.label("Check that the device shows the same code:");
                ui.label(RichText::new(noise::show_code(&code)).monospace().size(32.0).strong());
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Codes match: may type and approve").clicked() {
                        devices.decide(Some(Access::Control));
                    }
                    if ui.button("Codes match: view only").clicked() {
                        devices.decide(Some(Access::View));
                    }
                    if ui.button("Codes differ: refuse").clicked() {
                        devices.decide(None);
                    }
                });
                ui.colored_label(
                    palette.muted,
                    format!("A different code means another device answered the link. {} left.", minutes(left)),
                );
                ui.ctx().request_repaint_after(Duration::from_secs(1));
            }
        }
    }

    fn device_list(&mut self, ui: &mut egui::Ui, devices: &Devices) {
        let palette = self.palette();
        let list = devices.list();
        ui.label(RichText::new("Paired devices").strong());
        if list.is_empty() {
            ui.colored_label(palette.muted, "No device is paired with this host.");
            return;
        }
        let now = super::remote::now_ms();
        egui::Grid::new("paired_devices").num_columns(4).spacing([12.0, 6.0]).show(ui, |ui| {
            for (device, connected) in list {
                ui.label(device.name.as_str());
                ui.colored_label(palette.muted, devices::access_words(device.access));
                if connected {
                    ui.colored_label(palette.success, "connected");
                } else {
                    ui.colored_label(palette.muted, format!("seen {}", ago(now, device.seen_ms)));
                }
                self.revoke_button(ui, devices, &device.key, &device.name);
                ui.end_row();
            }
        });
    }

    /// Revoking cannot be undone (the device must pair again), so it asks once.
    fn revoke_button(&mut self, ui: &mut egui::Ui, devices: &Devices, key: &PublicKey, name: &str) {
        if self.client.revoke_confirm == Some(*key) {
            ui.horizontal(|ui| {
                if ui.button("Revoke").on_hover_text(format!("“{name}” must pair again to connect")).clicked() {
                    devices.revoke(key);
                    self.client.revoke_confirm = None;
                }
                if ui.button("Keep").clicked() {
                    self.client.revoke_confirm = None;
                }
            });
        } else if ui.button("Revoke…").clicked() {
            self.client.revoke_confirm = Some(*key);
        }
    }
}

/// `4:05`.
fn minutes(left: Duration) -> String {
    let seconds = left.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// How long ago `then_ms` was, roughly.
fn ago(now_ms: u64, then_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(then_ms) / 1000;
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86_400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

/// `text` as a QR code, dark on white whatever the theme (scanners need the contrast), with the
/// four-module quiet zone the standard asks for.
fn qr_code(ui: &mut egui::Ui, text: &str) {
    let Ok(code) = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium) else {
        ui.label("The link is too long for a QR code; open it on the device instead.");
        return;
    };
    const QUIET: i32 = 4;
    let modules = code.size() + 2 * QUIET;
    let scale = (QR_SIDE / modules as f32).floor().max(2.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(scale * modules as f32), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::WHITE);
    for y in 0..code.size() {
        for x in 0..code.size() {
            if code.get_module(x, y) {
                let at = rect.min + Vec2::new((x + QUIET) as f32 * scale, (y + QUIET) as f32 * scale);
                painter.rect_filled(Rect::from_min_size(at, Vec2::splat(scale)), 0.0, Color32::BLACK);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_as_people_say_them() {
        assert_eq!(minutes(Duration::from_secs(245)), "4:05");
        assert_eq!(ago(10_000, 9_000), "just now");
        assert_eq!(ago(10 * 60_000, 0), "10 min ago");
        assert_eq!(ago(3 * 3_600_000, 0), "3 h ago");
        assert_eq!(ago(2 * 86_400_000, 0), "2 days ago");
    }

    #[test]
    fn a_pairing_link_fits_a_qr_code() {
        let fragment = format!("1.0190a1b2-c3d4-8e5f-a617-283940516273.{}.{}", "A".repeat(43), "B".repeat(43));
        let link = format!("https://service.example/atomics/pair#{fragment}");
        let code = qrcodegen::QrCode::encode_text(&link, qrcodegen::QrCodeEcc::Medium).unwrap();
        assert!(code.size() <= 57, "small enough to scan from a screen: {}", code.size());
    }
}
