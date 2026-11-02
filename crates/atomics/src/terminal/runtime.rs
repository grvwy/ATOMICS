//! Host-owned PTY, child and emulator; notifications never depend on egui.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use anyhow::Result;
use atomics_core::lease::GridSize;

use crate::protocol::{Lease, TerminalControl};

#[derive(Clone, Copy, Default)]
struct InputHistory {
    last_typed: Option<Instant>,
    draft_at: Option<Instant>,
}

impl InputHistory {
    fn record(&mut self, at: Instant, submitted: bool) {
        self.last_typed = Some(at);
        self.draft_at = (!submitted).then_some(at);
    }

    fn empty_since(self, turn_ended: Option<Instant>) -> Option<bool> {
        self.last_typed.map(|_| self.draft_at.is_none_or(|draft| turn_ended.is_some_and(|ended| draft <= ended)))
    }
}

pub struct Shared {
    pub title: Mutex<Option<String>>,
    /// The last line typed into this terminal and sent with Enter.
    pub submitted: Mutex<Option<String>>,
    pub exited: AtomicBool,
    pub bell: AtomicBool,
    pub changed: AtomicBool,
    revision: AtomicU64,
    pub clipboard: Mutex<Option<String>>,
    wake: crate::host::Wake,
    size: Mutex<WindowSize>,
    shown: Mutex<Option<(Instant, bool)>>,
    input: Mutex<InputHistory>,
    /// The theme's 256 colors, foreground, background and cursor, answered to OSC 4/10/11/12.
    colors: Mutex<Vec<Rgb>>,
    /// The program's current title. `title` above is taken by the host; this one is only read.
    screen_title: Mutex<Option<String>>,
    /// Moves with title and theme changes, which do not move `revision`, so a remote screen
    /// stream knows to copy again. Nothing on the desktop reads it.
    screen_changes: AtomicU64,
    /// Which remote client may type into and resize this terminal. Nobody holds it unless the
    /// protocol server runs and a client asked; until then the desktop never takes its lock to type.
    lease: Lease,
    /// The desktop's own size (its pane, or the size the terminal started with). `size` is what is
    /// in force, which is a lease holder's size while it has one.
    local: Mutex<WindowSize>,
}

impl Shared {
    fn new(wake: crate::host::Wake, size: WindowSize, colors: Vec<Rgb>) -> Self {
        Self {
            title: Mutex::new(None),
            submitted: Mutex::new(None),
            exited: AtomicBool::new(false),
            bell: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            revision: AtomicU64::new(0),
            clipboard: Mutex::new(None),
            wake,
            size: Mutex::new(size),
            shown: Mutex::new(None),
            input: Mutex::new(InputHistory::default()),
            colors: Mutex::new(colors),
            screen_title: Mutex::new(None),
            screen_changes: AtomicU64::new(0),
            lease: Lease::default(),
            local: Mutex::new(size),
        }
    }

    fn set_title(&self, title: Option<String>) {
        *self.screen_title.lock().unwrap() = title.clone();
        self.screen_changes.fetch_add(1, Ordering::Release);
        *self.title.lock().unwrap() = title;
    }
}

#[derive(Clone)]
pub(super) struct Listener {
    sender: Arc<OnceLock<EventLoopSender>>,
    shared: Arc<Shared>,
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let write = |text: String| {
            if let Some(sender) = self.sender.get() {
                let _ = sender.send(Msg::Input(Cow::Owned(text.into_bytes())));
            }
        };
        match event {
            TermEvent::Title(title) => self.shared.set_title(Some(title)),
            TermEvent::ResetTitle => self.shared.set_title(None),
            TermEvent::PtyWrite(text) => write(text),
            TermEvent::ColorRequest(index, format) => {
                let colors = self.shared.colors.lock().unwrap();
                if let Some(color) = colors.get(index).or(colors.get(256)) {
                    write(format(*color));
                }
            }
            TermEvent::TextAreaSizeRequest(format) => write(format(*self.shared.size.lock().unwrap())),
            TermEvent::ClipboardStore(_, text) => *self.shared.clipboard.lock().unwrap() = Some(text),
            TermEvent::Bell => self.shared.bell.store(true, Ordering::Relaxed),
            TermEvent::ChildExit(_) | TermEvent::Exit => self.shared.exited.store(true, Ordering::Release),
            TermEvent::ClipboardLoad(..) => return,
            TermEvent::Wakeup => {
                self.shared.revision.fetch_add(1, Ordering::Release);
            }
            TermEvent::MouseCursorDirty | TermEvent::CursorBlinkingChange => {}
        }
        self.shared.changed.store(true, Ordering::Release);
        self.shared.wake.notify();
    }
}

