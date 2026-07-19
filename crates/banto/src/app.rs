//! TUI application state.
//!
//! All filtering, sorting, selection and scroll math lives here as a plain,
//! UI-free struct so it can be unit-tested without a terminal. The render loop
//! in [`crate::tui`] is a thin shell over this state.

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

/// The whole TUI state: the full session list plus the live query, filter
/// result, selection and scroll position.
pub struct App {
    /// All sessions, pre-sorted newest-first; never reordered after `new`.
    rows: Vec<SessionRow>,
    /// Search haystacks, parallel to `rows` (`title + " " + cwd`).
    haystacks: Vec<String>,
    /// Current search query.
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

impl App {
    /// Build the state from a session list (already sorted newest-first).
    pub fn new(rows: Vec<SessionRow>) -> Self {
        let haystacks = rows.iter().map(SessionRow::haystack).collect();
        let filtered = (0..rows.len()).collect();
        Self {
            rows,
            haystacks,
            query: String::new(),
            filtered,
            selected: 0,
            offset: 0,
            viewport_height: 0,
            last_click: None,
            status: None,
            should_quit: false,
        }
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
        self.filtered = rank_indices(&self.query, &self.haystacks);
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
    pub fn visible(&self) -> Vec<&SessionRow> {
        let end = (self.offset + self.viewport_height).min(self.filtered.len());
        self.filtered[self.offset..end]
            .iter()
            .map(|&i| &self.rows[i])
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
        }
    }

    fn numbered(count: usize) -> Vec<SessionRow> {
        (0..count)
            .map(|i| row(&format!("id{i}"), &format!("title {i}"), ""))
            .collect()
    }

    fn ids(app: &App) -> Vec<String> {
        app.visible().iter().map(|row| row.id.clone()).collect()
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
}
