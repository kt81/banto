//! TUI application state.
//!
//! All filtering, sorting, selection and scroll math lives here as a plain,
//! UI-free struct so it can be unit-tested without a terminal. The render loop
//! in [`crate::tui`] is a thin shell over this state.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::session::SessionRow;

/// Maximum gap between two clicks on the same row to count as a double-click.
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);

/// Outcome of a left-click on the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickOutcome {
    /// The row was selected (single click).
    Selected,
    /// The row was activated (double click) — equivalent to pressing Enter.
    Activated,
}

/// Which input mode the TUI is in. Letter keys mean different things in
/// each: `j`/`k`/`p`/`a`/`/`/`q` are commands in [`Mode::Normal`], while any
/// character types into the query in [`Mode::Search`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Letter keys are commands (`j`/`k` move, `/` searches, `p` pins, `a`
    /// toggles the agent filter, `q`/Esc quit).
    Normal,
    /// Typed characters filter the list; Esc cancels back to [`Mode::Normal`].
    Search,
}

/// A modal dialog overlaying the list, blocking Normal/Search key handling
/// until it's confirmed or cancelled. Only one is ever open at a time.
pub enum Modal {
    /// The `n` new-session dialog: pick or type a cwd to launch `claude`
    /// fresh in (not a resume of an existing session).
    NewSession(NewSessionState),
}

/// State for the new-session modal: a free-text cwd input plus a
/// fuzzy-filtered list of previously seen cwds (extracted from the loaded
/// sessions) to pick from instead of typing a full path.
pub struct NewSessionState {
    /// Every distinct cwd seen across the loaded sessions, most-recent-use
    /// first — captured once when the modal opens rather than re-derived on
    /// every keystroke (`base_rows` is already mtime-descending, so keeping
    /// the first occurrence of each cwd preserves that order).
    candidates: Vec<String>,
    /// What the user has typed so far.
    input: String,
    /// Indices into `candidates` matching `input`, best match first.
    filtered: Vec<usize>,
    /// Selected position within `filtered`.
    selected: usize,
}

impl NewSessionState {
    fn new(rows: &[SessionRow]) -> Self {
        let mut state = Self {
            candidates: unique_cwds(rows),
            input: String::new(),
            filtered: Vec::new(),
            selected: 0,
        };
        state.refilter();
        state
    }

    fn refilter(&mut self) {
        self.filtered = rank_indices(&self.input, &self.candidates);
        self.selected = 0;
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let max = self.filtered.len() - 1;
        let target = (self.selected as isize + delta).clamp(0, max as isize);
        self.selected = target as usize;
    }

    /// The cwd typed so far.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Candidates matching `input`, best match first.
    pub fn candidates(&self) -> Vec<&str> {
        self.filtered
            .iter()
            .map(|&i| self.candidates[i].as_str())
            .collect()
    }

    /// Position within [`Self::candidates`] that's highlighted, or `None`
    /// when nothing matches.
    pub fn selected(&self) -> Option<usize> {
        (!self.filtered.is_empty()).then_some(self.selected)
    }

    /// The cwd that would be launched if confirmed right now: the
    /// highlighted candidate if any match, otherwise the raw typed input
    /// (`None` if that's empty too — nothing to launch).
    fn target(&self) -> Option<PathBuf> {
        if let Some(&i) = self.filtered.get(self.selected) {
            return Some(PathBuf::from(&self.candidates[i]));
        }
        (!self.input.is_empty()).then(|| PathBuf::from(&self.input))
    }
}

/// Distinct cwd strings across `rows`, most-recently-used first (rows are
/// already mtime-descending, so keeping the first occurrence of each
/// preserves that order). Rows with no known cwd are skipped.
fn unique_cwds(rows: &[SessionRow]) -> Vec<String> {
    let mut seen = HashSet::new();
    rows.iter()
        .filter_map(|row| row.cwd.as_ref())
        .map(|cwd| cwd.display().to_string())
        .filter(|cwd| seen.insert(cwd.clone()))
        .collect()
}

/// The whole TUI state: the full session list plus the live query, filter
/// result, selection and scroll position.
pub struct App {
    /// All sessions exactly as last loaded/replaced, in the provider's
    /// original order (mtime-descending). Never reordered in place — `rows`
    /// is re-derived from this (plus `pinned`) on every `resort_rows` call,
    /// so a pin followed by an unpin restores this exact order instead of
    /// leaving the session at whatever position it had while pinned.
    base_rows: Vec<SessionRow>,
    /// `base_rows` sorted pinned-first (each group keeping `base_rows`'s
    /// relative order); what `haystacks`/`filtered`/rendering index into.
    rows: Vec<SessionRow>,
    /// Search haystacks, parallel to `rows` (`title + " " + cwd`).
    haystacks: Vec<String>,
    /// Ids of pinned sessions. A cache for sorting/display only — the store
    /// is the durable source of truth; see [`Self::toggle_pin`].
    pinned: HashSet<String>,
    /// Whether agent-run sessions (`SessionRow::is_agent`) are included in
    /// `filtered`. Off by default: a human browsing their own sessions
    /// doesn't usually want every spawned-agent session cluttering the list.
    show_agents: bool,
    /// Current input mode; see [`Mode`].
    mode: Mode,
    /// A modal dialog currently overlaying the list, if any; see [`Modal`].
    modal: Option<Modal>,
    /// Current search query. Always empty outside [`Mode::Search`] — entering
    /// Normal mode always clears it (see [`Self::exit_search`]).
    query: String,
    /// Indices into `rows` that match the query, in display order.
    filtered: Vec<usize>,
    /// Selected position within `filtered`.
    selected: usize,
    /// First visible `filtered` position (scroll offset).
    offset: usize,
    /// Number of list rows currently visible.
    viewport_height: usize,
    /// Last click (filtered index + time) for double-click detection.
    last_click: Option<(usize, Instant)>,
    /// Transient status-bar message (e.g. the phase-2 open notice).
    status: Option<String>,
    /// Set once the user asks to quit.
    should_quit: bool,
}

