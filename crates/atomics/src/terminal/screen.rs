//! Copies an alacritty emulator's live screen into the protocol's plain [`Screen`] and keeps
//! line IDs stable between copies. Callers hold the emulator lock only for [`capture`] and
//! [`history`]; comparing and encoding happen after it is released.
//!
//! The live screen is read directly (`grid[Line(0..rows)]`), never through the display offset, so
//! the desktop scrolling its view back changes nothing a remote client sees. [`view`] is the
//! desktop's own copy: the lines it displays, at its scroll position, for the shared painter.

use std::sync::atomic::{AtomicU64, Ordering};

use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line as GridLine};
use alacritty_terminal::term::cell::{Cell as TermCell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as TermColor, CursorShape as TermShape, NamedColor, Rgb as TermRgb};
use atomics_core::screen::{Cell, Color, Cursor, CursorShape, Line, Modes, PALETTE_LEN, Rgb, Screen, Style};

/// Line numbering for one terminal: the generation it belongs to and the ID of the top screen
/// line. Lives as long as the terminal's stream; [`capture`] and [`history`] bring it up to date.
#[derive(Debug, Default)]
pub struct Tracker {
    pub generation: u64,
    pub first_line: u64,
    anchor: Option<Anchor>,
}

/// What the last look saw, to tell how many lines scrolled into history since.
#[derive(Debug)]
struct Anchor {
    /// Address of the top line's cell storage. alacritty scrolls by rotating rows, not by moving
    /// their cells, so finding this address `j` lines up means `j` lines scrolled into history.
    top: usize,
    /// The newest history line's content then. History lines never change, so when storage may
    /// have been reused (eviction, a cleared history) it must still sit just above the old top.
    newest: Option<Line>,
    history: usize,
    cols: usize,
    rows: usize,
    alt: bool,
}

fn row_address(grid: &Grid<TermCell>, line: i32) -> usize {
    &grid[GridLine(line)][Column(0)] as *const TermCell as usize
}

impl Tracker {
    /// Follow the lines since the last look. A resize, an alternate-screen switch, or a change that
    /// cannot be followed exactly (more output than history holds, a reset) starts a new
    /// generation instead: line IDs are never guessed.
    fn sync<T>(&mut self, term: &Term<T>, generations: &AtomicU64) {
        let grid = term.grid();
        let (cols, rows, history) = (grid.columns(), grid.screen_lines(), grid.history_size());
        let alt = term.mode().contains(TermMode::ALT_SCREEN);
        let scrolled = self.anchor.as_ref().and_then(|a| {
            if (a.cols, a.rows, a.alt) != (cols, rows, alt) {
                return None;
            }
            let j = (0..=history).find(|&j| row_address(grid, -(j as i32)) == a.top)?;
            if j == 0 && history < a.history {
                return Some(0); // history was cleared or shortened, and nothing scrolled
            }
            match &a.newest {
                Some(newest) => (j < history && line(grid, -(j as i32) - 1) == *newest).then_some(j),
                // No history then: every line that scrolled since must still be there.
                None => (history == j).then_some(j),
            }
        });
        match scrolled {
            Some(j) => self.first_line += j as u64,
            None => {
                self.generation = generations.fetch_add(1, Ordering::Relaxed) + 1;
                self.first_line = history as u64;
            }
        }
        self.anchor = Some(Anchor {
            top: row_address(grid, 0),
            newest: (history > 0).then(|| line(grid, -1)),
            history,
            cols,
            rows,
            alt,
        });
    }
}

fn color(c: TermColor) -> Color {
    match c {
        TermColor::Named(name) => Color::Slot(name as u16),
        TermColor::Indexed(i) => Color::Slot(u16::from(i)),
        TermColor::Spec(rgb) => Color::Rgb(Rgb([rgb.r, rgb.g, rgb.b])),
    }
}

/// The wire attributes of alacritty's flags, in plain shifts so that copying a screen stays cheap
/// in debug builds too. `attrs_follow_the_flags` checks every combination against the flags.
fn attrs(flags: Flags) -> u16 {
    let b = flags.bits();
    // bold and italic; underline; double, curly, dotted and dashed underlines; inverse; dim,
    // hidden and strikeout.
    ((b >> 1) & 0b11) | (((b >> 3) & 1) << 2) | (((b >> 11) & 0b1111) << 3) | ((b & 1) << 7) | (((b >> 7) & 0b111) << 8)
}

fn style(cell: &TermCell) -> Style {
    let underline = if cell.extra.is_some() { cell.underline_color().map(color) } else { None };
    Style { fg: color(cell.fg), bg: color(cell.bg), attrs: attrs(cell.flags), underline }
}

