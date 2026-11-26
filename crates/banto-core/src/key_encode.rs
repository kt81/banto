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
        KeyCode::Enter if key.modifiers.shift && !ctrl && !alt => out.push(b'\n'),
        // Alt without Ctrl sends ESC CR, with or without Shift: Ghostty 1.4.0
        // on macOS sends Option+Enter as ESC CR (raw byte dump, 2026-11-26) and
        // crossterm reads it as Enter+ALT, so a bare CR here submitted a Claude
        // prompt the operator meant as a newline. Claude Code documents
        // Option+Enter as a newline, and Codex's composer binds Alt+Enter to
        // one. ESC CR rather than LF because it is the bytes the terminal
        // itself sent for the key, so each agent's own documentation holds in
        // a pane. Every Enter with Ctrl stays CR: Ctrl+Enter is send-now to
        // Claude, and on Windows AltGr arrives as Ctrl+Alt, which must stay a
        // plain Enter.
        KeyCode::Enter if alt && !ctrl => out.extend_from_slice(b"\x1b\r"),
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

/// Whether a key submits the child's input, for the request watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEffect {
    Submits,
    Neutral,
}

fn input_effect(key: &KeyEvent) -> InputEffect {
    if key.code == KeyCode::Enter && key_to_bytes(key) == b"\r" {
        InputEffect::Submits
    } else {
        InputEffect::Neutral
    }
}

