//! Phase 0 spike (design §4.4): a terminal-like egui widget fed by one of two input paths, for the iOS
//! soft-keyboard test.
//!
//! - `agent`: eframe's hidden text input (the "text agent"), driven by egui's IME output.
//! - `textarea`: a native `<textarea>` overlay in `www/index.html` that forwards composed text through
//!   [`inject`]. The key bar uses [`inject`] in both paths.
//!
//! Everything the widget receives and every byte it would send to the PTY goes to the page's on-screen log.
use std::cell::RefCell;
use std::collections::VecDeque;

use eframe::egui::{self, Color32, Event, EventFilter, ImeEvent, Key, Sense, Stroke, TextFormat, Vec2};
use wasm_bindgen::prelude::*;

#[cfg(not(any(feature = "wgpu", feature = "glow")))]
compile_error!("enable exactly one renderer feature: `wgpu` or `glow`");

#[wasm_bindgen]
extern "C" {
    /// `window.spikeLog(source, line)` in `www/index.html`: one line in the on-screen log.
    #[wasm_bindgen(js_name = spikeLog)]
    fn spike_log(source: &str, line: &str);
    /// `window.spikeTerminalRect(x, y, w, h)`: the terminal's rect in CSS px, relative to the canvas.
    /// The textarea overlay is placed on it.
    #[wasm_bindgen(js_name = spikeTerminalRect)]
    fn spike_terminal_rect(x: f32, y: f32, w: f32, h: f32);
    /// `window.spikeCtrl(on)`: lights the key bar's sticky Ctrl.
    #[wasm_bindgen(js_name = spikeCtrl)]
    fn spike_ctrl(on: bool);
}

/// What the page pushes into the app (textarea overlay and key bar).
enum Injected {
    Text(String),
    Preedit(String),
    Key(String),
    Focus(bool),
    Zoom(f32),
}

thread_local! {
    static QUEUE: RefCell<VecDeque<Injected>> = const { RefCell::new(VecDeque::new()) };
    static CTX: RefCell<Option<egui::Context>> = const { RefCell::new(None) };
}

/// Called by the page. `kind`: `text` (committed text), `preedit` (composition in progress), `key` (a key name:
/// `Escape`, `Tab`, `Enter`, `Backspace`, `ArrowUp`/`Down`/`Left`/`Right`, `Ctrl`), `focus` (`1`/`0`), `zoom`
/// (a scale factor).
#[wasm_bindgen]
pub fn inject(kind: &str, data: &str) {
    let ev = match kind {
        "text" => Injected::Text(data.to_owned()),
        "preedit" => Injected::Preedit(data.to_owned()),
        "key" => Injected::Key(data.to_owned()),
        "focus" => Injected::Focus(data == "1"),
        "zoom" => Injected::Zoom(data.parse().unwrap_or(1.0)),
        _ => return spike_log("app", &format!("inject: unknown kind {kind:?}")),
    };
    QUEUE.with_borrow_mut(|q| q.push_back(ev));
    CTX.with_borrow(|c| {
        if let Some(c) = c {
            c.request_repaint();
        }
    });
}

#[derive(Clone, Copy, PartialEq)]
enum Path {
    Agent,
    Textarea,
}

struct App {
    path: Path,
    /// What a shell would echo for the bytes sent (control bytes in caret notation).
    screen: String,
    preedit: String,
    /// Sticky Ctrl from the key bar: applies to the next character.
    ctrl: bool,
    font: f32,
    textarea_focus: bool,
    last_rect: Option<egui::Rect>,
    noise: String,
}

/// `"ö" U+00F6`: code points make precomposed vs combining forms and emoji sequences visible in the log.
fn show(s: &str) -> String {
    if s.is_ascii() {
        format!("{s:?}")
    } else {
        let cps: Vec<String> = s.chars().take(12).map(|c| format!("U+{:04X}", c as u32)).collect();
        format!("{s:?} {}", cps.join(" "))
    }
}

fn ctrl_byte(c: char) -> Option<char> {
    match c.to_ascii_uppercase() {
        u @ '@'..='_' => Some((u as u8 - b'@') as char),
        ' ' => Some('\0'),
        '?' => Some('\u{7f}'),
        _ => None,
    }
}

fn key_bytes(name: &str) -> Option<&'static str> {
    Some(match name {
        "Escape" => "\u{1b}",
        "Tab" => "\t",
        "Enter" => "\r",
        "Backspace" => "\u{7f}",
        "ArrowUp" => "\u{1b}[A",
        "ArrowDown" => "\u{1b}[B",
        "ArrowRight" => "\u{1b}[C",
        "ArrowLeft" => "\u{1b}[D",
        _ => return None,
    })
}