/// One row as rendered in the viewport.
pub struct VisibleRow<'a> {
    pub row: &'a SessionRow,
    pub pinned: bool,
}

impl App {
    /// Build the state from a session list (already sorted newest-first).
    /// No sessions are pinned initially; see [`Self::with_pinned`].
    pub fn new(rows: Vec<SessionRow>) -> Self {
        let mut app = Self {
            base_rows: rows,
            rows: Vec::new(),
            haystacks: Vec::new(),
            pinned: HashSet::new(),
            show_agents: false,
            mode: Mode::Normal,
            modal: None,
            query: String::new(),
            filtered: Vec::new(),
            selected: 0,
            offset: 0,
            viewport_height: 0,
            last_click: None,
            status: None,
            should_quit: false,
        };
        app.resort_rows();
        app.filtered = app.compute_filtered();
        app
    }

    // --- mode -------------------------------------------------------------

    /// The current input mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Enter [`Mode::Search`] (bound to `/` in Normal mode).
    pub fn enter_search(&mut self) {
        self.mode = Mode::Search;
    }

    /// Cancel the search: clear the query and return to [`Mode::Normal`]
    /// (bound to Esc in Search mode).
    pub fn exit_search(&mut self) {
        self.clear_query();
        self.mode = Mode::Normal;
    }

    /// Confirm the search: keep the query and filtered results exactly as
    /// they are, just return to [`Mode::Normal`] (bound to Enter in Search
    /// mode). Unlike [`Self::exit_search`], does not clear the query — the
    /// user can then navigate the filtered list with `j`/`k` without losing
    /// it, and a second Enter (now in Normal mode) opens the selection.
    pub fn confirm_search(&mut self) {
        self.mode = Mode::Normal;
    }

    // --- modal ------------------------------------------------------------

    /// The currently open modal, if any.
    pub fn modal(&self) -> Option<&Modal> {
        self.modal.as_ref()
    }

    /// Open the `n` new-session modal, seeding its candidate list from every
    /// distinct cwd across all loaded sessions (bound to `n` in
    /// [`Mode::Normal`]).
    pub fn open_new_session_modal(&mut self) {
        self.modal = Some(Modal::NewSession(NewSessionState::new(&self.base_rows)));
    }

    /// Close whatever modal is open (no-op if none); bound to Esc while a
    /// modal is open.
    pub fn close_modal(&mut self) {
        self.modal = None;
    }

