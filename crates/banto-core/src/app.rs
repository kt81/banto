//! TUI application state.
//!
//! All filtering, sorting, selection and scroll math lives here as a plain,
//! UI-free struct so it can be unit-tested without a terminal. The render loop
//! in `banto::tui` (the chōba list) is a thin shell over this state; the
//! emporium's `crate::engine` uses it too.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::model::{Activity, SessionRow};

/// A group id, mirroring `banto_io::store::GroupId` (`i64`) without
/// coupling `App` to the store crate's types.
pub type GroupId = i64;

/// Maximum gap between two clicks on the same row to count as a double-click.
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);

/// How long a transient status-bar message stays visible before
/// auto-clearing on its own, even if the user never presses another key
/// (see [`App::expire_status`]).
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Number of Unicode scalar values in `s` — the unit every text-input cursor
/// position is counted in, so a cursor index can never land inside a
/// multi-byte character's encoding.
fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Byte offset of the char-index `idx` position within `s` (i.e. where a
/// cursor at that index actually sits), or `s.len()` once `idx` is at or past
/// the end.
fn byte_offset(s: &str, idx: usize) -> usize {
    s.char_indices().nth(idx).map(|(i, _)| i).unwrap_or(s.len())
}

fn insert_at_cursor(s: &mut String, cursor: usize, c: char) {
    s.insert(byte_offset(s, cursor), c);
}

/// Remove the character immediately before char-index `cursor`, returning
/// whether anything was removed (`false` when `cursor == 0`, matching how
/// Backspace at the start of a field does nothing).
fn remove_before_cursor(s: &mut String, cursor: usize) -> bool {
    if cursor == 0 {
        return false;
    }
    let end = byte_offset(s, cursor);
    let start = byte_offset(s, cursor - 1);
    s.replace_range(start..end, "");
    true
}

/// Remove the character at char-index `cursor`, returning whether anything
/// was removed (`false` once the cursor is at or past the end).
fn remove_at_cursor(s: &mut String, cursor: usize) -> bool {
    let start = byte_offset(s, cursor);
    let Some(c) = s[start..].chars().next() else {
        return false;
    };
    s.replace_range(start..start + c.len_utf8(), "");
    true
}

/// Outcome of a left-click on the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickOutcome {
    Selected,
    /// The row was activated (double click) — equivalent to pressing Enter.
    Activated,
}

/// Which open path a risky-open confirmation ([`App::confirm_director_open`])
/// applies to. Enter and double-click both resume in place, so they share
/// [`Self::Resume`]; `s` (split) is tracked separately so confirming one
/// never silently authorizes the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAction {
    Resume,
    Split,
}

/// An armed but unconfirmed risky open — see [`App::confirm_director_open`].
struct PendingRiskyOpen {
    session_id: String,
    action: OpenAction,
    armed_at: Instant,
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
    /// The `d` archive confirm dialog: Enter archives, Esc cancels.
    ConfirmArchive { session_id: String, title: String },
    /// The `g` group-join dialog: pick an existing group or type a new name.
    GroupJoin(GroupJoinState),
    /// The emporium's `B`-on-a-Director disband confirm dialog: Enter
    /// disbands the brigade (`brigade_id`), Esc cancels. `name` is the
    /// Director's title, for the prompt. The chōba never opens this, but
    /// still has to render/dispatch it since the two share `App`/`render_modal`.
    ConfirmDisband { brigade_id: i64, name: String },
    /// The emporium's prefix-`x` kill confirm dialog: Enter kills the
    /// session's process (`key`, its `SessionKey` as a plain string — see
    /// `crate::embedded::engine::SessionKey`; kept as `String` here so
    /// `Modal` doesn't need to depend on an `embedded`-internal type), Esc
    /// cancels. `title` is the session's display title, for the prompt.
    /// The chōba never opens this either, for the same reason as
    /// `ConfirmDisband`.
    ConfirmKill {
        key: String,
        title: String,
        /// `Some` only when the focused pane is a Worker's (the engine
        /// decides this structurally, from which pane is focused — see
        /// `crate::embedded::engine::PrefixAction::Kill`) — which of the
        /// dialog's two choices is currently highlighted. `None` for a
        /// Director or solo pane, whose dialog stays the single kill-only
        /// confirm it always was: dismissing a Director is disband's job
        /// (`B`), and a solo pane has no brigade to dismiss from at all.
        worker_choice: Option<KillChoice>,
    },
}

/// The two choices in a Worker's prefix-`x` kill confirm dialog (see
/// [`Modal::ConfirmKill`]). Toggled by Up/Down, same as any other modal's
/// candidate selection ([`App::modal_select_prev`]/[`App::modal_select_next`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillChoice {
    /// End the process; membership survives — the token respawns fresh next
    /// time its brigade is staged (today's only behavior, for every pane
    /// kind). Highlighted by default: the less destructive of the two.
    ClosePane,
    /// Remove the Worker from the brigade for good (暇を出す): its
    /// membership, read cursor, and any mail addressed specifically to it
    /// are gone too, on top of ending the process.
    Dismiss,
}

impl KillChoice {
    fn toggle(self) -> Self {
        match self {
            KillChoice::ClosePane => KillChoice::Dismiss,
            KillChoice::Dismiss => KillChoice::ClosePane,
        }
    }
}

/// State for the group-join modal: a free-text new-group-name input plus a
/// substring-filtered list of existing groups to pick from instead.
pub struct GroupJoinState {
    session_id: String,
    /// Every existing group, alphabetical (same order as `App::groups`) —
    /// captured once when the modal opens.
    candidates: Vec<(GroupId, String)>,
    /// What the user has typed so far (a new group name, unless it matches
    /// an existing one — see [`Self::target`]).
    input: String,
    /// Char-index position of the text cursor within `input` (0..=its char
    /// length); see [`Self::push_char`]/[`Self::move_cursor_left`] etc.
    cursor: usize,
    /// Indices into `candidates` whose name contains `input`
    /// (case-insensitive), in the same order as `candidates`.
    filtered: Vec<usize>,
    /// Selected position within `filtered`.
    selected: usize,
}

/// What confirming the group-join modal would do.
#[derive(Debug)]
pub enum GroupJoinTarget {
    /// Join the highlighted existing group (id, name).
    Existing(GroupId, String),
    /// Create a new group with this name, then join it.
    New(String),
}

impl GroupJoinState {
    fn new(session_id: String, candidates: Vec<(GroupId, String)>) -> Self {
        let mut state = Self {
            session_id,
            candidates,
            input: String::new(),
            cursor: 0,
            filtered: Vec::new(),
            selected: 0,
        };
        state.refilter();
        state
    }

    fn refilter(&mut self) {
        let needle = self.input.to_lowercase();
        self.filtered = self
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, (_, name))| needle.is_empty() || name.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        self.selected = 0;
    }

    fn push_char(&mut self, c: char) {
        insert_at_cursor(&mut self.input, self.cursor, c);
        self.cursor += 1;
        self.refilter();
    }

    /// Delete the character before the cursor and re-filter (no-op at the
    /// start of the input).
    fn backspace(&mut self) {
        if remove_before_cursor(&mut self.input, self.cursor) {
            self.cursor -= 1;
            self.refilter();
        }
    }

    /// Delete the character at the cursor and re-filter (no-op at the end of
    /// the input).
    fn delete_forward(&mut self) {
        if remove_at_cursor(&mut self.input, self.cursor) {
            self.refilter();
        }
    }

    fn move_cursor_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_cursor_right(&mut self) {
        self.cursor = (self.cursor + 1).min(char_len(&self.input));
    }

    fn move_cursor_home(&mut self) {
        self.cursor = 0;
    }

    fn move_cursor_end(&mut self) {
        self.cursor = char_len(&self.input);
    }

    /// Char-index position of the text cursor within [`Self::input`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let max = self.filtered.len() - 1;
        let target = (self.selected as isize + delta).clamp(0, max as isize);
        self.selected = target as usize;
    }

    /// The session this modal is assigning to a group.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// What the user has typed so far.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Existing group names matching the input, alphabetical.
    pub fn candidates(&self) -> Vec<&str> {
        self.filtered
            .iter()
            .map(|&i| self.candidates[i].1.as_str())
            .collect()
    }

    /// Position within [`Self::candidates`] that's highlighted, or `None`
    /// when nothing matches.
    pub fn selected(&self) -> Option<usize> {
        (!self.filtered.is_empty()).then_some(self.selected)
    }

    /// What confirming right now would do: join the highlighted existing
    /// group if any match, otherwise create a new one named after the raw
    /// input (`None` if that's empty too — nothing to confirm).
    fn target(&self) -> Option<GroupJoinTarget> {
        if let Some(&i) = self.filtered.get(self.selected) {
            let (id, name) = &self.candidates[i];
            return Some(GroupJoinTarget::Existing(*id, name.clone()));
        }
        (!self.input.is_empty()).then(|| GroupJoinTarget::New(self.input.clone()))
    }
}

/// Where confirming the new-session modal launches the session — mirrors
/// the list's own Enter (in-place) / `s` (split) distinction. Chosen up
/// front by which key opened the modal (`n` = in-place, `N` = split — see
/// [`App::open_new_session_modal`]/[`App::open_new_session_modal_split`])
/// rather than toggled inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewSessionPlacement {
    /// `n`: hand banto's own terminal to the new session (the default,
    /// matching Enter on the list).
    InPlace,
    /// `N`: launch it in a separate psmux pane / Windows Terminal tab
    /// (matching `s` on the list).
    Split,
}

/// State for the new-session modal: a free-text cwd input plus a
/// substring-filtered list of previously seen cwds (extracted from the
/// loaded sessions) to pick from instead of typing a full path.
pub struct NewSessionState {
    /// Every distinct cwd seen across the loaded sessions, most-recent-use
    /// first — captured once when the modal opens rather than re-derived on
    /// every keystroke (`base_rows` is already mtime-descending, so keeping
    /// the first occurrence of each cwd preserves that order).
    candidates: Vec<String>,
    /// What the user has typed so far.
    input: String,
    /// Char-index position of the text cursor within `input` (0..=its char
    /// length); see [`Self::push_char`]/[`Self::move_cursor_left`] etc.
    cursor: usize,
    /// Indices into `candidates` whose text contains `input`
    /// (case-insensitive), in the same recency order as `candidates` —
    /// filtering narrows the list, it doesn't re-rank it.
    filtered: Vec<usize>,
    /// Selected position within `filtered`.
    selected: usize,
    /// Inline validation error from the last confirm attempt (e.g. the typed
    /// path doesn't exist), cleared as soon as the input changes again.
    error: Option<String>,
    /// Fixed for the lifetime of the modal (see [`NewSessionPlacement`]).
    placement: NewSessionPlacement,
    /// Set while a `Cmd::CheckNewSessionCwd` round trip is in flight for
    /// this modal (Enter sent it, `Event::NewSessionCwdChecked` hasn't
    /// answered yet) — see `engine::confirm_new_session_modal`'s doc. Gates
    /// a second Enter from firing a second round trip before the first
    /// answers.
    checking: bool,
}

