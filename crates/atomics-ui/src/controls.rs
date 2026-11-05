//! What the controls share beyond a plain `ui.button`. The theme gives every control one height, radius and hover
//! ([`Palette::style`](crate::theme::Palette::style)), and a button's own `.fill` or an unselected `.stroke` would
//! switch that hover off. So the primary action and the flat tabs get their look here, and keep it. A view has at most
//! one [`primary`] action.

use egui::{Atom, Button, Color32, Rect, Response, RichText, Stroke, Ui, Vec2, vec2};

use crate::status::{Mark, paint_mark};
use crate::theme::{Palette, lerp};
use crate::touch::{self, Touch};

/// The action a view is there for: filled with the accent, lighter under the pointer.
pub fn primary(ui: &mut Ui, text: impl Into<String>, touch: Option<Touch>, p: &Palette) -> Response {
    let label = RichText::new(text.into()).color(p.on_accent);
    ui.scope(|ui| {
        let widgets = &mut ui.style_mut().visuals.widgets;
        for (state, lift) in [(&mut widgets.inactive, 0.0), (&mut widgets.hovered, 0.14), (&mut widgets.active, 0.26)] {
            let fill = lerp(p.accent, p.on_accent, lift);
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
        }
        ui.add(touch::sized(Button::new(label), touch))
    })
    .inner
}

/// A tab in a row of them: no frame until the pointer is over it, and outlined while it is the one selected.
pub fn flat<'a>(button: Button<'a>, selected: bool, p: &Palette) -> Button<'a> {
    if selected { button.stroke(Stroke::new(1.0, p.accent)) } else { button.frame_when_inactive(false) }
}

/// `edit` as tall as the buttons beside it, its text in the middle.
pub fn field<'a>(edit: egui::TextEdit<'a>) -> egui::TextEdit<'a> {
    edit.min_size(vec2(0.0, crate::theme::CONTROL_HEIGHT)).align(egui::Align2::LEFT_CENTER)
}

/// A one-line text field, as tall as the buttons beside it.
pub fn single_line(text: &mut dyn egui::TextBuffer) -> egui::TextEdit<'_> {
    field(egui::TextEdit::singleline(text))
}

/// The down-pointing mark of what opens a menu, in `rect`.
pub fn chevron(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let points = vec![c + vec2(-3.5, -1.5), c + vec2(0.0, 2.0), c + vec2(3.5, -1.5)];
    painter.add(egui::Shape::line(points, Stroke::new(1.3, color)));
}

/// [`chevron`] as a part of a button.
pub fn chevron_atom<'a>(color: Color32) -> Atom<'a> {
    Atom::paint(vec2(9.0, 9.0), move |ui: &Ui, args: egui::AtomPaintArgs| chevron(ui.painter(), args.rect, color))
}

/// A button that opens a menu and says so with a chevron; inside a menu, a submenu with its own arrow.
pub fn menu_button(ui: &mut Ui, text: impl Into<egui::WidgetText>, content: impl FnOnce(&mut Ui)) -> Response {
    if egui::containers::menu::is_in_menu(ui) {
        return ui.menu_button(text, content).response;
    }
    let arrow = chevron_atom(ui.visuals().weak_text_color());
    egui::containers::menu::MenuButton::from_button(Button::new((text.into(), arrow))).ui(ui, content).0
}

/// A line of text after a painted `mark`, wrapping under itself.
pub fn marked_line(ui: &mut Ui, mark: Mark, color: Color32, text: RichText) {
    ui.horizontal_top(|ui| {
        let height = ui.text_style_height(&egui::TextStyle::Body);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(12.0, height), egui::Sense::hover());
        paint_mark(ui.painter(), rect.center(), mark, color);
        ui.add(egui::Label::new(text).wrap());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomics_core::config::ThemeConfig;

    fn palette() -> Palette {
        let config = ThemeConfig { name: "claude".into(), ..ThemeConfig::default() };
        Palette::new(&atomics_core::theme::resolve(&config, |_| None).tokens, Default::default())
    }

    /// The fills of the rectangles a frame paints, drawing `add` with the pointer at `pointer`, once it has settled.
    fn fills(pointer: Option<egui::Pos2>, p: &Palette, add: impl Fn(&mut Ui) -> Response) -> (Vec<Color32>, Rect) {
        let ctx = egui::Context::default();
        p.apply(&ctx, &crate::theme::tests_support::NoFonts);
        let mut rect = Rect::NOTHING;
        let mut shown = Vec::new();
        for _ in 0..3 {
            let events = pointer.map(egui::Event::PointerMoved).into_iter().collect();
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(400.0, 200.0))),
                events,
                ..Default::default()
            };
            let output = ctx.run_ui(input, |ui| rect = add(ui).rect);
            shown = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Rect(painted) if painted.rect.intersects(rect) => Some(painted.fill),
                    _ => None,
                })
                .collect();
            output.drop_without_applying_deltas();
        }
        (shown, rect)
    }

    #[test]
    fn the_primary_action_is_the_accent_and_answers_the_pointer() {
        let p = palette();
        let draw = |ui: &mut Ui| primary(ui, "Approve plan", None, &p);
        let (rest, rect) = fills(None, &p, draw);
        assert!(rest.contains(&p.accent), "{rest:?}");
        assert_eq!(rect.height(), crate::theme::CONTROL_HEIGHT);
        let (hover, _) = fills(Some(rect.center()), &p, draw);
        assert!(!hover.contains(&p.accent) && hover.contains(&lerp(p.accent, p.on_accent, 0.14)), "{hover:?}");
    }

    #[test]
    fn a_flat_tab_has_no_frame_until_the_pointer_is_over_it() {
        let p = palette();
        let draw = |ui: &mut Ui| ui.add(flat(Button::new("All · 1"), false, &p));
        let (rest, rect) = fills(None, &p, draw);
        assert!(rest.iter().all(|fill| *fill == Color32::TRANSPARENT), "{rest:?}");
        assert_eq!(rect.height(), crate::theme::CONTROL_HEIGHT);
        let (hover, _) = fills(Some(rect.center()), &p, draw);
        assert!(hover.iter().any(|fill| *fill != Color32::TRANSPARENT), "the theme's hover shows: {hover:?}");
    }
}
