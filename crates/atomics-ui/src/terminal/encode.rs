//! What a terminal client sends: keys, typed text, a paste and mouse reports, encoded from the
//! terminal's published modes (docs/daemon.md: clients encode keys and paste with the latest
//! published modes, and mouse reports from cell coordinates). The desktop's terminal panes and the
//! browser encode through here, so a key both send sends the same bytes in both.
//!
//! The browser sends more in one place. A character typed with Ctrl (a hardware keyboard's
//! Ctrl+character, or the key bar's sticky Ctrl, both of which arrive as characters) becomes xterm's
//! control byte through [`ctrl_char`], so Ctrl with `@`, `` ` ``, `^`, `~`, `_`, `/`, `-` or `?` sends
//! one there, and so does the sticky Ctrl with `2`-`8`; on a phone the key bar is the only way to type
//! them. A keyboard's Ctrl with a digit is the window's on every client and never comes here
//! (`crate::window_keys`). The desktop reads egui's keys and sends Ctrl only with a letter, Space, `[`,
//! `\` or `]` ([`ctrl_key`]), its bytes since before the browser, and it sends nothing for the rest.
//!
//! Keys are xterm's. The cursor keys follow application-cursor mode while no modifier is held, and
//! carry xterm's modifier code when one is; Shift or Alt with Return sends ESC CR, a newline in
//! Claude Code and most line editors. A program's kitty keyboard flags are not used: programs that
//! set them still read these forms, and the desktop always sent them.
//!
//! Everything is text: `terminal_input` carries UTF-8, so a legacy mouse report whose coordinates
//! need a byte above 127 cannot be sent and is left out.

use atomics_core::screen::Modes;
use egui::{Key, Modifiers};