impl NewSessionState {
    fn new(rows: &[SessionRow], placement: NewSessionPlacement) -> Self {
        let mut state = Self {
            candidates: unique_cwds(rows),
            input: String::new(),
            cursor: 0,
            filtered: Vec::new(),
            selected: 0,
            error: None,
            placement,
            checking: false,
        };
        state.refilter();
        state
    }

    fn refilter(&mut self) {
        let needle = self.input.to_lowercase();
        self.filtered = self
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                needle.is_empty() || candidate.to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect();
        self.selected = 0;
        self.error = None;
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let max = self.filtered.len() - 1;
        let target = (self.selected as isize + delta).clamp(0, max as isize);
        self.selected = target as usize;
    }

    /// Complete the input to the currently highlighted candidate (bound to
    /// Tab), moving the cursor to the end of the completed text — the same
    /// place a normal shell's tab-completion leaves it. No-op when nothing is
    /// highlighted.
    fn complete_candidate(&mut self) {
        if let Some(&i) = self.filtered.get(self.selected) {
            self.input = self.candidates[i].clone();
            self.cursor = char_len(&self.input);
            self.refilter();
        }
    }

    fn push_char(&mut self, c: char) {
        insert_at_cursor(&mut self.input, self.cursor, c);
        self.cursor += 1;
        self.refilter();
    }

    /// Delete the character before the cursor and re-filter (no-op at the
    /// start of the input).
    fn backspace(&mut self) {
        if remove_before_cursor(&mut self.input, self.cursor) {
            self.cursor -= 1;
            self.refilter();
        }
    }

    /// Delete the character at the cursor and re-filter (no-op at the end of
    /// the input).
    fn delete_forward(&mut self) {
        if remove_at_cursor(&mut self.input, self.cursor) {
            self.refilter();
        }
    }

    fn move_cursor_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_cursor_right(&mut self) {
        self.cursor = (self.cursor + 1).min(char_len(&self.input));
    }

    fn move_cursor_home(&mut self) {
        self.cursor = 0;
    }

    fn move_cursor_end(&mut self) {
        self.cursor = char_len(&self.input);
    }

    /// Char-index position of the text cursor within [`Self::input`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The cwd typed so far.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Candidates whose text contains the input, most-recent-use first.
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

    /// The inline validation error from the last confirm attempt, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Where confirming this modal launches the session (see
    /// [`NewSessionPlacement`]).
    pub fn placement(&self) -> NewSessionPlacement {
        self.placement
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
    /// Claude session ids of brigade Workers, hidden from `filtered` (see
    /// [`Self::compute_filtered`]) — a Worker is banto's own implementation
    /// detail, not a session the user picks directly. A cache loaded from the
    /// store at startup and on every reload (see [`Self::with_hidden_worker_ids`]/
    /// [`Self::set_hidden_worker_ids`]); never used to pre-filter `base_rows`
    /// itself, so `row_for_id` can still resolve a hidden Worker's row when
    /// staging its brigade.
    hidden: HashSet<String>,
    /// Claude session ids of brigade Directors, for the list/summary marker
    /// (mirrors `pinned`'s cache-only role); see
    /// [`Self::with_directors`]/[`Self::set_directors`].
    directors: HashSet<String>,
    /// Session ids superseded by an auto-compaction continuation (every id
    /// with a known successor; see `banto_io::lineage`/`Store::lineage_parent_ids`).
    /// Hidden from `filtered` like `hidden`, gated by [`Self::show_agents`]
    /// rather than always-off, UNLESS the session is currently live (see
    /// [`Self::compute_filtered`]) — a running resumed ancestor must stay
    /// visible, hiding it would lie about what's actually running. A cache
    /// loaded from the store at startup and on every reload; see
    /// [`Self::with_superseded`]/[`Self::set_superseded`].
    superseded: HashSet<String>,
    /// Whether agent-run sessions (`SessionRow::is_agent`) and non-live
    /// superseded sessions are included in `filtered`. Off by default: a
    /// human browsing their own sessions doesn't usually want every
    /// spawned-agent or superseded-ancestor session cluttering the list.
    show_agents: bool,
    /// Every known group, alphabetical by name — a cache for sorting/display
    /// only, the store is the durable source of truth; see [`Self::with_groups`].
    groups: Vec<(GroupId, String)>,
    /// Session id -> group id, for sessions that belong to one. A cache
    /// mirroring `groups`; see [`Self::with_groups`].
    session_group: HashMap<String, GroupId>,
    /// Whether the list is shown grouped into sections (Pinned, then each
    /// group by name, then Ungrouped) — default on, toggled by Tab. Only
    /// takes effect with an empty query; an active search always stays flat
    /// ranked regardless of this flag (see [`Self::compute_filtered`]).
    grouped_view: bool,
    /// Current input mode; see [`Mode`].
    mode: Mode,
    /// A modal dialog currently overlaying the list, if any; see [`Modal`].
    modal: Option<Modal>,
    /// Current search query. Always empty outside [`Mode::Search`] — entering
    /// Normal mode always clears it (see [`Self::exit_search`]).
    query: String,
    /// Char-index position of the text cursor within `query` (0..=its char
    /// length); see [`Self::push_char`]/[`Self::move_cursor_left`] etc.
    query_cursor: usize,
    /// Indices into `rows` that match the query, in display order.
    filtered: Vec<usize>,
    /// Selected position within `filtered`.
    selected: usize,
    /// First visible position within [`Self::display_sequence`] (scroll
    /// offset) — a display-line index, not a `filtered` index: in grouped
    /// view, section headers occupy lines of their own, so the two spaces
    /// diverge whenever any header is above the current scroll position.
    offset: usize,
    /// Number of list rows currently visible.
    viewport_height: usize,
    /// Last click (filtered index + time) for double-click detection.
    last_click: Option<(usize, Instant)>,
    /// Transient status-bar message (e.g. the phase-2 open notice).
    status: Option<String>,
    /// When `status` was posted, so [`Self::expire_status`] can clear it
    /// after [`STATUS_TIMEOUT`] even if the user never presses another key.
    status_set_at: Option<Instant>,
    /// An armed-but-unconfirmed risky open (currently: opening a brigade
    /// Director from the chōba) — see [`Self::confirm_director_open`].
    pending_risky_open: Option<PendingRiskyOpen>,
    /// Set once the user asks to quit.
    should_quit: bool,
}

/// One session row as rendered in the viewport (see [`ListLine`]).
pub struct VisibleRow<'a> {
    pub row: &'a SessionRow,
    pub pinned: bool,
    pub director: bool,
    /// True when this session has a known auto-compaction continuation —
    /// shown regardless of *why* the row is visible (still live, or the
    /// `a` toggle is on).
    pub superseded: bool,
}

/// One physical line in the rendered list, in display order: either a real
/// session row, or (grouped view only) a section header — its own line, not
/// bundled into the row after it, so it occupies exactly one slot in the
/// same index space [`App::click`]/[`App::scroll`]/[`App::ensure_visible`]
/// use. That's what lets a click below a header land on the right row and
/// keeps "how many rows fit in the viewport" accurate even when headers are
/// present — seeded in the previous pass, headers were bundled into the
/// following row's item instead, which under-counted physical rows and
/// misaligned every click below a header. See [`App::display_sequence`].
pub enum ListLine<'a> {
    /// `count` is how many rows fall under this section under the current
    /// filter — varies as the filter changes, so a consumer comparing
    /// header identity (not just displaying it) must compare `name` only.
    Header {
        name: String,
        count: usize,
    },
    Row(VisibleRow<'a>),
}

/// A line in the full grouped-view display sequence (every matching row
/// plus its headers, not just the current viewport window) — the index
/// space [`App::click`]/[`App::scroll`]/[`App::ensure_visible`] all convert
/// through, via [`App::display_sequence`]. `Row` holds a position *within
/// `filtered`* (not a row index directly), so translating a display line
/// back to a session is always `self.rows[self.filtered[k]]`.
enum DisplayLine {
    Header { name: String, count: usize },
    Row(usize),
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
            hidden: HashSet::new(),
            directors: HashSet::new(),
            superseded: HashSet::new(),
            show_agents: false,
            groups: Vec::new(),
            session_group: HashMap::new(),
            grouped_view: true,
            mode: Mode::Normal,
            modal: None,
            query: String::new(),
            query_cursor: 0,
            filtered: Vec::new(),
            selected: 0,
            offset: 0,
            viewport_height: 0,
            last_click: None,
            status: None,
            status_set_at: None,
            pending_risky_open: None,
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

