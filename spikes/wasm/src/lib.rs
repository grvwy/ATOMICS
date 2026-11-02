//! Phase 0 spike: minimal eframe app with a terminal-like widget for the iOS soft-keyboard test.
use eframe::egui::{self, Event, EventFilter, Key, Sense, TextEdit, Vec2};
use wasm_bindgen::prelude::*;

#[cfg(not(any(feature = "wgpu", feature = "glow")))]
compile_error!("enable exactly one renderer feature: `wgpu` or `glow`");

#[derive(PartialEq)]
enum Mode {
    Terminal,
    TextEditBaseline,
}

struct App {
    mode: Mode,
    /// What the "terminal" received, as the host would see it (`terminal_input` payloads).
    screen: String,
    /// Raw event log, newest last.
    log: Vec<String>,
    preedit: String,
    baseline: String,
    noise: String,
}

impl App {
    fn new() -> Self {
        Self {
            mode: Mode::Terminal,
            screen: String::new(),
            log: Vec::new(),
            preedit: String::new(),
            baseline: String::new(),
            noise: noise_report(),
        }
    }

    fn note(&mut self, line: String) {
        self.log.push(line);
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }

    fn terminal(&mut self, ui: &mut egui::Ui) {
        let size = Vec2::new(ui.available_width(), 160.0);
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        if resp.clicked() {
            resp.request_focus();
        }
        let id = resp.id;
        // A terminal wants Tab, arrows and Escape for itself.
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
            )
        });
        let focused = resp.has_focus();
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, egui::Color32::BLACK);
        let stroke = if focused { egui::Color32::LIGHT_GREEN } else { egui::Color32::DARK_GRAY };
        painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
        let text = format!("$ {}{}{}", self.screen, self.preedit, if focused { "\u{2588}" } else { "" });
        painter.text(
            rect.min + Vec2::splat(6.0),
            egui::Align2::LEFT_TOP,
            text,
            egui::FontId::monospace(16.0),
            egui::Color32::LIGHT_GREEN,
        );
        if focused {
            // Asks the web backend to show its hidden text input (the "text agent").
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput { rect, cursor_rect: egui::Rect::from_min_size(rect.min, Vec2::new(10.0, 18.0)) })
            });
            let events = ui.input(|i| i.events.clone());
            for ev in events {
                match ev {
                    Event::Text(t) => {
                        self.screen.push_str(&t);
                        self.note(format!("Text {t:?}"));
                    }
                    Event::Ime(ime) => {
                        match &ime {
                            egui::ImeEvent::Preedit(s) => self.preedit = s.clone(),
                            egui::ImeEvent::Commit(s) => {
                                self.preedit.clear();
                                self.screen.push_str(s);
                            }
                            _ => self.preedit.clear(),
                        }
                        self.note(format!("Ime {ime:?}"));
                    }
                    Event::Key { key, pressed: true, modifiers, repeat: false, .. } => {
                        if key == Key::Backspace {
                            self.screen.pop();
                        }
                        if key == Key::Enter {
                            self.screen.push('\n');
                        }
                        self.note(format!("Key {key:?} {modifiers:?}"));
                    }
                    Event::Paste(s) => {
                        self.screen.push_str(&s);
                        self.note(format!("Paste {s:?}"));
                    }
                    _ => {}
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("egui keyboard test");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.mode, Mode::Terminal, "terminal widget");
                ui.selectable_value(&mut self.mode, Mode::TextEditBaseline, "TextEdit baseline");
            });
            match self.mode {
                Mode::Terminal => self.terminal(ui),
                Mode::TextEditBaseline => {
                    ui.add(TextEdit::multiline(&mut self.baseline).desired_rows(6).desired_width(f32::INFINITY));
                }
            }
            if !self.noise.is_empty() {
                ui.label(&self.noise);
            }
            ui.separator();
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for l in &self.log {
                    ui.monospace(l);
                }
            });
        });
    }
}

#[cfg(feature = "snow")]
fn noise_report() -> String {
    const P: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
    let run = || -> Result<usize, snow::Error> {
        let mut i = snow::Builder::new(P.parse()?);
        let mut r = snow::Builder::new(P.parse()?);
        let (ik, rk) = (i.generate_keypair()?, r.generate_keypair()?);
        let mut i = i.local_private_key(&ik.private)?.build_initiator()?;
        let mut r = r.local_private_key(&rk.private)?.build_responder()?;
        let (mut a, mut b, mut c) = ([0u8; 512], [0u8; 512], [0u8; 512]);
        let n = i.write_message(&[], &mut a)?;
        r.read_message(&a[..n], &mut c)?;
        let n = r.write_message(&[], &mut a)?;
        i.read_message(&a[..n], &mut c)?;
        let n = i.write_message(&[], &mut a)?;
        r.read_message(&a[..n], &mut c)?;
        let mut i = i.into_transport_mode()?;
        let mut r = r.into_transport_mode()?;
        let n = i.write_message(b"ping", &mut a)?;
        r.read_message(&a[..n], &mut b)
    };
    match run() {
        Ok(n) => format!("snow Noise_XX handshake + transport ok ({n} bytes round trip)"),
        Err(e) => format!("snow failed: {e}"),
    }
}

#[cfg(not(feature = "snow"))]
fn noise_report() -> String {
    String::new()
}

/// Called from `www/index.html` with the canvas element id.
#[wasm_bindgen]
pub async fn start(canvas_id: String) -> Result<(), JsValue> {
    use wasm_bindgen::JsCast;
    let canvas = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(&canvas_id))
        .ok_or("canvas not found")?
        .dyn_into::<web_sys::HtmlCanvasElement>()?;
    eframe::WebRunner::new()
        .start(canvas, eframe::WebOptions::default(), Box::new(|_cc| Ok(Box::new(App::new()))))
        .await
}