/// Native runtime handle. Dropping it shuts down the PTY, even if no view was drawn.
pub struct Terminal {
    process_id: u32,
    stopped: AtomicBool,
    pub(super) term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    pub shared: Arc<Shared>,
}

/// Remove instance routing from every terminal, including inherited bundle settings. A session's
/// own hook pane is set again after the inherited one is removed.
pub(super) fn isolated_args(program: String, args: Vec<String>, hook_pane: Option<String>) -> Vec<String> {
    [
        "-u",
        "ATOMICS_SOCKET",
        "-u",
        "ATOMICS_PANE",
        "-u",
        "ATOMICS_HOME",
        "-u",
        "ATOMICS_CONFIG",
        "-u",
        "ATOMICS_AZURE_BEARER",
        "-u",
        "ATOMICS_AZURE_RELAY_URL",
        "-u",
        atomics_core::hook::HOOK_PANE_ENV,
    ]
    .into_iter()
    .map(str::to_string)
    .chain(hook_pane.map(|pane| format!("{}={pane}", atomics_core::hook::HOOK_PANE_ENV)))
    .chain([program])
    .chain(args)
    .collect()
}

impl Terminal {
    pub(super) fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }

    pub fn shown(&self, focused: bool) {
        *self.shared.shown.lock().unwrap() = Some((Instant::now(), focused));
    }

    /// Native keystrokes and pastes are authoritative; a plain Enter clears the pending draft.
    pub fn record_input(&self, at: Instant, submitted: bool) {
        self.shared.input.lock().unwrap().record(at, submitted);
    }

    pub fn last_typed(&self) -> Option<Instant> {
        self.shared.input.lock().unwrap().last_typed
    }

    pub fn input_empty_since(&self, turn_ended: Option<Instant>) -> bool {
        if let Some(empty) = self.shared.input.lock().unwrap().empty_since(turn_ended) {
            return empty;
        }
        // Before native input is observed, ignore suggestions rendered with styling or a custom color.
        let term = self.term.lock();
        let grid = term.grid();
        let lines: Vec<String> = (0..grid.screen_lines())
            .map(|r| {
                let row = &grid[Line(r as i32)];
                let first = (0..grid.columns()).find(|&c| {
                    let ch = row[Column(c)].c;
                    ch != '\0' && !ch.is_whitespace()
                });
                (0..grid.columns())
                    .filter(|&c| !row[Column(c)].flags.contains(Flags::WIDE_CHAR_SPACER))
                    .map(|c| {
                        let cell = &row[Column(c)];
                        let marker = Some(c) == first && matches!(cell.c, '❯' | '›' | '>' | '─' | '━' | '═' | '╰');
                        let styled = cell.fg != Color::Named(NamedColor::Foreground)
                            || cell.flags.intersects(
                                Flags::DIM
                                    | Flags::BOLD_ITALIC
                                    | Flags::ALL_UNDERLINES
                                    | Flags::INVERSE
                                    | Flags::HIDDEN
                                    | Flags::STRIKEOUT,
                            );
                        if cell.c == '\0' || styled && !marker { ' ' } else { cell.c }
                    })
                    .collect()
            })
            .collect();
        let Some((index, prompt)) = lines
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, line)| line.trim_start().strip_prefix(['❯', '›', '>']).map(|prompt| (i, prompt)))
        else {
            return true;
        };
        prompt.trim().is_empty()
            && lines[index + 1..]
                .iter()
                .take_while(|line| !line.trim_start().starts_with(['─', '━', '═', '╰']))
                .all(|line| line.trim().is_empty())
    }

    /// Output cadence is a host decision informed by native-client visibility.
    /// These hints expire by wall clock, independently of egui frame counters.
    pub fn repaint_delay(&self) -> Duration {
        let shown = *self.shared.shown.lock().unwrap();
        let visible = shown.is_some_and(|(at, _)| at.elapsed() < Duration::from_millis(500));
        let focused = visible && shown.is_some_and(|(_, focused)| focused);
        if focused && self.last_typed().is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
            Duration::ZERO
        } else if focused {
            Duration::from_millis(16)
        } else if visible {
            Duration::from_millis(33)
        } else {
            Duration::from_millis(500)
        }
    }

    pub fn process_id(&self) -> u32 {
        self.process_id
    }

    pub fn spawn(
        wake: crate::host::Wake,
        colors: Vec<Rgb>,
        program: String,
        args: Vec<String>,
        cwd: &Path,
        mut env: HashMap<String, String>,
        scrollback: usize,
    ) -> Result<Terminal> {
        let hook_pane = env.remove(atomics_core::hook::HOOK_PANE_ENV);
        let options = tty::Options {
            shell: Some(tty::Shell::new("/usr/bin/env".into(), isolated_args(program, args, hook_pane))),
            working_directory: Some(cwd.to_path_buf()),
            drain_on_exit: true,
            env,
        };
        let size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };
        let pty = tty::new(&options, size, 0)?;
        let process_id = pty.child().id();
        let shared = Arc::new(Shared::new(wake, size, colors));
        let listener = Listener { sender: Arc::new(OnceLock::new()), shared: shared.clone() };
        let config = TermConfig { scrolling_history: scrollback, ..TermConfig::default() };
        let term = Arc::new(FairMutex::new(Term::new(config, &TermSize::new(80, 24), listener.clone())));
        let event_loop = EventLoop::new(term.clone(), listener.clone(), pty, true, false)?;
        let sender = event_loop.channel();
        let _ = listener.sender.set(sender.clone());
        event_loop.spawn();
        Ok(Terminal { process_id, stopped: AtomicBool::new(false), term, sender, shared })
    }

    /// Colors answered to later color queries, after a theme change.
    pub fn set_colors(&self, colors: Vec<Rgb>) {
        *self.shared.colors.lock().unwrap() = colors;
        self.shared.screen_changes.fetch_add(1, Ordering::Release);
    }

    /// The identity of this terminal's screen handles (see `ScreenHandle::identity`).
    pub fn screen_identity(&self) -> usize {
        Arc::as_ptr(&self.shared) as usize
    }

    /// A handle the protocol uses to stream this terminal's screen and, for a lease holder, to
    /// type into it and resize it.
    pub fn screen_handle(&self) -> ScreenHandle {
        ScreenHandle { term: self.term.clone(), shared: self.shared.clone(), input: Input::Pty(self.sender.clone()) }
    }

    /// Someone typed on the desktop: a remote client's lease on this terminal ends, and its size
    /// with it. Nothing happens (and no lock is taken) when nobody holds one, which is always so
    /// with remote access off.
    pub fn take_back(&self) {
        if self.shared.lease.held() {
            let handle = self.screen_handle();
            (&handle as &dyn TerminalControl).take_back();
        }
    }

    /// Lines kept above the screen; fewer drops the oldest at once.
    pub fn set_scrollback(&self, lines: usize) {
        self.term.lock().set_options(TermConfig { scrolling_history: lines, ..TermConfig::default() });
    }

    pub fn exited(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
    }

    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let _ = self.sender.send(Msg::Input(bytes.into()));
    }

    /// Types `text` as one message and presses Enter, the way a person would.
    pub fn submit(&self, text: &str) {
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        self.term.lock().scroll_display(Scroll::Bottom);
        self.write(atomics_core::steer::paste_bytes(text, bracketed));
        // Programs that debounce pastes would read an immediate Enter as part of it.
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            let _ = sender.send(Msg::Input(Cow::Borrowed(b"\r")));
        });
    }

    /// Types `text` into the program's prompt without pressing Enter.
    pub fn draft(&self, text: &str) {
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        self.term.lock().scroll_display(Scroll::Bottom);
        self.write(atomics_core::steer::paste_bytes(text, bracketed));
    }

    /// Types editor commands: each step after a lone Escape, with pauses so the
    /// editor reads Escape as a key of its own rather than as Alt+next key.
    pub fn type_steps(&self, steps: Vec<String>, initial_delay: Duration) {
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            std::thread::sleep(initial_delay);
            for step in steps {
                let _ = sender.send(Msg::Input(Cow::Borrowed(b"\x1b")));
                std::thread::sleep(Duration::from_millis(120));
                let _ = sender.send(Msg::Input(Cow::Owned(step.into_bytes())));
                std::thread::sleep(Duration::from_millis(120));
            }
        });
    }

    /// The visible screen as text, without trailing blank lines; the last `max_lines` of it.
    pub fn screen_text(&self, max_lines: usize) -> String {
        self.screen_reader(max_lines)()
    }

    pub fn screen_reader(&self, max_lines: usize) -> impl FnOnce() -> String + Send + 'static {
        let term = self.term.clone();
        move || {
            let term = term.lock();
            let grid = term.grid();
            let mut lines: Vec<String> = (0..grid.screen_lines())
                .map(|r| {
                    let row = &grid[Line(r as i32)];
                    let mut text: String = (0..grid.columns())
                        .map(|c| &row[Column(c)])
                        .filter(|cell| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
                        .map(|cell| if cell.c == '\0' { ' ' } else { cell.c })
                        .collect();
                    text.truncate(text.trim_end().len());
                    text
                })
                .collect();
            while lines.last().is_some_and(|l| l.is_empty()) {
                lines.pop();
            }
            let skip = lines.len().saturating_sub(max_lines);
            lines[skip..].join("\n")
        }
    }

    pub fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        // alacritty creates a new session/process group with setsid before exec.
        // Stop it before taking the descendant snapshot so the leader cannot spawn more work.
        // Interactive shells can give jobs their own groups; those must go away too.
        let leader = self.process_id as i32;
        unsafe {
            libc::kill(-leader, libc::SIGSTOP);
        }
        if !self.exited()
            && let Ok(output) = std::process::Command::new("/bin/ps").args(["-axo", "pid=,ppid=,pgid="]).output()
            && output.status.success()
        {
            let rows: Vec<Vec<i32>> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|line| line.split_whitespace().filter_map(|part| part.parse().ok()).collect())
                .filter(|row: &Vec<i32>| row.len() == 3)
                .collect();
            let mut owned = std::collections::HashSet::from([leader]);
            loop {
                let before = owned.len();
                for row in &rows {
                    if owned.contains(&row[1]) {
                        owned.insert(row[0]);
                    }
                }
                if before == owned.len() {
                    break;
                }
            }
            let our_group = unsafe { libc::getpgrp() };
            for row in rows.iter().filter(|row| owned.contains(&row[0]) && row[2] > 1 && row[2] != our_group) {
                unsafe {
                    libc::kill(-row[2], libc::SIGKILL);
                }
            }
        }
        unsafe {
            libc::kill(-leader, libc::SIGKILL);
        }
        let _ = self.sender.send(Msg::Shutdown);
    }

    /// The desktop's pane size. Applied at once, unless a remote lease holder asked for its own
    /// size; then it is applied when that lease ends.
    pub fn resize(&self, size: WindowSize) {
        let handle = self.screen_handle();
        (&handle as &dyn TerminalControl).local_resize(|| *self.shared.local.lock().unwrap() = size);
    }
}