    /// Enter [`Mode::Search`] (bound to `/` in Normal mode). The cursor
    /// starts at the end of whatever query is already there (e.g. after a
    /// [`Self::confirm_search`] kept it) — the same place re-focusing a
    /// normal text field would leave it.
    pub fn enter_search(&mut self) {
        self.mode = Mode::Search;
        self.query_cursor = char_len(&self.query);
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

    /// Whether typed characters are currently being accepted as text:
    /// [`Mode::Search`], or a modal with a text field ([`Modal::NewSession`]/
    /// [`Modal::GroupJoin`]). `false` for a confirm-only modal
    /// (`ConfirmArchive`/`ConfirmDisband`/`ConfirmKill`) — those ignore
    /// [`Self::push_char`]/[`Self::modal_push_char`] entirely, and their
    /// y/n/Enter keys must stay zero-latency. Named for its one consumer so
    /// far, the emporium's paste accumulator (`paste_accum::is_in_scope`),
    /// which widens paste synthesis to exactly this set of contexts on top
    /// of a focused pane, without teaching the shell about modal variants.
    pub fn accepts_text_input(&self) -> bool {
        if self.mode == Mode::Search {
            return true;
        }
        matches!(
            self.modal,
            Some(Modal::NewSession(_)) | Some(Modal::GroupJoin(_))
        )
    }

    /// Open the `n` new-session modal for an in-place launch, seeding its
    /// candidate list from every distinct cwd across all loaded sessions
    /// (bound to `n` in [`Mode::Normal`] — the default placement, matching
    /// Enter on the list).
    pub fn open_new_session_modal(&mut self) {
        self.open_new_session_modal_as(NewSessionPlacement::InPlace);
    }

    /// Open the new-session modal for a split launch (bound to `N` in
    /// [`Mode::Normal`] — matching `s` on the list). Otherwise identical to
    /// [`Self::open_new_session_modal`].
    pub fn open_new_session_modal_split(&mut self) {
        self.open_new_session_modal_as(NewSessionPlacement::Split);
    }

    fn open_new_session_modal_as(&mut self, placement: NewSessionPlacement) {
        self.modal = Some(Modal::NewSession(NewSessionState::new(
            &self.base_rows,
            placement,
        )));
    }

    /// Open the `d` archive confirm modal for the selected session (no-op
    /// when nothing is selected; bound to `d` in [`Mode::Normal`]).
    pub fn open_confirm_archive_modal(&mut self) {
        if let Some(row) = self.selected_row() {
            self.modal = Some(Modal::ConfirmArchive {
                session_id: row.id.clone(),
                title: row.display_title().to_string(),
            });
        }
    }

    /// Open the `g` group-join modal for the selected session, seeding its
    /// candidate list from every known group (no-op when nothing is
    /// selected; bound to `g` in [`Mode::Normal`]).
    pub fn open_group_join_modal(&mut self) {
        if let Some(row) = self.selected_row() {
            self.modal = Some(Modal::GroupJoin(GroupJoinState::new(
                row.id.clone(),
                self.groups.clone(),
            )));
        }
    }

    /// Close whatever modal is open (no-op if none); bound to Esc while a
    /// modal is open.
    pub fn close_modal(&mut self) {
        self.modal = None;
    }

    /// Insert a character at the open modal's text-input cursor and
    /// re-filter its candidates. No-op when no modal is open, or it has no
    /// text input (e.g. the archive confirm dialog).
    pub fn modal_push_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        match &mut self.modal {
            Some(Modal::NewSession(state)) => state.push_char(c),
            Some(Modal::GroupJoin(state)) => state.push_char(c),
            Some(Modal::ConfirmArchive { .. })
            | Some(Modal::ConfirmDisband { .. })
            | Some(Modal::ConfirmKill { .. })
            | None => {}
        }
    }

    /// Delete the character before the open modal's text-input cursor and
    /// re-filter. No-op when no modal is open, the cursor is at the start,
    /// or it has no text input.
    pub fn modal_backspace(&mut self) {
        match &mut self.modal {
            Some(Modal::NewSession(state)) => state.backspace(),
            Some(Modal::GroupJoin(state)) => state.backspace(),
            _ => {}
        }
    }

    /// Delete the character at the open modal's text-input cursor and
    /// re-filter. No-op when no modal is open, the cursor is at the end, or
    /// it has no text input.
    pub fn modal_delete_forward(&mut self) {
        match &mut self.modal {
            Some(Modal::NewSession(state)) => state.delete_forward(),
            Some(Modal::GroupJoin(state)) => state.delete_forward(),
            _ => {}
        }
    }

    /// Move the open modal's text-input cursor one character left, clamped
    /// at the start. No-op when no modal is open or it has no text input.
    pub fn modal_cursor_left(&mut self) {
        match &mut self.modal {
            Some(Modal::NewSession(state)) => state.move_cursor_left(),
            Some(Modal::GroupJoin(state)) => state.move_cursor_left(),
            _ => {}
        }
    }

    /// Move the open modal's text-input cursor one character right, clamped
    /// at the end. No-op when no modal is open or it has no text input.
    pub fn modal_cursor_right(&mut self) {
        match &mut self.modal {
            Some(Modal::NewSession(state)) => state.move_cursor_right(),
            Some(Modal::GroupJoin(state)) => state.move_cursor_right(),
            _ => {}
        }
    }

    /// Move the open modal's text-input cursor to the start. No-op when no
    /// modal is open or it has no text input.
    pub fn modal_cursor_home(&mut self) {
        match &mut self.modal {
            Some(Modal::NewSession(state)) => state.move_cursor_home(),
            Some(Modal::GroupJoin(state)) => state.move_cursor_home(),
            _ => {}
        }
    }

    /// Move the open modal's text-input cursor to the end. No-op when no
    /// modal is open or it has no text input.
    pub fn modal_cursor_end(&mut self) {
        match &mut self.modal {
            Some(Modal::NewSession(state)) => state.move_cursor_end(),
            Some(Modal::GroupJoin(state)) => state.move_cursor_end(),
            _ => {}
        }
    }

    /// Move the open modal's candidate selection. No-op when no modal is
    /// open, or it has no candidate list. A Worker's kill-confirm dialog has
    /// only two choices, so prev/next both just toggle between them.
    pub fn modal_select_prev(&mut self) {
        match &mut self.modal {
            Some(Modal::NewSession(state)) => state.move_selection(-1),
            Some(Modal::GroupJoin(state)) => state.move_selection(-1),
            Some(Modal::ConfirmKill {
                worker_choice: Some(choice),
                ..
            }) => *choice = choice.toggle(),
            _ => {}
        }
    }

    /// Move the open modal's candidate selection. No-op when no modal is
    /// open, or it has no candidate list. A Worker's kill-confirm dialog has
    /// only two choices, so prev/next both just toggle between them.
    pub fn modal_select_next(&mut self) {
        match &mut self.modal {
            Some(Modal::NewSession(state)) => state.move_selection(1),
            Some(Modal::GroupJoin(state)) => state.move_selection(1),
            Some(Modal::ConfirmKill {
                worker_choice: Some(choice),
                ..
            }) => *choice = choice.toggle(),
            _ => {}
        }
    }

    /// Complete the open modal's input to its highlighted candidate (bound
    /// to Tab). No-op when no modal is open or nothing is highlighted (only
    /// the new-session modal supports this).
    pub fn modal_complete_candidate(&mut self) {
        if let Some(Modal::NewSession(state)) = &mut self.modal {
            state.complete_candidate();
        }
    }

    /// The cwd the new-session modal would launch if confirmed right now
    /// (see [`NewSessionState::target`]); `None` if no modal is open or
    /// there's nothing to launch. Does not close the modal — the caller
    /// does that once the launch itself succeeds.
    pub fn modal_new_session_target(&self) -> Option<PathBuf> {
        match &self.modal {
            Some(Modal::NewSession(state)) => state.target(),
            _ => None,
        }
    }

    /// Whether the new-session modal has a `Cmd::CheckNewSessionCwd` round
    /// trip already in flight — `false` when no modal is open, since there
    /// is nothing to gate. See [`NewSessionState::checking`].
    pub fn modal_new_session_check_pending(&self) -> bool {
        matches!(&self.modal, Some(Modal::NewSession(state)) if state.checking)
    }

    /// Mark the open new-session modal as awaiting a check-cwd verdict. No-op
    /// when no such modal is open.
    pub fn modal_begin_new_session_check(&mut self) {
        if let Some(Modal::NewSession(state)) = &mut self.modal {
            state.checking = true;
        }
    }

    /// Resolve a `Cmd::CheckNewSessionCwd` round trip for `cwd`: `true` if
    /// this verdict still applies (the new-session modal is open, was
    /// awaiting one, and `cwd` still matches its *current* target) and the
    /// caller should act on it; `false` — with nothing touched beyond
    /// clearing the pending marker, if it was set — for a stale verdict
    /// (the operator kept typing and the target moved on) or one with
    /// nothing left to resolve (no such modal open, or none pending). Either
    /// way the pending marker ends up cleared, so a fresh Enter is live
    /// again immediately.
    pub fn modal_new_session_check_resolves(&mut self, cwd: &Path) -> bool {
        let Some(Modal::NewSession(state)) = &mut self.modal else {
            return false;
        };
        if !state.checking {
            return false;
        }
        state.checking = false;
        state.target().as_deref() == Some(cwd)
    }

    /// What confirming the open group-join modal would do (see
    /// [`GroupJoinState::target`]); `None` if no group-join modal is open or
    /// there's nothing to confirm.
    pub fn modal_group_join_target(&self) -> Option<GroupJoinTarget> {
        match &self.modal {
            Some(Modal::GroupJoin(state)) => state.target(),
            _ => None,
        }
    }

    /// Set the open modal's inline validation error (e.g. "not a
    /// directory"), leaving it open so the user can correct the input. No-op
    /// when no modal is open, or it has no text input.
    pub fn modal_set_error(&mut self, message: String) {
        if let Some(Modal::NewSession(state)) = &mut self.modal {
            state.error = Some(message);
        }
    }

    // --- groups -------------------------------------------------------------

    /// Seed the initial group cache (loaded once from the store at startup)
    /// and re-sort so grouped view reflects it.
    pub fn with_groups(
        mut self,
        groups: Vec<(GroupId, String)>,
        session_group: HashMap<String, GroupId>,
    ) -> Self {
        self.groups = groups;
        self.groups.sort_by(|a, b| a.1.cmp(&b.1));
        self.session_group = session_group;
        let selected_id = self.selected_row().map(|row| row.id.clone());
        self.refilter_keeping_selected(selected_id);
        self
    }

    /// Record a session's group assignment in the cache after the `g` modal
    /// confirms (the caller persists the change to the store first). Inserts
    /// `group_name` into the known-groups cache if it's a newly created
    /// group. Keeps the same session selected (by id), since grouping may
    /// move it to a new section.
    pub fn set_session_group_cache(
        &mut self,
        session_id: &str,
        group_id: GroupId,
        group_name: String,
    ) {
        self.session_group.insert(session_id.to_string(), group_id);
        if !self.groups.iter().any(|&(id, _)| id == group_id) {
            self.groups.push((group_id, group_name));
            self.groups.sort_by(|a, b| a.1.cmp(&b.1));
        }
        let selected_id = Some(session_id.to_string());
        self.refilter_keeping_selected(selected_id);
    }

    /// Whether the list is shown grouped into sections; see the
    /// `grouped_view` field doc.
    pub fn grouped_view(&self) -> bool {
        self.grouped_view
    }

    /// Toggle grouped view, returning the new state (bound to Tab in
    /// [`Mode::Normal`]).
    pub fn toggle_grouped_view(&mut self) -> bool {
        self.grouped_view = !self.grouped_view;
        let selected_id = self.selected_row().map(|row| row.id.clone());
        self.refilter_keeping_selected(selected_id);
        self.grouped_view
    }