/// One grid line (negative for history) as plain cells. A wide character is two columns only when
/// its spacer follows it; a stray spacer is a blank, so the wire form always lines up.
fn line(grid: &Grid<TermCell>, at: i32) -> Line {
    let row = &grid[GridLine(at)];
    let cols = grid.columns();
    let mut cells: Vec<Cell> = Vec::with_capacity(cols);
    for col in 0..cols {
        let cell = &row[Column(col)];
        let style = style(cell);
        let spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);
        if spacer && let Some(wide) = cells.last().filter(|c| c.width == 2) {
            let style = wide.style;
            cells.push(Cell { c: ' ', combining: None, width: 0, style });
            continue;
        }
        let wide = cell.flags.contains(Flags::WIDE_CHAR)
            && col + 1 < cols
            && row[Column(col + 1)].flags.contains(Flags::WIDE_CHAR_SPACER);
        let c = if spacer || cell.c == '\0' { ' ' } else { cell.c };
        let combining = cell.zerowidth().filter(|z| !z.is_empty()).map(Box::from);
        cells.push(Cell { c, combining, width: if wide { 2 } else { 1 }, style });
    }
    let wrapped = cols > 0 && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
    Line { cells, wrapped }
}

fn modes(mode: TermMode) -> Modes {
    let on = |m: TermMode| mode.contains(m);
    Modes {
        alt_screen: on(TermMode::ALT_SCREEN),
        app_cursor: on(TermMode::APP_CURSOR),
        app_keypad: on(TermMode::APP_KEYPAD),
        bracketed_paste: on(TermMode::BRACKETED_PASTE),
        mouse_click: on(TermMode::MOUSE_REPORT_CLICK),
        mouse_drag: on(TermMode::MOUSE_DRAG),
        mouse_motion: on(TermMode::MOUSE_MOTION),
        sgr_mouse: on(TermMode::SGR_MOUSE),
        utf8_mouse: on(TermMode::UTF8_MOUSE),
        focus_reporting: on(TermMode::FOCUS_IN_OUT),
        alternate_scroll: on(TermMode::ALTERNATE_SCROLL),
        keyboard: ((mode & TermMode::KITTY_KEYBOARD_PROTOCOL).bits()
            >> TermMode::DISAMBIGUATE_ESC_CODES.bits().trailing_zeros()) as u8,
    }
}

/// The cursor's shape, `Hidden` while the program hides it.
fn cursor_shape<T>(term: &Term<T>) -> CursorShape {
    if !term.mode().contains(TermMode::SHOW_CURSOR) {
        return CursorShape::Hidden;
    }
    match term.cursor_style().shape {
        TermShape::Block => CursorShape::Block,
        TermShape::Underline => CursorShape::Underline,
        TermShape::Beam => CursorShape::Beam,
        TermShape::HollowBlock => CursorShape::HollowBlock,
        TermShape::Hidden => CursorShape::Hidden,
    }
}

fn cursor<T>(term: &Term<T>) -> Cursor {
    let grid = term.grid();
    let mut point = grid.cursor.point;
    if point.column.0 > 0 && grid[point].flags.contains(Flags::WIDE_CHAR_SPACER) {
        point.column -= 1;
    }
    let style = term.cursor_style();
    let shape = cursor_shape(term);
    let row = point.line.0.clamp(0, grid.screen_lines() as i32 - 1) as u16;
    let col = point.column.0.min(grid.columns() - 1) as u16;
    Cursor { row, col, shape, blinking: style.blinking }
}

/// The theme's colours with the program's own colour changes (OSC 4/10/11/12) applied.
fn palette<T>(term: &Term<T>, theme: &[TermRgb]) -> Vec<Rgb> {
    let mut palette = vec![Rgb::default(); PALETTE_LEN];
    fill_palette(term, theme, &mut palette);
    palette
}

fn fill_palette<T>(term: &Term<T>, theme: &[TermRgb], palette: &mut [Rgb]) {
    let set = term.colors();
    for (i, slot) in palette.iter_mut().enumerate() {
        let c = set[i].or_else(|| theme.get(i).copied()).unwrap_or(TermRgb { r: 0, g: 0, b: 0 });
        *slot = Rgb([c.r, c.g, c.b]);
    }
}

