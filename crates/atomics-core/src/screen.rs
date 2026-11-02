//! Terminal screens for `atomics.v1`: a browser-safe cell model, its compact wire form and the
//! reference client (`ScreenMirror`).
//!
//! The host copies an emulator's live screen into a [`Screen`] under the emulator's short lock and
//! encodes it after unlocking. A `Screen` is plain data (no emulator types, no egui, no I/O), so the
//! host and the wasm client build the same code and a client cannot drift from what the host meant.
//!
//! Wire form: a frame lists the styles its cells use once (`styles`) and each line as runs of
//! cells sharing a style. A run without `w` is one cell per `char`, each one column wide. A run
//! with `w` is a single cell spanning `w` columns: a wide character, or a character followed by
//! the combining characters drawn over it. The column a wide character covers is implied, and
//! trailing default blanks are left out. Line IDs are stable within a generation: the top screen
//! line is `first_line`, history lines count down from it, and a resize or alternate-screen switch
//! starts a new generation with new IDs.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};

use crate::wire::Seq;

pub const MAX_COLS: u16 = 1024;
pub const MAX_ROWS: u16 = 512;
/// Palette slots on the wire: 256 indexed colours, then foreground, background and cursor.
pub const PALETTE_LEN: usize = 259;
/// The most history lines one `history_get` may ask for; the host may send fewer.
pub const MAX_HISTORY_LINES: u32 = 1000;

pub const FOREGROUND: u16 = 256;
pub const BACKGROUND: u16 = 257;
pub const CURSOR: u16 = 258;
/// Slots the client derives from the palette, as the desktop does: 259-266 dim black to dim
/// white, 267 bright foreground, 268 dim foreground.
pub const LAST_SLOT: u16 = 268;

/// Cell attributes, a stable wire bitfield independent of any emulator's.
pub mod attr {
    pub const BOLD: u16 = 1;
    pub const ITALIC: u16 = 1 << 1;
    pub const UNDERLINE: u16 = 1 << 2;
    pub const DOUBLE_UNDERLINE: u16 = 1 << 3;
    pub const UNDERCURL: u16 = 1 << 4;
    pub const DOTTED_UNDERLINE: u16 = 1 << 5;
    pub const DASHED_UNDERLINE: u16 = 1 << 6;
    pub const INVERSE: u16 = 1 << 7;
    pub const DIM: u16 = 1 << 8;
    pub const HIDDEN: u16 = 1 << 9;
    pub const STRIKEOUT: u16 = 1 << 10;
    pub const ALL_UNDERLINES: u16 = UNDERLINE | DOUBLE_UNDERLINE | UNDERCURL | DOTTED_UNDERLINE | DASHED_UNDERLINE;
}

/// An RGB colour; `"#rrggbb"` on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Rgb(pub [u8; 3]);

impl Serialize for Rgb {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let [r, g, b] = self.0;
        s.collect_str(&format_args!("#{r:02x}{g:02x}{b:02x}"))
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<str>>::deserialize(d)?;
        let hex = text.strip_prefix('#').filter(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()));
        let hex = hex.ok_or_else(|| de::Error::custom("a colour is \"#rrggbb\""))?;
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(de::Error::custom);
        Ok(Rgb([byte(0)?, byte(2)?, byte(4)?]))
    }
}

/// A cell colour: a palette slot (a number) or an RGB value (a string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Color {
    Slot(u16),
    Rgb(Rgb),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Style {
    #[serde(default = "Style::default_fg", skip_serializing_if = "Style::is_default_fg")]
    pub fg: Color,
    #[serde(default = "Style::default_bg", skip_serializing_if = "Style::is_default_bg")]
    pub bg: Color,
    #[serde(default, rename = "a", skip_serializing_if = "is_zero_u16")]
    pub attrs: u16,
    /// The underline colour, when it is not the foreground.
    #[serde(default, rename = "ul", skip_serializing_if = "Option::is_none")]
    pub underline: Option<Color>,
}

impl Style {
    fn default_fg() -> Color {
        Color::Slot(FOREGROUND)
    }

    fn default_bg() -> Color {
        Color::Slot(BACKGROUND)
    }

    fn is_default_fg(c: &Color) -> bool {
        *c == Self::default_fg()
    }

