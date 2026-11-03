//! A terminal in the browser: the host's grid, never resized by looking at it, with the input the
//! `<textarea>` overlay forwards (design §4.4). The page puts the overlay over the rect this pane says
//! ([`Overlay::rect`]); everything typed, every key-bar key and every touch there arrives here as a
//! [`TermInput`], is encoded with the screen's latest modes (`atomics_ui::terminal::encode`) and typed
//! under the lease.
//!
//! - **The grid is the desktop's.** The pane pans and zooms it: the font fits the width until the user
//!   pinches, a wider grid pans sideways, and a grid taller than the pane keeps the cursor in view. "Fit
//!   to this screen" is an explicit resize, shown on the desktop, that lasts as long as the lease.
//! - **Touch.** A tap focuses the overlay (the browser opens the keyboard) and, where the program takes
//!   mouse reports and this client types, clicks the cell; a drag scrolls history (this client's view
//!   only) and pans; a long press selects, with Copy; a pinch sets the font size. A mouse clicks and
//!   drag-selects; its wheel scrolls.
//! - **Typing needs the lease**, taken when the overlay gains focus and only where `welcome` grants
//!   `type`. Losing it to the desktop or another client blurs the overlay, so typing again is a new tap.
//! - **Offline** the screen shows as it was, marked disconnected, and nothing is typed.

use atomics_core::lease::{GridSize, MAX_CELL_PIXELS};
use atomics_core::ops::Grant;
use atomics_core::screen::{Modes, Screen};
use atomics_ui::terminal::{self as painter, Metrics, Point, Selection, encode};
use atomics_ui::{Backend, Client, approvals};
use egui::{Align2, Key, Modifiers, Pos2, Rect, RichText, Sense, Vec2, pos2, vec2};

use crate::gesture::{Gesture, Pointer, Recognizer};
use crate::remote::RemoteBackend;
use crate::screens::{Lease, Lost};

/// The font size range a pinch moves in, and the fitted default's.
pub const MIN_FONT: f32 = 5.0;
pub const MAX_FONT: f32 = 28.0;
const FIT_MIN: f32 = 7.0;
const FIT_MAX: f32 = 14.0;
/// Space between the pane's edge and the cells.
const PAD: f32 = 4.0;
/// How often the lease is renewed while the user stays at the terminal without typing.
const RENEW_MS: f64 = 10_000.0;

/// What the overlay forwards.
#[derive(Debug, Clone, PartialEq)]
pub enum TermInput {
    /// Committed text: typed, dictated or composed.
    Text(String),
    /// The composition in progress; empty when it ends.
    Preedit(String),
    /// A named key (`Escape`, `ArrowUp`, `PageDown`, …, or `Ctrl` and `Alt`, the key bar's sticky
    /// modifiers), or one character with Ctrl from a hardware keyboard.
    Key {
        name: String,
        mods: Modifiers,
    },
    Paste(String),
    Focus(bool),
    Pointer(Pointer),
    /// A wheel over the terminal, in points; `zoom` when Ctrl was held.
    Wheel {
        delta: Vec2,
        at: Pos2,
        zoom: bool,
    },
    /// The page copied the selection.
    Copied,
}

/// What the page should make of the overlay after a frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overlay {
    /// Over the terminal's cells, while one shows.
    pub rect: Option<Rect>,
    /// May be typed into: a device that may type, connected, and nobody else typing.
    pub writable: bool,
    /// The key bar's sticky modifiers.
    pub ctrl: bool,
    pub alt: bool,
    /// A selection to offer Copy for, and where.
    pub copy: Option<(String, Pos2)>,
    /// Take the focus away: the lease went elsewhere.
    pub blur: bool,
}

/// One line and column of the terminal: `line` is a line ID (history below the screen's first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Cell {
    line: u64,
    col: u16,
}

/// A terminal pane's own state: the view (font, panning, history), the selection, the composition, the
/// sticky modifiers and the gestures in progress.
pub struct TermPane {
    session: Option<String>,
    /// The font size the user pinched to; `None` fits the width.
    pub font: Option<f32>,
    pan_x: f32,
    /// The top line while scrolled back; `None` follows the screen.
    top: Option<u64>,
    scroll_rest: f32,
    selection: Option<(Cell, Cell)>,
    preedit: String,
    ctrl: bool,
    alt: bool,
    focused: bool,
    held: bool,
    gestures: Recognizer,
    view: Screen,
    /// When the lease was last used or renewed.
    active_ms: f64,
    /// What the overlay forwarded since the last frame.
    pub inputs: Vec<TermInput>,
    pub overlay: Overlay,
    /// This frame drew a terminal.
    drawn: bool,
}