/// Resize the emulator and the PTY, and record the size answered to size queries.
fn apply_size(term: &FairMutex<Term<Listener>>, shared: &Shared, input: &Input, size: WindowSize) {
    let mut locked = term.lock();
    locked.resize(TermSize::new(size.num_cols as usize, size.num_lines as usize));
    shared.revision.fetch_add(1, Ordering::Release);
    drop(locked);
    *shared.size.lock().unwrap() = size;
    match input {
        Input::Pty(sender) => {
            let _ = sender.send(Msg::Resize(size));
        }
        #[cfg(test)]
        Input::Echo(_) => {}
    }
}

/// Where a lease holder's input goes.
#[derive(Clone)]
enum Input {
    Pty(EventLoopSender),
    /// A test emulator with no PTY: input is recorded and parsed as if the program echoed it.
    #[cfg(test)]
    Echo(Arc<Echo>),
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// What the protocol thread needs to stream one terminal: the emulator, locked only to copy the
/// screen, and the counters that say whether there is anything new to copy. Holding one does not
/// keep the PTY or child alive.
#[derive(Clone)]
pub struct ScreenHandle {
    term: Arc<FairMutex<Term<Listener>>>,
    shared: Arc<Shared>,
    input: Input,
}

impl ScreenHandle {
    /// The same for every handle to one terminal, different for its replacement.
    #[cfg(test)]
    pub fn identity(&self) -> usize {
        Arc::as_ptr(&self.shared) as usize
    }

