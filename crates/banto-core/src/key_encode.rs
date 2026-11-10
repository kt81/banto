//! Encode key events into the byte sequences a PTY child expects.

use crate::input::{KeyCode, KeyEvent};

/// Translate a key press into the bytes to write to the child's stdin. Returns
/// an empty vector for keys that produce no input (e.g. bare modifiers).
pub fn key_to_bytes(key: &KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.ctrl;
    let alt = key.modifiers.alt;
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
        // Shift alone sends LF, the byte Ctrl+J sends: Claude Code documents
        // Ctrl+J as its newline key on every terminal, and a Claude pane in
        // banto was confirmed on the operator's machine (2026-11-10) to take
        // a Ctrl+J LF as a newline. Shift+Enter used to send CR like any
        // Enter, so what the operator meant as a newline sent the message.
        // Alt and Ctrl+Enter stay CR: Ctrl+Enter is send-now to Claude, and
        // Alt+Enter rarely reaches banto at all (Windows Terminal takes it).
        KeyCode::Enter if key.modifiers.shift && !ctrl && !alt => out.push(b'\n'),
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

/// What forwarding a key does to the text waiting in a child's input line, as
/// far as banto can tell from the key alone. banto cannot see the child's
/// input line, so this is a classification of intent made from the encoding
/// above, not a model of any agent's key bindings — an agent that binds a key
/// differently changes nothing here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEffect {
    /// May put characters into the input line (a printable key, a newline
    /// insertion, a completion). When unsure, a key lands here: a wrongly
    /// kept mark only delays a relay nudge, a missed one lets the nudge
    /// splice into what the operator typed.
    AddsText,
    /// Plain Enter — the one key that sends what was typed.
    Submits,
    /// Moves, deletes or cancels without adding anything, so it neither
    /// proves nor disproves that unsent text exists.
    Neutral,
}