    /// Append a character to the open modal's text input and re-filter its
    /// candidates. No-op when no modal is open.
    pub fn modal_push_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        match &mut self.modal {
            Some(Modal::NewSession(state)) => {
                state.input.push(c);
                state.refilter();
            }
            None => {}
        }
    }

    /// Delete the open modal's last input character and re-filter. No-op
    /// when no modal is open or the input is already empty.
    pub fn modal_backspace(&mut self) {
        if let Some(Modal::NewSession(state)) = &mut self.modal
            && state.input.pop().is_some()
        {
            state.refilter();
        }
    }

    /// Move the open modal's candidate selection. No-op when no modal is
    /// open.
    pub fn modal_select_prev(&mut self) {
        if let Some(Modal::NewSession(state)) = &mut self.modal {
            state.move_selection(-1);
        }
    }

    /// Move the open modal's candidate selection. No-op when no modal is
    /// open.
    pub fn modal_select_next(&mut self) {
        if let Some(Modal::NewSession(state)) = &mut self.modal {
            state.move_selection(1);
        }
    }

    /// The cwd the new-session modal would launch if confirmed right now
    /// (see [`NewSessionState::target`]); `None` if no modal is open or
    /// there's nothing to launch. Does not close the modal — the caller
    /// does that once the launch itself succeeds.
    pub fn modal_new_session_target(&self) -> Option<PathBuf> {
        match &self.modal {
            Some(Modal::NewSession(state)) => state.target(),
            None => None,
        }
    }

    // --- agent filter -------------------------------------------------------

    /// Whether agent-run sessions are currently included in the list. Only
    /// used by tests; production code reads `hidden_agent_count()` instead.
    #[cfg(test)]
    fn show_agents(&self) -> bool {
        self.show_agents
    }

    /// Toggle whether agent-run sessions are included, returning the new
    /// state. Keeps the current selection (by id) if it's still visible.
    pub fn toggle_agent_filter(&mut self) -> bool {
        self.show_agents = !self.show_agents;
        let selected_id = self.selected_row().map(|row| row.id.clone());
        self.refilter_keeping_selected(selected_id);
        self.show_agents
    }

    /// Number of agent sessions matching the current query that the filter
    /// is currently hiding (always `0` once [`Self::show_agents`] is on).
    pub fn hidden_agent_count(&self) -> usize {
        if self.show_agents {
            return 0;
        }
        rank_indices(&self.query, &self.haystacks)
            .into_iter()
            .filter(|&i| self.rows[i].is_agent)
            .count()
    }

    /// Seed the initial pinned-id set (loaded once from the store at
    /// startup) and re-sort so pinned sessions appear first.
    pub fn with_pinned(mut self, pinned: HashSet<String>) -> Self {
        self.pinned = pinned;
        self.resort_rows();
        let selected_id = self.selected_row().map(|row| row.id.clone());
        self.refilter_keeping_selected(selected_id);
        self
    }

    /// Toggle the pinned state of the selected session (no-op when nothing
    /// is selected), returning its id and new pinned state. `App` only
    /// caches pin state for sorting/display — the caller persists the
    /// change to the store, which is the durable source of truth. Re-sorts
    /// and keeps the same session selected (by id), since pinning may move
    /// it to a new position.
    pub fn toggle_pin(&mut self) -> Option<(String, bool)> {
        let id = self.selected_row()?.id.clone();
        let now_pinned = if self.pinned.remove(&id) {
            false
        } else {
            self.pinned.insert(id.clone());
            true
        };
        self.resort_rows();
        self.refilter_keeping_selected(Some(id.clone()));
        Some((id, now_pinned))
    }

    /// Replace the session list (e.g. after a filesystem-change reload),
    /// re-applying the current query unchanged. The previously selected
    /// session stays selected if it still exists, by id rather than index
    /// (rows may have been reordered or removed); otherwise the selection
    /// falls back to the top of the new list. Scroll is re-clamped to the
    /// new bounds.
    pub fn replace_rows(&mut self, rows: Vec<SessionRow>) {
        let selected_id = self.selected_row().map(|row| row.id.clone());
        self.base_rows = rows;
        self.resort_rows();
        self.refilter_keeping_selected(selected_id);
    }

    /// Rebuild `rows` from `base_rows` sorted pinned-first, and `haystacks`
    /// to match. The sort is stable and `base_rows` is already
    /// mtime-descending, so each group (pinned / not) keeps that relative
    /// order without needing to know actual mtimes here. Always rebuilding
    /// from `base_rows` (rather than re-sorting `rows` in place) is what
    /// makes pin/unpin round trips restore the exact original order.
    fn resort_rows(&mut self) {
        self.rows = self.base_rows.clone();
        self.rows.sort_by_key(|row| !self.pinned.contains(&row.id));
        self.haystacks = self.rows.iter().map(SessionRow::haystack).collect();
    }

    /// Recompute `filtered` from the current query/haystacks, then re-select
    /// `keep_id` if it's still present — by id, since sorting or reloading
    /// makes index-based tracking meaningless — falling back to the top of
    /// the list. Clears `last_click`, whose filtered index no longer
    /// corresponds to anything meaningful after a reorder.
    fn refilter_keeping_selected(&mut self, keep_id: Option<String>) {
        self.filtered = self.compute_filtered();
        self.selected = keep_id
            .and_then(|id| self.filtered.iter().position(|&i| self.rows[i].id == id))
            .unwrap_or(0);
        self.last_click = None;
        self.ensure_visible();
    }

    /// Rank `rows` against the current query, then drop agent-run sessions
    /// unless [`Self::show_agents`] is on. Ranking (and, with an empty
    /// query, the pinned-first base order) always runs first and is never
    /// affected by the agent filter — it only removes results afterward.
    fn compute_filtered(&self) -> Vec<usize> {
        rank_indices(&self.query, &self.haystacks)
            .into_iter()
            .filter(|&i| self.show_agents || !self.rows[i].is_agent)
            .collect()
    }

    // --- query editing --------------------------------------------------

    /// Append a printable character to the query and re-filter.
    pub fn push_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        self.query.push(c);
        self.refilter();
    }

    /// Delete the last query character (if any) and re-filter.
    pub fn backspace(&mut self) {
        if self.query.pop().is_some() {
            self.refilter();
        }
    }

    /// Clear the query and re-filter. No-op when the query is already empty.
    pub fn clear_query(&mut self) {
        if !self.query.is_empty() {
            self.query.clear();
            self.refilter();
        }
    }

    /// Recompute the filter result and reset selection/scroll to the top.
    fn refilter(&mut self) {
        self.filtered = self.compute_filtered();
        self.selected = 0;
        self.offset = 0;
        self.status = None;
        self.last_click = None;
        self.ensure_visible();
    }

    // --- selection ------------------------------------------------------

    /// Move the selection up one row.
    pub fn select_prev(&mut self) {
        self.move_selection(-1);
    }

    /// Move the selection down one row.
    pub fn select_next(&mut self) {
        self.move_selection(1);
    }

    /// Move the selection down by one page.
    pub fn page_down(&mut self) {
        self.move_selection(self.page_step());
    }

    /// Move the selection up by one page.
    pub fn page_up(&mut self) {
        self.move_selection(-self.page_step());
    }

    /// Select the first row.
    pub fn select_first(&mut self) {
        self.selected = 0;
        self.ensure_visible();
    }

    /// Select the last row.
    pub fn select_last(&mut self) {
        self.selected = self.filtered.len().saturating_sub(1);
        self.ensure_visible();
    }

    /// Page size for PgUp/PgDn (at least one row).
    fn page_step(&self) -> isize {
        self.viewport_height.max(1) as isize
    }

    /// Move the selection by `delta`, clamped to the filtered range, then
    /// scroll so the selection stays visible.
    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let max = self.filtered.len() - 1;
        let target = (self.selected as isize + delta).clamp(0, max as isize);
        self.selected = target as usize;
        self.ensure_visible();
    }

    // --- scrolling ------------------------------------------------------

    /// Update the viewport height (called each frame from the render area).
    pub fn set_viewport_height(&mut self, height: usize) {
        self.viewport_height = height;
        self.ensure_visible();
    }

    /// Scroll the viewport by `delta` rows without moving the selection
    /// (mouse-wheel behavior). Clamped to the valid offset range.
    pub fn scroll(&mut self, delta: isize) {
        if self.viewport_height == 0 {
            return;
        }
        let max_offset = self.max_offset() as isize;
        let target = (self.offset as isize + delta).clamp(0, max_offset);
        self.offset = target as usize;
    }

    /// Largest offset that still fills the viewport.
    fn max_offset(&self) -> usize {
        self.filtered.len().saturating_sub(self.viewport_height)
    }

    /// Scroll the minimum amount so the selection is inside the viewport, then
    /// clamp the offset so we never scroll past the end.
    fn ensure_visible(&mut self) {
        if self.viewport_height == 0 {
            return;
        }
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + self.viewport_height {
            self.offset = self.selected + 1 - self.viewport_height;
        }
        let max_offset = self.max_offset();
        if self.offset > max_offset {
            self.offset = max_offset;
        }
    }

    // --- mouse ----------------------------------------------------------

    /// Handle a left click on viewport row `viewport_row` (0 = top visible
    /// row). Returns `None` when the click lands past the last row.
    pub fn click(&mut self, viewport_row: usize, now: Instant) -> Option<ClickOutcome> {
        let filtered_index = self.offset.checked_add(viewport_row)?;
        if filtered_index >= self.filtered.len() {
            return None;
        }
        let is_double = matches!(
            self.last_click,
            Some((idx, when))
                if idx == filtered_index
                    && now.saturating_duration_since(when) <= DOUBLE_CLICK_INTERVAL
        );
        self.selected = filtered_index;
        self.ensure_visible();
        if is_double {
            // Reset so a third quick click starts a fresh pair.
            self.last_click = None;
            Some(ClickOutcome::Activated)
        } else {
            self.last_click = Some((filtered_index, now));
            Some(ClickOutcome::Selected)
        }
    }

    // --- actions --------------------------------------------------------

    /// Post a transient status-bar message.
    pub fn set_status(&mut self, message: String) {
        self.status = Some(message);
    }

    /// Request that the render loop exit.
    pub fn request_quit(&mut self) {
        self.should_quit = true;
    }

    // --- accessors (for the render loop) --------------------------------

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Raw selection index into the filtered list. Only the render loop's
    /// viewport-relative [`Self::selected_in_viewport`] is used in production;
    /// this is exposed for tests that check selection/scroll math directly.
    #[cfg(test)]
    fn selected(&self) -> usize {
        self.selected
    }

    pub fn filtered_len(&self) -> usize {
        self.filtered.len()
    }

    pub fn total_len(&self) -> usize {
        self.rows.len()
    }

    /// The currently selected session, if any.
    pub fn selected_row(&self) -> Option<&SessionRow> {
        self.filtered.get(self.selected).map(|&i| &self.rows[i])
    }

    /// Selection index relative to the viewport, or `None` when the selection
    /// is scrolled out of view (or the list is empty).
    pub fn selected_in_viewport(&self) -> Option<usize> {
        if self.filtered.is_empty() || self.selected < self.offset {
            return None;
        }
        let local = self.selected - self.offset;
        (local < self.viewport_height).then_some(local)
    }

    /// The rows currently visible in the viewport, top to bottom. Which one
    /// (if any) is selected is reported separately by
    /// [`Self::selected_in_viewport`].
    pub fn visible(&self) -> Vec<VisibleRow<'_>> {
        let end = (self.offset + self.viewport_height).min(self.filtered.len());
        self.filtered[self.offset..end]
            .iter()
            .map(|&i| {
                let row = &self.rows[i];
                VisibleRow {
                    row,
                    pinned: self.pinned.contains(&row.id),
                }
            })
            .collect()
    }
}