/// Copy the live screen. Call with the emulator locked; nothing here allocates more than the
/// screen itself or does I/O.
pub fn capture<T>(
    term: &Term<T>,
    theme: &[TermRgb],
    title: Option<String>,
    tracker: &mut Tracker,
    generations: &AtomicU64,
) -> Screen {
    tracker.sync(term, generations);
    let grid = term.grid();
    let (cols, rows) = (grid.columns(), grid.screen_lines());
    Screen {
        cols: cols as u16,
        rows: rows as u16,
        lines: (0..rows as i32).map(|r| line(grid, r)).collect(),
        cursor: cursor(term),
        modes: modes(*term.mode()),
        title,
        palette: palette(term, theme),
        first_line: tracker.first_line,
        history: grid.history_size() as u64,
    }
}

/// Copy what the desktop displays into `screen`, reusing its buffers: the screen's lines at the
/// view's scroll offset, the theme's palette with the program's colour changes, and the cursor,
/// hidden while it is scrolled out of view. Call with the emulator locked.
///
/// Unlike [`capture`]'s lines, cells keep the emulator's own wide-character flags, also where an
/// edit split a wide character from the column it covers: the desktop has always drawn a wide
/// character two columns wide and nothing on its spacer, and it keeps doing so. `first_line` is 0;
/// line IDs are the protocol's.
pub fn view<T>(term: &Term<T>, theme: &[TermRgb], screen: &mut Screen) {
    view_cells(term, screen);
    view_palette(term, theme, screen);
}

/// [`view`]'s palette only, for a frame whose cells have not changed.
pub fn view_palette<T>(term: &Term<T>, theme: &[TermRgb], screen: &mut Screen) {
    screen.palette.resize(PALETTE_LEN, Rgb::default());
    fill_palette(term, theme, &mut screen.palette);
}

fn view_cells<T>(term: &Term<T>, screen: &mut Screen) {
    let grid = term.grid();
    let (cols, rows) = (grid.columns(), grid.screen_lines());
    let offset = grid.display_offset() as i32;
    screen.cols = cols as u16;
    screen.rows = rows as u16;
    screen.lines.resize_with(rows, Line::default);
    let (spacer, wide) = (Flags::WIDE_CHAR_SPACER.bits(), Flags::WIDE_CHAR.bits());
    let plain = Style::default();
    for (r, line) in screen.lines.iter_mut().enumerate() {
        let row = &grid[GridLine(r as i32 - offset)][..Column(cols)];
        line.cells.resize_with(cols, Cell::default);
        // Runs of cells share their colours and flags: reuse the style converted last.
        let mut last: Option<(&TermCell, Style)> = None;
        for (out, cell) in line.cells.iter_mut().zip(row) {
            let flags = cell.flags.bits();
            // Most cells of a screen are default blanks.
            if flags == 0
                && cell.c == ' '
                && cell.extra.is_none()
                && matches!(cell.fg, TermColor::Named(NamedColor::Foreground))
                && matches!(cell.bg, TermColor::Named(NamedColor::Background))
            {
                (out.c, out.width, out.style) = (' ', 1, plain);
                if out.combining.is_some() {
                    out.combining = None;
                }
                continue;
            }
            out.width = if flags & spacer != 0 {
                0
            } else if flags & wide != 0 {
                2
            } else {
                1
            };
            out.c = if cell.c == '\0' { ' ' } else { cell.c };
            if cell.extra.is_some() || out.combining.is_some() {
                out.combining = cell.zerowidth().filter(|z| !z.is_empty()).map(Box::from);
            }
            out.style = match last {
                Some((before, style))
                    if cell.extra.is_none()
                        && cell.fg == before.fg
                        && cell.bg == before.bg
                        && cell.flags == before.flags =>
                {
                    style
                }
                _ => style(cell),
            };
            last = cell.extra.is_none().then_some((cell, out.style));
        }
        line.wrapped = row.last().is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
    }

    // As alacritty's own renderer places it: on a wide character, not its spacer.
    let mut point = grid.cursor.point;
    if point.column.0 > 0 && grid[point].flags.contains(Flags::WIDE_CHAR_SPACER) {
        point.column -= 1;
    }
    let row = point.line.0 + offset;
    let shown = (0..rows as i32).contains(&row);
    screen.cursor = Cursor {
        row: if shown { row as u16 } else { 0 },
        col: point.column.0.min(cols.saturating_sub(1)) as u16,
        shape: if shown { cursor_shape(term) } else { CursorShape::Hidden },
        blinking: term.cursor_style().blinking,
    };
    screen.modes = modes(*term.mode());
    screen.title = None;
    screen.first_line = 0;
    screen.history = grid.history_size() as u64;
}

/// Retained history lines for a `history_chunk`.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryLines {
    pub generation: u64,
    pub cols: u16,
    /// The ID of `lines[0]`.
    pub first: u64,
    pub oldest: u64,
    pub evicted: bool,
    pub lines: Vec<Line>,
}

