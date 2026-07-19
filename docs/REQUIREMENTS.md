# banto — Requirements & Design

A resident TUI tool that manages local Claude Code session history with
Claude-Desktop-like listing and grouping, and resumes a selected session in a
separate pane/tab. Windows-first.

Name origin: 番頭 (bantō) — the head clerk of a traditional Japanese shop, who
stays on the premises and directs and watches over the guests (sessions).

## MVP requirements

- Fast search over local session history (Claude Code CLI only)
- Grouping and pinning stored by banto itself (never writes to Claude's files)
- Click or Enter on a search result resumes the session in a separate pane/tab
- If a session is already resumed, activate its existing pane/tab instead
  (a double resume forks the session history and is therefore forbidden)
- Activity indicator (colored dot) in the list. Busy sessions get special
  treatment; the rest are bucketed by time since last update
- Mouse support including wheel scrolling
- Runs on Windows; keeps a structure that also builds on macOS / Linux
- "Open in new tab vs. new pane" is configurable, default `auto` (see opener)

Out of MVP scope: Claude Desktop (claude.ai) history, other agents (trait only),
built-in PTY, remote/SSH.

## Architecture decision (2026-07-19)

**TUI launcher + external terminal control.** No built-in multiplexer.
banto does no terminal emulation of its own; resuming is delegated to a real
terminal (psmux / Windows Terminal). `banto-core` (lib) and `banto` (bin:
TUI/CLI) are separated so that a future Tauri GUI or a "single-screen
switcher" built-in view (portable-pty + tui-term) can evolve on the same core.

## Data sources (measured 2026-07-19, Claude Code 2.1.215)

| Source | Content |
|---|---|
| `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl` | Session body. One JSON per line |
| First few records of the jsonl | `{"type":"custom-title","customTitle":...}` / `{"type":"ai-title","aiTitle":...}` — the title can be extracted by reading only the head chunk. Older formats fall back to the first user message |
| `~/.claude/sessions/<pid>.json` | Live state of a running session: `pid`, `sessionId`, `cwd`, `status` ("busy" etc.), `kind` ("interactive"/"bg"), `name`, `updatedAt` |
| `~/.claude/history.jsonl` and others | Unused in the MVP |

Note: the format is undocumented and subject to change. Defend with **lenient
parsing** (ignore unknown records/fields, skip broken lines) plus tests against
synthetic fixtures. **Never bring real session data into the repository.**

## Module layout

```
crates/
├─ banto-core/          # UI-free logic (everything testable)
│  ├─ provider/         # SessionProvider trait + claude_code impl (discovery/parsing)
│  ├─ status/           # live state (sessions/<pid>.json + PID liveness + mtime buckets)
│  ├─ store/            # rusqlite: index cache, FTS5, groups/pins, session<->pane map
│  ├─ search/           # nucleo fuzzy search
│  ├─ opener/           # Opener trait + tmux(psmux) / windows-terminal impls + auto detection
│  └─ config/           # config.toml (dirs::config_dir/banto), DB in dirs::data_local_dir/banto
└─ banto/               # bin: ratatui TUI + clap subcommands (banto, banto _wrap, ...)
```

## Opener spec

Priority: **1. psmux (tmux-compatible CLI) = primary target** 2. Windows
Terminal tab 3. future: Ghostty etc.
Auto detection checks environment variables in the order **`$TMUX` →
`WT_SESSION`** (inside psmux both are set, so the order matters).

- psmux/tmux: spawn with `split-window` / `new-window`, tag with
  `select-pane -T`, match with `list-panes -F`, focus with
  `select-window` + `select-pane`.
  psmux confirmed to support all required commands
  ([compatibility.md](https://github.com/psmux/psmux/blob/master/docs/compatibility.md)).
  `swap-pane` works, so a Desktop-like "sidebar + main" switcher is possible.
- Windows Terminal: spawn with `wt -w 0 new-tab`. **There is no API to
  enumerate or focus tabs**, so activating an existing tab is best-effort.
  When reliability is required, a "one session = one window" mode
  (SetForegroundWindow via HWND) is provided as a config option.
- Every backend goes through
  `banto _wrap --session <id> -- claude --resume <id>`, which registers the
  PID, tracks liveness, detects exit, and prevents double resume
  (`wt.exe` detaches immediately, so the wrapper is mandatory).
- Resume always starts in the session's original cwd.

## Activity indicator

1. `sessions/<pid>.json` exists, PID alive, and `status=busy` → **busy**
   (special color, highest priority)
2. PID alive (not busy) → **active** (idle)
3. Otherwise bucket by jsonl mtime: today / this week / older
   (thresholds and colors configurable)

Watch `projects/` and `sessions/` with `notify` for realtime updates.

## Stack

Rust workspace (edition 2024). ratatui + crossterm / nucleo /
rusqlite(bundled)+FTS5 / notify / serde, serde_json / clap / dirs /
sysinfo (PID liveness) / thiserror, anyhow.

## Phases

1. Indexer + search + TUI list (mouse support) — useful on its own — done
2. Opener (psmux / WT) + `_wrap` + double-resume prevention + focus — done
3. Activity dots + notify live updates — done
4. Groups / pins — pins done; groups UI remaining

## Risks

- JSONL format changes → contained by lenient parsing + fixtures
- WT tab focus limitations → window mode as fallback
- psmux-specific incompatibilities (claims tmux 3.3.6 compatibility but is an
  independent implementation) → flush out with spikes and on-device checks
