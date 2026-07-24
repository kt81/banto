//! Host-side paste synthesis for the Windows reality documented in
//! `super::convert`'s module doc and confirmed against crossterm 0.29's own
//! source: `crossterm::event::Event::Paste` is never constructed by the
//! Windows input backend (`ReadConsoleInput` maps every key press to one
//! `Event::Key`, one at a time — there is no code path that ever batches a
//! paste). On Windows, a real paste — and, it turns out, a drag-and-drop
//! path and an IME commit too — arrives as a burst of ordinary key events at
//! machine pace. This module re-derives "that was a paste" from timing
//! alone, entirely in the shell: `banto_core` never sees a raw key stream,
//! only the `InputEvent::Paste` this synthesizes (or the original single key,
//! completely unchanged, when nothing needed joining).
//!
//! The threshold below is evidence, not a guess — from an operator-captured
//! `BANTO_INPUT_LOG` (three machine-pace bursts, one human-pace baseline):
//! - A real multiline paste: 212 keys, internal gaps averaging 2.41ms, max
//!   17ms.
//! - A drag-and-drop path: 68 keys spelling a quoted path, no `Enter`s — to
//!   this module that is just another machine-pace key burst, so fixing
//!   paste synthesis fixes D&D delivery for free (whether the embedded
//!   `claude` then turns the dropped path into an image attachment is its
//!   own concern, downstream of this module).
//! - An IME commit: 10 Japanese characters, avg 2.22ms — also machine-pace,
//!   and so also classified as a paste by this module. That is accepted,
//!   not fought: a chars-only "paste" with no `Enter`s is behaviorally
//!   identical to the same characters typed individually, as far as the
//!   child process is concerned.
//! - The tightest gap anywhere else in the capture — real human typing and
//!   Windows' own key auto-repeat — was 31ms (a cluster sitting at exactly
//!   31ms, auto-repeat at its fastest).
//!
//! Paste-shaped bursts land at ≤17ms; humans and auto-repeat never beat
//! 31ms. [`PASTE_GAP`] sits in between, with margin on both sides.
//!
//! Unix backends are unaffected: a real `Event::Paste` still arrives and
//! converts exactly as before (`super::convert::from_crossterm`); this
//! accumulator only ever *joins* keys that were already going to be
//! forwarded one at a time, so on a backend that never produces
//! machine-pace key-only bursts it simply never has cause to merge anything
//! past a lone key falling straight through. Not cfg-gated.

use std::time::{Duration, Instant};

use banto_core::engine::{EmporiumState, Focus};
use banto_core::input::{InputEvent, KeyCode, KeyEvent};

/// See the module doc for the capture this is derived from.
pub(super) const PASTE_GAP: Duration = Duration::from_millis(25);

/// A safety valve, not an expected case: bounds how large a single buffered
/// run can grow before it is flushed and a fresh one started, so a
/// pathological machine-pace stream (a stuck key repeat, a script hammering
/// banto) can't grow the buffer without limit.
pub(super) const PASTE_SIZE_CAP: usize = 65536;

/// Whether `input` is eligible to join the accumulator: only a key that
/// would otherwise be forwarded straight to a focused pane unchanged — a
/// plain `Char` (no ctrl/alt; shift is fine, a shifted char already arrives
/// pre-uppercased per crossterm's convention — see `convert.rs`'s
/// `shifted_uppercase_char_preserves_both_the_uppercase_code_and_shift`),
/// `Enter`, or `Tab` — and only while a pane actually has focus with no
/// prefix chord armed. Everything else (sidebar focus, an armed prefix, a
/// ctrl/alt-modified key, arrows, F-keys, mouse, resize, a real
/// `InputEvent::Paste` on a non-Windows backend) is out of scope and must
/// never come near the buffer — this is what keeps sidebar `j`/`k`
/// auto-repeat and an in-flight `Ctrl+B` prefix instant and untouched.
pub(super) fn is_in_scope(state: &EmporiumState, input: &InputEvent) -> bool {
    if state.focus != Focus::Pane || state.prefix_armed.is_some() {
        return false;
    }
    match input {
        InputEvent::Key(key) => {
            !key.modifiers.ctrl
                && !key.modifiers.alt
                && matches!(key.code, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab)
        }
        _ => false,
    }
}

