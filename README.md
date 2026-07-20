# banto (番頭)

A resident TUI that searches and groups your local Claude Code session
history and resumes a selected session into a separate psmux pane or
Windows Terminal tab.

*Name origin: 番頭 (bantō) — the head clerk of a traditional Japanese shop,
who stays on the premises and directs and watches over the guests
(sessions).*

## What it does

banto indexes the session files Claude Code CLI already writes under
`~/.claude`, gives you a fast, Claude-Desktop-like fuzzy-searchable list with
your own grouping and pinning on top, and resumes whatever you pick into a
real terminal pane or tab — so you can jump back into any past conversation
without hunting through directories or losing your place. If a session is
already resumed somewhere, banto activates that existing pane/tab instead of
opening a second one, since resuming the same session twice forks its
history.

## Status

- **Phase 1 — done:** session discovery + fuzzy search + the TUI list
  (activity dots, mouse support including wheel scrolling).
- **Phase 2 — done:** the opener (psmux / Windows Terminal), the
  `banto _wrap` resume wrapper, and double-resume prevention — Enter or a
  double-click opens a session, or focuses its pane/tab if it's already open.
- **Phase 3 — done:** live updates. A `notify`-backed watch of
  `projects/`/`sessions/`, debounced, reloads the list automatically.
- **Phase 4a (pins) — done:** `p` toggles a pin; pinned sessions sort first
  (when not searching) and carry a `*` marker.
- **Phase 4b (groups UI) — done:** `g` opens a group-join dialog (pick an
  existing group or type a new name); a session belongs to at most one
  group. `Tab` toggles a grouped list view (Pinned / each group / Ungrouped).
  Delivered alongside it: an `n` new-session dialog, `d` session archiving
  (soft-hide only), and an always-visible summary panel below the list.

See [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) for the full design and
phase breakdown.

## Read-only guarantee

**banto never writes anything under `~/.claude`.** Everything Claude Code
owns — session `.jsonl` files, live-state files, history — is treated as
strictly read-only. banto's own data (config, pins, groups, its search
cache) lives entirely under its own directories:

- Config: `dirs::config_dir()/banto/config.toml`
- Data (sqlite cache): `dirs::data_local_dir()/banto/banto.db`

On Windows that's `%APPDATA%\banto\` and `%LOCALAPPDATA%\banto\`; on Linux,
`~/.config/banto/` and `~/.local/share/banto/`.

## Install & run

Requires a recent stable Rust toolchain (edition 2024).

```sh
cargo build

# Launch the TUI (default action)
cargo run --

# Print all sessions as plain text instead (newest first, one per line)
cargo run -- list

# Point at a different Claude home instead of the default ~/.claude
cargo run -- --claude-home /path/to/.claude
```

`--claude-home` takes priority over `config.toml`'s `claude_home` key, which
in turn takes priority over the `~/.claude` default.

### TUI keybindings