    fn is_default_bg(c: &Color) -> bool {
        *c == Self::default_bg()
    }
}

impl Default for Style {
    fn default() -> Self {
        Self { fg: Self::default_fg(), bg: Self::default_bg(), attrs: 0, underline: None }
    }
}

fn is_zero_u16(n: &u16) -> bool {
    *n == 0
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn is_zero_u8(n: &u8) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One grid cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Cell {
    pub c: char,
    /// Combining characters drawn over `c`.
    pub combining: Option<Box<[char]>>,
    /// Columns this cell spans: 1, 2 for a wide character, 0 for the column a wide character covers.
    pub width: u8,
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self { c: ' ', combining: None, width: 1, style: Style::default() }
    }
}

impl Cell {
    fn is_blank(&self) -> bool {
        self.c == ' ' && self.combining.is_none() && self.width == 1 && self.style == Style::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Line {
    /// Exactly the screen's column count.
    pub cells: Vec<Cell>,
    /// The line continues on the next one (soft wrap), for selection and copy.
    pub wrapped: bool,
}

impl Line {
    pub fn blank(cols: u16) -> Self {
        Self { cells: vec![Cell::default(); cols as usize], wrapped: false }
    }

    /// The text of the line without trailing blanks, wide characters once.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for cell in self.cells.iter().filter(|c| c.width > 0) {
            text.push(cell.c);
            text.extend(cell.combining.iter().flat_map(|c| c.iter()));
        }
        text.truncate(text.trim_end().len());
        text
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    HollowBlock,
    Hidden,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub shape: CursorShape,
    #[serde(default, skip_serializing_if = "is_false")]
    pub blinking: bool,
}

/// The terminal modes a client needs to draw the screen and to encode keys, paste and mouse
/// reports the way the program expects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Modes {
    #[serde(skip_serializing_if = "is_false")]
    pub alt_screen: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub app_cursor: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub app_keypad: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub bracketed_paste: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub mouse_click: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub mouse_drag: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub mouse_motion: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub sgr_mouse: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub utf8_mouse: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub focus_reporting: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub alternate_scroll: bool,
    /// Kitty keyboard protocol flags in force (its progressive-enhancement bits), 0 when off.
    #[serde(skip_serializing_if = "is_zero_u8")]
    pub keyboard: u8,
}

/// A terminal's live screen at one moment.
#[derive(Debug, Clone, PartialEq)]
pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    /// Exactly `rows` lines, top first.
    pub lines: Vec<Line>,
    pub cursor: Cursor,
    pub modes: Modes,
    pub title: Option<String>,
    /// `PALETTE_LEN` colours: the host's theme with the program's colour changes applied.
    pub palette: Vec<Rgb>,
    /// The stable ID of the top screen line. History lines have the IDs below it.
    pub first_line: u64,
    /// History lines still retained above the screen: IDs `first_line - history .. first_line`.
    pub history: u64,
}

