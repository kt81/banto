//! Encode crossterm key events into the byte sequences a PTY child expects.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Translate a key press into the bytes to write to the child's stdin. Returns
/// an empty vector for keys that produce no input (e.g. bare modifiers).
pub fn key_to_bytes(key: &KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut out = Vec::new();
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                if let Some(b) = ctrl_byte(c) {
                    out.push(b);
                } else {
                    push_utf8(&mut out, c);
                }
            } else {
                if alt {
                    out.push(0x1b);
                }
                push_utf8(&mut out, c);
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        _ => {}
    }
    out
}

fn push_utf8(out: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

fn ctrl_byte(c: char) -> Option<u8> {
    let upper = c.to_ascii_uppercase();
    if upper.is_ascii_alphabetic() {
        return Some((upper as u8) & 0x1f);
    }
    match c {
        ' ' | '@' => Some(0),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::key_to_bytes;

    fn bytes(code: KeyCode, mods: KeyModifiers) -> Vec<u8> {
        key_to_bytes(&KeyEvent::new(code, mods))
    }

    #[test]
    fn plain_chars_and_enter() {
        assert_eq!(bytes(KeyCode::Char('a'), KeyModifiers::NONE), b"a");
        assert_eq!(bytes(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(bytes(KeyCode::Backspace, KeyModifiers::NONE), vec![0x7f]);
    }

    #[test]
    fn multibyte_char() {
        assert_eq!(
            bytes(KeyCode::Char('あ'), KeyModifiers::NONE),
            "あ".as_bytes()
        );
    }

    #[test]
    fn ctrl_c_is_etx() {
        assert_eq!(bytes(KeyCode::Char('c'), KeyModifiers::CONTROL), vec![0x03]);
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(bytes(KeyCode::Char('x'), KeyModifiers::ALT), b"\x1bx");
    }

    #[test]
    fn arrows_are_csi() {
        assert_eq!(bytes(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(bytes(KeyCode::Left, KeyModifiers::NONE), b"\x1b[D");
    }
}