| Key | Action |
|---|---|
| `Up` / `Down` | Move selection |
| `PgUp` / `PgDn` | Move selection by one page |
| `Home` / `End` | Jump to first / last session |
| `Enter` | Open the selected session, or focus its pane/tab if already open (confirms an open modal instead — see [Modals](#modals)) |
| `/` | Enter search mode (only when the query is empty) |
| type any character | Add to the search query |
| `Backspace` | Delete the last query character |
| `Esc` | Clear the query if non-empty, otherwise quit (closes an open modal instead, without acting) |
| `q` | Quit (only when the query is empty) |
| `p` | Toggle pin on the selected session (only when the query is empty) |
| `a` | Toggle showing agent-run sessions (only when the query is empty) |
| `n` | Open the new-session dialog (only when the query is empty) — see [Modals](#modals) |
| `d` | Open the archive-confirm dialog for the selected session (only when the query is empty) — see [Modals](#modals) |
| `g` | Open the group-join dialog for the selected session (only when the query is empty) — see [Modals](#modals) |
| `Tab` | Toggle grouped list view (only when the query is empty) — see [Grouped view](#grouped-view); completes the highlighted candidate inside the new-session dialog |
| `Ctrl+C` | Quit (best-effort; inside psmux it does not reach banto — use `q`/`Esc`) |

Mouse: wheel scrolls the list, a single click selects a row, and a quick
second click on the same row (double-click) activates it — same as `Enter`.

### Modals

Three dialogs, opened from Normal mode, take over input until confirmed with
`Enter` or cancelled with `Esc`:

- **`n` — New session.** Type or pick a previously seen working directory
  (from loaded sessions, most-recently-used first) and `Enter` launches a
  fresh `claude` there — not a resume of an existing session. `Tab`
  completes the highlighted candidate into the input.
- **`d` — Archive.** `Enter` confirms. Archiving only hides the session from
  banto's own list (via banto's own sqlite store); the session file under
  `~/.claude` is never touched. There's currently no keybinding to
  unarchive it — that's API-only.
- **`g` — Join group.** Type a new group name, or pick an existing one from
  the filtered list below the input; `Enter` joins/creates it. A session
  belongs to at most one group — joining moves it out of whichever group it
  was in before.

### Grouped view

By default the list is grouped into sections — **Pinned**, then each group
alphabetically, then **Ungrouped** — with a header line above each. `Tab`
toggles it off (a flat, unsectioned list). Grouping is skipped automatically
(shown flat) while searching, and when every session falls into a single
section, since a lone header wouldn't add anything.

### Summary panel

Below the list, an always-visible "Details" panel shows the selected
session's activity dot + title, a one-line preview of its first message, its
working directory, and a meta line (relative age, size, short id, and
pinned/agent markers). It's hidden in a short terminal (under 12 rows) to
leave room for the list.

### Configuration

`config.toml` is optional — a missing file just means all defaults, and a
broken one falls back to defaults rather than blocking startup. Recognized
keys (see `crates/banto-core/src/config`):

```toml
# Which backend resumes sessions into panes/tabs: "auto" (default), "psmux",
# or "windows-terminal". Auto detects from $TMUX / $WT_SESSION.
opener = "auto"

# Overrides the default ~/.claude location.
claude_home = "C:/Users/you/.claude"

# Overrides the default sqlite cache path.
db_path = "C:/Users/you/AppData/Local/banto/banto.db"

[activity]
# Hours / days after which a session drops from "today" to "this week" to
# "older" in the activity bucketing (defaults: 24 and 7).
today_hours = 24
week_days = 7
```

## Layout

Two-crate Rust workspace:

```
crates/
├─ banto-core/   # UI-free logic (everything unit-tested)
│  ├─ provider/  # SessionProvider trait + Claude Code discovery/parsing
│  ├─ status/    # live state (sessions/<pid>.json + PID liveness + age buckets)
│  ├─ store/     # rusqlite: index cache, FTS5, groups/pins, session<->pane map
│  ├─ search/    # nucleo fuzzy search
│  ├─ opener/    # Opener trait + psmux(tmux) / Windows Terminal + auto detection
│  ├─ watch/     # notify-backed filesystem watching + debounce
│  └─ config/    # config.toml + default paths
└─ banto/        # bin: ratatui TUI + clap subcommands (banto, banto list, ...)
```

`banto-core` has no UI dependencies, so a future Tauri GUI or an in-process
"single-screen switcher" view can reuse it unchanged.

A rough sketch of the TUI list (activity dot, title, cwd):

```
 banto ── search: fix login ──────────────────────────
 ● busy   Fix the login redirect loop      ~/work/app
 ○ alive  Refactor session provider tests  ~/work/banto
 · today  Investigate flaky CI             ~/work/app
 · week   Draft README                     ~/work/banto
```

## Further reading

- [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) — full design/requirements
  document, including the opener spec and activity-indicator rules.
- [docs/notes/psmux-spike.md](docs/notes/psmux-spike.md) — on-device
  verification notes for the psmux write commands the opener relies on.