impl Screen {
    pub fn blank(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            lines: vec![Line::blank(cols); rows as usize],
            cursor: Cursor::default(),
            modes: Modes::default(),
            title: None,
            palette: vec![Rgb::default(); PALETTE_LEN],
            first_line: 0,
            history: 0,
        }
    }

    /// The oldest line ID still retained.
    pub fn oldest_line(&self) -> u64 {
        self.first_line.saturating_sub(self.history)
    }

    /// The screen as text, one line per row, trailing blank lines dropped.
    pub fn text(&self) -> String {
        let mut lines: Vec<String> = self.lines.iter().map(Line::text).collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// The top `n` lines leave the screen for history; blank lines fill in at the bottom. Both
    /// sides apply exactly this before comparing or applying a delta's lines.
    fn scroll_up(lines: &mut Vec<Line>, n: u64, cols: u16) {
        let n = (n.min(lines.len() as u64)) as usize;
        if n > 0 {
            lines.drain(..n);
            lines.extend(std::iter::repeat_n(Line::blank(cols), n));
        }
    }

    /// The whole screen as snapshot frames. `encode` turns one into a frame, or returns `None` when
    /// it is over the frame budget; then the screen is split into parts by rows, each
    /// self-contained, until every part fits. `None` when even a single line does not fit.
    pub fn snapshot<T>(
        &self,
        session: &str,
        generation: u64,
        revision: u64,
        mut encode: impl FnMut(ScreenSnapshot) -> Option<T>,
    ) -> Option<Vec<T>> {
        let rows: Vec<u16> = (0..self.rows).filter(|&r| !self.lines[r as usize].is_blank_line()).collect();
        let mut count = 1usize;
        loop {
            let chunk = rows.len().div_ceil(count).max(1);
            let groups: Vec<&[u16]> = if rows.is_empty() { vec![&[][..]] } else { rows.chunks(chunk).collect() };
            let total = groups.len() as u16;
            let parts = groups
                .iter()
                .enumerate()
                .map(|(index, group)| {
                    let mut encoder = RowsEncoder::default();
                    for &row in *group {
                        encoder.push(row, &self.lines[row as usize]);
                    }
                    let (styles, lines) = encoder.finish();
                    ScreenSnapshot {
                        session: session.to_string(),
                        generation: Seq(generation),
                        revision: Seq(revision),
                        cols: self.cols,
                        rows: self.rows,
                        first_line: Seq(self.first_line),
                        history: Seq(self.history),
                        cursor: self.cursor,
                        modes: self.modes,
                        title: self.title.clone(),
                        palette: self.palette.clone(),
                        styles,
                        lines,
                        part: (total > 1).then_some(Part { index: index as u16, count: total }),
                    }
                })
                .map(&mut encode)
                .collect::<Option<Vec<T>>>();
            if parts.is_some() {
                return parts;
            }
            if chunk == 1 {
                return None; // a single line over the budget: nothing smaller to send
            }
            count *= 2;
        }
    }

    /// The change from `self` to `new` in the same generation, or `None` when nothing changed.
    /// `new.first_line` must not be lower than `self.first_line`.
    pub fn delta(&self, new: &Screen, session: &str, generation: u64, base: u64, revision: u64) -> Option<ScreenDelta> {
        debug_assert_eq!((self.cols, self.rows), (new.cols, new.rows));
        let scroll = new.first_line.checked_sub(self.first_line)?;
        let mut shifted = self.lines.clone();
        Self::scroll_up(&mut shifted, scroll, self.cols);
        let mut encoder = RowsEncoder::default();
        for (row, (now, before)) in new.lines.iter().zip(&shifted).enumerate() {
            if now != before {
                encoder.push(row as u16, now);
            }
        }
        let cursor = (new.cursor != self.cursor).then_some(new.cursor);
        let modes = (new.modes != self.modes).then_some(new.modes);
        let title = (new.title != self.title).then(|| new.title.clone().unwrap_or_default());
        let palette = (new.palette != self.palette).then(|| new.palette.clone());
        if scroll == 0
            && encoder.lines.is_empty()
            && cursor.is_none()
            && modes.is_none()
            && title.is_none()
            && palette.is_none()
            && new.history == self.history
        {
            return None;
        }
        let (styles, lines) = encoder.finish();
        Some(ScreenDelta {
            session: session.to_string(),
            generation: Seq(generation),
            base: Seq(base),
            revision: Seq(revision),
            scroll: u32::try_from(scroll).unwrap_or(u32::MAX),
            first_line: Seq(new.first_line),
            history: Seq(new.history),
            cursor,
            modes,
            title,
            palette,
            styles,
            lines,
        })
    }

    /// Apply a delta whose base is this screen.
    pub fn apply(&mut self, delta: &ScreenDelta) -> Result<(), ScreenError> {
        let expected = self.first_line.checked_add(u64::from(delta.scroll));
        if expected != Some(delta.first_line.0) {
            return Err(ScreenError::new("first_line does not follow from scroll"));
        }
        let mut lines = self.lines.clone();
        Self::scroll_up(&mut lines, u64::from(delta.scroll), self.cols);
        for wire in &delta.lines {
            let row = usize::from(wire.row);
            if row >= lines.len() {
                return Err(ScreenError::new("a line is below the screen"));
            }
            lines[row] = decode_line(&delta.styles, wire, self.cols)?;
        }
        if let Some(palette) = &delta.palette {
            check_palette(palette)?;
            self.palette = palette.clone();
        }
        if let Some(cursor) = delta.cursor {
            check_cursor(&cursor, self.cols, self.rows)?;
            self.cursor = cursor;
        }
        if let Some(modes) = delta.modes {
            self.modes = modes;
        }
        if let Some(title) = &delta.title {
            self.title = (!title.is_empty()).then(|| title.clone());
        }
        self.lines = lines;
        self.first_line = delta.first_line.0;
        self.history = delta.history.0;
        Ok(())
    }
}