/// The bytes `key` sends with `m` held, or `None` for a key a terminal does not take this way (a
/// letter without Ctrl arrives as text).
pub fn key(key: Key, m: Modifiers, modes: &Modes) -> Option<Vec<u8>> {
    let code = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.ctrl as u8;
    let cursor = |c: char| {
        if code > 1 {
            format!("\x1b[1;{code}{c}")
        } else if modes.app_cursor {
            format!("\x1bO{c}")
        } else {
            format!("\x1b[{c}")
        }
    };
    let tilde = |n: u8| if code > 1 { format!("\x1b[{n};{code}~") } else { format!("\x1b[{n}~") };
    let text = match key {
        // Shift/Option+Enter insert a newline in Claude Code and most line editors.
        Key::Enter if m.shift || m.alt => "\x1b\r".to_string(),
        Key::Enter => "\r".to_string(),
        Key::Tab if m.shift => "\x1b[Z".to_string(),
        Key::Tab => "\t".to_string(),
        Key::Backspace if m.alt => "\x1b\x7f".to_string(),
        Key::Backspace if m.ctrl => "\x08".to_string(),
        Key::Backspace => "\x7f".to_string(),
        Key::Escape => "\x1b".to_string(),
        Key::ArrowUp => cursor('A'),
        Key::ArrowDown => cursor('B'),
        Key::ArrowRight => cursor('C'),
        Key::ArrowLeft => cursor('D'),
        Key::Home => cursor('H'),
        Key::End => cursor('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => "\x1bOP".to_string(),
        Key::F2 => "\x1bOQ".to_string(),
        Key::F3 => "\x1bOR".to_string(),
        Key::F4 => "\x1bOS".to_string(),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        _ if m.ctrl => return ctrl_key(key).map(|b| vec![b]),
        _ => return None,
    };
    Some(text.into_bytes())
}

/// Ctrl with `key` as one control byte: a letter, Space, `[`, `\` or `]`.
pub fn ctrl_key(key: Key) -> Option<u8> {
    let name = key.name();
    if name.len() == 1 {
        let c = name.chars().next()?.to_ascii_lowercase();
        if c.is_ascii_lowercase() {
            return Some(c as u8 & 0x1f);
        }
    }
    match key {
        Key::Space => Some(0),
        Key::OpenBracket => Some(0x1b),
        Key::Backslash => Some(0x1c),
        Key::CloseBracket => Some(0x1d),
        _ => None,
    }
}

/// Ctrl with a typed character, as xterm sends it: a letter (either case) is 1-26, `@`, `` ` ``, Space
/// and `2` are 0, `[` and `3` are ESC, `\` and `4` are 28, `]` and `5` are 29, `^`, `~` and `6` are 30,
/// `_`, `/`, `-` and `7` are 31, and `?` and `8` are DEL. `None` for a character Ctrl does not change.
pub fn ctrl_char(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' | 'A'..='Z' => c.to_ascii_lowercase() as u8 & 0x1f,
        '@' | '`' | ' ' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '~' | '6' => 0x1e,
        '_' | '/' | '-' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// Text typed while sticky modifiers are held (a phone's key bar): Ctrl turns its first character
/// into a control byte where it has one, and Alt puts ESC before it. The rest goes as typed.
pub fn text(text: &str, ctrl: bool, alt: bool) -> String {
    let mut chars = text.chars();
    let Some(first) = chars.next() else { return String::new() };
    let mut out = String::with_capacity(text.len() + 1);
    if alt {
        out.push('\x1b');
    }
    match ctrl.then(|| ctrl_char(first)).flatten() {
        Some(byte) => out.push(char::from(byte)),
        None => out.push(first),
    }
    out.extend(chars);
    out
}

/// A paste: bracketed when the program asked for it, so its newlines do not submit early.
pub fn paste(text: &str, modes: &Modes) -> String {
    let bytes = atomics_core::steer::paste_bytes(text, modes.bracketed_paste);
    String::from_utf8(bytes).expect("a paste of text stays text")
}

/// A mouse button, as a report names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

/// Whether the program asked for mouse reports at all.
pub fn reports_mouse(modes: &Modes) -> bool {
    modes.mouse_click || modes.mouse_drag || modes.mouse_motion
}

/// The report for `button` going down (`pressed`) or up over the cell at `col`, `row` (from 0, on
/// the screen), or `None` when the program asked for no reports or the cell cannot be named. SGR
/// reports when the program asked for them; else xterm's, with UTF-8 coordinates in mode 1005.
/// A wheel has no release.
pub fn mouse(button: Button, pressed: bool, col: u16, row: u16, m: Modifiers, modes: &Modes) -> Option<String> {
    if !reports_mouse(modes) {
        return None;
    }
    let wheel = matches!(button, Button::WheelUp | Button::WheelDown);
    if wheel && !pressed {
        return None;
    }
    let base: u32 = match button {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
        Button::WheelUp => 64,
        Button::WheelDown => 65,
    };
    let mods = 4 * u32::from(m.shift) + 8 * u32::from(m.alt) + 16 * u32::from(m.ctrl);
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);
    if modes.sgr_mouse {
        let end = if pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{};{x};{y}{end}", base + mods));
    }
    // xterm's legacy form: a release names button 3, and every value is offset by 32.
    let code = if pressed { base } else { 3 } + mods;
    let limit = if modes.utf8_mouse { 2047 } else { 127 };
    let at = |v: u32| (v + 32 <= limit).then(|| char::from_u32(v + 32)).flatten();
    let mut out = String::from("\x1b[M");
    out.push(char::from_u32(code + 32)?);
    out.push(at(x)?);
    out.push(at(y)?);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes(set: impl FnOnce(&mut Modes)) -> Modes {
        let mut modes = Modes::default();
        set(&mut modes);
        modes
    }

    fn sent(key_: Key, m: Modifiers, modes: &Modes) -> String {
        String::from_utf8(key(key_, m, modes).expect("the key sends something")).unwrap()
    }

    #[test]
    fn keys_follow_the_cursor_mode_and_carry_modifiers() {
        let (none, normal) = (Modifiers::NONE, Modes::default());
        let app = modes(|m| m.app_cursor = true);
        for (k, plain, application) in [
            (Key::ArrowUp, "\x1b[A", "\x1bOA"),
            (Key::ArrowDown, "\x1b[B", "\x1bOB"),
            (Key::ArrowRight, "\x1b[C", "\x1bOC"),
            (Key::ArrowLeft, "\x1b[D", "\x1bOD"),
            (Key::Home, "\x1b[H", "\x1bOH"),
            (Key::End, "\x1b[F", "\x1bOF"),
        ] {
            assert_eq!(sent(k, none, &normal), plain, "{k:?}");
            assert_eq!(sent(k, none, &app), application, "{k:?} in application-cursor mode");
        }
        // A modifier wins over the mode: xterm's modifier code, the same in both modes.
        for m in [&normal, &app] {
            assert_eq!(sent(Key::ArrowLeft, Modifiers::ALT, m), "\x1b[1;3D");
            assert_eq!(sent(Key::ArrowUp, Modifiers::CTRL, m), "\x1b[1;5A");
            assert_eq!(sent(Key::ArrowRight, Modifiers::SHIFT | Modifiers::CTRL, m), "\x1b[1;6C");
        }
        assert_eq!(sent(Key::PageUp, none, &app), "\x1b[5~", "the editing keys do not change with the mode");
        assert_eq!(sent(Key::PageDown, Modifiers::SHIFT, &normal), "\x1b[6;2~");
        assert_eq!(sent(Key::Delete, none, &normal), "\x1b[3~");
        assert_eq!(sent(Key::F1, none, &normal), "\x1bOP");
        assert_eq!(sent(Key::F5, Modifiers::CTRL, &normal), "\x1b[15;5~");
    }

    #[test]
    fn named_keys_send_what_the_desktop_always_sent() {
        let (none, m) = (Modifiers::NONE, Modes::default());
        assert_eq!(sent(Key::Enter, none, &m), "\r");
        assert_eq!(sent(Key::Enter, Modifiers::SHIFT, &m), "\x1b\r");
        assert_eq!(sent(Key::Enter, Modifiers::ALT, &m), "\x1b\r");
        assert_eq!(sent(Key::Tab, none, &m), "\t");
        assert_eq!(sent(Key::Tab, Modifiers::SHIFT, &m), "\x1b[Z");
        assert_eq!(sent(Key::Backspace, none, &m), "\x7f");
        assert_eq!(sent(Key::Backspace, Modifiers::CTRL, &m), "\x08");
        assert_eq!(sent(Key::Backspace, Modifiers::ALT, &m), "\x1b\x7f");
        assert_eq!(sent(Key::Escape, none, &m), "\x1b");
        assert_eq!(key(Key::C, Modifiers::CTRL, &m), Some(vec![3]));
        assert_eq!(key(Key::Space, Modifiers::CTRL, &m), Some(vec![0]));
        assert_eq!(key(Key::OpenBracket, Modifiers::CTRL, &m), Some(vec![0x1b]));
        assert_eq!(key(Key::C, none, &m), None, "a letter is text");
        assert_eq!(key(Key::Num1, Modifiers::CTRL, &m), None);
    }

    #[test]
    fn sticky_modifiers_change_the_first_character_typed() {
        assert_eq!(text("c", true, false), "\x03");
        assert_eq!(text("C", true, false), "\x03", "either case");
        assert_eq!(text("cd", true, false), "\x03d", "only the first character");
        assert_eq!(text("b", false, true), "\x1bb");
        assert_eq!(text("x", true, true), "\x1b\x18");
        assert_eq!(text("é", true, false), "é", "Ctrl leaves a character it has no byte for");
        assert_eq!(text("", true, true), "");
        assert_eq!(text("ls", false, false), "ls");
        for (c, byte) in [('@', 0), (' ', 0), ('[', 0x1b), ('\\', 0x1c), (']', 0x1d), ('~', 0x1e), ('/', 0x1f)] {
            assert_eq!(ctrl_char(c), Some(byte), "Ctrl+{c}");
        }
        assert_eq!((ctrl_char('-'), ctrl_char('?'), ctrl_char('|')), (Some(0x1f), Some(0x7f), None));
    }

    #[test]
    fn a_paste_is_bracketed_only_when_the_program_asked() {
        let plain = Modes::default();
        let bracketed = modes(|m| m.bracketed_paste = true);
        assert_eq!(paste("a\nb\r\nc", &plain), "a\rb\rc", "newlines are Returns");
        assert_eq!(paste("a\nb", &bracketed), "\x1b[200~a\nb\x1b[201~");
        assert_eq!(paste("x\x1b[201~y", &bracketed), "\x1b[200~xy\x1b[201~", "a paste cannot end itself early");
    }

    #[test]
    fn mouse_reports_follow_the_reporting_mode() {
        let none = Modifiers::NONE;
        assert_eq!(mouse(Button::Left, true, 4, 2, none, &Modes::default()), None, "no reports asked for");
        let sgr = modes(|m| {
            m.mouse_click = true;
            m.sgr_mouse = true;
        });
        assert_eq!(mouse(Button::Left, true, 4, 2, none, &sgr).unwrap(), "\x1b[<0;5;3M");
        assert_eq!(mouse(Button::Left, false, 4, 2, none, &sgr).unwrap(), "\x1b[<0;5;3m");
        assert_eq!(mouse(Button::Right, true, 0, 0, Modifiers::CTRL, &sgr).unwrap(), "\x1b[<18;1;1M");
        assert_eq!(mouse(Button::WheelUp, true, 9, 9, none, &sgr).unwrap(), "\x1b[<64;10;10M");
        assert_eq!(mouse(Button::WheelDown, false, 9, 9, none, &sgr), None, "a wheel has no release");
        assert_eq!(mouse(Button::Left, true, 400, 300, none, &sgr).unwrap(), "\x1b[<0;401;301M", "SGR has no limit");

        let legacy = modes(|m| m.mouse_click = true);
        assert_eq!(mouse(Button::Left, true, 4, 2, none, &legacy).unwrap(), "\x1b[M %#");
        assert_eq!(mouse(Button::Left, false, 4, 2, none, &legacy).unwrap(), "\x1b[M#%#", "a release is button 3");
        assert_eq!(mouse(Button::Middle, true, 0, 0, Modifiers::SHIFT, &legacy).unwrap(), "\x1b[M%!!");
        assert_eq!(mouse(Button::Left, true, 94, 0, none, &legacy).unwrap(), "\x1b[M \x7f!", "column 95 is the last");
        assert_eq!(mouse(Button::Left, true, 95, 0, none, &legacy), None, "a byte above 127 is not text");
        let utf8 = modes(|m| {
            m.mouse_drag = true;
            m.utf8_mouse = true;
        });
        assert_eq!(mouse(Button::Left, true, 200, 0, none, &utf8).unwrap(), "\x1b[M \u{e9}!");
        assert_eq!(mouse(Button::Left, true, 2100, 0, none, &utf8), None, "past mode 1005's range");
    }
}