impl Default for TermPane {
    fn default() -> Self {
        Self {
            session: None,
            font: None,
            pan_x: 0.0,
            top: None,
            scroll_rest: 0.0,
            selection: None,
            preedit: String::new(),
            ctrl: false,
            alt: false,
            focused: false,
            held: false,
            gestures: Recognizer::default(),
            view: Screen::blank(0, 0),
            active_ms: 0.0,
            inputs: Vec::new(),
            overlay: Overlay::default(),
            drawn: false,
        }
    }
}

/// The geometry of one frame: where the cells are and which lines they show.
#[derive(Debug, Clone, Copy)]
struct Frame {
    rect: Rect,
    origin: Pos2,
    /// The font size in use, and its cell.
    size: f32,
    cell: Vec2,
    /// The live screen's first line.
    first: u64,
    top: u64,
    rows: u16,
    cols: u16,
    live_top: u64,
    oldest: u64,
}

impl Frame {
    fn cell_at(&self, pos: Pos2) -> Cell {
        let col = ((pos.x - self.origin.x) / self.cell.x).floor().clamp(0.0, f32::from(self.cols.max(1) - 1));
        let row = ((pos.y - self.origin.y) / self.cell.y).floor().clamp(0.0, f32::from(self.rows.max(1) - 1));
        Cell { line: self.top + row as u64, col: col as u16 }
    }

    /// The live screen's row of `cell`, if it is on the live screen.
    fn screen_row(&self, cell: Cell) -> Option<u16> {
        cell.line.checked_sub(self.first).map(|row| row as u16)
    }
}

/// Whether `key`'s name is one the overlay sends.
pub fn named_key(name: &str) -> Option<Key> {
    Some(match name {
        "Escape" => Key::Escape,
        "Tab" => Key::Tab,
        "Enter" => Key::Enter,
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Insert" => Key::Insert,
        "ArrowUp" => Key::ArrowUp,
        "ArrowDown" => Key::ArrowDown,
        "ArrowLeft" => Key::ArrowLeft,
        "ArrowRight" => Key::ArrowRight,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        _ => return None,
    })
}

/// What a key from the overlay sends, with the sticky modifiers added to its own.
pub fn key_text(name: &str, mods: Modifiers, (ctrl, alt): (bool, bool), modes: &Modes) -> Option<String> {
    let mods = Modifiers { ctrl: mods.ctrl || ctrl, alt: mods.alt || alt, ..mods };
    if let Some(key) = named_key(name) {
        return encode::key(key, mods, modes).and_then(|bytes| String::from_utf8(bytes).ok());
    }
    let mut chars = name.chars();
    let (Some(_), None) = (chars.next(), chars.next()) else { return None };
    Some(encode::text(name, mods.ctrl, mods.alt))
}

impl TermPane {
    /// Start over for another terminal: the view follows it; the font stays.
    fn switch(&mut self, session: &str) {
        if self.session.as_deref() != Some(session) {
            self.session = Some(session.to_string());
            self.top = None;
            self.pan_x = 0.0;
            self.scroll_rest = 0.0;
            self.selection = None;
            self.preedit.clear();
            self.held = false;
        }
    }

    /// Whether the overlay has the focus: the user is at the keyboard.
    pub fn typing(&self) -> bool {
        self.focused
    }

    pub fn begin_frame(&mut self) {
        self.drawn = false;
    }

    /// After a frame that drew no terminal: the overlay goes, the focus with it, and what it forwarded
    /// is dropped, never typed later.
    pub fn end_frame(&mut self) {
        if self.drawn {
            return;
        }
        self.inputs.clear();
        self.preedit.clear();
        let blur = std::mem::take(&mut self.focused);
        self.overlay = Overlay { blur, ..Overlay::default() };
    }