impl Line {
    fn is_blank_line(&self) -> bool {
        !self.wrapped && self.cells.iter().all(Cell::is_blank)
    }
}

/// A frame's runs of cells sharing a style.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    /// Index into the frame's `styles`.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub s: u16,
    pub t: String,
    /// Set when the run is one cell spanning this many columns (see the module docs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub w: Option<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireLine {
    /// The screen row, or for history the offset from the chunk's `first`.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub row: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<Run>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub wrapped: bool,
}

/// Builds a frame's lines, listing each style once.
#[derive(Default)]
pub struct RowsEncoder {
    styles: Vec<Style>,
    index: HashMap<Style, u16>,
    lines: Vec<WireLine>,
}

impl RowsEncoder {
    fn style(&mut self, style: Style) -> u16 {
        if let Some(&i) = self.index.get(&style) {
            return i;
        }
        let i = self.styles.len() as u16;
        self.styles.push(style);
        self.index.insert(style, i);
        i
    }

    pub fn push(&mut self, row: u16, line: &Line) {
        let end = line.cells.iter().rposition(|c| !c.is_blank()).map_or(0, |i| i + 1);
        let mut runs: Vec<Run> = Vec::new();
        for cell in line.cells[..end].iter().filter(|c| c.width > 0) {
            let s = self.style(cell.style);
            if cell.width == 1 && cell.combining.is_none() {
                match runs.last_mut() {
                    Some(run) if run.s == s && run.w.is_none() => run.t.push(cell.c),
                    _ => runs.push(Run { s, t: cell.c.to_string(), w: None }),
                }
            } else {
                let mut t = cell.c.to_string();
                t.extend(cell.combining.iter().flat_map(|c| c.iter()));
                runs.push(Run { s, t, w: Some(cell.width) });
            }
        }
        self.lines.push(WireLine { row, runs, wrapped: line.wrapped });
    }

    pub fn finish(self) -> (Vec<Style>, Vec<WireLine>) {
        (self.styles, self.lines)
    }
}

/// Why a screen frame cannot be applied. The client asks for a fresh snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenError(pub String);

impl ScreenError {
    fn new(message: &str) -> Self {
        Self(message.to_string())
    }
}

impl fmt::Display for ScreenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScreenError {}

/// Turn one wire line back into `cols` cells.
pub fn decode_line(styles: &[Style], wire: &WireLine, cols: u16) -> Result<Line, ScreenError> {
    let cols = usize::from(cols);
    let mut cells = Vec::with_capacity(cols);
    for run in &wire.runs {
        let style = *styles.get(usize::from(run.s)).ok_or_else(|| ScreenError::new("unknown style"))?;
        match run.w {
            None => {
                for c in run.t.chars() {
                    cells.push(Cell { c, combining: None, width: 1, style });
                }
            }
            Some(width @ 1..=2) => {
                let mut chars = run.t.chars();
                let c = chars.next().ok_or_else(|| ScreenError::new("an empty cell"))?;
                let combining: Vec<char> = chars.collect();
                let combining = (!combining.is_empty()).then(|| combining.into_boxed_slice());
                cells.push(Cell { c, combining, width, style });
                if width == 2 {
                    cells.push(Cell { c: ' ', combining: None, width: 0, style });
                }
            }
            Some(_) => return Err(ScreenError::new("a cell is one or two columns wide")),
        }
        if cells.len() > cols {
            return Err(ScreenError::new("a line is wider than the screen"));
        }
    }
    cells.resize(cols, Cell::default());
    Ok(Line { cells, wrapped: wire.wrapped })
}

fn check_palette(palette: &[Rgb]) -> Result<(), ScreenError> {
    if palette.len() != PALETTE_LEN {
        return Err(ScreenError::new("the palette has the wrong length"));
    }
    Ok(())
}

fn check_cursor(cursor: &Cursor, cols: u16, rows: u16) -> Result<(), ScreenError> {
    if cursor.col >= cols || cursor.row >= rows {
        return Err(ScreenError::new("the cursor is outside the screen"));
    }
    Ok(())
}