/// The egui keys the terminal maps itself, by the names [`key_bytes`] and the key bar use.
fn terminal_key(key: Key) -> Option<&'static str> {
    Some(match key {
        Key::Escape => "Escape",
        Key::Tab => "Tab",
        Key::Enter => "Enter",
        Key::Backspace => "Backspace",
        Key::ArrowUp => "ArrowUp",
        Key::ArrowDown => "ArrowDown",
        Key::ArrowRight => "ArrowRight",
        Key::ArrowLeft => "ArrowLeft",
        _ => return None,
    })
}

impl App {
    fn new(path: Path) -> Self {
        Self {
            path,
            screen: String::new(),
            preedit: String::new(),
            ctrl: false,
            font: 16.0,
            textarea_focus: false,
            last_rect: None,
            noise: noise_report(),
        }
    }

    /// The bytes the host would get as `terminal_input`.
    fn send(&mut self, bytes: &str) {
        spike_log("pty", &format!("terminal_input {}", show(bytes)));
        for c in bytes.chars() {
            match c {
                '\u{7f}' => {
                    self.screen.pop();
                }
                '\r' => self.screen.push('\n'),
                c if (c as u32) < 0x20 => {
                    self.screen.push('^');
                    self.screen.push((c as u8 + b'@') as char);
                }
                c => self.screen.push(c),
            }
        }
        let excess = self.screen.chars().count().saturating_sub(4000);
        if excess > 0 {
            self.screen = self.screen.chars().skip(excess).collect();
        }
    }

    fn set_ctrl(&mut self, on: bool) {
        self.ctrl = on;
        spike_ctrl(on);
    }

    /// Committed text from either path; a latched Ctrl applies to its first character.
    fn text(&mut self, t: &str) {
        let mut chars = t.chars();
        if self.ctrl
            && let Some(first) = chars.next()
        {
            self.set_ctrl(false);
            match ctrl_byte(first) {
                Some(b) => self.send(&b.to_string()),
                None => self.send(&first.to_string()),
            }
            let rest = chars.as_str();
            if !rest.is_empty() {
                self.send(rest);
            }
        } else {
            self.send(t);
        }
    }

    fn named_key(&mut self, name: &str) {
        if name == "Ctrl" {
            self.set_ctrl(!self.ctrl);
        } else if let Some(b) = key_bytes(name) {
            if self.ctrl {
                self.set_ctrl(false);
            }
            self.send(b);
        } else {
            spike_log("app", &format!("no mapping for key {name:?}"));
        }
    }

    fn zoom(&mut self, factor: f32) {
        if factor.is_finite() && factor > 0.0 && factor != 1.0 {
            self.font = (self.font * factor).clamp(8.0, 48.0);
        }
    }

    fn drain_injected(&mut self) {
        let queued: Vec<Injected> = QUEUE.with_borrow_mut(|q| q.drain(..).collect());
        for ev in queued {
            match ev {
                Injected::Text(t) => {
                    spike_log("app", &format!("inject text {}", show(&t)));
                    self.preedit.clear();
                    self.text(&t);
                }
                Injected::Preedit(p) => {
                    spike_log("app", &format!("inject preedit {}", show(&p)));
                    self.preedit = p;
                }
                Injected::Key(k) => {
                    spike_log("app", &format!("inject key {k}"));
                    self.named_key(&k);
                }
                Injected::Focus(f) => self.textarea_focus = f,
                Injected::Zoom(z) => {
                    spike_log("app", &format!("inject zoom {z:.3}"));
                    self.zoom(z);
                }
            }
        }
    }

