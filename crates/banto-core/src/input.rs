//! Pure input events: banto's own representation of a key/mouse/paste/resize
//! event, independent of any terminal backend. `banto-core` must never depend
//! on `crossterm` (`docs/DISCIPLINE.md` §2's crate table) — the bin crate
//! converts crossterm's events into these once, at the boundary
//! (`embedded::convert`), before feeding them into the emporium's pure
//! `update`.
//!
//! [`KeyCode`] is deliberately not a full terminal keycode taxonomy — it's
//! exactly the set banto's emporium actually binds.
//!
//! Every type here also derives `Serialize`/`Deserialize`: `InputEvent` is
//! reachable from `engine::Event`, which the record/replay stream
//! (`crate::replay`) serializes one line per event (`docs/DISCIPLINE.md`
//! §8).

use serde::{Deserialize, Serialize};

/// One recognized key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyCode {
    Char(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Insert,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

/// Which modifier keys were held. A plain struct rather than a bitflags
/// type — banto only ever tests/sets these three individually or compares
/// against [`Modifiers::NONE`], so a `bitflags` dependency buys nothing here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Modifiers {
    pub const NONE: Modifiers = Modifiers {
        ctrl: false,
        alt: false,
        shift: false,
    };
    pub const CONTROL: Modifiers = Modifiers {
        ctrl: true,
        alt: false,
        shift: false,
    };
    pub const ALT: Modifiers = Modifiers {
        ctrl: false,
        alt: true,
        shift: false,
    };
    pub const SHIFT: Modifiers = Modifiers {
        ctrl: false,
        alt: false,
        shift: true,
    };

    pub fn is_empty(&self) -> bool {
        *self == Self::NONE
    }
}

/// One key press: what was pressed, plus what was held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: Modifiers,
}

impl KeyEvent {
    pub fn new(code: KeyCode, modifiers: Modifiers) -> Self {
        Self { code, modifiers }
    }
}

/// A mouse button, for [`MouseEventKind::Down`]/[`MouseEventKind::Up`]/
/// [`MouseEventKind::Drag`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// What kind of mouse event occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseEventKind {
    Down(MouseButton),
    Up(MouseButton),
    Drag(MouseButton),
    ScrollUp,
    ScrollDown,
    Moved,
}

/// One mouse event, at a terminal cell coordinate (0-based, matching
/// `ratatui::layout::Position`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseEvent {
    pub kind: MouseEventKind,
    pub column: u16,
    pub row: u16,
}

/// One recognized input event — everything the emporium's `update` ever
/// reacts to from the terminal. Events banto ignores (focus change, key
/// release, ...) never become one of these; they're dropped at the
/// crossterm boundary instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize { width: u16, height: u16 },
}