/// Which part of a snapshot split across frames this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    pub index: u16,
    pub count: u16,
}

/// A whole screen (or one part of it). Rows not listed are blank.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    pub session: String,
    pub generation: Seq,
    pub revision: Seq,
    pub cols: u16,
    pub rows: u16,
    pub first_line: Seq,
    pub history: Seq,
    pub cursor: Cursor,
    pub modes: Modes,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub palette: Vec<Rgb>,
    pub styles: Vec<Style>,
    pub lines: Vec<WireLine>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<Part>,
}

/// The change from revision `base` to `revision` in one generation. Apply `scroll` first (the top
/// lines leave the screen and blank lines fill in), then replace the listed lines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenDelta {
    pub session: String,
    pub generation: Seq,
    pub base: Seq,
    pub revision: Seq,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub scroll: u32,
    pub first_line: Seq,
    pub history: Seq,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<Modes>,
    /// The new title; empty when the program cleared it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub palette: Option<Vec<Rgb>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub styles: Vec<Style>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<WireLine>,
}

/// Retained history lines `before - count .. before` of one generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryGet {
    pub session: String,
    pub generation: Seq,
    /// The line ID after the last one wanted, usually the screen's `first_line`.
    pub before: Seq,
    pub count: u32,
}

/// Lines `first .. first + lines.len()`. `evicted` says the request reached lines older than
/// `oldest`, which are gone for good.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryChunk {
    pub session: String,
    pub generation: Seq,
    pub cols: u16,
    pub first: Seq,
    pub oldest: Seq,
    pub evicted: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub styles: Vec<Style>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<WireLine>,
}

impl HistoryChunk {
    pub fn decode(&self) -> Result<Vec<Line>, ScreenError> {
        let mut out = vec![Line::blank(self.cols); self.lines.len()];
        for (i, wire) in self.lines.iter().enumerate() {
            let at = usize::from(wire.row);
            if at != i {
                return Err(ScreenError::new("history lines are in order"));
            }
            out[i] = decode_line(&self.styles, wire, self.cols)?;
        }
        Ok(out)
    }
}

/// Bounds a decoded screen frame keeps; `wire::Message::validate` calls these.
pub(crate) fn validate_snapshot(s: &ScreenSnapshot) -> Option<&'static str> {
    if s.cols == 0 || s.rows == 0 || s.cols > MAX_COLS || s.rows > MAX_ROWS {
        return Some("screen dimensions are out of range");
    }
    if s.lines.len() > usize::from(s.rows) || s.palette.len() != PALETTE_LEN || s.styles.len() > usize::from(u16::MAX) {
        return Some("a snapshot is larger than its screen");
    }
    if s.part.is_some_and(|p| p.count == 0 || p.index >= p.count || p.count > s.rows) {
        return Some("a snapshot part is out of range");
    }
    None
}

pub(crate) fn validate_delta(d: &ScreenDelta) -> Option<&'static str> {
    if d.lines.len() > usize::from(MAX_ROWS) || d.styles.len() > usize::from(u16::MAX) {
        return Some("a delta is larger than any screen");
    }
    if d.palette.as_ref().is_some_and(|p| p.len() != PALETTE_LEN) {
        return Some("the palette has the wrong length");
    }
    (d.revision <= d.base).then_some("a delta moves its revision forward")
}

/// How a screen frame changed the mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenApplied {
    Changed,
    /// One part of a split snapshot; the screen changes when the last part arrives.
    Partial,
    /// A delta older than what is held (it crossed a snapshot); nothing to do.
    Stale,
}

/// Why the mirror cannot follow and needs a fresh snapshot (`resync` with this session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenGap {
    /// A delta arrived for a generation without its snapshot.
    NoSnapshot,
    /// A delta does not start at the revision held: one was dropped.
    Missing {
        have: u64,
        base: u64,
    },
    Malformed(ScreenError),
}

/// A client's copy of one terminal screen, kept current by applying `screen_snapshot` and
/// `screen_delta` frames. A gap is reported, never guessed over.
#[derive(Debug, Clone, Default)]
pub struct ScreenMirror {
    pub screen: Option<Screen>,
    pub generation: u64,
    pub revision: u64,
    assembly: Option<Assembly>,
}