/// Rank `haystacks` against `query`, returning matching indices best-first.
///
/// Delegates to `banto_core::search` (nucleo smart-case fuzzy matching): an
/// empty query yields every index in the original order, otherwise only
/// matches are returned, best score first.
fn rank_indices(query: &str, haystacks: &[String]) -> Vec<usize> {
    banto_core::search::rank(query, haystacks)
        .into_iter()
        .map(|m| m.index)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use banto_core::model::{Activity, AgeBucket};
    use std::path::PathBuf;

    fn row(id: &str, title: &str, cwd: &str) -> SessionRow {
        SessionRow {
            id: id.to_string(),
            title: (!title.is_empty()).then(|| title.to_string()),
            cwd: (!cwd.is_empty()).then(|| PathBuf::from(cwd)),
            activity: Activity::Idle(AgeBucket::Older),
            is_agent: false,
            preview: None,
        }
    }

    fn agent_row(id: &str, title: &str, cwd: &str) -> SessionRow {
        SessionRow {
            is_agent: true,
            ..row(id, title, cwd)
        }
    }

    fn numbered(count: usize) -> Vec<SessionRow> {
        (0..count)
            .map(|i| row(&format!("id{i}"), &format!("title {i}"), ""))
            .collect()
    }

    fn ids(app: &App) -> Vec<String> {
        app.visible().iter().map(|v| v.row.id.clone()).collect()
    }

    #[test]
    fn empty_query_keeps_all_in_order() {
        let mut app = App::new(numbered(3));
        app.set_viewport_height(10);
        assert_eq!(ids(&app), vec!["id0", "id1", "id2"]);
        assert_eq!(app.filtered_len(), 3);
    }

    #[test]
    fn filter_is_case_insensitive() {
        // A lowercase query matches case-insensitively (nucleo smart-case).
        let mut app = App::new(vec![
            row("a", "Apple pie", "/x"),
            row("b", "Banana bread", "/y"),
            row("c", "apple tart", "/z"),
        ]);
        app.set_viewport_height(10);
        for c in "app".chars() {
            app.push_char(c);
        }
        let got = ids(&app);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&"a".to_string()));
        assert!(got.contains(&"c".to_string()));
        assert_eq!(app.selected(), 0);
    }

    #[test]
    fn filter_preserves_order_on_score_ties() {
        // Identical prefix matches score equally, so the original (mtime)
        // order is preserved via the stable index tie-break.
        let mut app = App::new(vec![
            row("a", "match one", ""),
            row("b", "match two", ""),
            row("c", "match three", ""),
        ]);
        app.set_viewport_height(10);
        for c in "match".chars() {
            app.push_char(c);
        }
        assert_eq!(ids(&app), vec!["a", "b", "c"]);
    }

    #[test]
    fn filter_also_searches_cwd() {
        let mut app = App::new(vec![
            row("a", "one", "/work/alpha"),
            row("b", "two", "/work/beta"),
        ]);
        app.set_viewport_height(10);
        for c in "beta".chars() {
            app.push_char(c);
        }
        assert_eq!(ids(&app), vec!["b"]);
    }

    #[test]
    fn backspace_and_clear_restore_matches() {
        let mut app = App::new(numbered(3));
        app.set_viewport_height(10);
        app.push_char('t'); // "title N" all match
        assert_eq!(app.filtered_len(), 3);
        app.push_char('z'); // "titlez" matches nothing
        assert_eq!(app.filtered_len(), 0);
        app.backspace(); // back to "t"
        assert_eq!(app.filtered_len(), 3);
        app.push_char('z');
        app.clear_query();
        assert_eq!(app.filtered_len(), 3);
        assert_eq!(app.query(), "");
    }

    #[test]
    fn selection_clamps_at_both_ends() {
        let mut app = App::new(numbered(3));
        app.set_viewport_height(10);
        app.select_prev(); // already at top
        assert_eq!(app.selected(), 0);
        app.select_next();
        app.select_next();
        app.select_next(); // past the end
        assert_eq!(app.selected(), 2);
        app.select_last();
        assert_eq!(app.selected(), 2);
        app.select_first();
        assert_eq!(app.selected(), 0);
    }

    #[test]
    fn selection_on_empty_list_stays_zero() {
        let mut app = App::new(numbered(2));
        app.set_viewport_height(10);
        for c in "nomatch".chars() {
            app.push_char(c);
        }
        assert_eq!(app.filtered_len(), 0);
        app.select_next();
        app.select_last();
        assert_eq!(app.selected(), 0);
        assert!(app.selected_row().is_none());
        assert_eq!(app.selected_in_viewport(), None);
    }

    #[test]
    fn scroll_window_follows_selection() {
        let mut app = App::new(numbered(10));
        app.set_viewport_height(3);
        // Jump to the end: offset should reveal the last row.
        app.select_last();
        assert_eq!(app.selected(), 9);
        assert_eq!(app.selected_in_viewport(), Some(2)); // 9 - 7
        // Back to top.
        app.select_first();
        assert_eq!(app.selected_in_viewport(), Some(0));
        // Step down just past the viewport.
        for _ in 0..3 {
            app.select_next();
        }
        assert_eq!(app.selected(), 3);
        assert_eq!(app.selected_in_viewport(), Some(2)); // offset became 1
        assert_eq!(ids(&app), vec!["id1", "id2", "id3"]);
    }

    #[test]
    fn wheel_scroll_moves_offset_without_selection() {
        let mut app = App::new(numbered(10));
        app.set_viewport_height(3);
        app.scroll(2);
        assert_eq!(app.selected(), 0); // selection unchanged
        assert_eq!(ids(&app), vec!["id2", "id3", "id4"]);
        // Cannot scroll past the end.
        app.scroll(100);
        assert_eq!(ids(&app), vec!["id7", "id8", "id9"]);
        // Nor before the start.
        app.scroll(-100);
        assert_eq!(ids(&app), vec!["id0", "id1", "id2"]);
    }

    #[test]
    fn page_navigation_moves_by_viewport_height() {
        let mut app = App::new(numbered(10));
        app.set_viewport_height(4);
        app.page_down();
        assert_eq!(app.selected(), 4);
        app.page_down();
        assert_eq!(app.selected(), 8);
        app.page_down(); // clamps
        assert_eq!(app.selected(), 9);
        app.page_up();
        assert_eq!(app.selected(), 5);
    }

    #[test]
    fn double_click_activates_only_within_threshold_on_same_row() {
        let mut app = App::new(numbered(5));
        app.set_viewport_height(5);
        let t0 = Instant::now();

        // First click selects.
        assert_eq!(app.click(2, t0), Some(ClickOutcome::Selected));
        assert_eq!(app.selected(), 2);

        // Second quick click on the same row activates.
        let t1 = t0 + Duration::from_millis(200);
        assert_eq!(app.click(2, t1), Some(ClickOutcome::Activated));

        // The pair resets: a third quick click just selects again.
        let t2 = t1 + Duration::from_millis(50);
        assert_eq!(app.click(2, t2), Some(ClickOutcome::Selected));

        // A click on a different row never activates.
        let t3 = t2 + Duration::from_millis(50);
        assert_eq!(app.click(3, t3), Some(ClickOutcome::Selected));
        assert_eq!(app.selected(), 3);

        // Too slow: not a double click.
        let t4 = t3 + Duration::from_millis(500);
        assert_eq!(app.click(3, t4), Some(ClickOutcome::Selected));
    }

    #[test]
    fn click_past_last_row_is_ignored() {
        let mut app = App::new(numbered(2));
        app.set_viewport_height(5);
        assert_eq!(app.click(4, Instant::now()), None);
    }

    #[test]
    fn set_status_reflects_the_selected_row() {
        // The render loop (not App) drives opening; this only checks the two
        // primitives it composes: reading the selection and posting a message.
        let mut app = App::new(numbered(3));
        app.set_viewport_height(10);
        app.select_next(); // id1
        let id = app.selected_row().unwrap().id.clone();
        app.set_status(format!("opened session {id}"));
        assert_eq!(app.status(), Some("opened session id1"));
    }

    #[test]
    fn replace_rows_preserves_selection_by_id_across_reorder() {
        let mut app = App::new(numbered(3)); // id0, id1, id2
        app.set_viewport_height(10);
        app.select_next(); // id1
        assert_eq!(app.selected_row().unwrap().id, "id1");

        // Reordered: id1 now comes first.
        app.replace_rows(vec![
            row("id1", "title 1", ""),
            row("id0", "title 0", ""),
            row("id2", "title 2", ""),
        ]);

        assert_eq!(app.selected_row().unwrap().id, "id1");
        assert_eq!(app.selected(), 0);
    }

    #[test]
    fn replace_rows_falls_back_to_the_top_when_the_selected_session_vanishes() {
        let mut app = App::new(numbered(3)); // id0, id1, id2
        app.set_viewport_height(10);
        app.select_next(); // id1

        app.replace_rows(vec![row("id0", "title 0", ""), row("id2", "title 2", "")]); // id1 gone

        assert_eq!(app.selected(), 0);
        assert_eq!(app.selected_row().unwrap().id, "id0");
    }

    #[test]
    fn replace_rows_with_no_matching_session_yields_an_empty_selection() {
        let mut app = App::new(numbered(3));
        app.set_viewport_height(10);
        app.select_next(); // id1

        app.replace_rows(Vec::new());

        assert_eq!(app.selected(), 0);
        assert!(app.selected_row().is_none());
    }

    #[test]
    fn replace_rows_clamps_scroll_when_the_list_shrinks() {
        let mut app = App::new(numbered(10));
        app.set_viewport_height(3);
        app.select_last(); // id9, scrolled near the bottom

        // id9 no longer exists: selection falls back to the top, and the
        // viewport must not still be scrolled past the (now much shorter) end.
        app.replace_rows(numbered(3));

        assert_eq!(app.selected(), 0);
        assert_eq!(app.selected_in_viewport(), Some(0));
        assert_eq!(ids(&app), vec!["id0", "id1", "id2"]);
    }

    #[test]
    fn replace_rows_reapplies_the_current_query_without_changing_it() {
        let mut app = App::new(vec![row("a", "Alpha", ""), row("b", "Beta", "")]);
        app.set_viewport_height(10);
        for c in "alpha".chars() {
            app.push_char(c);
        }
        assert_eq!(app.filtered_len(), 1);

        app.replace_rows(vec![
            row("a", "Alpha", ""),
            row("b", "Beta", ""),
            row("c", "Alpha 2", ""),
        ]);

        assert_eq!(app.query(), "alpha");
        assert_eq!(app.filtered_len(), 2);
    }

    #[test]
    fn with_pinned_sorts_pinned_sessions_first_preserving_group_order() {
        // id0, id1, id2 arrive in mtime-descending (i.e. incoming) order;
        // pinning id2 must move it to the front without disturbing the
        // relative order of the other two.
        let mut app = App::new(numbered(3));
        app.set_viewport_height(10);

        app = app.with_pinned(["id2".to_string()].into_iter().collect());

        assert_eq!(ids(&app), vec!["id2", "id0", "id1"]);
    }

    #[test]
    fn visible_reports_pinned_status_per_row() {
        let mut app = App::new(numbered(3));
        app.set_viewport_height(10);
        app = app.with_pinned(["id1".to_string()].into_iter().collect());

        let pinned_flags: Vec<bool> = app.visible().iter().map(|v| v.pinned).collect();
        // id1 sorted first (pinned), then id0, id2.
        assert_eq!(pinned_flags, vec![true, false, false]);
    }

    #[test]
    fn toggle_pin_moves_the_session_to_the_front_and_keeps_it_selected() {
        let mut app = App::new(numbered(3)); // id0, id1, id2
        app.set_viewport_height(10);
        app.select_next(); // id1

        let (id, now_pinned) = app.toggle_pin().unwrap();

        assert_eq!(id, "id1");
        assert!(now_pinned);
        assert_eq!(ids(&app), vec!["id1", "id0", "id2"]);
        // Still selected, just moved to its new (front) position.
        assert_eq!(app.selected(), 0);
        assert_eq!(app.selected_row().unwrap().id, "id1");
    }

    #[test]
    fn toggle_pin_twice_unpins_and_restores_original_order() {
        let mut app = App::new(numbered(3));
        app.set_viewport_height(10);
        app.select_next(); // id1

        let (_, first) = app.toggle_pin().unwrap();
        let (_, second) = app.toggle_pin().unwrap();

        assert!(first);
        assert!(!second);
        assert_eq!(ids(&app), vec!["id0", "id1", "id2"]);
    }

    #[test]
    fn toggle_pin_on_empty_list_returns_none() {
        let mut app = App::new(Vec::new());
        app.set_viewport_height(10);
        assert_eq!(app.toggle_pin(), None);
    }

    #[test]
    fn starts_in_normal_mode_and_search_round_trip_clears_the_query() {
        let mut app = App::new(numbered(3));
        app.set_viewport_height(10);
        assert_eq!(app.mode(), Mode::Normal);

        app.enter_search();
        assert_eq!(app.mode(), Mode::Search);
        app.push_char('t');
        assert_eq!(app.query(), "t");

        app.exit_search();
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.query(), "");
        // Cleared query means the full list is back.
        assert_eq!(app.filtered_len(), 3);
    }

    #[test]
    fn confirm_search_returns_to_normal_but_keeps_the_query_and_filter() {
        let mut app = App::new(vec![row("a", "Alpha", ""), row("b", "Beta", "")]);
        app.set_viewport_height(10);

        app.enter_search();
        app.push_char('b');
        assert_eq!(app.filtered_len(), 1);

        app.confirm_search();

        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.query(), "b"); // kept, unlike exit_search
        assert_eq!(app.filtered_len(), 1); // filter preserved
    }

    #[test]
    fn hidden_agent_count_reflects_the_current_query() {
        let mut app = App::new(vec![
            row("h1", "orange soda", ""),
            agent_row("a1", "orange fruit", ""),
            agent_row("a2", "apple pie", ""),
        ]);
        app.set_viewport_height(10);

        // No query: both agent rows count as hidden.
        assert_eq!(app.hidden_agent_count(), 2);

        for c in "orange".chars() {
            app.push_char(c);
        }
        // Only "orange fruit" (agent) matches "orange"; "apple pie" doesn't.
        assert_eq!(app.hidden_agent_count(), 1);

        app.toggle_agent_filter();
        assert_eq!(app.hidden_agent_count(), 0);
    }

    #[test]
    fn agent_sessions_are_hidden_by_default() {
        let mut app = App::new(vec![
            row("h1", "Human session", ""),
            agent_row("a1", "Agent session", ""),
        ]);
        app.set_viewport_height(10);

        assert!(!app.show_agents());
        assert_eq!(app.total_len(), 2);
        assert_eq!(ids(&app), vec!["h1"]);
    }

    #[test]
    fn toggle_agent_filter_reveals_agent_sessions() {
        let mut app = App::new(vec![
            row("h1", "Human session", ""),
            agent_row("a1", "Agent session", ""),
        ]);
        app.set_viewport_height(10);

        let showing = app.toggle_agent_filter();

        assert!(showing);
        assert!(app.show_agents());
        assert_eq!(ids(&app), vec!["h1", "a1"]);

        let hiding = app.toggle_agent_filter();
        assert!(!hiding);
        assert_eq!(ids(&app), vec!["h1"]);
    }

    #[test]
    fn toggle_agent_filter_keeps_the_current_selection() {
        let mut app = App::new(vec![
            row("h1", "Human one", ""),
            row("h2", "Human two", ""),
            agent_row("a1", "Agent one", ""),
        ]);
        app.set_viewport_height(10);
        app.select_next(); // h2

        app.toggle_agent_filter();

        assert_eq!(app.selected_row().unwrap().id, "h2");
    }

    #[test]
    fn agent_filter_hides_matches_during_search_without_affecting_ranking() {
        let mut app = App::new(vec![
            row("h1", "match one", ""),
            agent_row("a1", "match two", ""),
        ]);
        app.set_viewport_height(10);
        for c in "match".chars() {
            app.push_char(c);
        }

        // The agent session matches the query too, but stays hidden.
        assert_eq!(ids(&app), vec!["h1"]);

        app.toggle_agent_filter();
        assert_eq!(ids(&app), vec!["h1", "a1"]);
    }

    #[test]
    fn opening_the_new_session_modal_seeds_deduped_recency_ordered_candidates() {
        // Rows arrive mtime-descending (newest first); "/a" repeats, so its
        // first (most recent) occurrence is what should survive the dedup.
        let mut app = App::new(vec![
            row("1", "one", "/a"),
            row("2", "two", "/b"),
            row("3", "three", "/a"),
        ]);
        app.set_viewport_height(10);

        app.open_new_session_modal();

        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.candidates(), vec!["/a", "/b"]);
        assert_eq!(state.input(), "");
    }

    #[test]
    fn modal_push_char_and_backspace_filter_and_restore_candidates() {
        let mut app = App::new(vec![
            row("1", "one", "/work/alpha"),
            row("2", "two", "/work/beta"),
        ]);
        app.set_viewport_height(10);
        app.open_new_session_modal();

        for c in "beta".chars() {
            app.modal_push_char(c);
        }
        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.candidates(), vec!["/work/beta"]);

        app.modal_backspace();
        app.modal_backspace();
        app.modal_backspace();
        app.modal_backspace();
        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.input(), "");
        assert_eq!(state.candidates(), vec!["/work/alpha", "/work/beta"]);
    }

    #[test]
    fn modal_select_next_prev_clamps_within_candidates() {
        let mut app = App::new(vec![row("1", "one", "/a"), row("2", "two", "/b")]);
        app.set_viewport_height(10);
        app.open_new_session_modal();

        app.modal_select_prev(); // already at the top candidate
        assert_eq!(app.modal_new_session_target(), Some(PathBuf::from("/a")));

        app.modal_select_next();
        assert_eq!(app.modal_new_session_target(), Some(PathBuf::from("/b")));

        app.modal_select_next(); // clamps at the last candidate
        assert_eq!(app.modal_new_session_target(), Some(PathBuf::from("/b")));
    }

    #[test]
    fn modal_new_session_target_falls_back_to_raw_input_when_nothing_matches() {
        let mut app = App::new(vec![row("1", "one", "/work/alpha")]);
        app.set_viewport_height(10);
        app.open_new_session_modal();

        for c in "/brand/new/path".chars() {
            app.modal_push_char(c);
        }

        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert!(state.candidates().is_empty());
        assert_eq!(
            app.modal_new_session_target(),
            Some(PathBuf::from("/brand/new/path"))
        );
    }

    #[test]
    fn modal_new_session_target_is_none_with_empty_input_and_no_candidates() {
        let mut app = App::new(Vec::new());
        app.set_viewport_height(10);
        app.open_new_session_modal();

        assert_eq!(app.modal_new_session_target(), None);
    }

    #[test]
    fn close_modal_clears_it_and_modal_methods_become_noops() {
        let mut app = App::new(vec![row("1", "one", "/a")]);
        app.set_viewport_height(10);
        app.open_new_session_modal();
        assert!(app.modal().is_some());

        app.close_modal();
        assert!(app.modal().is_none());

        // No modal open: these must not panic and must do nothing.
        app.modal_push_char('x');
        app.modal_backspace();
        app.modal_select_next();
        app.modal_select_prev();
        assert_eq!(app.modal_new_session_target(), None);
        assert!(app.modal().is_none());
    }
}
