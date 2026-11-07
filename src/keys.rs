//! Converts crossterm key, mouse and paste events into the byte sequences a terminal program expects on its stdin.
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

const ESC: u8 = 0x1b;

/// xterm modifier parameter: 1 plus shift(1), alt(2), ctrl(4).
fn modifier_param(modifiers: KeyModifiers) -> u8 {
    1 + u8::from(modifiers.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(modifiers.contains(KeyModifiers::ALT))
        + 4 * u8::from(modifiers.contains(KeyModifiers::CONTROL))
}

/// Encodes a cursor-style key (arrows, Home, End). Plain keys use SS3 in application-cursor mode, CSI otherwise;
/// modified keys always use the `CSI 1 ; <mods> <final>` form.
fn cursor_key(final_byte: u8, modifiers: KeyModifiers, application_cursor: bool) -> Vec<u8> {
    if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL) {
        format!("\x1b[1;{}{}", modifier_param(modifiers), final_byte as char).into_bytes()
    } else if application_cursor {
        vec![ESC, b'O', final_byte]
    } else {
        vec![ESC, b'[', final_byte]
    }
}

/// Encodes a `CSI <number> ~` key (PageUp, Delete, F5...), adding the modifier parameter when needed.
fn tilde_key(number: u8, modifiers: KeyModifiers) -> Vec<u8> {
    if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL) {
        format!("\x1b[{number};{}~", modifier_param(modifiers)).into_bytes()
    } else {
        format!("\x1b[{number}~").into_bytes()
    }
}

/// Encodes a key press as terminal input bytes; empty for keys with no encoding.
/// `application_cursor` reflects the program's DECCKM setting.
pub fn encode_key(key: KeyEvent, application_cursor: bool) -> Vec<u8> {
    let modifiers = key.modifiers;
    let alt = modifiers.contains(KeyModifiers::ALT);
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let mut bytes = match key.code {
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => {
            vec![c.to_ascii_lowercase() as u8 - b'a' + 1]
        }
        KeyCode::Char(c) if ctrl => match c {
            ' ' | '@' => vec![0],
            '[' => vec![ESC],
            // crossterm reports bytes 0x1c-0x1f as Ctrl+4..Ctrl+7 (and 0x7f-style Ctrl+8).
            '\\' | '4' => vec![28],
            ']' | '5' => vec![29],
            '^' | '6' => vec![30],
            '_' | '7' => vec![31],
            '?' | '8' => vec![127],
            _ => c.to_string().into_bytes(),
        },
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![ESC, b'[', b'Z'],
        KeyCode::Esc => vec![ESC],
        KeyCode::Up => return cursor_key(b'A', modifiers, application_cursor),
        KeyCode::Down => return cursor_key(b'B', modifiers, application_cursor),
        KeyCode::Right => return cursor_key(b'C', modifiers, application_cursor),
        KeyCode::Left => return cursor_key(b'D', modifiers, application_cursor),
        KeyCode::Home => return cursor_key(b'H', modifiers, application_cursor),
        KeyCode::End => return cursor_key(b'F', modifiers, application_cursor),
        KeyCode::Insert => return tilde_key(2, modifiers),
        KeyCode::Delete => return tilde_key(3, modifiers),
        KeyCode::PageUp => return tilde_key(5, modifiers),
        KeyCode::PageDown => return tilde_key(6, modifiers),
        KeyCode::F(n @ 1..=4) => vec![ESC, b'O', b'P' + n - 1],
        KeyCode::F(n @ 5..=12) => {
            // xterm's F5..F12 numbers skip 16 and 22.
            const NUMBERS: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
            return tilde_key(NUMBERS[usize::from(n - 5)], modifiers);
        }
        _ => return Vec::new(),
    };
    // Alt is conventionally sent as an ESC prefix; it is already folded into the CSI forms above.
    if alt {
        bytes.insert(0, ESC);
    }
    bytes
}

/// Wraps pasted text in bracketed-paste markers when the program asked for them, else sends it raw.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.replace('\n', "\r").into_bytes()
    }
}