#[derive(Debug, Clone)]
struct Assembly {
    generation: u64,
    revision: u64,
    count: u16,
    parts: BTreeMap<u16, ScreenSnapshot>,
}

impl ScreenMirror {
    pub fn apply_snapshot(&mut self, s: &ScreenSnapshot) -> Result<ScreenApplied, ScreenGap> {
        let (generation, revision) = (s.generation.0, s.revision.0);
        let Some(part) = s.part else {
            self.assembly = None;
            return self.install(&[s]);
        };
        let fresh = self
            .assembly
            .as_ref()
            .is_none_or(|a| (a.generation, a.revision, a.count) != (generation, revision, part.count));
        if fresh {
            // A newer snapshot supersedes a half-assembled one; only one is ever kept.
            self.assembly = Some(Assembly { generation, revision, count: part.count, parts: BTreeMap::new() });
        }
        let assembly = self.assembly.as_mut().expect("just set");
        assembly.parts.insert(part.index, s.clone());
        if assembly.parts.len() < usize::from(assembly.count) {
            return Ok(ScreenApplied::Partial);
        }
        let assembly = self.assembly.take().expect("complete");
        let parts: Vec<&ScreenSnapshot> = assembly.parts.values().collect();
        self.install(&parts)
    }

    fn install(&mut self, parts: &[&ScreenSnapshot]) -> Result<ScreenApplied, ScreenGap> {
        let malformed = |m: &str| ScreenGap::Malformed(ScreenError::new(m));
        let head = parts[0];
        if let Some(why) = validate_snapshot(head) {
            return Err(malformed(why));
        }
        check_cursor(&head.cursor, head.cols, head.rows).map_err(ScreenGap::Malformed)?;
        let mut screen = Screen {
            cols: head.cols,
            rows: head.rows,
            lines: vec![Line::blank(head.cols); usize::from(head.rows)],
            cursor: head.cursor,
            modes: head.modes,
            title: head.title.clone(),
            palette: head.palette.clone(),
            first_line: head.first_line.0,
            history: head.history.0,
        };
        for part in parts {
            if (part.cols, part.rows, part.generation, part.revision)
                != (head.cols, head.rows, head.generation, head.revision)
            {
                return Err(malformed("snapshot parts disagree"));
            }
            for wire in &part.lines {
                let row = usize::from(wire.row);
                if row >= screen.lines.len() {
                    return Err(malformed("a line is below the screen"));
                }
                screen.lines[row] = decode_line(&part.styles, wire, head.cols).map_err(ScreenGap::Malformed)?;
            }
        }
        self.generation = head.generation.0;
        self.revision = head.revision.0;
        self.screen = Some(screen);
        Ok(ScreenApplied::Changed)
    }