/// Host-side paste/IME/drag-drop detector: joins in-scope keys arriving
/// within [`PASTE_GAP`] of one another into a single buffer. See
/// [`Self::accept`]/[`Self::bypass`]/[`Self::tick`] for the three ways a
/// flush is triggered, and [`Self::flush`] for what a flush actually
/// produces.
#[derive(Default)]
pub(super) struct PasteAccumulator {
    buffered: Vec<KeyEvent>,
    last_at: Option<Instant>,
}

impl PasteAccumulator {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Whether the buffer currently holds anything. The shell drops its
    /// poll timeout while this is true (see `emporium::event_loop`) so a
    /// lone buffered key still flushes within one `PASTE_GAP` instead of
    /// waiting on the ordinary idle poll cadence.
    pub(super) fn is_pending(&self) -> bool {
        !self.buffered.is_empty()
    }

    /// Join an in-scope key (the caller has already checked
    /// [`is_in_scope`]) at `now`. Only the size cap is checked here — gap
    /// staleness is the caller's responsibility via a [`Self::tick`] call
    /// made *before* this one, using a timestamp taken at the same moment
    /// as `now`, so a stale buffer is already flushed by the time a new key
    /// reaches this method; see `emporium::event_loop`'s call order.
    pub(super) fn accept(&mut self, key: KeyEvent, now: Instant) -> Option<InputEvent> {
        let flushed = if self.buffered.len() >= PASTE_SIZE_CAP {
            self.flush()
        } else {
            None
        };
        self.buffered.push(key);
        self.last_at = Some(now);
        flushed
    }

    /// An event that does not qualify for accumulation has arrived.
    /// Unconditionally flushes the buffer first, preserving key order —
    /// *unless* `is_mouse`, in which case the buffer is left completely
    /// untouched and the mouse event simply passes through: during a real
    /// paste burst the gap timing is the only guard that matters, and
    /// reordering a mouse event around still-buffered keys is harmless.
    pub(super) fn bypass(&mut self, is_mouse: bool) -> Option<InputEvent> {
        if is_mouse { None } else { self.flush() }
    }

    /// Per-tick idle check: flush if [`PASTE_GAP`] has passed since the
    /// last buffered key with nothing new arriving to extend it. A no-op on
    /// an empty buffer.
    pub(super) fn tick(&mut self, now: Instant) -> Option<InputEvent> {
        let idle = self
            .last_at
            .is_some_and(|last| now.duration_since(last) >= PASTE_GAP);
        if idle { self.flush() } else { None }
    }

    /// Empties the buffer: exactly one buffered key releases as the
    /// ordinary `InputEvent::Key` it always was (typed-char path, zero
    /// behavior change beyond the latency of waiting to see whether more
    /// followed); two or more synthesize one `InputEvent::Paste`, `Char`s
    /// verbatim, `Enter` as `'\r'`, `Tab` as `'\t'` — the same shape a real
    /// `Event::Paste` would have produced, so it flows through the
    /// existing `engine::update_paste` path unchanged (normalize +
    /// bracketed-wrap when the child has DECSET 2004 on).
    fn flush(&mut self) -> Option<InputEvent> {
        self.last_at = None;
        match self.buffered.len() {
            0 => None,
            1 => Some(InputEvent::Key(
                self.buffered.pop().expect("len checked above"),
            )),
            _ => {
                let mut text = String::with_capacity(self.buffered.len());
                for key in self.buffered.drain(..) {
                    match key.code {
                        KeyCode::Char(c) => text.push(c),
                        KeyCode::Enter => text.push('\r'),
                        KeyCode::Tab => text.push('\t'),
                        _ => unreachable!("is_in_scope only ever admits Char/Enter/Tab"),
                    }
                }
                Some(InputEvent::Paste(text))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use banto_core::engine::PrefixKey;
    use banto_core::input::Modifiers;

    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), Modifiers::NONE)
    }

