//! Sizing for a finger on a small screen. A platform that draws a phone layout (the browser client) sets
//! [`Client::touch`](crate::Client::touch); the shared views then stack what sits side by side, wrap long
//! lines, keep their content off the screen's sides by [`Touch::gutter`] and make every control at least
//! [`Touch::target`] square. The desktop leaves it `None` and draws as it always has.

use egui::{Button, IntoAtoms, LayerId, Rect, Response, Ui, vec2};

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

/// Says that a tap on `response` opens a text box. iOS opens the soft keyboard only in the touch's own handler, so the
/// platform has to know where before the frame that opens the box. A text box itself needs no mark.
pub fn opens_keyboard(ui: &Ui, response: &Response) {
    let opener = (ui.layer_id(), response.interact_rect);
    ui.ctx().data_mut(|data| data.get_temp_mut_or_default::<Vec<(LayerId, Rect)>>(openers()).push(opener));
}

/// What this frame marked with [`opens_keyboard`].
pub fn keyboard_openers(ctx: &egui::Context) -> Vec<(LayerId, Rect)> {
    ctx.data_mut(|data| std::mem::take(data.get_temp_mut_or_default::<Vec<(LayerId, Rect)>>(openers())))
}

fn openers() -> egui::Id {
    egui::Id::NULL.with("keyboard-openers")
}

/// A button, one a finger can hit under `touch`.
pub fn button<'a>(ui: &mut Ui, touch: Option<Touch>, atoms: impl IntoAtoms<'a>) -> Response {
    ui.add(sized(Button::new(atoms), touch))
}