    pub fn apply_delta(&mut self, d: &ScreenDelta) -> Result<ScreenApplied, ScreenGap> {
        let Some(screen) = self.screen.as_mut().filter(|_| self.generation == d.generation.0) else {
            return Err(ScreenGap::NoSnapshot);
        };
        if d.revision.0 <= self.revision {
            return Ok(ScreenApplied::Stale);
        }
        if d.base.0 != self.revision {
            return Err(ScreenGap::Missing { have: self.revision, base: d.base.0 });
        }
        screen.apply(d).map_err(ScreenGap::Malformed)?;
        self.revision = d.revision.0;
        Ok(ScreenApplied::Changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled(c: char, attrs: u16, fg: Color) -> Cell {
        Cell { c, combining: None, width: 1, style: Style { fg, attrs, ..Style::default() } }
    }

    /// A deterministic screen with wide, combining, coloured and wrapped cells.
    fn sample(cols: u16, rows: u16, seed: u64) -> Screen {
        let mut screen = Screen::blank(cols, rows);
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for line in &mut screen.lines {
            let mut col = 0;
            let used = (next() % u64::from(cols + 1)) as usize;
            while col < used {
                let pick = next() % 10;
                let fg = match next() % 3 {
                    0 => Color::Slot(FOREGROUND),
                    1 => Color::Slot((next() % 256) as u16),
                    _ => Color::Rgb(Rgb([next() as u8, 7, 9])),
                };
                if pick == 0 && col + 1 < cols as usize {
                    let style = Style { fg, ..Style::default() };
                    line.cells[col] = Cell { c: '中', combining: None, width: 2, style };
                    line.cells[col + 1] = Cell { c: ' ', combining: None, width: 0, style };
                    col += 2;
                    continue;
                }
                line.cells[col] = if pick == 1 {
                    Cell {
                        c: 'e',
                        combining: Some(vec!['\u{301}'].into_boxed_slice()),
                        width: 1,
                        style: Style::default(),
                    }
                } else {
                    styled((b'a' + (next() % 26) as u8) as char, (next() % 4) as u16, fg)
                };
                col += 1;
            }
            line.wrapped = next() % 5 == 0;
        }
        screen.cursor = Cursor { row: rows - 1, col: 2, shape: CursorShape::Beam, blinking: true };
        screen.palette[1] = Rgb([200, 10, 10]);
        screen
    }

    fn encode_decode(screen: &Screen) -> Screen {
        let parts = screen.snapshot("s", 1, 1, Some).unwrap();
        let mut mirror = ScreenMirror::default();
        for part in &parts {
            let json = serde_json::to_string(part).unwrap();
            mirror.apply_snapshot(&serde_json::from_str(&json).unwrap()).unwrap();
        }
        mirror.screen.unwrap()
    }

    #[test]
    fn a_snapshot_round_trips_wide_combining_coloured_and_wrapped_cells() {
        for seed in 0..20 {
            let screen = sample(37, 9, seed);
            assert_eq!(encode_decode(&screen), screen, "seed {seed}");
        }
    }

    #[test]
    fn the_wire_form_shares_styles_and_drops_trailing_blanks() {
        let mut screen = Screen::blank(80, 3);
        for (i, c) in "hello".chars().enumerate() {
            screen.lines[0].cells[i].c = c;
        }
        screen.lines[1].cells[0] = Cell { c: '界', combining: None, width: 2, style: Style::default() };
        screen.lines[1].cells[1].width = 0;
        let parts = screen.snapshot("s", 1, 1, Some).unwrap();
        let json = serde_json::to_value(&parts[0]).unwrap();
        assert_eq!(json["styles"], serde_json::json!([{}]), "one style, the default, written as {{}}");
        assert_eq!(
            json["lines"],
            serde_json::json!([{ "runs": [{ "t": "hello" }] }, { "row": 1, "runs": [{ "t": "界", "w": 2 }] }]),
            "blank rows and trailing blanks are left out"
        );
        assert_eq!(serde_json::to_value(Color::Rgb(Rgb([255, 0, 16]))).unwrap(), "#ff0010");
        assert_eq!(serde_json::to_value(Color::Slot(FOREGROUND)).unwrap(), 256);
    }

    #[test]
    fn a_delta_carries_scroll_and_only_the_changed_lines() {
        let old = sample(20, 6, 3);
        let mut new = old.clone();
        // Two lines scroll into history, then the bottom line is written.
        new.lines.drain(..2);
        new.lines.extend([Line::blank(20), Line::blank(20)]);
        new.lines[5].cells[0].c = '$';
        new.first_line += 2;
        new.history += 2;
        let delta = old.delta(&new, "s", 1, 4, 5).unwrap();
        assert_eq!(delta.scroll, 2);
        assert_eq!(delta.lines.iter().map(|l| l.row).collect::<Vec<_>>(), vec![5]);
        let mut applied = old.clone();
        applied.apply(&delta).unwrap();
        assert_eq!(applied, new);
        assert!(new.delta(&new, "s", 1, 5, 6).is_none(), "nothing changed, nothing to send");
    }

    #[test]
    fn deltas_between_any_two_screens_apply_exactly() {
        for seed in 0..40 {
            let old = sample(23, 7, seed);
            let mut new = sample(23, 7, seed + 1000);
            new.first_line = old.first_line + seed % 9;
            new.history = seed % 4;
            new.title = (seed % 2 == 0).then(|| format!("t{seed}"));
            let delta = old.delta(&new, "s", 2, 1, 2).unwrap();
            let json = serde_json::to_string(&delta).unwrap();
            let mut applied = old.clone();
            applied.apply(&serde_json::from_str(&json).unwrap()).unwrap();
            assert_eq!(applied, new, "seed {seed}");
        }
    }

    #[test]
    fn a_large_snapshot_splits_into_parts_that_apply_only_when_complete() {
        let screen = sample(60, 40, 7);
        let whole = serde_json::to_string(&screen.snapshot("s", 3, 9, Some).unwrap()[0]).unwrap().len();
        let parts = screen
            .snapshot("s", 3, 9, |p| (serde_json::to_string(&p).unwrap().len() < whole / 3).then_some(p))
            .unwrap();
        assert!(parts.len() >= 4, "{}", parts.len());
        let mut mirror = ScreenMirror::default();
        for (i, part) in parts.iter().enumerate().rev() {
            let applied = mirror.apply_snapshot(part).unwrap();
            if i > 0 {
                assert_eq!(applied, ScreenApplied::Partial);
                assert!(mirror.screen.is_none(), "nothing shows until the last part");
            }
        }
        assert_eq!(mirror.screen.as_ref(), Some(&screen));
        assert_eq!((mirror.generation, mirror.revision), (3, 9));
        assert!(screen.snapshot("s", 3, 9, |_| None::<()>).is_none(), "a line that can never fit is refused");
    }

    #[test]
    fn a_dropped_delta_is_a_gap_and_a_crossed_one_is_stale() {
        let a = sample(10, 4, 1);
        let b = sample(10, 4, 2);
        let c = sample(10, 4, 3);
        let mut mirror = ScreenMirror::default();
        assert_eq!(mirror.apply_delta(&a.delta(&b, "s", 1, 1, 2).unwrap()), Err(ScreenGap::NoSnapshot));
        mirror.apply_snapshot(&a.snapshot("s", 1, 1, Some).unwrap()[0]).unwrap();
        let ab = a.delta(&b, "s", 1, 1, 2).unwrap();
        let bc = b.delta(&c, "s", 1, 2, 3).unwrap();
        assert_eq!(mirror.apply_delta(&bc), Err(ScreenGap::Missing { have: 1, base: 2 }));
        assert_eq!(mirror.screen.as_ref(), Some(&a), "a gap changes nothing");
        mirror.apply_delta(&ab).unwrap();
        mirror.apply_delta(&bc).unwrap();
        assert_eq!(mirror.screen.as_ref(), Some(&c));
        assert_eq!(mirror.apply_delta(&ab), Ok(ScreenApplied::Stale));
        let other_generation = ScreenDelta { generation: Seq(2), ..bc };
        assert_eq!(mirror.apply_delta(&other_generation), Err(ScreenGap::NoSnapshot));
    }

    #[test]
    fn malformed_lines_are_refused() {
        let styles = [Style::default()];
        let line = |runs: Vec<Run>| WireLine { row: 0, runs, wrapped: false };
        let run = |t: &str, w: Option<u8>, s: u16| Run { s, t: t.into(), w };
        for (wire, why) in [
            (line(vec![run("abcdef", None, 0)]), "wider"),
            (line(vec![run("a", None, 3)]), "style"),
            (line(vec![run("", Some(1), 0)]), "empty"),
            (line(vec![run("a", Some(3), 0)]), "columns"),
            (line(vec![run("abcd", None, 0), run("中", Some(2), 0)]), "wider"),
        ] {
            let error = decode_line(&styles, &wire, 5).unwrap_err();
            assert!(error.0.contains(why), "{error} for {wire:?}");
        }
        let mut s = sample(5, 2, 1).snapshot("s", 1, 1, Some).unwrap().remove(0);
        s.cursor.row = 9;
        assert!(matches!(ScreenMirror::default().apply_snapshot(&s), Err(ScreenGap::Malformed(_))));
    }

    #[test]
    fn history_chunks_decode_in_order() {
        let a = sample(8, 3, 4);
        let mut encoder = RowsEncoder::default();
        for (i, line) in a.lines.iter().enumerate() {
            encoder.push(i as u16, line);
        }
        let (styles, lines) = encoder.finish();
        let chunk = HistoryChunk {
            session: "s".into(),
            generation: Seq(1),
            cols: 8,
            first: Seq(10),
            oldest: Seq(10),
            evicted: true,
            styles,
            lines,
        };
        assert_eq!(chunk.decode().unwrap(), a.lines);
    }
}