    fn shifted(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), Modifiers::SHIFT)
    }

    fn t0() -> Instant {
        Instant::now()
    }

    // --- is_in_scope --------------------------------------------------

    fn pane_state() -> EmporiumState {
        let mut state = EmporiumState::new(PrefixKey::parse("C-b"));
        state.focus = Focus::Pane;
        state
    }

    #[test]
    fn a_plain_char_in_pane_focus_is_in_scope() {
        let state = pane_state();
        assert!(is_in_scope(&state, &InputEvent::Key(key('a'))));
    }

    #[test]
    fn enter_and_tab_are_in_scope() {
        let state = pane_state();
        assert!(is_in_scope(
            &state,
            &InputEvent::Key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE))
        ));
        assert!(is_in_scope(
            &state,
            &InputEvent::Key(KeyEvent::new(KeyCode::Tab, Modifiers::NONE))
        ));
    }

    #[test]
    fn a_shifted_char_is_still_in_scope() {
        let state = pane_state();
        assert!(is_in_scope(&state, &InputEvent::Key(shifted('B'))));
    }

    #[test]
    fn sidebar_focus_is_out_of_scope() {
        let mut state = pane_state();
        state.focus = Focus::Sidebar;
        assert!(!is_in_scope(&state, &InputEvent::Key(key('a'))));
    }

    #[test]
    fn an_armed_prefix_is_out_of_scope() {
        let mut state = pane_state();
        state.prefix_armed = Some(t0());
        assert!(!is_in_scope(&state, &InputEvent::Key(key('a'))));
    }

    #[test]
    fn a_ctrl_modified_char_is_out_of_scope() {
        let state = pane_state();
        let ctrl_v = KeyEvent::new(KeyCode::Char('v'), Modifiers::CONTROL);
        assert!(!is_in_scope(&state, &InputEvent::Key(ctrl_v)));
    }

    #[test]
    fn an_alt_modified_char_is_out_of_scope() {
        let state = pane_state();
        let alt_a = KeyEvent::new(KeyCode::Char('a'), Modifiers::ALT);
        assert!(!is_in_scope(&state, &InputEvent::Key(alt_a)));
    }

    #[test]
    fn arrows_and_f_keys_are_out_of_scope() {
        let state = pane_state();
        assert!(!is_in_scope(
            &state,
            &InputEvent::Key(KeyEvent::new(KeyCode::Left, Modifiers::NONE))
        ));
        assert!(!is_in_scope(
            &state,
            &InputEvent::Key(KeyEvent::new(KeyCode::F(2), Modifiers::NONE))
        ));
    }

    #[test]
    fn non_key_input_is_out_of_scope() {
        let state = pane_state();
        assert!(!is_in_scope(
            &state,
            &InputEvent::Resize {
                width: 80,
                height: 24
            }
        ));
    }

    // --- PasteAccumulator ------------------------------------------------

    #[test]
    fn a_single_key_releases_unchanged_on_idle_tick() {
        let mut acc = PasteAccumulator::new();
        let now = t0();
        assert_eq!(acc.accept(key('a'), now), None);
        assert!(acc.is_pending());
        let flushed = acc.tick(now + PASTE_GAP);
        assert_eq!(flushed, Some(InputEvent::Key(key('a'))));
        assert!(!acc.is_pending());
    }

    #[test]
    fn a_machine_pace_burst_with_enters_synthesizes_one_paste() {
        let mut acc = PasteAccumulator::new();
        let now = t0();
        for (i, c) in "hi".chars().enumerate() {
            assert_eq!(
                acc.accept(key(c), now + Duration::from_millis(i as u64 * 2)),
                None
            );
        }
        let enter_at = now + Duration::from_millis(10);
        assert_eq!(
            acc.accept(KeyEvent::new(KeyCode::Enter, Modifiers::NONE), enter_at),
            None
        );
        for (i, c) in "there".chars().enumerate() {
            assert_eq!(
                acc.accept(key(c), enter_at + Duration::from_millis(2 + i as u64 * 2)),
                None
            );
        }
        let flushed = acc.tick(enter_at + Duration::from_millis(2 + 5 * 2) + PASTE_GAP);
        assert_eq!(flushed, Some(InputEvent::Paste("hi\rthere".to_string())));
    }

    #[test]
    fn a_gap_at_or_past_the_threshold_splits_the_burst() {
        let mut acc = PasteAccumulator::new();
        let now = t0();
        assert_eq!(acc.accept(key('a'), now), None);
        assert_eq!(acc.accept(key('b'), now + Duration::from_millis(2)), None);
        // A third key arrives after the gap elapsed: the accumulator's
        // `accept` only ever checks the size cap, so the caller's `tick`
        // just before this key — using a timestamp from the same moment —
        // is what actually performs the split; this test drives that exact
        // call order.
        let late = now + Duration::from_millis(2) + PASTE_GAP;
        assert_eq!(acc.tick(late), Some(InputEvent::Paste("ab".to_string())));
        assert_eq!(acc.accept(key('c'), late), None);
        let flushed = acc.tick(late + PASTE_GAP);
        assert_eq!(flushed, Some(InputEvent::Key(key('c'))));
    }

    #[test]
    fn a_gap_just_under_the_threshold_keeps_the_burst_joined() {
        let mut acc = PasteAccumulator::new();
        let now = t0();
        assert_eq!(acc.accept(key('a'), now), None);
        let almost = now + PASTE_GAP - Duration::from_millis(1);
        assert_eq!(acc.tick(almost), None, "not stale yet");
        assert_eq!(acc.accept(key('b'), almost), None);
        let flushed = acc.tick(almost + PASTE_GAP);
        assert_eq!(flushed, Some(InputEvent::Paste("ab".to_string())));
    }

    #[test]
    fn an_ime_style_short_chars_only_burst_is_still_synthesized_as_paste() {
        // Documented-acceptable: a chars-only "paste" is behaviorally
        // identical to the same characters typed individually.
        let mut acc = PasteAccumulator::new();
        let now = t0();
        for (i, c) in "ありがとう".chars().enumerate() {
            assert_eq!(
                acc.accept(key(c), now + Duration::from_millis(i as u64 * 2)),
                None
            );
        }
        let flushed = acc.tick(now + Duration::from_millis(2 * 4) + PASTE_GAP);
        assert_eq!(flushed, Some(InputEvent::Paste("ありがとう".to_string())));
    }

    #[test]
    fn a_bypass_event_flushes_the_buffer_first_preserving_order() {
        let mut acc = PasteAccumulator::new();
        let now = t0();
        assert_eq!(acc.accept(key('a'), now), None);
        // An arrow key (out of scope) arrives before the gap elapses: the
        // shell calls `bypass`, not `tick`, for it — an unconditional
        // flush regardless of how little time has passed.
        let flushed = acc.bypass(false);
        assert_eq!(flushed, Some(InputEvent::Key(key('a'))));
        assert!(!acc.is_pending());
    }

    #[test]
    fn a_mouse_event_passes_through_without_flushing() {
        let mut acc = PasteAccumulator::new();
        let now = t0();
        assert_eq!(acc.accept(key('a'), now), None);
        let flushed = acc.bypass(true);
        assert_eq!(flushed, None, "mouse never flushes");
        assert!(acc.is_pending(), "buffer is untouched");
        // The still-buffered key resolves normally afterward.
        let resolved = acc.tick(now + PASTE_GAP);
        assert_eq!(resolved, Some(InputEvent::Key(key('a'))));
    }

    #[test]
    fn reaching_the_size_cap_flushes_and_starts_a_fresh_buffer() {
        let mut acc = PasteAccumulator::new();
        let now = t0();
        for i in 0..PASTE_SIZE_CAP {
            let at = now + Duration::from_micros(i as u64);
            assert_eq!(acc.accept(key('x'), at), None);
        }
        // The cap+1th key triggers a flush of everything buffered so far.
        let at = now + Duration::from_micros(PASTE_SIZE_CAP as u64);
        let flushed = acc.accept(key('y'), at);
        match flushed {
            Some(InputEvent::Paste(text)) => assert_eq!(text.len(), PASTE_SIZE_CAP),
            other => panic!("expected a capped paste flush, got {other:?}"),
        }
        assert!(
            acc.is_pending(),
            "the triggering key started a fresh buffer"
        );
    }

    #[test]
    fn rapid_enter_enter_synthesizes_one_paste_not_two_submits() {
        let mut acc = PasteAccumulator::new();
        let now = t0();
        let enter = KeyEvent::new(KeyCode::Enter, Modifiers::NONE);
        assert_eq!(acc.accept(enter, now), None);
        assert_eq!(acc.accept(enter, now + Duration::from_millis(2)), None);
        let flushed = acc.tick(now + Duration::from_millis(2) + PASTE_GAP);
        assert_eq!(flushed, Some(InputEvent::Paste("\r\r".to_string())));
    }

    #[test]
    fn tick_on_an_empty_buffer_is_a_noop() {
        let mut acc = PasteAccumulator::new();
        assert_eq!(acc.tick(t0()), None);
    }

    #[test]
    fn bypass_on_an_empty_buffer_is_a_noop() {
        let mut acc = PasteAccumulator::new();
        assert_eq!(acc.bypass(false), None);
    }
}
