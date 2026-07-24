//! Convert crossterm's terminal events into `banto_core::input`'s pure
//! event types — the one place in the emporium's shell that touches
//! crossterm's `Event`/`KeyEvent`/`MouseEvent` (see `docs/DISCIPLINE.md` §2
//! and `super::engine`'s module doc: the core must never depend on
//! crossterm). Everything from here down (`super::emporium`'s event loop,
//! `super::engine`'s `update`) sees only `banto_core::input::InputEvent`.

use banto_core::input::{
    InputEvent, KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind,
};

/// Convert one crossterm event, or `None` for a kind banto never acts on: a
/// key *release* (only ever reported at all if a terminal opts into
/// keyboard-enhancement event-type reporting, which banto never requests,
/// but dropped defensively in case one arrives anyway), terminal focus
/// change, and horizontal scroll (no binding uses it).
pub(super) fn from_crossterm(event: crossterm::event::Event) -> Option<InputEvent> {
    match event {
        crossterm::event::Event::Key(key) => {
            if matches!(
                key.kind,
                crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
            ) {
                convert_key(key).map(InputEvent::Key)
            } else {
                None
            }
        }
        crossterm::event::Event::Mouse(mouse) => convert_mouse(mouse).map(InputEvent::Mouse),
        crossterm::event::Event::Paste(text) => Some(InputEvent::Paste(text)),
        crossterm::event::Event::Resize(width, height) => {
            Some(InputEvent::Resize { width, height })
        }
        crossterm::event::Event::FocusGained | crossterm::event::Event::FocusLost => None,
    }
}

/// Convert one crossterm key press/repeat, or `None` if its `KeyCode` isn't
/// one banto recognizes (see [`convert_key_code`]). Also used directly by
/// `super::run_embedded`'s standalone `banto _embed` demo, which forwards
/// keys straight to `EmbeddedSession::send_key` without going through a
/// full `Event`.
pub(super) fn convert_key(key: crossterm::event::KeyEvent) -> Option<KeyEvent> {
    Some(KeyEvent::new(
        convert_key_code(key.code)?,
        convert_modifiers(key.modifiers),
    ))
}

/// `None` for a crossterm `KeyCode` banto has no binding for (Null,
/// CapsLock, media keys, ...) — dropped here rather than threaded through
/// as some placeholder, since every one of those would just reach `update`'s
/// per-key matches and fall through to their unbound arm anyway.
fn convert_key_code(code: crossterm::event::KeyCode) -> Option<KeyCode> {
    Some(match code {
        crossterm::event::KeyCode::Char(c) => KeyCode::Char(c),
        crossterm::event::KeyCode::Enter => KeyCode::Enter,
        crossterm::event::KeyCode::Esc => KeyCode::Esc,
        crossterm::event::KeyCode::Tab => KeyCode::Tab,
        crossterm::event::KeyCode::BackTab => KeyCode::BackTab,
        crossterm::event::KeyCode::Backspace => KeyCode::Backspace,
        crossterm::event::KeyCode::Delete => KeyCode::Delete,
        crossterm::event::KeyCode::Insert => KeyCode::Insert,
        crossterm::event::KeyCode::Left => KeyCode::Left,
        crossterm::event::KeyCode::Right => KeyCode::Right,
        crossterm::event::KeyCode::Up => KeyCode::Up,
        crossterm::event::KeyCode::Down => KeyCode::Down,
        crossterm::event::KeyCode::Home => KeyCode::Home,
        crossterm::event::KeyCode::End => KeyCode::End,
        crossterm::event::KeyCode::PageUp => KeyCode::PageUp,
        crossterm::event::KeyCode::PageDown => KeyCode::PageDown,
        crossterm::event::KeyCode::F(n) => KeyCode::F(n),
        _ => return None,
    })
}

/// Only `SHIFT`/`CONTROL`/`ALT` are tracked — `SUPER`/`HYPER`/`META` have no
/// binding anywhere in banto.
fn convert_modifiers(mods: crossterm::event::KeyModifiers) -> Modifiers {
    Modifiers {
        ctrl: mods.contains(crossterm::event::KeyModifiers::CONTROL),
        alt: mods.contains(crossterm::event::KeyModifiers::ALT),
        shift: mods.contains(crossterm::event::KeyModifiers::SHIFT),
    }
}

fn convert_mouse(mouse: crossterm::event::MouseEvent) -> Option<MouseEvent> {
    let kind = match mouse.kind {
        crossterm::event::MouseEventKind::Down(b) => MouseEventKind::Down(convert_mouse_button(b)),
        crossterm::event::MouseEventKind::Up(b) => MouseEventKind::Up(convert_mouse_button(b)),
        crossterm::event::MouseEventKind::Drag(b) => MouseEventKind::Drag(convert_mouse_button(b)),
        crossterm::event::MouseEventKind::Moved => MouseEventKind::Moved,
        crossterm::event::MouseEventKind::ScrollUp => MouseEventKind::ScrollUp,
        crossterm::event::MouseEventKind::ScrollDown => MouseEventKind::ScrollDown,
        crossterm::event::MouseEventKind::ScrollLeft
        | crossterm::event::MouseEventKind::ScrollRight => return None,
    };
    Some(MouseEvent {
        kind,
        column: mouse.column,
        row: mouse.row,
    })
}