    /// When the pane wants a frame without input: a long press coming due, or a lease to renew.
    pub fn due_ms(&self) -> Option<f64> {
        let renew = (self.held && self.focused).then_some(self.active_ms + RENEW_MS);
        self.gestures.due_ms().into_iter().chain(renew).reduce(f64::min)
    }

    /// Draw `session`'s terminal into `rect`, with its header and any approval it waits on, after
    /// carrying out what the overlay forwarded.
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        session: &str,
        remote: &RemoteBackend,
        client: &mut Client,
        now_ms: f64,
    ) {
        self.switch(session);
        self.drawn = true;
        remote.show_screen(session, true);
        let p = client.palette.clone();
        let lease = remote.lease(session);
        let may_type = remote.may(Grant::Type);
        let live = remote.live();

        // Header: who types, and what this client may do about it.
        let header = Rect::from_min_size(rect.min, vec2(rect.width(), 30.0));
        let mut actions = Actions::default();
        let mut child = ui.new_child(
            egui::UiBuilder::new().id_salt(("term-header", session)).max_rect(header.shrink2(vec2(8.0, 3.0))),
        );
        child.horizontal(|ui| self.header(ui, &lease, may_type, live, &p, &mut actions));
        let mut body = rect.with_min_y(header.bottom());

        // The approval this session waits on, as the desktop shows it over its terminal.
        let backend: &dyn Backend = remote;
        if let Some(index) = backend.session_list().iter().position(|s| s.id == session) {
            body = approvals::banner(ui, body, index, backend, client);
        }

        let Some(frame) = self.frame(ui, body, session, remote, &p) else {
            ui.painter().rect_filled(body, 0.0, p.terminal);
            let text = if live { "Waiting for the terminal…" } else { "Not connected" };
            ui.painter().text(body.center(), Align2::CENTER_CENTER, text, p.ui(13.0), p.muted);
            self.inputs.clear();
            self.overlay = Overlay { blur: std::mem::take(&mut self.focused), ..Overlay::default() };
            return;
        };

        let modes = remote.with_screen(session, |s| s.map(|(screen, _)| screen.modes)).unwrap_or_default();
        let mut typed = String::new();
        for input in std::mem::take(&mut self.inputs) {
            self.input(input, &frame, &modes, &lease, remote, session, &mut typed, now_ms);
        }
        for gesture in self.gestures.tick(now_ms) {
            self.gesture(gesture, &frame, &modes, &lease, &mut typed);
        }
        if !typed.is_empty() && live && may_type {
            self.top = None;
            if !remote.type_text(session, &typed) {
                remote.say("Someone else is typing in this terminal. Take over to type here.");
            }
            self.active_ms = now_ms;
        }
        if actions.take_over {
            remote.acquire(session, true);
        }
        if actions.live {
            self.top = None;
        }
        if let Some(size) = actions.fit {
            remote.resize(session, size.then(|| fit_size(&frame)));
        }

        // Losing the lease ends this round of typing: typing again is a new tap.
        let lease = remote.lease(session);
        let lost = self.held && !lease.yours;
        self.held = lease.yours;
        if lease.yours && self.focused && now_ms - self.active_ms >= RENEW_MS {
            remote.renew(session);
            self.active_ms = now_ms;
        }

        let frame = self.frame(ui, body, session, remote, &p).unwrap_or(frame);
        self.paint(ui, &frame, session, remote, &p, lease.yours);

        let writable = live && may_type && (lease.yours || lease.holder.is_none());
        let copy = self.selection.and_then(|(a, b)| {
            let text = self.selection_text(a, b);
            let at = frame.origin + vec2(0.0, (a.line.min(b.line).saturating_sub(frame.top)) as f32 * frame.cell.y);
            (!text.is_empty()).then(|| (text, pos2(frame.rect.center().x, at.y.max(frame.rect.top()))))
        });
        self.overlay = Overlay {
            rect: Some(frame.rect),
            writable,
            ctrl: self.ctrl,
            alt: self.alt,
            copy,
            blur: (lost && self.focused) || (!writable && self.focused),
        };
        if self.overlay.blur {
            self.focused = false;
        }
    }

    fn header(
        &self,
        ui: &mut egui::Ui,
        lease: &Lease,
        may_type: bool,
        live: bool,
        p: &atomics_ui::theme::Palette,
        actions: &mut Actions,
    ) {
        let (text, color) = if !live {
            ("Disconnected: typing is off".to_string(), p.warning)
        } else if !may_type {
            ("View only".to_string(), p.muted)
        } else if lease.yours {
            ("You are typing".to_string(), p.success)
        } else if let Some(holder) = &lease.holder {
            (format!("Typing from {holder}"), p.warning)
        } else {
            match lease.lost {
                Some(Lost::Desktop) => ("The computer took this terminal back. Tap to type.".to_string(), p.muted),
                Some(Lost::Expired) => ("Tap the terminal to type".to_string(), p.muted),
                _ if lease.acquiring => ("Asking to type…".to_string(), p.muted),
                _ => ("Tap the terminal to type".to_string(), p.muted),
            }
        };
        ui.label(RichText::new(text).color(color).size(12.5));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if self.top.is_some() && ui.small_button("↓ Live").on_hover_text("Back to the live screen").clicked() {
                actions.live = true;
            }
            if !live || !may_type {
                return;
            }
            if lease.yours {
                if lease.resized {
                    if ui.small_button("Computer's size").clicked() {
                        actions.fit = Some(false);
                    }
                } else if ui
                    .small_button("Fit to this screen")
                    .on_hover_text("Resize the terminal to this screen; the computer shows it, and gets its size back when it types")
                    .clicked()
                {
                    actions.fit = Some(true);
                }
            } else if lease.holder.is_some() && ui.small_button("Take over").clicked() {
                actions.take_over = true;
            }
        });
    }

    /// The cell size at the font in use, and which lines show where.
    fn frame(
        &mut self,
        ui: &egui::Ui,
        body: Rect,
        session: &str,
        remote: &RemoteBackend,
        p: &atomics_ui::theme::Palette,
    ) -> Option<Frame> {
        let (cols, rows, first, cursor_row, oldest) = remote
            .with_screen(session, |s| s.map(|(s, _)| (s.cols, s.rows, s.first_line, s.cursor.row, s.oldest_line())))?;
        let ratio = ui.fonts_mut(|f| f.glyph_width(&p.mono(100.0), 'M')) / 100.0;
        let fit = (body.width() - 2.0 * PAD) / (f32::from(cols) * ratio.max(0.01));
        let size = self.font.unwrap_or(fit.clamp(FIT_MIN, FIT_MAX)).clamp(MIN_FONT, MAX_FONT);
        let font = p.mono(size);
        let cell = ui.fonts_mut(|f| vec2(f.glyph_width(&font, 'M'), f.row_height(&font).ceil()));
        let fit_rows = (((body.height() - 2.0 * PAD) / cell.y).floor().max(1.0)) as u16;
        let shown = fit_rows.min(rows);
        // The lowest window that still shows the cursor, with a line under it where there is one.
        let below = (u64::from(cursor_row) + 2).saturating_sub(u64::from(shown));
        let live_top = first + below.min(u64::from(rows - shown));
        let top = self.top.map_or(live_top, |top| top.clamp(oldest, live_top));
        if self.top.is_some_and(|t| t >= live_top) {
            self.top = None;
        }
        let width = f32::from(cols) * cell.x;
        self.pan_x = self.pan_x.clamp(0.0, (width - (body.width() - 2.0 * PAD)).max(0.0));
        let origin = body.min + vec2(PAD - self.pan_x, PAD);
        Some(Frame { rect: body, origin, size, cell, first, top, rows: shown, cols, live_top, oldest })
    }

    #[allow(clippy::too_many_arguments)]
    fn input(
        &mut self,
        input: TermInput,
        frame: &Frame,
        modes: &Modes,
        lease: &Lease,
        remote: &RemoteBackend,
        session: &str,
        typed: &mut String,
        now_ms: f64,
    ) {
        match input {
            TermInput::Text(text) => {
                typed.push_str(&encode::text(&text, self.ctrl, self.alt));
                (self.ctrl, self.alt) = (false, false);
            }
            TermInput::Preedit(text) => self.preedit = text,
            TermInput::Key { name, .. } if name == "Ctrl" => self.ctrl = !self.ctrl,
            TermInput::Key { name, .. } if name == "Alt" => self.alt = !self.alt,
            TermInput::Key { name, mods } => {
                if let Some(text) = key_text(&name, mods, (self.ctrl, self.alt), modes) {
                    typed.push_str(&text);
                }
                (self.ctrl, self.alt) = (false, false);
            }
            TermInput::Paste(text) => typed.push_str(&encode::paste(&text, modes)),
            TermInput::Focus(true) => {
                self.focused = true;
                self.active_ms = now_ms;
                if !lease.yours && lease.holder.is_none() {
                    remote.acquire(session, false);
                }
            }
            TermInput::Focus(false) => {
                self.focused = false;
                self.preedit.clear();
            }
            TermInput::Pointer(pointer) => {
                for gesture in self.gestures.event(&pointer) {
                    self.gesture(gesture, frame, modes, lease, typed);
                }
            }
            TermInput::Wheel { delta, at, zoom } => {
                if zoom {
                    self.zoom((-delta.y / 200.0).exp(), at, frame);
                } else {
                    self.scroll(-delta.y, at, frame, modes, lease, typed);
                }
            }
            TermInput::Copied => self.selection = None,
        }
    }

    fn gesture(&mut self, gesture: Gesture, frame: &Frame, modes: &Modes, lease: &Lease, typed: &mut String) {
        match gesture {
            Gesture::Tap(at) => {
                if self.selection.take().is_some() {
                    return;
                }
                if lease.yours && encode::reports_mouse(modes) {
                    let cell = frame.cell_at(at);
                    if let Some(row) = frame.screen_row(cell) {
                        for pressed in [true, false] {
                            if let Some(report) =
                                encode::mouse(encode::Button::Left, pressed, cell.col, row, Modifiers::NONE, modes)
                            {
                                typed.push_str(&report);
                            }
                        }
                    }
                }
            }
            Gesture::Drag(delta) => {
                self.pan_x -= delta.x;
                self.scroll(delta.y, frame.rect.center(), frame, modes, lease, typed);
            }
            Gesture::Pinch { factor, center } => self.zoom(factor, center, frame),
            Gesture::Select { from, to, .. } => {
                self.selection = Some((frame.cell_at(from), frame.cell_at(to)));
            }
        }
    }

    /// The font size times `factor`, keeping the column under `at` where it is.
    fn zoom(&mut self, factor: f32, at: Pos2, frame: &Frame) {
        let current = frame.size;
        let next = (current * factor).clamp(MIN_FONT, MAX_FONT);
        let scale = next / current;
        let x = at.x - frame.rect.left() - PAD + self.pan_x;
        self.pan_x = x * scale - (at.x - frame.rect.left() - PAD);
        self.font = Some(next);
    }

    /// Scroll by `dy` points (positive: toward older lines). A program in the alternate screen that
    /// asked for it gets arrows, and one that takes mouse reports wheel reports, as on the desktop;
    /// both only while this client types. Otherwise this client's view moves through history.
    fn scroll(&mut self, dy: f32, at: Pos2, frame: &Frame, modes: &Modes, lease: &Lease, typed: &mut String) {
        self.scroll_rest += dy / frame.cell.y;
        let lines = self.scroll_rest.trunc() as i64;
        self.scroll_rest -= lines as f32;
        if lines == 0 {
            return;
        }
        let count = lines.unsigned_abs() as usize;
        if lease.yours && encode::reports_mouse(modes) && modes.sgr_mouse {
            let cell = frame.cell_at(at);
            let row = frame.screen_row(cell).unwrap_or(0);
            let button = if lines > 0 { encode::Button::WheelUp } else { encode::Button::WheelDown };
            if let Some(report) = encode::mouse(button, true, cell.col, row, Modifiers::NONE, modes) {
                typed.push_str(&report.repeat(count));
            }
            return;
        }
        if lease.yours && modes.alt_screen && modes.alternate_scroll {
            typed.push_str(&(if lines > 0 { "\x1bOA" } else { "\x1bOB" }).repeat(count));
            return;
        }
        let top = frame.top as i64 - lines;
        let top = top.clamp(frame.oldest as i64, frame.live_top as i64) as u64;
        self.top = (top < frame.live_top).then_some(top);
    }

    fn paint(
        &mut self,
        ui: &mut egui::Ui,
        frame: &Frame,
        session: &str,
        remote: &RemoteBackend,
        p: &atomics_ui::theme::Palette,
        yours: bool,
    ) {
        remote.fill_screen(session, frame.top, frame.rows, &mut self.view);
        let metrics = Metrics { font: p.mono(frame.size), cell: frame.cell };
        let inner = Rect::from_min_size(
            frame.origin,
            vec2(f32::from(frame.cols) * frame.cell.x, f32::from(frame.rows) * frame.cell.y),
        );
        let selection = self.selection.map(|(a, b)| {
            let (a, b) = if a <= b { (a, b) } else { (b, a) };
            let row = |c: Cell| c.line as i64 - frame.top as i64;
            Selection {
                start: Point { row: row(a).clamp(i32::MIN as i64, i32::MAX as i64) as i32, col: a.col },
                end: Point { row: row(b).clamp(i32::MIN as i64, i32::MAX as i64) as i32, col: b.col },
                block: false,
            }
        });
        let theme = p.terminal_theme();
        painter::paint(
            ui.painter(),
            (frame.rect, inner),
            &self.view,
            selection.as_ref(),
            &metrics,
            &theme,
            self.focused && yours,
        );
        let stale = remote.with_screen(session, |s| s.is_some_and(|(_, stale)| stale));
        if stale || !remote.live() {
            ui.painter().rect_filled(frame.rect, 0.0, p.terminal.gamma_multiply(0.55));
            ui.painter().text(
                frame.rect.center_top() + vec2(0.0, 18.0),
                Align2::CENTER_CENTER,
                "Disconnected",
                p.ui(13.0),
                p.warning,
            );
        }
        // The composition, over the cursor's cell, until it is committed.
        if !self.preedit.is_empty() && self.view.cursor.shape != atomics_core::screen::CursorShape::Hidden {
            let at = frame.origin
                + vec2(f32::from(self.view.cursor.col) * frame.cell.x, f32::from(self.view.cursor.row) * frame.cell.y);
            let galley = ui.painter().layout_no_wrap(self.preedit.clone(), metrics.font.clone(), p.foreground);
            let back = Rect::from_min_size(at, vec2(galley.size().x.max(frame.cell.x), frame.cell.y));
            ui.painter().rect_filled(back, 2.0, p.raised);
            ui.painter().galley(at, galley, p.foreground);
            ui.painter().line_segment([back.left_bottom(), back.right_bottom()], egui::Stroke::new(1.5, p.accent));
        }
        ui.allocate_rect(frame.rect, Sense::hover());
    }

    /// The selected text, line by line; a line that wraps onto the next joins it.
    fn selection_text(&self, a: Cell, b: Cell) -> String {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let mut out = String::new();
        let top = self.view.first_line;
        for (row, line) in self.view.lines.iter().enumerate() {
            let id = top + row as u64;
            if id < a.line || id > b.line {
                continue;
            }
            let from = if id == a.line { usize::from(a.col) } else { 0 };
            let to = if id == b.line { usize::from(b.col) + 1 } else { line.cells.len() };
            let mut text = String::new();
            for cell in line.cells.iter().take(to.min(line.cells.len())).skip(from).filter(|c| c.width > 0) {
                text.push(if cell.c == '\0' { ' ' } else { cell.c });
                text.extend(cell.combining.iter().flat_map(|c| c.iter()));
            }
            if id != b.line && line.wrapped {
                out.push_str(&text);
            } else {
                out.push_str(text.trim_end());
                if id != b.line {
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// The grid that fits the pane at the font in use.
fn fit_size(frame: &Frame) -> GridSize {
    let cols = ((frame.rect.width() - 2.0 * PAD) / frame.cell.x).floor().max(2.0) as u16;
    let rows = ((frame.rect.height() - 2.0 * PAD) / frame.cell.y).floor().max(1.0) as u16;
    let pixels = |v: f32| Some((v.round() as u16).clamp(1, MAX_CELL_PIXELS));
    GridSize { cols, rows, cell_width: pixels(frame.cell.x), cell_height: pixels(frame.cell.y) }
}

#[derive(Default)]
struct Actions {
    take_over: bool,
    live: bool,
    /// Fit to this screen (`true`) or back to the computer's size.
    fit: Option<bool>,
}

#[cfg(test)]
mod tests;