    /// Whether grouped-section display is actually in effect right now: the
    /// toggle is on, the query is empty (an active search always stays flat
    /// ranked), and `ranked` actually spans more than one section —
    /// otherwise a lone "Ungrouped" header would show above every row for
    /// no benefit (e.g. before the user has pinned or grouped anything at
    /// all). Takes `ranked` explicitly rather than reading `self.filtered`:
    /// the one call site inside [`Self::compute_filtered`] runs *before*
    /// `self.filtered` is reassigned, while `self.rows` may already have
    /// changed size (e.g. `replace_rows` shrinking it) — reading the stale
    /// field there would index into `self.rows` with indices from before
    /// the resize and panic.
    fn grouped_view_active(&self, ranked: &[usize]) -> bool {
        self.grouped_view && self.query.is_empty() && self.has_multiple_sections(ranked)
    }

    /// Public form of [`Self::grouped_view_active`] against the current
    /// `filtered` set, for a renderer deciding view-mode-wide layout (e.g.
    /// whether the pin marker's column exists at all this frame — see
    /// `banto_tui::view`'s row layout doc). Every pinned row's section is
    /// "Pinned" (`section_name` gives it top priority), so whenever this is
    /// true, per-row pin suppression under that header would apply to every
    /// pinned row without exception — hence "the slot doesn't exist" rather
    /// than "the slot renders blank".
    pub fn grouped_view_in_effect(&self) -> bool {
        self.grouped_view_active(&self.filtered)
    }

    /// Whether `ranked` spans more than one grouped-view section.
    fn has_multiple_sections(&self, ranked: &[usize]) -> bool {
        let mut ranks = ranked.iter().map(|&i| self.section_rank(i));
        let Some(first) = ranks.next() else {
            return false;
        };
        ranks.any(|rank| rank != first)
    }