    /// Logs egui's view of the input (both paths) and, in the agent path, feeds the terminal from it.
    fn egui_events(&mut self, ui: &egui::Ui, focused: bool) {
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            match &ev {
                Event::PointerMoved(_) | Event::MouseMoved(_) | Event::PointerGone => continue,
                Event::Touch { phase: egui::TouchPhase::Move, .. } => continue,
                Event::Touch { id, phase, .. } => spike_log("egui", &format!("Touch {id:?} {phase:?}")),
                Event::Text(t) => spike_log("egui", &format!("Text {}", show(t))),
                Event::Ime(ImeEvent::Preedit { text, active_range_chars }) => {
                    spike_log("egui", &format!("Ime Preedit {} range={active_range_chars:?}", show(text)))
                }
                Event::Ime(ImeEvent::Commit(t)) => spike_log("egui", &format!("Ime Commit {}", show(t))),
                other => spike_log("egui", &format!("{other:?}")),
            }
            if self.path != Path::Agent || !focused {
                continue;
            }
            match ev {
                Event::Text(t) => self.text(&t),
                Event::Paste(t) => self.text(&t),
                Event::Ime(ImeEvent::Preedit { text, .. }) => self.preedit = text,
                Event::Ime(ImeEvent::Commit(t)) => {
                    self.preedit.clear();
                    self.text(&t);
                }
                Event::Ime(ImeEvent::DeleteSurrounding { before_chars, .. }) => {
                    self.send(&"\u{7f}".repeat(before_chars));
                }
                Event::Key { key, pressed: true, modifiers, .. } => {
                    let letter = key.name();
                    if let Some(name) = terminal_key(key) {
                        self.named_key(name);
                    } else if modifiers.ctrl && letter.len() == 1 {
                        // A hardware keyboard's Ctrl+letter: eframe sends no Text for it.
                        if let Some(b) = letter.chars().next().and_then(ctrl_byte) {
                            self.send(&b.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn terminal(&mut self, ui: &mut egui::Ui) {
        let size = Vec2::new(ui.available_width(), 180.0);
        let sense = if self.path == Path::Agent { Sense::click() } else { Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(size, sense);
        if self.path == Path::Agent && resp.clicked() {
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
        let focused = match self.path {
            Path::Agent => resp.has_focus(),
            Path::Textarea => self.textarea_focus,
        };
        if self.path == Path::Agent && focused {
            // Asks eframe to focus and place its hidden text input.
            let cursor = egui::Rect::from_min_size(rect.left_bottom() - Vec2::new(0.0, 24.0), Vec2::new(10.0, 18.0));
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    purpose: egui::IMEPurpose::Terminal,
                    should_interrupt_composition: false,
                    rect,
                    cursor_rect: cursor,
                })
            });
        }
        if self.path == Path::Textarea && self.last_rect != Some(rect) {
            self.last_rect = Some(rect);
            let z = ui.ctx().zoom_factor();
            spike_terminal_rect(rect.min.x * z, rect.min.y * z, rect.width() * z, rect.height() * z);
        }

        self.egui_events(ui, focused);
        // Pinch: eframe turns Safari gesture events and two-finger touches over the canvas into a zoom delta.
        let zoom = ui.input(|i| i.zoom_delta());
        if zoom != 1.0 {
            spike_log("egui", &format!("zoom_delta {zoom:.3} -> font {:.1}", self.font * zoom));
            self.zoom(zoom);
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, Color32::BLACK);
        let edge = if focused { Color32::LIGHT_GREEN } else { Color32::DARK_GRAY };
        painter.rect_stroke(rect, 4.0, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
        let font = egui::FontId::monospace(self.font);
        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = rect.width() - 12.0;
        job.append(&format!("$ {}", self.screen), 0.0, TextFormat::simple(font.clone(), Color32::LIGHT_GREEN));
        if !self.preedit.is_empty() {
            let mut f = TextFormat::simple(font.clone(), Color32::YELLOW);
            f.underline = Stroke::new(1.0, Color32::YELLOW);
            job.append(&self.preedit, 0.0, f);
        }
        if focused {
            job.append("\u{2588}", 0.0, TextFormat::simple(font, Color32::LIGHT_GREEN));
        }
        let galley = painter.layout_job(job);
        // Keep the newest line visible: bottom-align once the text is taller than the box.
        let top = (rect.bottom() - 6.0 - galley.size().y).min(rect.top() + 6.0);
        painter.galley(egui::pos2(rect.left() + 6.0, top), galley, Color32::LIGHT_GREEN);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_injected();
        egui::CentralPanel::default().show(ui, |ui| {
            self.terminal(ui);
            let path = match self.path {
                Path::Agent => "eframe text agent",
                Path::Textarea => "textarea overlay",
            };
            ui.label(format!(
                "input: {path} | font {:.0}px | Ctrl {}",
                self.font,
                if self.ctrl { "latched" } else { "off" }
            ));
            if !self.noise.is_empty() {
                ui.label(&self.noise);
            }
        });
    }
}

#[cfg(feature = "snow")]
fn noise_report() -> String {
    const P: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
    let run = || -> Result<usize, snow::Error> {
        let i = snow::Builder::new(P.parse()?);
        let r = snow::Builder::new(P.parse()?);
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

/// Called from `www/index.html` with the canvas id and the input path (`agent` or `textarea`).
#[wasm_bindgen]
pub async fn start(canvas_id: String, input: String) -> Result<(), JsValue> {
    use wasm_bindgen::JsCast;
    let path = if input == "textarea" { Path::Textarea } else { Path::Agent };
    let canvas = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(&canvas_id))
        .ok_or("canvas not found")?
        .dyn_into::<web_sys::HtmlCanvasElement>()?;
    eframe::WebRunner::new()
        .start(
            canvas,
            eframe::WebOptions::default(),
            Box::new(move |cc| {
                CTX.set(Some(cc.egui_ctx.clone()));
                Ok(Box::new(App::new(path)))
            }),
        )
        .await
}