/// Encode a key and classify its effect together. `win32_codex` requires both
/// the negotiated Win32 input mode and a known Codex pane.
pub fn key_to_encoded_input(key: &KeyEvent, win32_codex: bool) -> (Vec<u8>, InputEffect) {
    if win32_codex {
        let mods = key.modifiers;
        let bytes: Option<&[u8]> = match key.code {
            KeyCode::Enter if mods.shift && !mods.ctrl && !mods.alt => {
                Some(b"\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_")
            }
            KeyCode::Char('j' | 'J') if mods.ctrl && !mods.shift && !mods.alt => {
                Some(b"\x1b[74;36;10;1;8;1_\x1b[74;36;10;0;8;1_")
            }
            _ => None,
        };
        if let Some(bytes) = bytes {
            return (bytes.to_vec(), InputEffect::Neutral);
        }
    }
    (key_to_bytes(key), input_effect(key))
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
        // crossterm's Unix parser reads the bytes 0x1C..=0x1F as Ctrl+'4'..'7'
        // (xterm sends those same bytes for Ctrl+4..7), so a Ctrl+\ ] ^ _ pressed
        // on macOS or Linux arrives as a digit. Without these arms the Ctrl is
        // lost and the pane gets the bare digit.
        '4'..='7' => Some(0x1c + (c as u8 - b'4')),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn win32_codex_encoded_newlines_preserve_effect_and_other_keys() {
        use super::{InputEffect, input_effect, key_to_bytes, key_to_encoded_input};
        use crate::input::{KeyCode, KeyEvent, Modifiers};
        let shift_enter = KeyEvent::new(KeyCode::Enter, Modifiers::SHIFT);
        let ctrl_j = KeyEvent::new(KeyCode::Char('j'), Modifiers::CONTROL);
        assert_eq!(
            key_to_encoded_input(&shift_enter, true),
            (
                b"\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_".to_vec(),
                InputEffect::Neutral
            )
        );
        for c in ['j', 'J'] {
            let key = KeyEvent::new(KeyCode::Char(c), Modifiers::CONTROL);
            assert_eq!(
                key_to_encoded_input(&key, true),
                (
                    b"\x1b[74;36;10;1;8;1_\x1b[74;36;10;0;8;1_".to_vec(),
                    InputEffect::Neutral
                )
            );
        }
        for key in [
            shift_enter,
            ctrl_j,
            KeyEvent::new(KeyCode::Enter, Modifiers::NONE),
            KeyEvent::new(KeyCode::Char('x'), Modifiers::NONE),
        ] {
            assert_eq!(
                key_to_encoded_input(&key, false),
                (key_to_bytes(&key), input_effect(&key))
            );
        }
        let plain_enter = KeyEvent::new(KeyCode::Enter, Modifiers::NONE);
        assert_eq!(
            key_to_encoded_input(&plain_enter, true),
            (b"\r".to_vec(), InputEffect::Submits)
        );
        for ctrl in [false, true] {
            for alt in [false, true] {
                for shift in [false, true] {
                    let mods = Modifiers { ctrl, alt, shift };
                    for code in [KeyCode::Enter, KeyCode::Char('j'), KeyCode::Char('J')] {
                        let key = KeyEvent::new(code, mods);
                        let special = (code == KeyCode::Enter && shift && !ctrl && !alt)
                            || (matches!(code, KeyCode::Char('j' | 'J')) && ctrl && !alt && !shift);
                        if !special {
                            assert_eq!(
                                key_to_encoded_input(&key, true),
                                (key_to_bytes(&key), input_effect(&key)),
                                "{key:?}"
                            );
                        }
                    }
                }
            }
        }
    }
    use crate::input::{KeyCode, KeyEvent, Modifiers};

    use super::{
        InputEffect, input_effect, key_to_bytes, normalize_paste_line_endings, wrap_bracketed_paste,
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
    fn the_control_symbols_and_the_digits_crossterm_reads_them_as_send_the_same_bytes() {
        // The digits are how crossterm's Unix parser reports 0x1C..=0x1F;
        // Windows reports Ctrl+^ as Ctrl+Shift+'6'.
        for (symbol, digit, byte) in [
            ('\\', '4', 0x1c),
            (']', '5', 0x1d),
            ('^', '6', 0x1e),
            ('_', '7', 0x1f),
        ] {
            assert_eq!(bytes(KeyCode::Char(symbol), Modifiers::CONTROL), vec![byte]);
            assert_eq!(bytes(KeyCode::Char(digit), Modifiers::CONTROL), vec![byte]);
        }
        let ctrl_shift = Modifiers {
            ctrl: true,
            alt: false,
            shift: true,
        };
        assert_eq!(bytes(KeyCode::Char('6'), ctrl_shift), vec![0x1e]);
    }

    #[test]
    fn a_digit_without_control_stays_a_digit() {
        for digit in ['3', '4', '5', '6', '7', '8'] {
            assert_eq!(
                bytes(KeyCode::Char(digit), Modifiers::NONE),
                digit.to_string().into_bytes()
            );
        }
        // Ctrl+3 and Ctrl+8 have no control byte here and keep the digit.
        assert_eq!(bytes(KeyCode::Char('3'), Modifiers::CONTROL), b"3");
        assert_eq!(bytes(KeyCode::Char('8'), Modifiers::CONTROL), b"8");
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
    fn an_enter_submits_exactly_when_it_is_sent_as_a_carriage_return() {
        let both = Modifiers {
            ctrl: true,
            alt: false,
            shift: true,
        };
        let alt_shift = Modifiers {
            ctrl: false,
            alt: true,
            shift: true,
        };
        let ctrl_alt = Modifiers {
            ctrl: true,
            alt: true,
            shift: false,
        };
        for (mods, submits) in [
            (Modifiers::NONE, true),
            (Modifiers::SHIFT, false),
            (Modifiers::ALT, false),
            (alt_shift, false),
            (Modifiers::CONTROL, true),
            (both, true),
            (ctrl_alt, true),
        ] {
            let key = KeyEvent::new(KeyCode::Enter, mods);
            let effect = input_effect(&key);
            assert_eq!(
                effect == InputEffect::Submits,
                key_to_bytes(&key) == b"\r",
                "{mods:?}: the classification follows the bytes"
            );
            assert_eq!(effect == InputEffect::Submits, submits, "{mods:?}");
        }
    }

    #[test]
    fn shift_enter_sends_a_line_feed_without_submitting() {
        let key = KeyEvent::new(KeyCode::Enter, Modifiers::SHIFT);
        assert_eq!(key_to_bytes(&key), b"\n");
        assert_eq!(input_effect(&key), InputEffect::Neutral);
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
    fn a_ctrl_enter_is_the_same_bytes_as_a_plain_one() {
        // AltGr arrives as Ctrl+Alt on Windows, so Alt does not change that.
        let ctrl_alt = Modifiers {
            ctrl: true,
            alt: true,
            shift: false,
        };
        let ctrl_shift = Modifiers {
            ctrl: true,
            alt: false,
            shift: true,
        };
        for mods in [Modifiers::NONE, Modifiers::CONTROL, ctrl_alt, ctrl_shift] {
            assert_eq!(bytes(KeyCode::Enter, mods), b"\r", "{mods:?}");
        }
    }

    #[test]
    fn an_alt_enter_without_ctrl_is_escape_then_carriage_return() {
        let alt_shift = Modifiers {
            ctrl: false,
            alt: true,
            shift: true,
        };
        for mods in [Modifiers::ALT, alt_shift] {
            assert_eq!(bytes(KeyCode::Enter, mods), b"\x1b\r", "{mods:?}");
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
                input_effect(&KeyEvent::new(code, Modifiers::NONE)),
                InputEffect::Neutral,
                "{code:?}"
            );
        }
        assert_eq!(
            input_effect(&KeyEvent::new(KeyCode::Char('c'), Modifiers::CONTROL)),
            InputEffect::Neutral
        );
    }
}
