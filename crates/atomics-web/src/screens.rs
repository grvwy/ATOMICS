//! Terminals over `atomics.v1`, the browser's side: each followed screen's mirror, its lease and typing
//! (`atomics_core::lease::Typist`), and the history lines kept while the user scrolls back. I/O free: the
//! [`RemoteBackend`](crate::remote::RemoteBackend) routes frames here and sends what this queues.
//!
//! - **Following.** A screen is subscribed while the UI shows it, focused (the faster cadence); a screen
//!   no frame showed is unsubscribed. A gap asks for that screen's snapshot alone.
//! - **Typing needs the lease.** Taking it is the user's act (starting to type); taking it from another
//!   client is a takeover the user asks for. Text typed while the grant is on its way waits for it, on this
//!   connection only, and is dropped if the lease goes elsewhere. Input refused as `slow_client` is typed
//!   again, before anything newer, at the screen's next change: the program read something.
//! - **No replay.** A disconnect forgets the lease, the waiting text and whatever was not known to be
//!   applied (`Typist::disconnected`). The screen stays on view as it was, marked stale, until the next
//!   snapshot.
//! - **History.** Lines that scroll off the top are kept as they go; older ones are asked for with
//!   `history_get` when the view reaches them. A snapshot starts the history over.

use std::collections::{BTreeMap, HashMap};

use atomics_core::lease::{GridSize, LeaseReason, LeaseState, Typist};
use atomics_core::screen::{
    HistoryChunk, HistoryGet, Line, Screen, ScreenApplied, ScreenDelta, ScreenGap, ScreenMirror, ScreenSnapshot,
};
use atomics_core::wire::{Envelope, ErrorCode, Message, Seq, Topic};

/// Text typed before a grant arrived that waits for it; more is dropped.
pub const MAX_WAITING_BYTES: usize = 16 * 1024;
/// History lines kept per terminal; the oldest go first.
const MAX_HISTORY_KEPT: usize = 5000;
/// Lines one `history_get` asks for.
const HISTORY_PAGE: u64 = 200;

/// Who types into a terminal, as this client knows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lease {
    /// A paired device's name or a client's id; `None` while nobody types (the desktop types without one).
    pub holder: Option<String>,
    /// This client holds it.
    pub yours: bool,
    /// The holder's own size is in force.
    pub resized: bool,
    /// This client asked for it and waits for the answer.
    pub acquiring: bool,
    /// Why this client last lost it or was refused it, until it asks again.
    pub lost: Option<Lost>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lost {
    /// Someone typed on the computer.
    Desktop,
    /// Another client took it over.
    TakenOver,
    /// Nothing was typed for the lease's time.
    Expired,
    /// Another client holds it.
    Held,
    /// The terminal went away.
    Gone,
}

enum Request {
    Input { session: String, seq: u64 },
    Lease(String),
    History(String),
}

#[derive(Default)]
struct Term {
    mirror: ScreenMirror,
    typist: Typist,
    /// Subscribed on this connection, and whether as focused.
    subscribed: Option<bool>,
    /// A frame showed it since the last [`Screens::end_frame`].
    shown: bool,
    acquiring: bool,
    /// Text that waits: for the grant, or (with `retype`) to be typed again after `slow_client`.
    waiting: String,
    retype: bool,
    resyncing: bool,
    /// The screen held is from before a disconnect.
    stale: bool,
    lost: Option<Lost>,
    /// History lines by ID, contiguous and ending at the screen's `first_line`.
    history: BTreeMap<u64, Line>,
    history_asked: bool,
}

impl Term {
    fn new(session: &str) -> Self {
        Self { typist: Typist::new(session), ..Self::default() }
    }

    fn screen(&self) -> Option<&Screen> {
        self.mirror.screen.as_ref()
    }

    /// Keep the lines a delta scrolls off the top, when they follow the history held.
    fn keep_scrolled(&mut self, top: Vec<Line>, from: u64) {
        let follows = self.history.last_key_value().is_none_or(|(last, _)| last + 1 == from);
        if !follows {
            self.history.clear();
        }
        for (i, line) in top.into_iter().enumerate() {
            self.history.insert(from + i as u64, line);
        }
        self.trim_history();
    }

    fn trim_history(&mut self) {
        let oldest = self.screen().map_or(0, Screen::oldest_line);
        self.history.retain(|id, _| *id >= oldest);
        while self.history.len() > MAX_HISTORY_KEPT {
            self.history.pop_first();
        }
    }
}