    /// The title and theme are cloned before the emulator is locked: the listener takes their
    /// locks while the emulator is locked, so taking them inside would invert that order.
    fn inputs(&self) -> (Vec<Rgb>, Option<String>) {
        (self.shared.colors.lock().unwrap().clone(), self.shared.screen_title.lock().unwrap().clone())
    }
}

impl crate::protocol::ScreenSource for ScreenHandle {
    fn changes(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire).wrapping_add(self.shared.screen_changes.load(Ordering::Acquire))
    }

    fn capture(&self, tracker: &mut super::screen::Tracker, generations: &AtomicU64) -> atomics_core::screen::Screen {
        let (theme, title) = self.inputs();
        let term = self.term.lock();
        super::screen::capture(&term, &theme, title, tracker, generations)
    }

    fn history(
        &self,
        tracker: &mut super::screen::Tracker,
        generations: &AtomicU64,
        before: u64,
        count: u32,
    ) -> super::screen::HistoryLines {
        let term = self.term.lock();
        super::screen::history(&term, tracker, generations, before, count)
    }
}

impl TerminalControl for ScreenHandle {
    fn lease(&self) -> &Lease {
        &self.shared.lease
    }

    fn write(&self, bytes: Vec<u8>) {
        match &self.input {
            Input::Pty(sender) => {
                let _ = sender.send(Msg::Input(Cow::Owned(bytes)));
            }
            #[cfg(test)]
            Input::Echo(echo) => {
                echo.written.lock().unwrap().extend_from_slice(&bytes);
                echo.feed(&self.term, &self.shared, &bytes);
            }
        }
    }