/// Encodes a mouse event for a program that enabled mouse reporting. `column`/`row` are 0-based
/// positions inside the program's screen. Returns None when the program did not ask for this kind of event.
pub fn encode_mouse(
    event: MouseEvent,
    column: u16,
    row: u16,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let button = |button: MouseButton| match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (code, release, motion) = match event.kind {
        MouseEventKind::Down(b) => (button(b), false, false),
        MouseEventKind::Up(b) => (button(b), true, false),
        MouseEventKind::Drag(b) => (button(b) + 32, false, true),
        MouseEventKind::Moved => (3 + 32, false, true),
        MouseEventKind::ScrollUp => (64, false, false),
        MouseEventKind::ScrollDown => (65, false, false),
        MouseEventKind::ScrollLeft => (66, false, false),
        MouseEventKind::ScrollRight => (67, false, false),
    };
    let wanted = match mode {
        MouseProtocolMode::None => false,
        MouseProtocolMode::Press => !release && !motion,
        MouseProtocolMode::PressRelease => !motion,
        MouseProtocolMode::ButtonMotion => !matches!(event.kind, MouseEventKind::Moved),
        MouseProtocolMode::AnyMotion => true,
    };
    if !wanted {
        return None;
    }
    let mut code = code;
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    Some(match encoding {
        MouseProtocolEncoding::Sgr => {
            let suffix = if release { 'm' } else { 'M' };
            format!("\x1b[<{code};{};{}{suffix}", column + 1, row + 1).into_bytes()
        }
        // Legacy encodings cannot name the released button and cap coordinates at 223.
        _ => {
            let code = if release { 3 } else { code };
            let clamp = |n: u16| (n + 33).min(255) as u8;
            vec![ESC, b'[', b'M', code as u8 + 32, clamp(column), clamp(row)]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};

    /// A key press with the given modifiers.
    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    // Control letters map to C0 codes and Alt adds an ESC prefix (how Option+b/f word-moves reach programs).
    #[test]
    fn letters_ctrl_and_alt() {
        assert_eq!(
            encode_key(key(KeyCode::Char('a'), KeyModifiers::NONE), false),
            b"a"
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), false),
            [3]
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('b'), KeyModifiers::ALT), false),
            [ESC, b'b']
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('é'), KeyModifiers::NONE), false),
            "é".as_bytes()
        );
    }

    // Arrow keys honor application-cursor mode, and modified arrows (Option+Left) use the CSI 1;mods form.
    #[test]
    fn arrows() {
        assert_eq!(
            encode_key(key(KeyCode::Up, KeyModifiers::NONE), false),
            b"\x1b[A"
        );
        assert_eq!(
            encode_key(key(KeyCode::Up, KeyModifiers::NONE), true),
            b"\x1bOA"
        );
        assert_eq!(
            encode_key(key(KeyCode::Left, KeyModifiers::ALT), false),
            b"\x1b[1;3D"
        );
        assert_eq!(
            encode_key(key(KeyCode::Right, KeyModifiers::CONTROL), true),
            b"\x1b[1;5C"
        );
    }

    // Editing and function keys use xterm's tilde sequences.
    #[test]
    fn editing_and_function_keys() {
        assert_eq!(
            encode_key(key(KeyCode::Delete, KeyModifiers::NONE), false),
            b"\x1b[3~"
        );
        assert_eq!(
            encode_key(key(KeyCode::PageUp, KeyModifiers::NONE), false),
            b"\x1b[5~"
        );
        assert_eq!(
            encode_key(key(KeyCode::F(1), KeyModifiers::NONE), false),
            b"\x1bOP"
        );
        assert_eq!(
            encode_key(key(KeyCode::F(6), KeyModifiers::NONE), false),
            b"\x1b[17~"
        );
        assert_eq!(
            encode_key(key(KeyCode::Enter, KeyModifiers::NONE), false),
            b"\r"
        );
        assert_eq!(
            encode_key(key(KeyCode::Backspace, KeyModifiers::NONE), false),
            [0x7f]
        );
    }

    // Pasted text is bracketed only when requested, so multi-line pastes are not run line by line.
    #[test]
    fn paste() {
        assert_eq!(encode_paste("a\nb", true), b"\x1b[200~a\nb\x1b[201~");
        assert_eq!(encode_paste("a\nb", false), b"a\rb");
    }

    // SGR mouse reports are 1-based; programs that only want presses do not get releases or motion.
    #[test]
    fn mouse_reporting() {
        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let release = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..press
        };
        let sgr = MouseProtocolEncoding::Sgr;
        assert_eq!(
            encode_mouse(press, 4, 2, MouseProtocolMode::Press, sgr).unwrap(),
            b"\x1b[<0;5;3M"
        );
        assert_eq!(
            encode_mouse(release, 4, 2, MouseProtocolMode::PressRelease, sgr).unwrap(),
            b"\x1b[<0;5;3m"
        );
        assert_eq!(
            encode_mouse(release, 4, 2, MouseProtocolMode::Press, sgr),
            None
        );
        assert_eq!(
            encode_mouse(press, 4, 2, MouseProtocolMode::None, sgr),
            None
        );
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            ..press
        };
        assert_eq!(
            encode_mouse(wheel, 0, 0, MouseProtocolMode::Press, sgr).unwrap(),
            b"\x1b[<64;1;1M"
        );
    }
}