/// History lines with IDs `before - count .. before` that are still retained, newest kept when the
/// range reaches the screen. Call with the emulator locked.
pub fn history<T>(
    term: &Term<T>,
    tracker: &mut Tracker,
    generations: &AtomicU64,
    before: u64,
    count: u32,
) -> HistoryLines {
    tracker.sync(term, generations);
    let grid = term.grid();
    let top = tracker.first_line;
    let oldest = top - grid.history_size() as u64;
    let end = before.min(top);
    let wanted = before.saturating_sub(u64::from(count));
    let start = wanted.max(oldest).min(end);
    HistoryLines {
        generation: tracker.generation,
        cols: grid.columns() as u16,
        first: start,
        oldest,
        evicted: wanted < oldest,
        lines: (start..end).map(|id| line(grid, (id as i64 - top as i64) as i32)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::Processor;
    use atomics_core::screen::attr;

    struct Emulator {
        term: Term<VoidListener>,
        parser: Processor,
        tracker: Tracker,
        generations: AtomicU64,
    }

    impl Emulator {
        fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
            let config = Config { scrolling_history: scrollback, ..Config::default() };
            Self {
                term: Term::new(config, &TermSize::new(cols, rows), VoidListener),
                parser: Processor::new(),
                tracker: Tracker::default(),
                generations: AtomicU64::new(0),
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            self.parser.advance(&mut self.term, bytes);
        }

        fn capture(&mut self) -> Screen {
            capture(&self.term, &[], None, &mut self.tracker, &self.generations)
        }

        fn history(&mut self, before: u64, count: u32) -> HistoryLines {
            history(&self.term, &mut self.tracker, &self.generations, before, count)
        }
    }

    #[test]
    fn attrs_follow_the_flags() {
        const ATTRS: [(Flags, u16); 11] = [
            (Flags::BOLD, attr::BOLD),
            (Flags::ITALIC, attr::ITALIC),
            (Flags::UNDERLINE, attr::UNDERLINE),
            (Flags::DOUBLE_UNDERLINE, attr::DOUBLE_UNDERLINE),
            (Flags::UNDERCURL, attr::UNDERCURL),
            (Flags::DOTTED_UNDERLINE, attr::DOTTED_UNDERLINE),
            (Flags::DASHED_UNDERLINE, attr::DASHED_UNDERLINE),
            (Flags::INVERSE, attr::INVERSE),
            (Flags::DIM, attr::DIM),
            (Flags::HIDDEN, attr::HIDDEN),
            (Flags::STRIKEOUT, attr::STRIKEOUT),
        ];
        for bits in 0..=u16::MAX {
            let flags = Flags::from_bits_truncate(bits);
            let expected = ATTRS.iter().filter(|(flag, _)| flags.contains(*flag)).fold(0, |a, (_, bit)| a | bit);
            assert_eq!(attrs(flags), expected, "{flags:?}");
        }
    }

    #[test]
    fn cells_carry_wide_combining_colour_attributes_and_wrapping() {
        let mut e = Emulator::new(10, 3, 100);
        e.feed("中a\u{301}\x1b[1;31mR\x1b[38;2;1;2;3mX\x1b[0m\x1b[4:3mU\x1b[0m".as_bytes());
        e.feed(b"\r\n0123456789wrap");
        let s = e.capture();
        let row = &s.lines[0].cells;
        assert_eq!((row[0].c, row[0].width, row[1].width), ('中', 2, 0));
        assert_eq!((row[2].c, row[2].combining.as_deref()), ('a', Some(&['\u{301}'][..])));
        assert_eq!((row[3].style.fg, row[3].style.attrs), (Color::Slot(1), attr::BOLD));
        assert_eq!(row[4].style.fg, Color::Rgb(Rgb([1, 2, 3])));
        assert_eq!(row[5].style.attrs, attr::UNDERCURL);
        assert_eq!(s.lines[0].text(), "中a\u{301}RXU");
        assert!(s.lines[1].wrapped && !s.lines[2].wrapped);
        assert_eq!(s.lines[2].text(), "wrap");
        assert_eq!((s.cursor.row, s.cursor.col), (2, 4));
    }

    #[test]
    fn modes_cursor_title_and_palette_follow_the_program() {
        let mut e = Emulator::new(20, 4, 100);
        e.feed(b"\x1b[?1h\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[6 q\x1b]4;1;rgb:12/34/56\x07");
        let theme: Vec<TermRgb> = (0..PALETTE_LEN).map(|i| TermRgb { r: i as u8, g: 0, b: 0 }).collect();
        let s = capture(&e.term, &theme, Some("t".into()), &mut e.tracker, &e.generations);
        assert!(s.modes.app_cursor && s.modes.bracketed_paste && s.modes.mouse_click && s.modes.sgr_mouse);
        assert!(!s.modes.alt_screen);
        assert_eq!((s.cursor.shape, s.cursor.blinking), (CursorShape::Beam, false));
        assert_eq!(s.palette[1], Rgb([0x12, 0x34, 0x56]), "the program's colour wins");
        assert_eq!(s.palette[2], Rgb([2, 0, 0]), "the theme fills the rest");
        assert_eq!(s.title.as_deref(), Some("t"));
        e.feed(b"\x1b[?25l");
        assert_eq!(e.capture().cursor.shape, CursorShape::Hidden);
    }

    #[test]
    fn scrolling_output_keeps_the_generation_and_counts_lines_exactly() {
        let mut e = Emulator::new(12, 4, 1000);
        let first = e.capture();
        for i in 0..10 {
            e.feed(format!("line {i}\r\n").as_bytes());
        }
        let s = e.capture();
        assert_eq!(e.tracker.generation, 1, "scrolling is not a new generation");
        assert_eq!((s.first_line, s.history), (first.first_line + 7, 7));
        assert_eq!(s.lines[0].text(), "line 7");
        // Many small looks and one big one agree on numbering.
        for i in 10..300 {
            e.feed(format!("line {i}\r\n").as_bytes());
            if i % 7 == 0 {
                e.capture();
            }
        }
        let s = e.capture();
        assert_eq!(e.tracker.generation, 1);
        assert_eq!(s.lines[0].text(), "line 297");
        assert_eq!(s.first_line, 297, "the top line's ID is its line number");
        let h = e.history(s.first_line, 3);
        assert_eq!(h.lines.iter().map(Line::text).collect::<Vec<_>>(), ["line 294", "line 295", "line 296"]);
        assert_eq!((h.first, h.evicted), (294, false));
    }

    #[test]
    fn the_desktop_scrolling_its_view_back_changes_nothing() {
        let mut e = Emulator::new(12, 4, 1000);
        for i in 0..40 {
            e.feed(format!("line {i}\r\n").as_bytes());
        }
        let before = e.capture();
        e.term.scroll_display(alacritty_terminal::grid::Scroll::Top);
        assert!(e.term.grid().display_offset() > 0);
        assert_eq!(e.capture(), before);
    }

    #[test]
    fn resize_and_the_alternate_screen_start_new_generations() {
        let mut e = Emulator::new(12, 4, 1000);
        e.feed(b"hello\r\n");
        e.capture();
        e.term.resize(TermSize::new(20, 6));
        let s = e.capture();
        assert_eq!((e.tracker.generation, s.cols, s.rows), (2, 20, 6));
        e.feed(b"\x1b[?1049h");
        let alt = e.capture();
        assert_eq!(e.tracker.generation, 3);
        assert!(alt.modes.alt_screen);
        e.feed(b"\x1b[?1049l");
        let back = e.capture();
        assert_eq!(e.tracker.generation, 4);
        assert!(back.lines[0].text().starts_with("hello"));
    }

    #[test]
    fn eviction_is_reported_and_too_much_output_is_a_new_generation() {
        let mut e = Emulator::new(12, 4, 20);
        for i in 0..30 {
            e.feed(format!("line {i}\r\n").as_bytes());
            e.capture();
        }
        let s = e.capture();
        assert_eq!(e.tracker.generation, 1, "a full history still scrolls exactly when looked at often");
        assert_eq!(s.history, 20);
        let h = e.history(s.first_line, 1000);
        assert!(h.evicted, "asking past the oldest line says so");
        assert_eq!((h.first, h.oldest, h.lines.len()), (s.first_line - 20, s.first_line - 20, 20));
        assert_eq!(h.lines[0].text(), format!("line {}", s.first_line - 20));

        // More output than the history holds between two looks cannot be followed exactly.
        for i in 0..500 {
            e.feed(format!("burst {i}\r\n").as_bytes());
        }
        let after = e.capture();
        assert_eq!(e.tracker.generation, 2);
        assert_eq!(after.lines[0].text(), "burst 497");

        // A cleared history keeps the generation and evicts everything.
        e.feed(b"\x1b[3J");
        let cleared = e.capture();
        assert_eq!(cleared.history, 0);
        let h = e.history(cleared.first_line, 10);
        assert!(h.evicted && h.lines.is_empty());
    }
}