#[derive(Default)]
pub struct Screens {
    terms: BTreeMap<String, Term>,
    /// Requests a refusal can name: input by its number, a lease request, a history page.
    requests: HashMap<String, Request>,
    next_request: u64,
}

impl Screens {
    fn request(&mut self, request: Request) -> String {
        self.next_request += 1;
        let id = format!("t-{}", self.next_request);
        self.requests.insert(id.clone(), request);
        id
    }

    fn term(&mut self, session: &str) -> &mut Term {
        self.terms.entry(session.to_string()).or_insert_with(|| Term::new(session))
    }

    /// The UI shows `session`'s terminal this frame. While `live` (a channel with `welcome`), it is
    /// subscribed; `focused` streams it at the faster cadence.
    pub fn show(&mut self, session: &str, focused: bool, live: bool, out: &mut Vec<Envelope>) {
        let term = self.term(session);
        term.shown = true;
        if live && term.subscribed != Some(focused) {
            term.subscribed = Some(focused);
            let topics = vec![Topic::Screen { session: session.to_string(), focused }];
            out.push(Envelope::new(Message::Subscribe { topics }));
        }
    }

    /// After a frame: stop following the screens it did not show, letting go of their leases.
    pub fn end_frame(&mut self, out: &mut Vec<Envelope>) {
        for (session, term) in &mut self.terms {
            if !std::mem::take(&mut term.shown) && term.subscribed.take().is_some() {
                if let Some(release) = term.typist.release() {
                    out.push(Envelope::new(release));
                }
                term.acquiring = false;
                term.waiting.clear();
                term.retype = false;
                let topics = vec![Topic::Screen { session: session.clone(), focused: false }];
                out.push(Envelope::new(Message::Unsubscribe { topics }));
            }
        }
    }

    /// The screen as last received, whether it is from before a disconnect, and who types.
    pub fn view(&self, session: &str) -> Option<(&Screen, bool)> {
        let term = self.terms.get(session)?;
        Some((term.screen()?, term.stale))
    }

    pub fn lease(&self, session: &str) -> Lease {
        let Some(term) = self.terms.get(session) else { return Lease::default() };
        let state = term.typist.state.as_ref();
        Lease {
            holder: state.and_then(|s| s.holder.clone()),
            yours: term.typist.may_type(),
            resized: state.is_some_and(|s| s.resized),
            acquiring: term.acquiring,
            lost: term.lost,
        }
    }

    /// Ask for the lease. `takeover` displaces another client: only for the user's explicit act.
    pub fn acquire(&mut self, session: &str, takeover: bool, out: &mut Vec<Envelope>) {
        let term = self.term(session);
        if term.typist.may_type() || term.acquiring || term.subscribed.is_none() {
            return;
        }
        term.acquiring = true;
        term.lost = None;
        let message = term.typist.acquire(takeover);
        let id = self.request(Request::Lease(session.to_string()));
        out.push(Envelope::new(message).reply_to(Some(id)));
    }

    /// Type `data` (already encoded for the terminal's modes). Without the lease, a free one is asked for
    /// and the text waits for the grant; with another client typing it is dropped (`false`).
    pub fn type_text(&mut self, session: &str, data: &str, out: &mut Vec<Envelope>) -> bool {
        if data.is_empty() {
            return true;
        }
        let term = self.term(session);
        if term.subscribed.is_none() {
            return false;
        }
        if term.typist.may_type() && term.waiting.is_empty() {
            self.send(session, data, out);
            return true;
        }
        let free = term.typist.state.as_ref().is_none_or(|s| s.holder.is_none());
        if !term.typist.may_type() && !term.acquiring && !free {
            return false;
        }
        if term.waiting.len() + data.len() > MAX_WAITING_BYTES {
            return false;
        }
        term.waiting.push_str(data);
        if !term.typist.may_type() {
            self.acquire(session, false, out);
        }
        true
    }

    fn send(&mut self, session: &str, data: &str, out: &mut Vec<Envelope>) {
        let messages = self.term(session).typist.input(data);
        for message in messages {
            let id = match &message {
                Message::TerminalInput { seq, .. } => {
                    Some(self.request(Request::Input { session: session.to_string(), seq: seq.0 }))
                }
                _ => None,
            };
            out.push(Envelope::new(message).reply_to(id));
        }
    }