    fn resize(&self, size: Option<GridSize>) {
        let local = *self.shared.local.lock().unwrap();
        let size = size.map_or(local, |s| WindowSize {
            num_cols: s.cols,
            num_lines: s.rows,
            cell_width: s.cell_width.unwrap_or(local.cell_width),
            cell_height: s.cell_height.unwrap_or(local.cell_height),
        });
        apply_size(&self.term, &self.shared, &self.input, size);
    }
}

#[cfg(test)]
pub(crate) struct Echo {
    parser: Mutex<alacritty_terminal::vte::ansi::Processor>,
    written: Mutex<Vec<u8>>,
}

#[cfg(test)]
impl Echo {
    /// Parse output as the PTY thread would, and say the screen changed.
    fn feed(&self, term: &FairMutex<Term<Listener>>, shared: &Shared, bytes: &[u8]) {
        let mut term = term.lock();
        self.parser.lock().unwrap().advance(&mut *term, bytes);
        drop(term);
        shared.revision.fetch_add(1, Ordering::Release);
    }
}

/// An emulator without a PTY or child, fed by the test, behind a real `ScreenHandle`. Input a
/// lease holder sends is recorded and echoed onto the screen.
#[cfg(test)]
pub(crate) struct Detached {
    pub handle: ScreenHandle,
    echo: Arc<Echo>,
}

