//! Sizing for a finger on a small screen. A platform that draws a phone layout (the browser client) sets
//! [`Client::touch`](crate::Client::touch); the shared views then stack what sits side by side, wrap long
//! lines, keep their content off the screen's sides by [`Touch::gutter`] and make every control at least
//! [`Touch::target`] square. The desktop leaves it `None` and draws as it always has.

use egui::{Button, IntoAtoms, Response, Ui, vec2};

/// A phone's measures, in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Touch {
    /// The least width and height of a control.
    pub target: f32,
    /// The space between the screen's sides and the content.
    pub gutter: f32,
}

/// `button` as it is, or at least a target square under `touch`.
pub fn sized(button: Button<'_>, touch: Option<Touch>) -> Button<'_> {
    match touch {
        Some(touch) => button.min_size(vec2(touch.target, touch.target)),
        None => button,
    }
}

/// [`Ui::small_button`], or a button a finger can hit under `touch`.
pub fn small_button<'a>(ui: &mut Ui, touch: Option<Touch>, atoms: impl IntoAtoms<'a>) -> Response {
    match touch {
        Some(_) => ui.add(sized(Button::new(atoms), touch)),
        None => ui.small_button(atoms),
    }
}
