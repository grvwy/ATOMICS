//! Laying out the top bar: measuring a row of widgets before drawing it, and drawing into a bounded slot.

use egui::{Rect, vec2};

/// Measure with the same fonts, padding and widgets that will actually be drawn.
/// The disabled, invisible child cannot activate controls or take space from the bar.
pub fn measure(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui)) -> f32 {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("topbar-measure")
            .max_rect(Rect::from_min_size(ui.max_rect().min, vec2(100_000.0, ui.max_rect().height())))
            .layout(egui::Layout::left_to_right(egui::Align::Center))
            .sizing_pass()
            .invisible(),
    );
    contents(&mut child);
    child.min_rect().width().ceil()
}

/// A slot bounds both painting and layout. Popups retain their own clip rects.
pub fn slot(ui: &mut egui::Ui, id: &str, rect: Rect, contents: impl FnOnce(&mut egui::Ui)) {
    let mut child = ui.new_child(
        egui::UiBuilder::new().id_salt(id).max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(ui.clip_rect().intersect(rect));
    child.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
    contents(&mut child);
}