fn convert_mouse_button(button: crossterm::event::MouseButton) -> MouseButton {
    match button {
        crossterm::event::MouseButton::Left => MouseButton::Left,
        crossterm::event::MouseButton::Middle => MouseButton::Middle,
        crossterm::event::MouseButton::Right => MouseButton::Right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ct_key(
        code: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
    ) -> crossterm::event::Event {
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(code, modifiers))
    }

    #[test]
    fn plain_char_has_no_modifiers() {
        let ev = from_crossterm(ct_key(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(
            ev,
            Some(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                Modifiers::NONE
            )))
        );
    }

    #[test]
    fn ctrl_char_sets_ctrl_only() {
        let ev = from_crossterm(ct_key(
            crossterm::event::KeyCode::Char('b'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert_eq!(
            ev,
            Some(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('b'),
                Modifiers::CONTROL
            )))
        );
    }

    #[test]
    fn alt_char_sets_alt_only() {
        let ev = from_crossterm(ct_key(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::ALT,
        ));
        assert_eq!(
            ev,
            Some(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                Modifiers::ALT
            )))
        );
    }

    #[test]
    fn shifted_uppercase_char_preserves_both_the_uppercase_code_and_shift() {
        // crossterm reports a shifted letter as the already-uppercased
        // `Char` *plus* `SHIFT` set, not a bare `Char('b')` — the shape
        // `engine::update_key`'s `'B'` binding relies on must survive
        // conversion unchanged.
        let ev = from_crossterm(ct_key(
            crossterm::event::KeyCode::Char('B'),
            crossterm::event::KeyModifiers::SHIFT,
        ));
        assert_eq!(
            ev,
            Some(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('B'),
                Modifiers::SHIFT
            )))
        );
    }

    #[test]
    fn arrows_convert() {
        for (ct, core) in [
            (crossterm::event::KeyCode::Left, KeyCode::Left),
            (crossterm::event::KeyCode::Right, KeyCode::Right),
            (crossterm::event::KeyCode::Up, KeyCode::Up),
            (crossterm::event::KeyCode::Down, KeyCode::Down),
        ] {
            let ev = from_crossterm(ct_key(ct, crossterm::event::KeyModifiers::NONE));
            assert_eq!(
                ev,
                Some(InputEvent::Key(KeyEvent::new(core, Modifiers::NONE)))
            );
        }
    }

    #[test]
    fn f_keys_carry_their_number_through() {
        let ev = from_crossterm(ct_key(
            crossterm::event::KeyCode::F(3),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(
            ev,
            Some(InputEvent::Key(KeyEvent::new(
                KeyCode::F(3),
                Modifiers::NONE
            )))
        );
    }

    #[test]
    fn tab_and_back_tab_are_distinct() {
        let tab = from_crossterm(ct_key(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
        ));
        let back_tab = from_crossterm(ct_key(
            crossterm::event::KeyCode::BackTab,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(
            tab,
            Some(InputEvent::Key(KeyEvent::new(
                KeyCode::Tab,
                Modifiers::NONE
            )))
        );
        assert_eq!(
            back_tab,
            Some(InputEvent::Key(KeyEvent::new(
                KeyCode::BackTab,
                Modifiers::NONE
            )))
        );
        assert_ne!(tab, back_tab);
    }

    #[test]
    fn a_key_release_is_dropped() {
        let mut key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        );
        key.kind = crossterm::event::KeyEventKind::Release;
        assert_eq!(from_crossterm(crossterm::event::Event::Key(key)), None);
    }

    #[test]
    fn a_key_repeat_still_converts() {
        let mut key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        );
        key.kind = crossterm::event::KeyEventKind::Repeat;
        assert_eq!(
            from_crossterm(crossterm::event::Event::Key(key)),
            Some(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                Modifiers::NONE
            )))
        );
    }

    #[test]
    fn an_unrecognized_key_code_is_dropped() {
        let ev = from_crossterm(ct_key(
            crossterm::event::KeyCode::CapsLock,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(ev, None);
    }

    #[test]
    fn mouse_down_carries_the_button_and_coordinates() {
        let ev = from_crossterm(crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 12,
                row: 5,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        ));
        assert_eq!(
            ev,
            Some(InputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 12,
                row: 5,
            }))
        );
    }

    #[test]
    fn mouse_drag_carries_the_button() {
        let ev = from_crossterm(crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Right),
                column: 1,
                row: 2,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        ));
        assert_eq!(
            ev,
            Some(InputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Right),
                column: 1,
                row: 2,
            }))
        );
    }

    #[test]
    fn mouse_scroll_up_and_down_convert() {
        for (ct, core) in [
            (
                crossterm::event::MouseEventKind::ScrollUp,
                MouseEventKind::ScrollUp,
            ),
            (
                crossterm::event::MouseEventKind::ScrollDown,
                MouseEventKind::ScrollDown,
            ),
        ] {
            let ev = from_crossterm(crossterm::event::Event::Mouse(
                crossterm::event::MouseEvent {
                    kind: ct,
                    column: 0,
                    row: 0,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
            ));
            assert_eq!(
                ev,
                Some(InputEvent::Mouse(MouseEvent {
                    kind: core,
                    column: 0,
                    row: 0,
                }))
            );
        }
    }

    #[test]
    fn mouse_horizontal_scroll_is_dropped() {
        let ev = from_crossterm(crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollLeft,
                column: 0,
                row: 0,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        ));
        assert_eq!(ev, None);
    }

    #[test]
    fn paste_carries_the_text() {
        let ev = from_crossterm(crossterm::event::Event::Paste("hi\nthere".to_string()));
        assert_eq!(ev, Some(InputEvent::Paste("hi\nthere".to_string())));
    }

    #[test]
    fn resize_carries_width_and_height() {
        let ev = from_crossterm(crossterm::event::Event::Resize(120, 40));
        assert_eq!(
            ev,
            Some(InputEvent::Resize {
                width: 120,
                height: 40
            })
        );
    }

    #[test]
    fn focus_change_is_dropped() {
        assert_eq!(from_crossterm(crossterm::event::Event::FocusGained), None);
        assert_eq!(from_crossterm(crossterm::event::Event::FocusLost), None);
    }
}
