//! What the page's overlay script (`web/terminal.js`) reports, read into the frame's own types. The script
//! sends plain objects, `{kind, …}`, in CSS pixels; `zoom` (egui's zoom factor) turns a pointer's into
//! points. The viewport stays in CSS pixels, since the layout it picks may change the zoom
//! ([`Viewport::points`]). Anything malformed is dropped.

use egui::{Modifiers, Rect, pos2, vec2};
use serde_json::Value;

use crate::gesture::{Kind, Phase, Pointer};
use crate::layout::Viewport;
use crate::term_pane::TermInput;

#[derive(Debug, Clone, PartialEq)]
pub enum PageEvent {
    Term(TermInput),
    Viewport(Viewport),
    /// Something to tell the user (the clipboard could not be read, say).
    Note(String),
}

/// The modifier bits the script sends: 1 Shift, 2 Alt, 4 Ctrl, 8 Meta.
pub fn modifiers(bits: u64) -> Modifiers {
    Modifiers {
        shift: bits & 1 != 0,
        alt: bits & 2 != 0,
        ctrl: bits & 4 != 0,
        mac_cmd: bits & 8 != 0,
        command: bits & 8 != 0,
    }
}

pub fn read(event: &Value, zoom: f32) -> Option<PageEvent> {
    let text = |field: &str| event.get(field).and_then(Value::as_str).map(str::to_string);
    let number = |field: &str| event.get(field).and_then(Value::as_f64).filter(|n| n.is_finite());
    let point = |v: f64| v as f32 / zoom.max(0.01);
    let at = || Some(pos2(point(number("x")?), point(number("y")?)));
    let term = |input| Some(PageEvent::Term(input));
    match event.get("kind")?.as_str()? {
        "text" => term(TermInput::Text(text("text")?)),
        "preedit" => term(TermInput::Preedit(text("text")?)),
        "paste" => term(TermInput::Paste(text("text")?)),
        "key" => term(TermInput::Key { name: text("text")?, mods: modifiers(number("mods").unwrap_or(0.0) as u64) }),
        "focus" => term(TermInput::Focus(event.get("on")?.as_bool()?)),
        "copied" => term(TermInput::Copied),
        "pointer" => {
            let phase = match text("phase")?.as_str() {
                "down" => Phase::Down,
                "move" => Phase::Move,
                "up" => Phase::Up,
                "cancel" => Phase::Cancel,
                _ => return None,
            };
            let kind = match text("pointer").as_deref() {
                Some("mouse") => Kind::Mouse,
                Some("pen") => Kind::Pen,
                _ => Kind::Touch,
            };
            let id = number("id")? as i32;
            term(TermInput::Pointer(Pointer { id, phase, pos: at()?, kind, time_ms: number("t")? }))
        }
        "wheel" => {
            let delta = vec2(point(number("dx")?), point(number("dy")?));
            let zoom = event.get("zoom").and_then(Value::as_bool).unwrap_or(false);
            term(TermInput::Wheel { delta, at: at()?, zoom })
        }
        "viewport" => {
            let css = |field: &str| number(field).map(|n| n as f32);
            let visible = Rect::from_min_size(pos2(css("x")?, css("y")?), vec2(css("w")?, css("h")?));
            // A page the user pinch-zoomed shows part of the canvas magnified; the layout then uses it all.
            let scaled = number("scale").is_some_and(|s| (s - 1.0).abs() > 0.01);
            let inset = |field: &str| css(field).unwrap_or(0.0).max(0.0);
            Some(PageEvent::Viewport(Viewport {
                visible: (!scaled && visible.width() > 0.0 && visible.height() > 0.0).then_some(visible),
                coarse: event.get("coarse").and_then(Value::as_bool).unwrap_or(false),
                keybar: inset("keybar"),
                inset_top: inset("insetTop"),
                inset_bottom: inset("insetBottom"),
                inset_left: inset("insetLeft"),
                inset_right: inset("insetRight"),
            }))
        }
        "note" => Some(PageEvent::Note(text("text")?.chars().filter(|c| !c.is_control()).take(200).collect())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_scripts_events_become_inputs_in_points() {
        let input = |v: Value| match read(&v, 2.0) {
            Some(PageEvent::Term(input)) => input,
            other => panic!("{v}: {other:?}"),
        };
        assert_eq!(input(json!({ "kind": "text", "text": "åäö" })), TermInput::Text("åäö".into()));
        assert_eq!(input(json!({ "kind": "preedit", "text": "にほ" })), TermInput::Preedit("にほ".into()));
        assert_eq!(
            input(json!({ "kind": "key", "text": "c", "mods": 4 })),
            TermInput::Key { name: "c".into(), mods: Modifiers::CTRL }
        );
        assert_eq!(
            input(json!({ "kind": "key", "text": "2", "mods": 8 })),
            TermInput::Key { name: "2".into(), mods: Modifiers { mac_cmd: true, command: true, ..Modifiers::NONE } },
            "Command, as a native window reads it"
        );
        assert_eq!(input(json!({ "kind": "focus", "on": true })), TermInput::Focus(true));
        assert_eq!(
            input(
                json!({ "kind": "pointer", "phase": "down", "pointer": "touch", "id": 3, "x": 100, "y": 50, "t": 7.5 })
            ),
            TermInput::Pointer(Pointer {
                id: 3,
                phase: Phase::Down,
                pos: pos2(50.0, 25.0),
                kind: Kind::Touch,
                time_ms: 7.5
            })
        );
        assert_eq!(
            input(json!({ "kind": "wheel", "dx": 0, "dy": -40, "x": 10, "y": 10, "zoom": true })),
            TermInput::Wheel { delta: vec2(0.0, -20.0), at: pos2(5.0, 5.0), zoom: true }
        );
        for bad in [
            json!({ "kind": "text" }),
            json!({ "kind": "pointer", "phase": "hover", "id": 1, "x": 0, "y": 0, "t": 0 }),
            json!({ "kind": "focus", "on": "yes" }),
            json!({ "kind": "unknown" }),
            json!("text"),
        ] {
            assert_eq!(read(&bad, 1.0), None, "{bad}");
        }
    }

    #[test]
    fn the_viewport_is_the_visible_part_unless_the_page_is_zoomed() {
        let event = json!({ "kind": "viewport", "x": 0, "y": 40, "w": 390, "h": 460, "scale": 1, "coarse": true,
                            "keybar": 88, "insetTop": 47, "insetBottom": 0, "insetLeft": 0, "insetRight": 12 });
        // In CSS pixels, whatever egui's zoom: the layout turns them into points.
        let Some(PageEvent::Viewport(port)) = read(&event, 1.25) else { panic!("a viewport") };
        assert_eq!(port.visible, Some(Rect::from_min_size(pos2(0.0, 40.0), vec2(390.0, 460.0))));
        assert!(port.coarse);
        assert_eq!((port.keybar, port.inset_top, port.inset_right), (88.0, 47.0, 12.0));
        let mut zoomed = event.clone();
        zoomed["scale"] = json!(1.6);
        let Some(PageEvent::Viewport(port)) = read(&zoomed, 1.0) else { panic!("a viewport") };
        assert_eq!(port.visible, None);
        assert_eq!(
            read(&json!({ "kind": "note", "text": "no\u{7} clipboard" }), 1.0),
            Some(PageEvent::Note("no clipboard".into()))
        );
    }
}
