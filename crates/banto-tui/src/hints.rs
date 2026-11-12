//! The emporium's status-bar key hints, as text.
//!
//! Pure functions of what decides them, so they are tested without a frame; the
//! bar itself only draws what these return. What the idle bar offers follows the
//! focus, because that is where keys go: with the session list focused banto
//! takes the list keys, with a pane focused every key goes to the session inside
//! it and banto itself takes only the prefix chord, `F2` and `F3`.

use banto_core::app::Mode;
use banto_core::engine::Focus;

/// Shown while the search box is open, whichever side has the focus.
pub const SEARCH_HINTS: &str = "type to search · Enter confirm · Esc cancel";

/// Shown while the prefix chord is armed: the keys the chord's follow-up takes.
pub const PREFIX_HINTS: &str = "prefix: Esc cancel · o/Tab cycle · arrows move · Shift+Up/Down reorder · m collapse · z solo · M restore · = reset sizes · 1-9 pane · b literal · s sidebar · x close";

const LIST_HINTS: &str = "j/k move · Enter open · F2 pane · B brigade/disband · b +worker · \
                          F3 next pane · / search · n new · d archive · g group · \
                          Tab grouped/flat · p pin · a reveal · q/Esc quit";

/// Appended only while an archived row is on screen — see the choba's own
/// `ARCHIVED_HINT` for why it is not in the bar unconditionally.
const ARCHIVED_HINT: &str = " · D restore";

/// What the idle bar's text depends on.
pub struct IdleHintsInput<'a> {
    pub focus: Focus,
    pub mode: Mode,
    /// An archived row is on screen (`App::any_archived_visible`).
    pub archived_visible: bool,
    /// The configured prefix chord as the operator would name it
    /// (`PrefixKey::label`).
    pub prefix_label: &'a str,
    /// How many tiles `F3` can step through: the staged brigade's tiles that are
    /// not collapsed, and 0 when what is staged is not a brigade — `F3` does
    /// nothing there, so it is not offered.
    pub focusable_tiles: usize,
}

/// The hints for an idle bar (no transient status, prefix not armed).
pub fn idle_hints(input: &IdleHintsInput<'_>) -> String {
    if input.mode == Mode::Search {
        return SEARCH_HINTS.to_string();
    }
    match input.focus {
        Focus::Sidebar => {
            let mut hints = LIST_HINTS.to_string();
            if input.archived_visible {
                hints.push_str(ARCHIVED_HINT);
            }
            hints
        }
        Focus::Pane => {
            let mut hints = format!("{} prefix · F2 list", input.prefix_label);
            if input.focusable_tiles > 1 {
                hints.push_str(" · F3 next pane");
            }
            hints
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(focus: Focus, mode: Mode) -> IdleHintsInput<'static> {
        IdleHintsInput {
            focus,
            mode,
            archived_visible: false,
            prefix_label: "Ctrl+B",
            focusable_tiles: 3,
        }
    }

    const LIST_ONLY_KEYS: [&str; 12] = [
        "j/k",
        "Enter open",
        "B brigade",
        "b +worker",
        "/ search",
        "n new",
        "d archive",
        "g group",
        "Tab",
        "p pin",
        "a reveal",
        "q quit",
    ];

    #[test]
    fn list_focus_names_what_each_key_does() {
        let hints = idle_hints(&input(Focus::Sidebar, Mode::Normal));
        assert_eq!(
            hints,
            "j/k move · Enter open · F2 pane · B brigade/disband · b +worker · \
             F3 next pane · / search · n new · d archive · g group · \
             Tab grouped/flat · p pin · a reveal · q/Esc quit"
        );
        assert!(!hints.contains("Tab view"));
    }

    #[test]
    fn the_archived_hint_appears_only_with_the_list_focused_and_an_archived_row_on_screen() {
        let mut archived = input(Focus::Sidebar, Mode::Normal);
        archived.archived_visible = true;
        assert!(idle_hints(&archived).ends_with(" · D restore"));
        assert!(!idle_hints(&input(Focus::Sidebar, Mode::Normal)).contains("D restore"));
        archived.focus = Focus::Pane;
        assert!(!idle_hints(&archived).contains("D restore"));
        archived.focus = Focus::Sidebar;
        archived.mode = Mode::Search;
        assert!(!idle_hints(&archived).contains("D restore"));
    }

    #[test]
    fn pane_focus_offers_only_what_banto_takes_there() {
        let hints = idle_hints(&input(Focus::Pane, Mode::Normal));
        assert_eq!(hints, "Ctrl+B prefix · F2 list · F3 next pane");
        for key in LIST_ONLY_KEYS {
            assert!(!hints.contains(key), "{key:?} in {hints:?}");
        }
    }

    #[test]
    fn f3_is_offered_only_with_more_than_one_focusable_tile() {
        for (tiles, offered) in [(0, false), (1, false), (2, true), (5, true)] {
            let mut pane = input(Focus::Pane, Mode::Normal);
            pane.focusable_tiles = tiles;
            assert_eq!(
                idle_hints(&pane).contains("F3"),
                offered,
                "{tiles} focusable tiles"
            );
        }
        // A non-brigade session: nothing for F3 to step through.
        let mut solo = input(Focus::Pane, Mode::Normal);
        solo.focusable_tiles = 0;
        assert_eq!(idle_hints(&solo), "Ctrl+B prefix · F2 list");
    }

    #[test]
    fn the_prefix_label_is_the_configured_one() {
        let mut pane = input(Focus::Pane, Mode::Normal);
        pane.prefix_label = "`";
        assert!(idle_hints(&pane).starts_with("` prefix"));
    }

    #[test]
    fn search_mode_shows_the_search_hints_whichever_side_has_focus() {
        for focus in [Focus::Sidebar, Focus::Pane] {
            assert_eq!(idle_hints(&input(focus, Mode::Search)), SEARCH_HINTS);
        }
    }

    #[test]
    fn the_prefix_hints_describe_current_behaviour() {
        assert!(PREFIX_HINTS.ends_with("x close"));
        // First, not last: the bar is clipped from the right on a narrow terminal,
        // and Esc is the key someone reaches for when they want out.
        assert!(PREFIX_HINTS.starts_with("prefix: Esc cancel"));
        assert!(!PREFIX_HINTS.contains("kill"));
        assert!(PREFIX_HINTS.contains("Shift+Up/Down reorder"));
    }
}
