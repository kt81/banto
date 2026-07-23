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

/// Normalize a pasted string's line endings for a PTY child: CRLF and a lone
/// LF both become a bare CR, matching what a real terminal delivers for
/// Enter (xterm-style paste normalization). Without this, a child that only
/// treats `\r` as "submit" would see `\n`-only lines as plain text instead of
/// line breaks — or vice versa, depending on the source of the paste.
pub fn normalize_paste_line_endings(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\r');
            }
            '\n' => out.push('\r'),
            other => out.push(other),
        }
    }
    out
}

/// Wrap already-normalized paste text in bracketed-paste markers (`ESC[200~`
/// / `ESC[201~`), so a child with bracketed paste mode enabled (DECSET 2004)
/// receives it as a single paste event end-to-end instead of a stream of
/// Enter keypresses submitting each line separately.
pub fn wrap_bracketed_paste(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
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

    use super::{key_to_bytes, normalize_paste_line_endings, wrap_bracketed_paste};

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

    #[test]
    fn normalize_paste_line_endings_converts_crlf() {
        assert_eq!(normalize_paste_line_endings("a\r\nb"), "a\rb");
    }

    #[test]
    fn normalize_paste_line_endings_converts_a_lone_lf() {
        assert_eq!(normalize_paste_line_endings("a\nb"), "a\rb");
    }

    #[test]
    fn normalize_paste_line_endings_leaves_an_already_bare_cr_alone() {
        assert_eq!(normalize_paste_line_endings("a\rb"), "a\rb");
    }

    #[test]
    fn normalize_paste_line_endings_handles_a_mix() {
        assert_eq!(normalize_paste_line_endings("a\r\nb\nc\rd"), "a\rb\rc\rd");
    }

    #[test]
    fn normalize_paste_line_endings_leaves_text_without_newlines_untouched() {
        assert_eq!(
            normalize_paste_line_endings("no newlines here"),
            "no newlines here"
        );
    }

    #[test]
    fn wrap_bracketed_paste_frames_the_text_in_start_and_end_markers() {
        assert_eq!(
            wrap_bracketed_paste("hello"),
            b"\x1b[200~hello\x1b[201~".to_vec()
        );
    }
}