#[cfg(test)]
impl Detached {
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        let size = WindowSize { num_lines: rows as u16, num_cols: cols as u16, cell_width: 8, cell_height: 16 };
        let shared = Arc::new(Shared::new(crate::host::Wake::default(), size, Vec::new()));
        let listener = Listener { sender: Arc::new(OnceLock::new()), shared: shared.clone() };
        let config = TermConfig { scrolling_history: scrollback, ..TermConfig::default() };
        let term = Arc::new(FairMutex::new(Term::new(config, &TermSize::new(cols, rows), listener)));
        let echo = Arc::new(Echo { parser: Mutex::new(Default::default()), written: Mutex::new(Vec::new()) });
        Self { handle: ScreenHandle { term, shared, input: Input::Echo(echo.clone()) }, echo }
    }

    /// Parse output as the PTY thread would, and say the screen changed.
    pub fn feed(&self, bytes: &[u8]) {
        self.echo.feed(&self.handle.term, &self.handle.shared, bytes);
    }

    /// Everything lease holders wrote, in order.
    pub fn written(&self) -> String {
        String::from_utf8_lossy(&self.echo.written.lock().unwrap()).into_owned()
    }

    /// The size in force, as the PTY would report it.
    pub fn size(&self) -> (u16, u16) {
        let size = *self.handle.shared.size.lock().unwrap();
        (size.num_cols, size.num_lines)
    }

    /// The desktop typed into this terminal.
    pub fn take_back(&self) -> bool {
        (&self.handle as &dyn TerminalControl).take_back()
    }

    /// Say the screen changed without touching the emulator.
    pub fn touch(&self) {
        self.handle.shared.revision.fetch_add(1, Ordering::Release);
    }

    /// The desktop's pane changed size: applied as `Terminal::resize` applies it.
    pub fn resize(&self, cols: usize, rows: usize) {
        let size = WindowSize { num_lines: rows as u16, num_cols: cols as u16, cell_width: 8, cell_height: 16 };
        (&self.handle as &dyn TerminalControl).local_resize(|| *self.handle.shared.local.lock().unwrap() = size);
    }

    /// Hold the emulator lock, as a busy PTY thread or a painting frame does.
    pub fn lock(&self) -> impl Drop + '_ {
        self.handle.term.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chief_input_history_clears_only_at_submit_or_turn_end() {
        let now = Instant::now();
        let mut input = InputHistory::default();
        assert_eq!(input.empty_since(Some(now)), None);
        input.record(now + Duration::from_secs(1), false);
        assert_eq!(input.empty_since(Some(now)), Some(false));
        input.record(now + Duration::from_secs(2), true);
        assert_eq!(input.empty_since(Some(now)), Some(true));
        input.record(now + Duration::from_secs(3), false);
        assert_eq!(input.empty_since(Some(now)), Some(false));
        assert_eq!(input.empty_since(Some(now + Duration::from_secs(4))), Some(true));
    }

    #[test]
    fn closing_or_replacing_terminal_kills_its_process_group() {
        for job_control in [false, true] {
            let dir = crate::test_support::tempdir().unwrap();
            let spawn = || {
                Terminal::spawn(
                    crate::host::Wake::default(),
                    Vec::new(),
                    "/bin/sh".into(),
                    vec![
                        if job_control { "-imc" } else { "-c" }.into(),
                        "trap '' HUP TERM; sleep 60 & echo $! > child; wait".into(),
                    ],
                    dir.path(),
                    HashMap::new(),
                    100,
                )
                .unwrap()
            };
            let mut terminal = Some(spawn());
            for replace in [true, false] {
                let pid = terminal.as_ref().unwrap().process_id() as i32;
                let deadline = Instant::now() + Duration::from_secs(5);
                let path = dir.path().join("child");
                while !path.exists() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(5));
                }
                let child: i32 = std::fs::read_to_string(&path).unwrap().trim().parse().unwrap();
                assert_eq!(unsafe { libc::getpgid(child) } == pid, !job_control);
                std::fs::remove_file(path).unwrap();
                terminal = replace.then(spawn);
                let deadline = Instant::now() + Duration::from_secs(5);
                while unsafe { libc::kill(-pid, 0) == 0 || libc::kill(child, 0) == 0 } {
                    assert!(Instant::now() < deadline, "process group {pid} survived closing");
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert_eq!(unsafe { libc::kill(child, 0) }, -1, "worker survived");
            }
        }
    }

    #[test]
    fn listener_exit_wakes_the_host_without_a_client() {
        let wake = crate::host::Wake::default();
        let size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };
        let shared = Arc::new(Shared::new(wake.clone(), size, Vec::new()));
        let listener = Listener { sender: Arc::new(OnceLock::new()), shared: shared.clone() };
        listener.send_event(TermEvent::Exit);
        let now = Instant::now();
        wake.wait_until(now + Duration::from_secs(30));
        assert!(now.elapsed() < Duration::from_secs(1));
        assert!(shared.exited.load(Ordering::Acquire));
        assert!(shared.changed.load(Ordering::Acquire));
    }
}
