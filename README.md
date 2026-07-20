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
- **Phase 4b (groups UI) — pending UX decisions.**

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
| `Enter` | Open the selected session, or focus its pane/tab if already open |
| type any character | Add to the search query |
| `Backspace` | Delete the last query character |
| `Esc` | Clear the query if non-empty, otherwise quit |
| `q` | Quit (only when the query is empty) |
| `p` | Toggle pin on the selected session (only when the query is empty) |
| `Ctrl+C` | Quit (best-effort; inside psmux it does not reach banto — use `q`/`Esc`) |

Mouse: wheel scrolls the list, a single click selects a row, and a quick
second click on the same row (double-click) activates it — same as `Enter`.

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