/// What the operator's input is, as handed to [`input_effect`].
#[derive(Debug, Clone, Copy)]
pub enum ForwardedInput<'a> {
    Key(&'a KeyEvent),
    Paste(&'a str),
}

/// The one place that decides what forwarding `input` to a pane means for
/// whether unsent operator text is waiting there (see [`InputEffect`]) —
/// every caller that forwards operator input asks here, so the rule can
/// change without touching them.
///
/// Enter is classified by the bytes [`key_to_bytes`] actually sends, not by
/// which key was pressed: the child sees only bytes. A `\r` sends; any other
/// Enter encoding (Shift+Enter is a `\n`, a newline) puts text into the line
/// instead. A change to the encoding changes the classification by itself,
/// with no change to this function.
pub fn input_effect(input: ForwardedInput<'_>) -> InputEffect {
    let key = match input {
        ForwardedInput::Key(key) => key,
        // Whatever it holds, it was put there by the operator.
        ForwardedInput::Paste(text) => {
            return if text.is_empty() {
                InputEffect::Neutral
            } else {
                InputEffect::AddsText
            };
        }
    };
    match key.code {
        KeyCode::Enter if key_to_bytes(key) == b"\r" => InputEffect::Submits,
        KeyCode::Enter => InputEffect::AddsText,
        // A Ctrl chord with a control byte writes no character; one without
        // (`Ctrl+1`) falls through `key_to_bytes` as the plain character.
        KeyCode::Char(c) if key.modifiers.ctrl && ctrl_byte(c).is_some() => InputEffect::Neutral,
        KeyCode::Char(_) | KeyCode::Tab | KeyCode::BackTab => InputEffect::AddsText,
        _ => InputEffect::Neutral,
    }
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
    use crate::input::{KeyCode, KeyEvent, Modifiers};

    use super::{
        ForwardedInput, InputEffect, input_effect, key_to_bytes, normalize_paste_line_endings,
        wrap_bracketed_paste,
    };

    fn bytes(code: KeyCode, mods: Modifiers) -> Vec<u8> {
        key_to_bytes(&KeyEvent::new(code, mods))
    }

    #[test]
    fn plain_chars_and_enter() {
        assert_eq!(bytes(KeyCode::Char('a'), Modifiers::NONE), b"a");
        assert_eq!(bytes(KeyCode::Enter, Modifiers::NONE), b"\r");
        assert_eq!(bytes(KeyCode::Backspace, Modifiers::NONE), vec![0x7f]);
    }

    #[test]
    fn multibyte_char() {
        assert_eq!(bytes(KeyCode::Char('あ'), Modifiers::NONE), "あ".as_bytes());
    }

    #[test]
    fn ctrl_c_is_etx() {
        assert_eq!(bytes(KeyCode::Char('c'), Modifiers::CONTROL), vec![0x03]);
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(bytes(KeyCode::Char('x'), Modifiers::ALT), b"\x1bx");
    }

    #[test]
    fn arrows_are_csi() {
        assert_eq!(bytes(KeyCode::Up, Modifiers::NONE), b"\x1b[A");
        assert_eq!(bytes(KeyCode::Left, Modifiers::NONE), b"\x1b[D");
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
    #[test]
    fn printable_keys_add_text_including_alt_and_shift_and_ctrl_without_a_control_byte() {
        for (code, mods) in [
            (KeyCode::Char('a'), Modifiers::NONE),
            (KeyCode::Char('A'), Modifiers::SHIFT),
            (KeyCode::Char(' '), Modifiers::NONE),
            (KeyCode::Char('あ'), Modifiers::NONE),
            (KeyCode::Char('x'), Modifiers::ALT),
            (KeyCode::Char('1'), Modifiers::CONTROL),
        ] {
            assert_eq!(
                input_effect(ForwardedInput::Key(&KeyEvent::new(code, mods))),
                InputEffect::AddsText,
                "{code:?} {mods:?}"
            );
        }
    }

    #[test]
    fn an_enter_submits_exactly_when_it_is_sent_as_a_carriage_return() {
        let both = Modifiers {
            ctrl: true,
            alt: false,
            shift: true,
        };
        for (mods, submits) in [
            (Modifiers::NONE, true),
            (Modifiers::SHIFT, false),
            (Modifiers::ALT, true),
            (Modifiers::CONTROL, true),
            (both, true),
        ] {
            let key = KeyEvent::new(KeyCode::Enter, mods);
            let effect = input_effect(ForwardedInput::Key(&key));
            assert_eq!(
                effect == InputEffect::Submits,
                key_to_bytes(&key) == b"\r",
                "{mods:?}: the classification follows the bytes"
            );
            assert_eq!(effect == InputEffect::Submits, submits, "{mods:?}");
        }
    }

    #[test]
    fn shift_enter_sends_a_line_feed_and_so_adds_text() {
        let key = KeyEvent::new(KeyCode::Enter, Modifiers::SHIFT);
        assert_eq!(key_to_bytes(&key), b"\n");
        assert_eq!(
            input_effect(ForwardedInput::Key(&key)),
            InputEffect::AddsText
        );
    }

    #[test]
    fn shift_enter_sends_the_byte_ctrl_j_sends() {
        // The claim the encoding rests on: Claude's documented newline key.
        assert_eq!(
            bytes(KeyCode::Enter, Modifiers::SHIFT),
            bytes(KeyCode::Char('j'), Modifiers::CONTROL)
        );
    }

    #[test]
    fn an_alt_or_ctrl_enter_is_the_same_bytes_as_a_plain_one() {
        // Only Shift alone is told apart; anything else is a plain send.
        for mods in [Modifiers::NONE, Modifiers::ALT, Modifiers::CONTROL] {
            assert_eq!(bytes(KeyCode::Enter, mods), b"\r", "{mods:?}");
        }
    }

    #[test]
    fn tab_and_backtab_lean_to_adding_text() {
        for code in [KeyCode::Tab, KeyCode::BackTab] {
            assert_eq!(
                input_effect(ForwardedInput::Key(&KeyEvent::new(code, Modifiers::NONE))),
                InputEffect::AddsText
            );
        }
    }

    #[test]
    fn keys_that_add_nothing_are_neutral() {
        for code in [
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Insert,
            KeyCode::Esc,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::F(5),
        ] {
            assert_eq!(
                input_effect(ForwardedInput::Key(&KeyEvent::new(code, Modifiers::NONE))),
                InputEffect::Neutral,
                "{code:?}"
            );
        }
        assert_eq!(
            input_effect(ForwardedInput::Key(&KeyEvent::new(
                KeyCode::Char('c'),
                Modifiers::CONTROL
            ))),
            InputEffect::Neutral
        );
    }
    #[test]
    fn a_paste_adds_text_unless_it_is_empty() {
        assert_eq!(
            input_effect(ForwardedInput::Paste("hello")),
            InputEffect::AddsText
        );
        assert_eq!(
            input_effect(ForwardedInput::Paste("")),
            InputEffect::Neutral
        );
    }
}