    /// Keep the lease while the user is at the terminal.
    pub fn renew(&mut self, session: &str, out: &mut Vec<Envelope>) {
        if let Some(renew) = self.terms.get(session).and_then(|t| t.typist.renew()) {
            out.push(Envelope::new(renew));
        }
    }

    /// "Fit to this screen" with a size; back to the computer's size with `None`. Needs the lease.
    pub fn resize(&mut self, session: &str, size: Option<GridSize>, out: &mut Vec<Envelope>) {
        if let Some(resize) = self.terms.get(session).and_then(|t| t.typist.resize(size)) {
            out.push(Envelope::new(resize));
        }
    }

    /// Fill `view` with `rows` lines from line ID `top` on: history, then the live screen. Lines not held
    /// yet are blank and asked for. `None` without a screen.
    pub fn fill(
        &mut self,
        session: &str,
        top: u64,
        rows: u16,
        view: &mut Screen,
        out: &mut Vec<Envelope>,
    ) -> Option<()> {
        let term = self.terms.get_mut(session)?;
        let screen = term.mirror.screen.as_ref()?;
        let (first, oldest) = (screen.first_line, screen.oldest_line());
        view.cols = screen.cols;
        view.rows = rows;
        view.modes = screen.modes;
        view.title.clone_from(&screen.title);
        view.palette.clone_from(&screen.palette);
        view.first_line = top;
        view.history = 0;
        view.lines.resize(usize::from(rows), Line::blank(screen.cols));
        let mut missing = false;
        for (row, line) in view.lines.iter_mut().enumerate() {
            let id = top + row as u64;
            let source = if id >= first {
                screen.lines.get((id - first) as usize)
            } else {
                let held = term.history.get(&id);
                missing |= held.is_none() && id >= oldest;
                held
            };
            match source {
                Some(source) => line.clone_from(source),
                None => *line = Line::blank(screen.cols),
            }
        }
        let cursor_id = first + u64::from(screen.cursor.row);
        view.cursor = screen.cursor;
        if cursor_id < top || cursor_id >= top + u64::from(rows) {
            view.cursor.shape = atomics_core::screen::CursorShape::Hidden;
        } else {
            view.cursor.row = (cursor_id - top) as u16;
        }
        if missing && !term.history_asked && term.subscribed.is_some() {
            let before = term.history.first_key_value().map_or(first, |(id, _)| *id);
            let count = before.saturating_sub(oldest).min(HISTORY_PAGE);
            if count > 0 {
                term.history_asked = true;
                let generation = Seq(term.mirror.generation);
                let message = Message::HistoryGet(HistoryGet {
                    session: session.to_string(),
                    generation,
                    before: Seq(before),
                    count: count as u32,
                });
                let id = self.request(Request::History(session.to_string()));
                out.push(Envelope::new(message).reply_to(Some(id)));
            }
        }
        Some(())
    }

    /// The channel ended: the leases with it, and anything typed that waits. Screens stay, stale.
    pub fn disconnected(&mut self) {
        self.requests.clear();
        for term in self.terms.values_mut() {
            term.typist.disconnected();
            term.subscribed = None;
            term.acquiring = false;
            term.waiting.clear();
            term.retype = false;
            term.resyncing = false;
            term.history_asked = false;
            term.stale = term.mirror.screen.is_some();
        }
    }

    /// A frame for the terminals; `false` when it is not one.
    pub fn receive(&mut self, envelope: &Envelope, out: &mut Vec<Envelope>) -> bool {
        match &envelope.message {
            Message::ScreenSnapshot(snapshot) => self.snapshot(snapshot, out),
            Message::ScreenDelta(delta) => self.delta(delta, out),
            Message::HistoryChunk(chunk) => self.chunk(chunk, envelope.request_id.as_deref()),
            Message::LeaseState(state) => self.lease_state(state, out),
            Message::Error { code, .. } => {
                let Some(request) = envelope.request_id.as_deref().and_then(|id| self.requests.remove(id)) else {
                    return false;
                };
                self.refused(request, *code);
            }
            _ => return false,
        }
        true
    }

    fn gap(&mut self, session: &str, out: &mut Vec<Envelope>) {
        let term = self.term(session);
        if !term.resyncing {
            term.resyncing = true;
            let message = Message::Resync { reason: Some("gap".into()), screens: vec![session.to_string()] };
            out.push(Envelope::new(message));
        }
    }

