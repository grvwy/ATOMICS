//! The desktop painter before the shared one (`atomics_ui::terminal`), kept for tests only: it
//! reads alacritty's `Term` directly, and `paint_tests` and `frame_bench` check the shared painter
//! against it. The body is the old `TerminalView::paint`, with the view's fields as arguments.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::selection::Selection;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use egui::{Color32, Pos2, Rect, Stroke, pos2, vec2};

use super::Metrics;
use crate::theme::Palette;

/// `rows` is the view's row count (`TerminalView::grid.1`).
#[allow(clippy::too_many_arguments)]
pub fn paint<T: EventListener>(
    term: &Term<T>,
    selection: Option<&Selection>,
    rows: usize,
    ui: &egui::Ui,
    (outer, rect): (Rect, Rect),
    metrics: &Metrics,
    palette: &Palette,
    focused: bool,
) {
    let painter = ui.painter_at(outer);
    painter.rect_filled(outer, 0.0, palette.terminal);

    let selection = selection.and_then(|s| s.to_range(term));
    let content = term.renderable_content();
    let colors = content.colors;
    let offset = content.display_offset as i32;
    let cell = metrics.cell;
    let origin = rect.min;
    let at = |row: i32, col: usize| pos2(origin.x + col as f32 * cell.x, origin.y + row as f32 * cell.y);

    let mut run = Run::default();
    for indexed in content.display_iter {
        let point = indexed.point;
        let c = indexed.cell;
        if c.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let row = point.line.0 + offset;
        let col = point.column.0;
        let mut fg = resolve(palette, c.fg, colors);
        let mut bg = resolve(palette, c.bg, colors);
        if c.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if c.flags.contains(Flags::DIM) {
            fg = dim(palette, fg);
        }
        if c.flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        if selection.is_some_and(|s| s.contains(point)) {
            bg = palette.selection;
        }
        let width = if c.flags.contains(Flags::WIDE_CHAR) { 2.0 } else { 1.0 };
        if bg != palette.terminal {
            painter.rect_filled(Rect::from_min_size(at(row, col), vec2(cell.x * width, cell.y)), 0.0, bg);
        }
        if c.flags.intersects(Flags::ALL_UNDERLINES) {
            let y = at(row, col).y + cell.y - 1.5;
            let x = at(row, col).x;
            painter.line_segment([pos2(x, y), pos2(x + cell.x * width, y)], Stroke::new(1.0, fg));
        }

        let ch = c.c;
        if ch == ' ' || ch == '\0' {
            // Blanks extend a run whatever their color, so a line stays one shape.
            if !run.text.is_empty() && run.row == row && run.next_col == col {
                run.text.push(' ');
                run.next_col += 1;
            }
            continue;
        }
        let simple = ch.is_ascii() && width == 1.0;
        if !run.continues(row, col, fg) || !simple {
            run.flush(&painter, &at, metrics);
        }
        if simple {
            if run.text.is_empty() {
                run = Run { row, col, fg, text: String::new(), next_col: col };
            }
            run.text.push(ch);
            run.next_col = col + 1;
        } else {
            let mut text = String::from(ch);
            if let Some(extra) = c.zerowidth() {
                text.extend(extra);
            }
            let center = at(row, col) + vec2(cell.x * width / 2.0, cell.y / 2.0);
            painter.text(center, egui::Align2::CENTER_CENTER, text, metrics.font.clone(), fg);
        }
    }
    run.flush(&painter, &at, metrics);

    let cursor = content.cursor;
    let cursor_row = cursor.point.line.0 + offset;
    if cursor.shape != CursorShape::Hidden && cursor_row >= 0 && (cursor_row as usize) < rows {
        let r = Rect::from_min_size(at(cursor_row, cursor.point.column.0), cell);
        match (focused, cursor.shape) {
            (true, CursorShape::Beam) => {
                painter.rect_filled(r.with_max_x(r.left() + 2.0), 0.0, palette.cursor);
            }
            (true, CursorShape::Underline) => {
                painter.rect_filled(r.with_min_y(r.bottom() - 2.0), 0.0, palette.cursor);
            }
            (true, _) => {
                painter.rect_filled(r, 0.0, palette.cursor.gamma_multiply(0.7));
            }
            (false, _) => {
                painter.rect_stroke(
                    r,
                    0.0,
                    Stroke::new(1.0, palette.cursor.gamma_multiply(0.6)),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }
}

/// Consecutive ASCII cells with the same color, laid out as one text shape.
#[derive(Default)]
struct Run {
    row: i32,
    col: usize,
    next_col: usize,
    fg: Color32,
    text: String,
}

impl Run {
    fn continues(&self, row: i32, col: usize, fg: Color32) -> bool {
        self.text.is_empty() || (self.row == row && self.next_col == col && self.fg == fg)
    }

    fn flush(&mut self, painter: &egui::Painter, at: &impl Fn(i32, usize) -> Pos2, metrics: &Metrics) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        let text = text.trim_end().to_string();
        painter.text(at(self.row, self.col), egui::Align2::LEFT_TOP, text, metrics.font.clone(), self.fg);
    }
}

fn resolve(palette: &Palette, color: Color, overrides: &alacritty_terminal::term::color::Colors) -> Color32 {
    let rgb = |rgb: Rgb| Color32::from_rgb(rgb.r, rgb.g, rgb.b);
    match color {
        Color::Spec(spec) => rgb(spec),
        Color::Indexed(index) => overrides[index as usize].map(rgb).unwrap_or_else(|| palette.indexed(index as usize)),
        Color::Named(name) => overrides[name].map(rgb).unwrap_or_else(|| named(palette, name)),
    }
}

fn named(palette: &Palette, name: NamedColor) -> Color32 {
    match name {
        NamedColor::Foreground | NamedColor::BrightForeground => palette.foreground,
        NamedColor::Background => palette.terminal,
        NamedColor::Cursor => palette.cursor,
        NamedColor::DimForeground => dim(palette, palette.foreground),
        other => {
            let index = other as usize;
            if index < 16 {
                palette.ansi[index]
            } else {
                // Dim variants follow the 16 colors in order, starting at DimBlack.
                let base = index.saturating_sub(NamedColor::DimBlack as usize).min(7);
                dim(palette, palette.ansi[base])
            }
        }
    }
}

/// The old `Palette::dim`, independent of the shared painter's.
fn dim(palette: &Palette, color: Color32) -> Color32 {
    crate::theme::lerp(palette.terminal, color, 0.62)
}