    /// The section a row belongs to in grouped view: "Pinned" takes
    /// priority over group membership (a pinned+grouped session shows once,
    /// under Pinned, not duplicated into its group's section too), then the
    /// session's group name if it has one, else "Ungrouped".
    fn section_name(&self, row_index: usize) -> String {
        let row = &self.rows[row_index];
        if self.pinned.contains(&row.id) {
            "Pinned".to_string()
        } else if let Some(group_id) = self.session_group.get(&row.id) {
            self.groups
                .iter()
                .find(|&&(id, _)| id == *group_id)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| "Ungrouped".to_string())
        } else {
            "Ungrouped".to_string()
        }
    }

    /// Sort key for a row's section in grouped view: 0 = Pinned, 1..=N = each
    /// known group in alphabetical order (matching `self.groups`), `MAX` =
    /// Ungrouped (always sorts last).
    fn section_rank(&self, row_index: usize) -> usize {
        let row = &self.rows[row_index];
        if self.pinned.contains(&row.id) {
            return 0;
        }
        match self.session_group.get(&row.id) {
            Some(group_id) => self
                .groups
                .iter()
                .position(|&(id, _)| id == *group_id)
                .map_or(usize::MAX, |rank| rank + 1),
            None => usize::MAX,
        }
    }

    /// The full display sequence (not just the current viewport window):
    /// every entry of `filtered`, each represented as `DisplayLine::Row(k)`
    /// (`k` being its position within `filtered`), with a
    /// `DisplayLine::Header` inserted immediately before the first row of
    /// each new section when grouped view is actually in effect (see
    /// [`Self::grouped_view_active`]). This is the single source of truth
    /// [`Self::click`]/[`Self::scroll`]/[`Self::ensure_visible`]/
    /// [`Self::visible`] all convert through, so headers always occupy
    /// exactly one line in whichever index space each of those needs.
    fn display_sequence(&self) -> Vec<DisplayLine> {
        if !self.grouped_view_active(&self.filtered) {
            return (0..self.filtered.len()).map(DisplayLine::Row).collect();
        }
        // Sections are contiguous in `filtered` (sorted by `section_rank`),
        // so a plain occurrence count per name is exactly each header's
        // row count under the current filter.
        let mut section_counts: HashMap<String, usize> = HashMap::new();
        for &row_index in &self.filtered {
            *section_counts
                .entry(self.section_name(row_index))
                .or_insert(0) += 1;
        }
        let mut lines = Vec::with_capacity(self.filtered.len());
        let mut prev_section: Option<String> = None;
        for (k, &row_index) in self.filtered.iter().enumerate() {
            let section = self.section_name(row_index);
            if prev_section.as_ref() != Some(&section) {
                lines.push(DisplayLine::Header {
                    name: section.clone(),
                    count: section_counts[&section],
                });
                prev_section = Some(section);
            }
            lines.push(DisplayLine::Row(k));
        }
        lines
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

    /// Number of sessions matching the current query that the `a` toggle is
    /// currently hiding: agent-run sessions, plus superseded (auto-compaction
    /// ancestor) sessions that aren't currently live (see
    /// [`Self::is_hidden_superseded`]). Always `0` once [`Self::show_agents`]
    /// is on.
    pub fn hidden_count(&self) -> usize {
        if self.show_agents {
            return 0;
        }
        rank_indices(&self.query, &self.haystacks)
            .into_iter()
            .filter(|&i| self.rows[i].is_agent || self.is_hidden_superseded(i))
            .count()
    }

    /// True when `rows[i]` has a known auto-compaction continuation and is
    /// not currently live — the condition under which [`Self::compute_filtered`]
    /// hides it (subject to [`Self::show_agents`]). A live superseded row
    /// (a resumed ancestor still running) is never hidden: hiding a running
    /// session would lie about what's actually going on.
    fn is_hidden_superseded(&self, i: usize) -> bool {
        self.superseded.contains(&self.rows[i].id)
            && !matches!(self.rows[i].activity, Activity::Busy | Activity::Alive)
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

    /// Seed the initial hidden-worker-id set (loaded from the store at
    /// startup — see [`Self::set_hidden_worker_ids`] for reloads).
    pub fn with_hidden_worker_ids(mut self, hidden: HashSet<String>) -> Self {
        self.hidden = hidden;
        let selected_id = self.selected_row().map(|row| row.id.clone());
        self.refilter_keeping_selected(selected_id);
        self
    }

    /// Replace the hidden-worker-id set (e.g. after a reload, or once a
    /// brigade is formed/disbanded), keeping the current selection if it's
    /// still visible.
    pub fn set_hidden_worker_ids(&mut self, hidden: HashSet<String>) {
        self.hidden = hidden;
        let selected_id = self.selected_row().map(|row| row.id.clone());
        self.refilter_keeping_selected(selected_id);
    }

    /// Seed the initial brigade-director-id set (loaded from the store at
    /// startup — see [`Self::set_directors`] for reloads). Unlike
    /// [`Self::with_hidden_worker_ids`], this never affects filtering
    /// (`directors` is display-only), so no re-filter is needed.
    pub fn with_directors(mut self, directors: HashSet<String>) -> Self {
        self.directors = directors;
        self
    }

    /// Replace the brigade-director-id set (e.g. after a reload, or once a
    /// brigade is formed/disbanded).
    pub fn set_directors(&mut self, directors: HashSet<String>) {
        self.directors = directors;
    }

    /// Seed the initial superseded-session-id set (loaded from the store at
    /// startup — see [`Self::set_superseded`] for reloads). Unlike
    /// [`Self::with_directors`], this affects filtering (see
    /// [`Self::compute_filtered`]), so — like [`Self::with_hidden_worker_ids`]
    /// — it re-filters and keeps the current selection if it's still visible.
    pub fn with_superseded(mut self, superseded: HashSet<String>) -> Self {
        self.superseded = superseded;
        let selected_id = self.selected_row().map(|row| row.id.clone());
        self.refilter_keeping_selected(selected_id);
        self
    }

    /// Replace the superseded-session-id set (e.g. after a reload resolves
    /// more lineage links), keeping the current selection if it's still
    /// visible.
    pub fn set_superseded(&mut self, superseded: HashSet<String>) {
        self.superseded = superseded;
        let selected_id = self.selected_row().map(|row| row.id.clone());
        self.refilter_keeping_selected(selected_id);
    }

    /// Open the emporium's disband confirm dialog for the given brigade
    /// (bound to `B` on a session that is that brigade's Director).
    pub fn open_confirm_disband_modal(&mut self, brigade_id: i64, name: String) {
        self.modal = Some(Modal::ConfirmDisband { brigade_id, name });
    }

    /// Open the emporium's kill confirm dialog for the given session (bound
    /// to prefix-`x` on the focused pane). `is_worker` grows the dialog a
    /// second choice, defaulted to [`KillChoice::ClosePane`] — see
    /// [`Modal::ConfirmKill`].
    pub fn open_confirm_kill_modal(&mut self, key: String, title: String, is_worker: bool) {
        self.modal = Some(Modal::ConfirmKill {
            key,
            title,
            worker_choice: is_worker.then_some(KillChoice::ClosePane),
        });
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

    /// Rank `rows` against the current query, then drop agent-run and
    /// hidden-superseded sessions unless [`Self::show_agents`] is on (see
    /// [`Self::is_hidden_superseded`]), and always drop brigade Workers
    /// (banto's own implementation detail, not something the user picks
    /// directly — see `hidden`). Ranking (and, with an empty query, the
    /// pinned-first base order) always runs first and is never affected by
    /// either filter — they only remove results afterward. Finally, if
    /// grouped view is actually in effect (see [`Self::grouped_view_active`]),
    /// stably re-sorts into section order (Pinned, then each group
    /// alphabetically, then Ungrouped) — stable so each section keeps its
    /// mtime-descending relative order.
    fn compute_filtered(&self) -> Vec<usize> {
        let mut ranked: Vec<usize> = rank_indices(&self.query, &self.haystacks)
            .into_iter()
            .filter(|&i| self.show_agents || !self.rows[i].is_agent)
            .filter(|&i| self.show_agents || !self.is_hidden_superseded(i))
            .filter(|&i| !self.hidden.contains(&self.rows[i].id))
            .collect();
        if self.grouped_view_active(&ranked) {
            ranked.sort_by_key(|&i| self.section_rank(i));
        }
        ranked
    }

    // --- query editing --------------------------------------------------

    /// Insert a printable character at the query cursor and re-filter.
    pub fn push_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        insert_at_cursor(&mut self.query, self.query_cursor, c);
        self.query_cursor += 1;
        self.refilter();
    }

    /// Delete the character before the query cursor (if any) and re-filter.
    pub fn backspace(&mut self) {
        if remove_before_cursor(&mut self.query, self.query_cursor) {
            self.query_cursor -= 1;
            self.refilter();
        }
    }

    /// Delete the character at the query cursor (if any) and re-filter.
    pub fn delete_forward(&mut self) {
        if remove_at_cursor(&mut self.query, self.query_cursor) {
            self.refilter();
        }
    }

    /// Clear the query and re-filter. No-op when the query is already empty.
    pub fn clear_query(&mut self) {
        if !self.query.is_empty() {
            self.query.clear();
            self.query_cursor = 0;
            self.refilter();
        }
    }

    /// Move the query cursor one character left, clamped at the start.
    pub fn move_cursor_left(&mut self) {
        self.query_cursor = self.query_cursor.saturating_sub(1);
    }

    /// Move the query cursor one character right, clamped at the end.
    pub fn move_cursor_right(&mut self) {
        self.query_cursor = (self.query_cursor + 1).min(char_len(&self.query));
    }

    /// Move the query cursor to the start of the query.
    pub fn move_cursor_home(&mut self) {
        self.query_cursor = 0;
    }

    /// Move the query cursor to the end of the query.
    pub fn move_cursor_end(&mut self) {
        self.query_cursor = char_len(&self.query);
    }

    /// Recompute the filter result and reset selection/scroll to the top.
    fn refilter(&mut self) {
        self.filtered = self.compute_filtered();
        self.selected = 0;
        self.offset = 0;
        self.clear_status();
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

    /// Scroll the viewport by `delta` display lines without moving the
    /// selection (mouse-wheel behavior). Clamped to the valid offset range.
    pub fn scroll(&mut self, delta: isize) {
        if self.viewport_height == 0 {
            return;
        }
        let max_offset = self.max_offset(&self.display_sequence()) as isize;
        let target = (self.offset as isize + delta).clamp(0, max_offset);
        self.offset = target as usize;
    }

    /// Largest offset that still fills the viewport, in display-line space.
    fn max_offset(&self, display: &[DisplayLine]) -> usize {
        display.len().saturating_sub(self.viewport_height)
    }

    /// Scroll the minimum amount so the selection is inside the viewport,
    /// then clamp the offset so we never scroll past the end. Works in
    /// display-line space (see [`Self::display_sequence`]) so a section
    /// header above the selection is correctly counted as occupying a line
    /// of its own, rather than being invisible to this accounting.
    fn ensure_visible(&mut self) {
        if self.viewport_height == 0 {
            return;
        }
        let display = self.display_sequence();
        let Some(selected_line) = Self::display_line_of(&display, self.selected) else {
            return; // nothing selected (e.g. filtered is empty)
        };
        if selected_line < self.offset {
            self.offset = selected_line;
        } else if selected_line >= self.offset + self.viewport_height {
            self.offset = selected_line + 1 - self.viewport_height;
        }
        let max_offset = self.max_offset(&display);
        if self.offset > max_offset {
            self.offset = max_offset;
        }
    }

    /// The display-line index of the `filtered` position `k`, if it's
    /// present in `display` (it always is, unless `filtered` is empty).
    fn display_line_of(display: &[DisplayLine], k: usize) -> Option<usize> {
        display
            .iter()
            .position(|line| matches!(line, DisplayLine::Row(row_k) if *row_k == k))
    }

    // --- mouse ----------------------------------------------------------

    /// Handle a left click on viewport row `viewport_row` (0 = top visible
    /// display line). Returns `None` when the click lands past the last
    /// line, or on a section header (grouped view only) — headers aren't
    /// selectable.
    pub fn click(&mut self, viewport_row: usize, now: Instant) -> Option<ClickOutcome> {
        let display = self.display_sequence();
        let display_index = self.offset.checked_add(viewport_row)?;
        let filtered_index = match display.get(display_index)? {
            DisplayLine::Row(k) => *k,
            DisplayLine::Header { .. } => return None,
        };
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

    /// Post a transient status-bar message, timestamped (with the given
    /// `now`, not read internally — see [`Self::expire_status`]'s doc
    /// comment for why) for [`Self::expire_status`].
    pub fn set_status(&mut self, message: String, now: Instant) {
        self.status = Some(message);
        self.status_set_at = Some(now);
    }

    /// Clear any transient status-bar message, restoring the key-hint
    /// display. Called at the start of key dispatch (see `crate::tui::
    /// handle_key`) so a notification like "pinned session X" doesn't linger
    /// once the user has moved on to something else.
    pub fn clear_status(&mut self) {
        self.status = None;
        self.status_set_at = None;
    }

    /// Clear the status message once it's been showing for
    /// [`STATUS_TIMEOUT`], given the current time — a notification like
    /// "pinned session X" would otherwise linger forever if the user simply
    /// stops touching the keyboard instead of pressing another key (the
    /// event that normally clears it via [`Self::clear_status`]). Takes an
    /// injected `now` rather than reading `Instant::now()` itself so this
    /// stays testable with synthetic timestamps; the render loop calls this
    /// every tick with the real clock (see `crate::tui::event_loop`).
    pub fn expire_status(&mut self, now: Instant) {
        if let Some(set_at) = self.status_set_at
            && now.saturating_duration_since(set_at) >= STATUS_TIMEOUT
        {
            self.clear_status();
        }
    }

    /// Arm or confirm a risky open. Director-agnostic on purpose: this only
    /// tracks "has this exact `(id, action)` been requested twice within the
    /// freshness window" — the shell decides which sessions/actions the
    /// guard applies to (currently: opening a brigade Director from the
    /// chōba, gated on [`Self::is_selected_director`]).
    ///
    /// The first call for a given `(id, action)` — or one whose prior arm
    /// has gone stale — arms it and returns `false`. A second call for the
    /// *same* `(id, action)` within the freshness window disarms and
    /// returns `true`. A different id or a different action always re-arms
    /// rather than confirming, since it isn't a repeat of the last request.
    ///
    /// The freshness window matches [`STATUS_TIMEOUT`] rather than a timer
    /// of its own, so a caller that shows the warning via [`Self::status`]
    /// at the same `now` it arms with gets a confirm window that expires in
    /// lockstep with the visible warning — see `banto::tui`'s call sites.
    pub fn confirm_director_open(&mut self, id: &str, action: OpenAction, now: Instant) -> bool {
        if let Some(pending) = &self.pending_risky_open
            && pending.session_id == id
            && pending.action == action
            && now.saturating_duration_since(pending.armed_at) < STATUS_TIMEOUT
        {
            self.pending_risky_open = None;
            return true;
        }
        self.pending_risky_open = Some(PendingRiskyOpen {
            session_id: id.to_string(),
            action,
            armed_at: now,
        });
        false
    }

    /// Request that the render loop exit.
    pub fn request_quit(&mut self) {
        self.should_quit = true;
    }

    // --- accessors (for the render loop) --------------------------------

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Char-index position of the text cursor within [`Self::query`].
    pub fn query_cursor(&self) -> usize {
        self.query_cursor
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

    /// The loaded session with this id, if present — searches the full list,
    /// not just the current filter/sort, so a session referenced by id (e.g. a
    /// brigade member that the agent filter or search would otherwise hide)
    /// can still be resolved back to its title/cwd. A plain accessor, unaware
    /// of any brigade concept.
    pub fn row_for_id(&self, id: &str) -> Option<&SessionRow> {
        self.base_rows.iter().find(|row| row.id == id)
    }

    /// Whether the currently selected session is pinned (for the summary
    /// panel's marker); `false` when nothing is selected.
    pub fn is_selected_pinned(&self) -> bool {
        self.selected_row()
            .is_some_and(|row| self.pinned.contains(&row.id))
    }

    /// Whether the currently selected session has a known auto-compaction
    /// continuation (for the summary panel's marker); `false` when nothing
    /// is selected.
    pub fn is_selected_superseded(&self) -> bool {
        self.selected_row()
            .is_some_and(|row| self.superseded.contains(&row.id))
    }

    /// Whether the currently selected session is a brigade Director (for the
    /// summary panel's marker); `false` when nothing is selected.
    pub fn is_selected_director(&self) -> bool {
        self.selected_row()
            .is_some_and(|row| self.directors.contains(&row.id))
    }

    /// Selection index relative to the viewport (in display-line space —
    /// see [`Self::display_sequence`]), or `None` when the selection is
    /// scrolled out of view (or the list is empty).
    pub fn selected_in_viewport(&self) -> Option<usize> {
        if self.filtered.is_empty() {
            return None;
        }
        let selected_line = Self::display_line_of(&self.display_sequence(), self.selected)?;
        if selected_line < self.offset {
            return None;
        }
        let local = selected_line - self.offset;
        (local < self.viewport_height).then_some(local)
    }

    /// The display lines currently visible in the viewport, top to bottom —
    /// real rows and (grouped view only) section headers, each occupying
    /// exactly one line (see [`ListLine`]/[`Self::display_sequence`]). Which
    /// one (if any) is selected is reported separately by
    /// [`Self::selected_in_viewport`].
    pub fn visible(&self) -> Vec<ListLine<'_>> {
        let display = self.display_sequence();
        let end = (self.offset + self.viewport_height).min(display.len());
        display[self.offset..end]
            .iter()
            .map(|line| match line {
                DisplayLine::Header { name, count } => ListLine::Header {
                    name: name.clone(),
                    count: *count,
                },
                DisplayLine::Row(k) => {
                    let row = &self.rows[self.filtered[*k]];
                    ListLine::Row(VisibleRow {
                        row,
                        pinned: self.pinned.contains(&row.id),
                        director: self.directors.contains(&row.id),
                        superseded: self.superseded.contains(&row.id),
                    })
                }
            })
            .collect()
    }
}

/// Rank `haystacks` against `query`, returning matching indices best-first.
///
/// Delegates to [`crate::search`] (nucleo smart-case fuzzy matching): an
/// empty query yields every index in the original order, otherwise only
/// matches are returned, best score first.
fn rank_indices(query: &str, haystacks: &[String]) -> Vec<usize> {
    crate::search::rank(query, haystacks)
        .into_iter()
        .map(|m| m.index)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Activity, AgeBucket, AgentKind};
    use std::path::PathBuf;
    use std::time::SystemTime;

    /// `Instant` has no stable constructor for an arbitrary value other than
    /// `now()` — this gives tests a concrete instant to seed the `now` they
    /// pass into `App`'s methods, without silencing `disallowed-methods` for
    /// the rest of this module. Not the clock access DISCIPLINE.md §3
    /// forbids: that's about production code reading the clock itself, not
    /// a test choosing its own fixed starting point.
    #[allow(clippy::disallowed_methods)]
    fn test_instant() -> Instant {
        Instant::now()
    }

    fn row(id: &str, title: &str, cwd: &str) -> SessionRow {
        SessionRow {
            id: id.to_string(),
            agent: AgentKind::ClaudeCode,
            title: (!title.is_empty()).then(|| title.to_string()),
            cwd: (!cwd.is_empty()).then(|| PathBuf::from(cwd)),
            activity: Activity::Idle(AgeBucket::Older),
            is_agent: false,
            preview: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
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

    /// Session ids among the visible rows, in display order (headers
    /// skipped — most tests only care about the rows).
    fn ids(app: &App) -> Vec<String> {
        app.visible()
            .iter()
            .filter_map(|line| match line {
                ListLine::Row(r) => Some(r.row.id.clone()),
                ListLine::Header { .. } => None,
            })
            .collect()
    }

    /// Section header labels among the visible lines, in display order
    /// (rows skipped).
    fn headers(app: &App) -> Vec<String> {
        app.visible()
            .iter()
            .filter_map(|line| match line {
                ListLine::Header { name, .. } => Some(name.clone()),
                ListLine::Row(_) => None,
            })
            .collect()
    }

    /// Section headers among the visible lines, paired with their row count
    /// under the current filter, in display order.
    fn header_counts(app: &App) -> Vec<(String, usize)> {
        app.visible()
            .iter()
            .filter_map(|line| match line {
                ListLine::Header { name, count } => Some((name.clone(), *count)),
                ListLine::Row(_) => None,
            })
            .collect()
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
        let t0 = test_instant();

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
        assert_eq!(app.click(4, test_instant()), None);
    }

    /// Sets up 4 rows spanning 3 grouped-view sections (Pinned / "work" /
    /// Ungrouped), so the display sequence is:
    /// `[Header(Pinned), Row(id3), Header(work), Row(id1), Header(Ungrouped),
    /// Row(id0), Row(id2)]` — 7 display lines for 4 rows.
    fn grouped_app_for_click_tests() -> App {
        let mut app = App::new(vec![
            row("id0", "zero", ""),
            row("id1", "one", ""),
            row("id2", "two", ""),
            row("id3", "three", ""),
        ])
        .with_pinned(["id3".to_string()].into_iter().collect())
        .with_groups(
            vec![(1, "work".to_string())],
            [("id1".to_string(), 1)].into_iter().collect(),
        );
        app.set_viewport_height(10);
        app
    }

    #[test]
    fn click_below_a_header_selects_the_right_session() {
        let mut app = grouped_app_for_click_tests();
        // Display line 3 is Row(id1), just below the "work" header at line 2.
        assert_eq!(app.click(3, test_instant()), Some(ClickOutcome::Selected));
        assert_eq!(app.selected_row().unwrap().id, "id1");
    }

    #[test]
    fn click_on_a_header_is_a_noop() {
        let mut app = grouped_app_for_click_tests();
        app.click(1, test_instant()); // select id3 first, as a baseline
        assert_eq!(app.selected_row().unwrap().id, "id3");

        // Display line 2 is the "work" header, not a row.
        assert_eq!(app.click(2, test_instant()), None);
        // Selection untouched by the no-op click.
        assert_eq!(app.selected_row().unwrap().id, "id3");

        // Same for line 0, the "Pinned" header, and line 4, "Ungrouped".
        assert_eq!(app.click(0, test_instant()), None);
        assert_eq!(app.click(4, test_instant()), None);
        assert_eq!(app.selected_row().unwrap().id, "id3");
    }

    #[test]
    fn bottom_row_stays_reachable_with_two_sections_in_a_short_viewport() {
        // 3 rows, 2 sections (Pinned + Ungrouped), so the display sequence
        // is `[Header(Pinned), Row(pinned), Header(Ungrouped), Row(a),
        // Row(b)]` — 5 display lines for 3 rows. A naive "viewport_height
        // rows fit" assumption (ignoring the two header lines) would leave
        // the last row unreachable in a 2-line viewport.
        let mut app = App::new(vec![
            row("a", "A", ""),
            row("b", "B", ""),
            row("pinned", "P", ""),
        ])
        .with_pinned(["pinned".to_string()].into_iter().collect());
        app.set_viewport_height(2);

        app.select_last();

        assert_eq!(app.selected_row().unwrap().id, "b");
        assert_eq!(ids(&app), vec!["a", "b"]);
        // The last row is on-screen, in the viewport's bottom slot.
        assert_eq!(app.selected_in_viewport(), Some(1));
    }

    #[test]
    fn set_status_reflects_the_selected_row() {
        // The render loop (not App) drives opening; this only checks the two
        // primitives it composes: reading the selection and posting a message.
        let mut app = App::new(numbered(3));
        app.set_viewport_height(10);
        app.select_next(); // id1
        let id = app.selected_row().unwrap().id.clone();
        app.set_status(format!("opened session {id}"), test_instant());
        assert_eq!(app.status(), Some("opened session id1"));
    }

    #[test]
    fn status_expires_after_the_timeout_but_not_before() {
        let mut app = App::new(numbered(1));
        let t0 = test_instant();
        app.set_status("hello".to_string(), t0);

        // Comfortably before the 5s timeout: still showing.
        app.expire_status(t0 + Duration::from_secs(4));
        assert_eq!(app.status(), Some("hello"));

        // Comfortably after the 5s timeout: cleared.
        app.expire_status(t0 + Duration::from_secs(6));
        assert_eq!(app.status(), None);
    }

    #[test]
    fn expire_status_is_a_noop_when_no_status_is_set() {
        let mut app = App::new(numbered(1));
        app.expire_status(test_instant() + Duration::from_secs(100));
        assert_eq!(app.status(), None);
    }

    #[test]
    fn confirm_director_open_arms_on_first_call_and_returns_false() {
        let mut app = App::new(numbered(1));
        assert!(!app.confirm_director_open("id0", OpenAction::Resume, test_instant()));
    }

    #[test]
    fn confirm_director_open_confirms_and_disarms_a_matching_repeat_within_the_window() {
        let mut app = App::new(numbered(1));
        let t0 = test_instant();
        assert!(!app.confirm_director_open("id0", OpenAction::Resume, t0));

        // Comfortably inside the freshness window.
        assert!(app.confirm_director_open("id0", OpenAction::Resume, t0 + Duration::from_secs(2)));

        // Disarmed: a third call is a fresh arm, not another confirm.
        assert!(!app.confirm_director_open("id0", OpenAction::Resume, t0 + Duration::from_secs(2)));
    }

    #[test]
    fn confirm_director_open_re_arms_instead_of_confirming_once_the_window_has_expired() {
        let mut app = App::new(numbered(1));
        let t0 = test_instant();
        assert!(!app.confirm_director_open("id0", OpenAction::Resume, t0));

        // Past the same 5s freshness window `expire_status` uses.
        assert!(!app.confirm_director_open("id0", OpenAction::Resume, t0 + Duration::from_secs(6)));
    }

    #[test]
    fn confirm_director_open_a_different_id_re_arms_rather_than_confirming() {
        let mut app = App::new(numbered(2));
        let t0 = test_instant();
        assert!(!app.confirm_director_open("id0", OpenAction::Resume, t0));

        // Selection moved on to a different session — this looks like a
        // fresh request, not a repeat of the id0 one.
        assert!(!app.confirm_director_open("id1", OpenAction::Resume, t0));
    }

    #[test]
    fn confirm_director_open_a_different_action_does_not_confirm() {
        let mut app = App::new(numbered(1));
        let t0 = test_instant();
        assert!(!app.confirm_director_open("id0", OpenAction::Resume, t0));

        // Same id, but `s` (Split) was never the thing that warned — an
        // Enter confirm must not silently authorize a split, or vice versa.
        assert!(!app.confirm_director_open("id0", OpenAction::Split, t0));
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
        app.toggle_grouped_view(); // flat: pinned/unpinned unrelated to sections

        let pinned_flags: Vec<bool> = app
            .visible()
            .iter()
            .filter_map(|line| match line {
                ListLine::Row(r) => Some(r.pinned),
                ListLine::Header { .. } => None,
            })
            .collect();
        // id1 sorted first (pinned), then id0, id2.
        assert_eq!(pinned_flags, vec![true, false, false]);
    }

    #[test]
    fn grouped_view_in_effect_reflects_the_current_display_mode() {
        let mut app = App::new(numbered(3)); // id0, id1, id2
        app.set_viewport_height(10);
        app = app.with_pinned(["id1".to_string()].into_iter().collect());

        // Grouped view is on by default and there are two sections
        // (Pinned, Ungrouped), so it's actually in effect — the renderer
        // (banto_tui::view) uses this to decide whether the pin marker's
        // column exists at all this frame (every pinned row's section is
        // "Pinned", so whenever this is true, no pin marker can ever
        // render).
        assert!(app.grouped_view_in_effect());

        // Flat view: never in effect.
        app.toggle_grouped_view();
        assert!(!app.grouped_view_in_effect());

        // Toggled back on, but an active search always flattens regardless
        // of the toggle.
        app.toggle_grouped_view();
        app.push_char('i');
        assert!(!app.grouped_view_in_effect());
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
    fn query_cursor_moves_and_clamps_at_both_ends() {
        let mut app = App::new(numbered(1));
        app.enter_search();
        for c in "abc".chars() {
            app.push_char(c);
        }
        assert_eq!(app.query_cursor(), 3);

        app.move_cursor_right(); // already at the end
        assert_eq!(app.query_cursor(), 3);

        app.move_cursor_left();
        app.move_cursor_left();
        app.move_cursor_left();
        assert_eq!(app.query_cursor(), 0);

        app.move_cursor_left(); // already at the start
        assert_eq!(app.query_cursor(), 0);
    }

    #[test]
    fn push_char_inserts_at_the_cursor_not_always_at_the_end() {
        let mut app = App::new(numbered(1));
        app.enter_search();
        app.push_char('a');
        app.push_char('c');
        assert_eq!(app.query(), "ac");

        app.move_cursor_left(); // cursor between 'a' and 'c'
        app.push_char('b');

        assert_eq!(app.query(), "abc");
        assert_eq!(app.query_cursor(), 2); // right after the inserted 'b'
    }

    #[test]
    fn backspace_deletes_the_character_before_the_cursor_not_always_the_last_one() {
        let mut app = App::new(numbered(1));
        app.enter_search();
        for c in "abc".chars() {
            app.push_char(c);
        }
        app.move_cursor_left(); // cursor between 'b' and 'c'

        app.backspace(); // removes 'b'

        assert_eq!(app.query(), "ac");
        assert_eq!(app.query_cursor(), 1);
    }

    #[test]
    fn delete_forward_removes_the_character_at_the_cursor() {
        let mut app = App::new(numbered(1));
        app.enter_search();
        for c in "abc".chars() {
            app.push_char(c);
        }
        app.move_cursor_home();

        app.delete_forward(); // removes 'a'

        assert_eq!(app.query(), "bc");
        assert_eq!(app.query_cursor(), 0);
    }

    #[test]
    fn move_cursor_home_and_end_jump_to_the_respective_edges() {
        let mut app = App::new(numbered(1));
        app.enter_search();
        for c in "abc".chars() {
            app.push_char(c);
        }

        app.move_cursor_home();
        assert_eq!(app.query_cursor(), 0);

        app.move_cursor_end();
        assert_eq!(app.query_cursor(), 3);
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
    fn hidden_count_reflects_the_current_query() {
        let mut app = App::new(vec![
            row("h1", "orange soda", ""),
            agent_row("a1", "orange fruit", ""),
            agent_row("a2", "apple pie", ""),
        ]);
        app.set_viewport_height(10);

        // No query: both agent rows count as hidden.
        assert_eq!(app.hidden_count(), 2);

        for c in "orange".chars() {
            app.push_char(c);
        }
        // Only "orange fruit" (agent) matches "orange"; "apple pie" doesn't.
        assert_eq!(app.hidden_count(), 1);

        app.toggle_agent_filter();
        assert_eq!(app.hidden_count(), 0);
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
    fn modal_push_char_inserts_at_the_cursor_not_always_at_the_end() {
        let mut app = App::new(vec![row("1", "one", "/work/alpha")]);
        app.set_viewport_height(10);
        app.open_new_session_modal();

        app.modal_push_char('a');
        app.modal_push_char('c');
        app.modal_cursor_left(); // cursor between 'a' and 'c'
        app.modal_push_char('b');

        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.input(), "abc");
        assert_eq!(state.cursor(), 2);
    }

    #[test]
    fn modal_delete_forward_removes_the_character_at_the_cursor() {
        let mut app = App::new(vec![row("1", "one", "/work/alpha")]);
        app.set_viewport_height(10);
        app.open_new_session_modal();
        for c in "abc".chars() {
            app.modal_push_char(c);
        }
        app.modal_cursor_home();

        app.modal_delete_forward(); // removes 'a'

        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.input(), "bc");
        assert_eq!(state.cursor(), 0);
    }

    #[test]
    fn modal_cursor_home_and_end_jump_to_the_respective_edges() {
        let mut app = App::new(vec![row("1", "one", "/work/alpha")]);
        app.set_viewport_height(10);
        app.open_new_session_modal();
        for c in "abc".chars() {
            app.modal_push_char(c);
        }

        app.modal_cursor_home();
        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.cursor(), 0);

        app.modal_cursor_end();
        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.cursor(), 3);
    }

    #[test]
    fn modal_cursor_left_right_move_the_text_cursor_independently_of_candidate_selection() {
        let mut app = App::new(vec![
            row("1", "one", "/work/alpha"),
            row("2", "two", "/work/beta"),
        ]);
        app.set_viewport_height(10);
        app.open_new_session_modal();
        for c in "wo".chars() {
            app.modal_push_char(c);
        }
        app.modal_select_next(); // highlight /work/beta
        let target_before = app.modal_new_session_target();

        app.modal_cursor_left();
        app.modal_cursor_left();

        assert_eq!(app.modal_new_session_target(), target_before);
        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.cursor(), 0);
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
        app.modal_delete_forward();
        app.modal_cursor_left();
        app.modal_cursor_right();
        app.modal_cursor_home();
        app.modal_cursor_end();
        app.modal_select_next();
        app.modal_select_prev();
        app.modal_complete_candidate();
        app.modal_set_error("ignored".to_string());
        assert_eq!(app.modal_new_session_target(), None);
        assert!(app.modal().is_none());
    }

    #[test]
    fn modal_complete_candidate_fills_the_input_with_the_highlighted_one() {
        let mut app = App::new(vec![row("1", "one", "/work/alpha")]);
        app.set_viewport_height(10);
        app.open_new_session_modal();
        app.modal_push_char('a');

        app.modal_complete_candidate();

        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.input(), "/work/alpha");
    }

    #[test]
    fn modal_complete_candidate_is_a_noop_when_nothing_matches() {
        let mut app = App::new(vec![row("1", "one", "/work/alpha")]);
        app.set_viewport_height(10);
        app.open_new_session_modal();
        for c in "/nonexistent".chars() {
            app.modal_push_char(c);
        }

        app.modal_complete_candidate();

        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.input(), "/nonexistent");
    }

    #[test]
    fn modal_set_error_is_cleared_by_further_editing() {
        let mut app = App::new(vec![row("1", "one", "/work/alpha")]);
        app.set_viewport_height(10);
        app.open_new_session_modal();

        app.modal_set_error("not a directory".to_string());
        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.error(), Some("not a directory"));

        app.modal_push_char('x');
        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.error(), None);
    }

    #[test]
    fn is_selected_pinned_reflects_the_current_selection() {
        let mut app = App::new(vec![row("a", "Alpha", ""), row("b", "Beta", "")]);
        app.set_viewport_height(10);
        assert!(!app.is_selected_pinned());

        app = app.with_pinned(["a".to_string()].into_iter().collect());
        assert!(app.is_selected_pinned()); // "a" sorted first (pinned)

        app.select_next();
        assert!(!app.is_selected_pinned()); // now on "b", unpinned
    }

    #[test]
    fn is_selected_pinned_is_false_with_nothing_selected() {
        let app = App::new(Vec::new());
        assert!(!app.is_selected_pinned());
    }

    #[test]
    fn hidden_worker_ids_are_excluded_from_filtered_but_still_resolve_by_id() {
        let mut app = App::new(numbered(3)); // id0, id1, id2
        app.set_viewport_height(10);

        app = app.with_hidden_worker_ids(["id1".to_string()].into_iter().collect());

        assert_eq!(ids(&app), vec!["id0", "id2"]);
        // Still resolvable by id (e.g. to stage its brigade), just not listed.
        assert!(app.row_for_id("id1").is_some());
    }

    #[test]
    fn set_hidden_worker_ids_updates_the_filter_after_a_reload() {
        let mut app = App::new(numbered(2)); // id0, id1
        app.set_viewport_height(10);
        assert_eq!(ids(&app), vec!["id0", "id1"]);

        app.set_hidden_worker_ids(["id0".to_string()].into_iter().collect());
        assert_eq!(ids(&app), vec!["id1"]);

        // Un-hiding (e.g. the brigade was disbanded) brings it back.
        app.set_hidden_worker_ids(HashSet::new());
        assert_eq!(ids(&app), vec!["id0", "id1"]);
    }

    #[test]
    fn is_selected_director_reflects_the_current_selection() {
        let mut app = App::new(vec![row("a", "Alpha", ""), row("b", "Beta", "")]);
        app.set_viewport_height(10);
        assert!(!app.is_selected_director());

        app = app.with_directors(["a".to_string()].into_iter().collect());
        assert!(app.is_selected_director());

        app.select_next();
        assert!(!app.is_selected_director()); // now on "b", not a director
    }

    #[test]
    fn visible_reports_director_status_per_row() {
        let mut app = App::new(numbered(3));
        app.set_viewport_height(10);
        app = app.with_directors(["id1".to_string()].into_iter().collect());
        app.toggle_grouped_view(); // flat: directors unrelated to sections

        let director_flags: Vec<bool> = app
            .visible()
            .iter()
            .filter_map(|line| match line {
                ListLine::Row(r) => Some(r.director),
                ListLine::Header { .. } => None,
            })
            .collect();
        assert_eq!(director_flags, vec![false, true, false]);
    }

    #[test]
    fn superseded_sessions_are_hidden_by_default() {
        let mut app = App::new(numbered(3)); // id0, id1, id2
        app.set_viewport_height(10);

        app = app.with_superseded(["id1".to_string()].into_iter().collect());

        assert_eq!(ids(&app), vec!["id0", "id2"]);
        // Still resolvable by id, just not listed.
        assert!(app.row_for_id("id1").is_some());
    }

    #[test]
    fn a_live_superseded_session_stays_visible() {
        let rows = vec![
            row("id0", "title 0", ""),
            SessionRow {
                activity: Activity::Busy,
                ..row("id1", "title 1", "")
            },
        ];
        let mut app = App::new(rows);
        app.set_viewport_height(10);

        app = app.with_superseded(["id1".to_string()].into_iter().collect());

        // A resumed, still-running ancestor must not be hidden — hiding it
        // would lie about what's actually running.
        assert_eq!(ids(&app), vec!["id0", "id1"]);
    }

    #[test]
    fn toggle_agent_filter_reveals_superseded_sessions_too() {
        let mut app = App::new(numbered(2)); // id0, id1
        app.set_viewport_height(10);
        app = app.with_superseded(["id1".to_string()].into_iter().collect());
        assert_eq!(ids(&app), vec!["id0"]);

        app.toggle_agent_filter();
        assert_eq!(ids(&app), vec!["id0", "id1"]);

        app.toggle_agent_filter();
        assert_eq!(ids(&app), vec!["id0"]);
    }

    #[test]
    fn set_superseded_updates_the_filter_after_a_reload() {
        let mut app = App::new(numbered(2)); // id0, id1
        app.set_viewport_height(10);
        assert_eq!(ids(&app), vec!["id0", "id1"]);

        app.set_superseded(["id0".to_string()].into_iter().collect());
        assert_eq!(ids(&app), vec!["id1"]);

        // A newly-resolved lineage link disappearing again (shouldn't
        // happen in practice, but the setter is symmetric) brings it back.
        app.set_superseded(HashSet::new());
        assert_eq!(ids(&app), vec!["id0", "id1"]);
    }

    #[test]
    fn hidden_count_includes_hidden_superseded_sessions() {
        let mut app = App::new(numbered(3)); // id0, id1, id2
        app.set_viewport_height(10);
        app = app.with_superseded(["id1".to_string()].into_iter().collect());

        assert_eq!(app.hidden_count(), 1);
        app.toggle_agent_filter();
        assert_eq!(app.hidden_count(), 0);
    }

    #[test]
    fn is_selected_superseded_reflects_the_current_selection() {
        // "a" is live so marking it superseded doesn't also hide it —
        // keeping the focus purely on `is_selected_superseded` itself
        // rather than the hidden-by-default interaction (covered by
        // `superseded_sessions_are_hidden_by_default`).
        let rows = vec![
            SessionRow {
                activity: Activity::Alive,
                ..row("a", "Alpha", "")
            },
            row("b", "Beta", ""),
        ];
        let mut app = App::new(rows);
        app.set_viewport_height(10);
        assert!(!app.is_selected_superseded());

        app = app.with_superseded(["a".to_string()].into_iter().collect());
        assert!(app.is_selected_superseded());

        app.select_next();
        assert!(!app.is_selected_superseded()); // now on "b"
    }

    #[test]
    fn visible_reports_superseded_status_per_row() {
        let mut app = App::new(numbered(3));
        app.set_viewport_height(10);
        app = app.with_superseded(["id1".to_string()].into_iter().collect());
        app.toggle_agent_filter(); // reveal it so it shows up in `visible()`
        app.toggle_grouped_view(); // flat: unrelated to sections

        let flags: Vec<bool> = app
            .visible()
            .iter()
            .filter_map(|line| match line {
                ListLine::Row(r) => Some(r.superseded),
                ListLine::Header { .. } => None,
            })
            .collect();
        assert_eq!(flags, vec![false, true, false]);
    }

    #[test]
    fn open_confirm_disband_modal_captures_the_brigade() {
        let mut app = App::new(vec![row("dir", "Director", "")]);
        app.set_viewport_height(10);

        app.open_confirm_disband_modal(7, "Director".to_string());

        let Some(Modal::ConfirmDisband { brigade_id, name }) = app.modal() else {
            panic!("expected an open disband-confirm modal");
        };
        assert_eq!(*brigade_id, 7);
        assert_eq!(name, "Director");
    }

    #[test]
    fn close_modal_dismisses_the_disband_confirm() {
        let mut app = App::new(Vec::new());
        app.open_confirm_disband_modal(1, "cell".to_string());
        app.close_modal();
        assert!(app.modal().is_none());
    }

    #[test]
    fn accepts_text_input_truth_table_across_modes_and_modals() {
        // Normal mode, no modal: not accepting text.
        let mut app = App::new(vec![row("a", "Alpha", "")]);
        app.set_viewport_height(10);
        assert!(!app.accepts_text_input());

        // Search mode: accepting.
        app.enter_search();
        assert!(app.accepts_text_input());
        app.exit_search();
        assert!(!app.accepts_text_input());

        // Text-field modals: accepting.
        app.open_new_session_modal();
        assert!(app.accepts_text_input());
        app.close_modal();

        app.open_group_join_modal();
        assert!(app.accepts_text_input());
        app.close_modal();

        // Confirm-only modals: not accepting — they ignore push_char
        // entirely, and their y/n/Enter keys must stay zero-latency.
        app.open_confirm_archive_modal();
        assert!(!app.accepts_text_input());
        app.close_modal();

        app.open_confirm_disband_modal(1, "cell".to_string());
        assert!(!app.accepts_text_input());
        app.close_modal();

        app.open_confirm_kill_modal("sess".to_string(), "Sess".to_string(), false);
        assert!(!app.accepts_text_input());
    }

    #[test]
    fn open_confirm_archive_modal_captures_the_selected_session() {
        let mut app = App::new(vec![row("a", "Alpha", "")]);
        app.set_viewport_height(10);

        app.open_confirm_archive_modal();

        let Some(Modal::ConfirmArchive { session_id, title }) = app.modal() else {
            panic!("expected an open archive-confirm modal");
        };
        assert_eq!(session_id, "a");
        assert_eq!(title, "Alpha");
    }

    #[test]
    fn open_confirm_archive_modal_is_a_noop_with_nothing_selected() {
        let mut app = App::new(Vec::new());
        app.open_confirm_archive_modal();
        assert!(app.modal().is_none());
    }

    #[test]
    fn open_group_join_modal_seeds_existing_groups_and_is_a_noop_with_nothing_selected() {
        let mut app = App::new(vec![row("a", "Alpha", "")]).with_groups(
            vec![(2, "work".to_string()), (1, "play".to_string())],
            HashMap::new(),
        );
        app.set_viewport_height(10);

        app.open_group_join_modal();

        let Some(Modal::GroupJoin(state)) = app.modal() else {
            panic!("expected an open group-join modal");
        };
        // Alphabetical, same as `with_groups` sorts them.
        assert_eq!(state.candidates(), vec!["play", "work"]);
        assert_eq!(state.session_id(), "a");

        app.close_modal();
        app = App::new(Vec::new());
        app.open_group_join_modal();
        assert!(app.modal().is_none());
    }

    #[test]
    fn modal_group_join_target_prefers_existing_group_else_creates_a_new_one() {
        let mut app = App::new(vec![row("a", "Alpha", "")])
            .with_groups(vec![(1, "work".to_string())], HashMap::new());
        app.set_viewport_height(10);
        app.open_group_join_modal();

        for c in "wor".chars() {
            app.modal_push_char(c);
        }
        assert!(matches!(
            app.modal_group_join_target(),
            Some(GroupJoinTarget::Existing(1, _))
        ));

        for _ in "wor".chars() {
            app.modal_backspace();
        }
        for c in "brand-new".chars() {
            app.modal_push_char(c);
        }
        match app.modal_group_join_target() {
            Some(GroupJoinTarget::New(name)) => assert_eq!(name, "brand-new"),
            other => panic!("expected New(\"brand-new\"), got {other:?}"),
        }
    }

    #[test]
    fn set_session_group_cache_updates_map_and_inserts_a_new_group() {
        let mut app = App::new(vec![row("a", "Alpha", "")])
            .with_groups(vec![(1, "work".to_string())], HashMap::new());
        app.set_viewport_height(10);

        // Joining an already-known group doesn't duplicate it.
        app.set_session_group_cache("a", 1, "work".to_string());
        app.open_group_join_modal();
        let Some(Modal::GroupJoin(state)) = app.modal() else {
            panic!("expected an open group-join modal");
        };
        assert_eq!(state.candidates(), vec!["work"]);
        app.close_modal();

        // Joining a brand-new group id adds it to the known-groups cache.
        app.set_session_group_cache("a", 2, "play".to_string());
        app.open_group_join_modal();
        let Some(Modal::GroupJoin(state)) = app.modal() else {
            panic!("expected an open group-join modal");
        };
        assert_eq!(state.candidates(), vec!["play", "work"]);
    }

    #[test]
    fn toggle_grouped_view_reorders_into_pinned_group_ungrouped_sections() {
        // mtime-descending arrival order: id0 newest .. id3 oldest.
        let mut app = App::new(vec![
            row("id0", "zero", ""),
            row("id1", "one", ""),
            row("id2", "two", ""),
            row("id3", "three", ""),
        ])
        .with_pinned(["id3".to_string()].into_iter().collect())
        .with_groups(
            vec![(1, "work".to_string())],
            [("id1".to_string(), 1)].into_iter().collect(),
        );
        app.set_viewport_height(10);

        // Pinned (id3) first, then the "work" group (id1), then Ungrouped
        // (id0, id2) in their original mtime-descending order.
        assert_eq!(ids(&app), vec!["id3", "id1", "id0", "id2"]);
        assert_eq!(headers(&app), vec!["Pinned", "work", "Ungrouped"]);

        let flat = app.toggle_grouped_view();
        assert!(!flat);
        // Flat view: back to the plain pinned-first order (no group section).
        assert_eq!(ids(&app), vec!["id3", "id0", "id1", "id2"]);
        assert!(headers(&app).is_empty());
    }

    #[test]
    fn headers_carry_the_row_count_of_their_section() {
        // Same layout as the test above: Pinned (id3, 1 row), "work" (id1, 1
        // row), Ungrouped (id0 + id2, 2 rows).
        let mut app = App::new(vec![
            row("id0", "zero", ""),
            row("id1", "one", ""),
            row("id2", "two", ""),
            row("id3", "three", ""),
        ])
        .with_pinned(["id3".to_string()].into_iter().collect())
        .with_groups(
            vec![(1, "work".to_string())],
            [("id1".to_string(), 1)].into_iter().collect(),
        );
        app.set_viewport_height(10);

        assert_eq!(
            header_counts(&app),
            vec![
                ("Pinned".to_string(), 1),
                ("work".to_string(), 1),
                ("Ungrouped".to_string(), 2),
            ]
        );
    }

    #[test]
    fn a_header_count_reflects_the_current_agent_filter() {
        // A search query flattens grouped view entirely (see
        // `grouped_view_stays_flat_while_searching`), so the agent filter —
        // which doesn't — is what actually demonstrates a header's count
        // tracking the *filtered* set rather than every row in its section.
        let mut app = App::new(vec![
            row("h1", "Human one", ""),
            row("h2", "Human two", ""),
            agent_row("a1", "Agent one", ""),
        ])
        .with_pinned(["h1".to_string()].into_iter().collect());
        app.set_viewport_height(10);

        // Agents hidden by default: Ungrouped has only h2.
        assert_eq!(
            header_counts(&app),
            vec![("Pinned".to_string(), 1), ("Ungrouped".to_string(), 1)]
        );

        app.toggle_agent_filter();

        // Now a1 joins Ungrouped too.
        assert_eq!(
            header_counts(&app),
            vec![("Pinned".to_string(), 1), ("Ungrouped".to_string(), 2)]
        );
    }

    #[test]
    fn grouped_view_stays_flat_while_searching() {
        let mut app = App::new(vec![row("a", "Alpha", ""), row("b", "Beta", "")]).with_groups(
            vec![(1, "work".to_string())],
            [("a".to_string(), 1)].into_iter().collect(),
        );
        app.set_viewport_height(10);
        assert!(app.grouped_view());

        app.enter_search();
        app.push_char('a'); // matches both "Alpha" and "Beta" via cwd/title? just Alpha here
        // A search never shows section headers, even though grouped_view is on.
        assert!(headers(&app).is_empty());
    }

    #[test]
    fn grouped_view_skips_headers_when_everything_is_in_one_section() {
        // No pins, no groups: everything is "Ungrouped" — a single section,
        // so no header should show at all (avoids showing a lone,
        // meaningless "Ungrouped" banner by default).
        let mut app = App::new(vec![row("a", "Alpha", ""), row("b", "Beta", "")]);
        app.set_viewport_height(10);
        assert!(app.grouped_view());

        assert!(headers(&app).is_empty());
    }

    #[test]
    fn with_groups_sorts_alphabetically_regardless_of_input_order() {
        let mut app = App::new(vec![row("a", "Alpha", "")]).with_groups(
            vec![(3, "zeta".to_string()), (1, "alpha".to_string())],
            HashMap::new(),
        );
        app.set_viewport_height(10);

        app.open_group_join_modal();

        let Some(Modal::GroupJoin(state)) = app.modal() else {
            panic!("expected an open group-join modal");
        };
        assert_eq!(state.candidates(), vec!["alpha", "zeta"]);
    }
}
