//! Sizing for a finger. A platform with a coarse pointer (the browser client) sets
//! [`Client::touch`](crate::Client::touch); the shared views then make every control at least [`Touch::target`] square
//! and keep their content off the screen's sides by [`Touch::gutter`], and on a small screen ([`Touch::stack`]) stack what
//! sits side by side and wrap long lines. The desktop leaves it `None` and draws as it always has.

use egui::{Button, IntoAtoms, LayerId, Rect, Response, Sense, Ui, vec2};

/// A finger's measures, in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Touch {
    /// The least width and height of a control.
    pub target: f32,
    /// The space between the screen's sides and the content.
    pub gutter: f32,
    /// A phone: no room for things side by side. A tablet has it.
    pub stack: bool,
}

/// Whether views stack what sits side by side.
pub fn stacks(touch: Option<Touch>) -> bool {
    touch.is_some_and(|touch| touch.stack)
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

/// `response`, with a finger's tap counting anywhere in a target around it, for a label or a mark drawn smaller than that.
/// What is drawn does not move. Where controls have a mouse's size it is as it was.
pub fn reach(ui: &mut Ui, response: Response) -> Response {
    let target = ui.spacing().interact_size.y;
    if target <= crate::theme::CONTROL_HEIGHT {
        return response;
    }
    let rect = Rect::from_center_size(response.rect.center(), response.rect.size().max(vec2(target, target)));
    let around = ui.interact(rect, response.id.with("finger"), Sense::click());
    response | around
}

/// A button, one a finger can hit under `touch`.
pub fn button<'a>(ui: &mut Ui, touch: Option<Touch>, atoms: impl IntoAtoms<'a>) -> Response {
    ui.add(sized(Button::new(atoms), touch))
}