    fn snapshot(&mut self, snapshot: &ScreenSnapshot, out: &mut Vec<Envelope>) {
        let Some(term) = self.terms.get_mut(&snapshot.session) else { return };
        match term.mirror.apply_snapshot(snapshot) {
            Ok(ScreenApplied::Changed) => {
                term.resyncing = false;
                term.stale = false;
                term.history.clear();
                term.history_asked = false;
            }
            Ok(_) => {}
            Err(_) => self.gap(&snapshot.session.clone(), out),
        }
    }

    fn delta(&mut self, delta: &ScreenDelta, out: &mut Vec<Envelope>) {
        let Some(term) = self.terms.get_mut(&delta.session) else { return };
        // The lines it scrolls off the top, while they are still on the screen held.
        let scrolled = term.mirror.screen.as_ref().filter(|s| {
            delta.scroll > 0
                && term.mirror.generation == delta.generation.0
                && term.mirror.revision == delta.base.0
                && delta.scroll <= u32::from(s.rows)
        });
        let top = scrolled.map(|s| (s.lines[..delta.scroll as usize].to_vec(), s.first_line));
        match term.mirror.apply_delta(delta) {
            Ok(ScreenApplied::Changed) => {
                match top {
                    Some((lines, from)) => term.keep_scrolled(lines, from),
                    None if delta.scroll > 0 => term.history.clear(),
                    None => term.trim_history(),
                }
                if term.retype && term.typist.may_type() {
                    term.retype = false;
                    let waiting = std::mem::take(&mut term.waiting);
                    let session = delta.session.clone();
                    self.send(&session, &waiting, out);
                }
            }
            Ok(_) => {}
            Err(ScreenGap::NoSnapshot | ScreenGap::Missing { .. } | ScreenGap::Malformed(_)) => {
                self.gap(&delta.session.clone(), out)
            }
        }
    }

    fn chunk(&mut self, chunk: &HistoryChunk, request: Option<&str>) {
        if let Some(id) = request {
            self.requests.remove(id);
        }
        let Some(term) = self.terms.get_mut(&chunk.session) else { return };
        term.history_asked = false;
        let Some(screen) = term.mirror.screen.as_ref() else { return };
        if chunk.generation.0 != term.mirror.generation || chunk.cols != screen.cols {
            return;
        }
        let Ok(lines) = chunk.decode() else { return };
        let end = chunk.first.0 + lines.len() as u64;
        let follows = term.history.first_key_value().map_or(end == screen.first_line, |(first, _)| *first == end);
        if !follows {
            return;
        }
        for (i, line) in lines.into_iter().enumerate() {
            term.history.insert(chunk.first.0 + i as u64, line);
        }
        term.trim_history();
    }

    fn lease_state(&mut self, state: &LeaseState, out: &mut Vec<Envelope>) {
        let Some(term) = self.terms.get_mut(&state.session) else { return };
        let had = term.typist.may_type() || term.acquiring;
        term.typist.apply(state);
        if term.typist.may_type() {
            term.acquiring = false;
            term.lost = None;
            if !term.waiting.is_empty() && !term.retype {
                let waiting = std::mem::take(&mut term.waiting);
                let session = state.session.clone();
                self.send(&session, &waiting, out);
            }
            return;
        }
        if had {
            term.lost = match state.reason {
                Some(LeaseReason::Desktop) => Some(Lost::Desktop),
                Some(LeaseReason::TakenOver) => Some(Lost::TakenOver),
                Some(LeaseReason::Expired) => Some(Lost::Expired),
                Some(LeaseReason::Disconnected) if state.holder.is_none() => Some(Lost::Gone),
                _ if state.holder.is_some() => Some(Lost::Held),
                _ => term.lost,
            };
        }
        term.acquiring = false;
        term.waiting.clear();
        term.retype = false;
    }

    fn refused(&mut self, request: Request, code: ErrorCode) {
        match request {
            Request::Input { session, seq } => {
                let Some(term) = self.terms.get_mut(&session) else { return };
                if let Some(again) = term.typist.refused(Seq(seq), code) {
                    // Before anything typed since; sent at the screen's next change.
                    term.waiting.insert_str(0, &again);
                    term.retype = true;
                }
            }
            Request::Lease(session) => {
                let Some(term) = self.terms.get_mut(&session) else { return };
                term.acquiring = false;
                term.waiting.clear();
                term.lost = Some(if code == ErrorCode::LeaseHeld { Lost::Held } else { Lost::Gone });
            }
            Request::History(session) => {
                if let Some(term) = self.terms.get_mut(&session) {
                    term.history_asked = false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
