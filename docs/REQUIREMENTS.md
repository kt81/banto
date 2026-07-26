# banto — Requirements & Design

A resident TUI tool that manages local Claude Code session history with
Claude-Desktop-like listing and grouping, and resumes a selected session —
by default **in-place**, in banto's own terminal, with resuming in a
separate psmux pane / Windows Terminal tab as a first-class alternate.
Windows-first.

Name origin: 番頭 (bantō) — the head clerk of a traditional Japanese shop, who
stays on the premises and directs and watches over the guests (sessions).

## MVP requirements

- Fast search over local session history (Claude Code CLI only)
- Grouping (a session belongs to at most one group), pinning, and archiving
  (soft-hide) — all stored by banto itself, never writing to Claude's files
- Enter on a search result resumes the session **in-place**: banto tears
  down its own TUI (leaves the alt screen, disables raw mode and mouse
  capture), runs the session as a direct child process in the same
  terminal, waits for it to exit, then reinitializes the TUI and returns to
  the (reloaded) list. This is the default and primary action; no terminal
  multiplexer is involved
- `s` resumes the session in a separate psmux pane / Windows Terminal tab
  instead, for users who want a multiplexer layout — see Opener spec
- A dedicated dialog also launches a brand-new session (pick or type a
  working directory), not just resumes an existing one — `n` opens it
  in-place, `N` opens it for a split launch, mirroring Enter/`s` on the list
- If a session is already resumed, refuse to start a second one instead
  (a double resume forks the session history and is therefore forbidden):
  in-place checks liveness up front and shows "already running"; split mode
  activates the existing pane/tab instead
- Activity indicator (colored dot) in the list. Busy sessions get special
  treatment; the rest are bucketed by time since last update
- Mouse support including wheel scrolling
- Runs on Windows; keeps a structure that also builds on macOS / Linux
- The overall default is in-place (`opener = "in-place"`); setting `opener`
  to `"auto"` / `"tmux"` / `"psmux"` / `"windows-terminal"` instead picks
  which split backend `s` uses (see Opener spec)

Out of MVP scope: Claude Desktop (claude.ai) history, other agents (trait only),
built-in PTY, remote/SSH.

## Architecture decision (2026-07-19)

**TUI launcher + external terminal control.** No built-in multiplexer.
banto does no terminal emulation of its own; resuming is delegated to a real
terminal (psmux / Windows Terminal). `banto-core` (lib) and `banto` (bin:
TUI/CLI) are separated so that a future Tauri GUI or a "single-screen
switcher" built-in view (portable-pty + tui-term) can evolve on the same core.

## Architecture decision (2026-07-20): in-place as the default action

**In-place resume is the default (Enter); split-into-a-pane/tab remains a
first-class alternate (`s` / `opener` config), not deprecated.** In-place
still needs no PTY emulation and no multiplexer, consistent with the
2026-07-19 decision above — it's the simplest possible case, banto's own
terminal handed straight to a direct child process. Motivated by psmux's
non-uniqueness of window/pane ids across sessions
(docs/notes/psmux-spike.md) making split-mode targeting inherently more
fragile than just running the session where banto already is.

## Architecture decision (2026-07-26): the chōba is feature-frozen

**The chōba (formerly the "classic" list mode; `banto` with no flags) takes bug fixes
and platform parity from here on, not new capability.** New behavior belongs
in the emporium, which is where the hosted-pane work is going.

"Platform parity" is what admitted the tmux backend above under the freeze:
`s` invoking a `psmux` binary that does not exist on Linux is a mode that
does not work off Windows, not a feature it lacks. The same reading covers
the input-path fix that preceded it. Anything that would make the chōba do
something new — rather than do what it already claims, on a platform where
it currently cannot — is out of scope by default.

## Data sources (measured 2026-07-19, Claude Code 2.1.215)

| Source | Content |
|---|---|
| `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl` | Session body. One JSON per line |
| First few records of the jsonl | `{"type":"custom-title","customTitle":...}` / `{"type":"ai-title","aiTitle":...}` — the title can be extracted by reading only the head chunk. Older formats fall back to the first user message |
| First record of the jsonl | `{"type":"agent-setting",...}` marks a session run by a spawned agent (subagent / Agent-Teams teammate); interactive sessions open with a `mode` record instead. Sets `SessionMeta.is_agent`, persisted in the store (schema v2) |
| `~/.claude/sessions/<pid>.json` | Live state of a running session: `pid`, `sessionId`, `cwd`, `status` ("busy" etc.), `kind` ("interactive"/"bg"), `name`, `updatedAt` |
| `~/.claude/history.jsonl` and others | Unused in the MVP |

**Timing (measured 2026-07-25, Claude Code 2.1.219).** The session-body and
live-state files do not appear together: `sessions/<pid>.json` is written at
**startup**, while the session's `<uuid>.jsonl` is only created at its
**first recorded activity** — a turn, a slash command, a `/rename`. An
untouched session therefore has an id and no history file at all,
indefinitely (observed: a Worker sitting at its prompt for minutes with no
jsonl anywhere).

That is why id discovery for a freshly-spawned session (emporium brigade
Workers) matches `sessions/<pid>.json` by the pid banto itself spawned, and
falls back to scanning session files only when the direct child isn't
`claude` itself. Waiting on the jsonl alone deadlocks: an unidentified Worker
is invisible to the relay engine, so it can never be nudged into the first
turn that would create the very file discovery is waiting for.

Note: the format is undocumented and subject to change. Defend with **lenient
parsing** (ignore unknown records/fields, skip broken lines) plus tests against
synthetic fixtures. **Never bring real session data into the repository.**

## Module layout

```
crates/
├─ banto-core/          # UI-free logic (everything testable)
│  ├─ provider/         # SessionProvider trait + claude_code impl (discovery/parsing)
│  ├─ status/           # live state (sessions/<pid>.json + PID liveness + mtime buckets)
│  ├─ store/            # rusqlite: groups/pins/archived, brigades, session<->pane map
│  ├─ search/           # nucleo fuzzy search
│  ├─ opener/           # Opener trait + tmux(psmux) / windows-terminal impls + auto detection
│  └─ config/           # config.toml (--config/BANTO_CONFIG/XDG/~/.config/dirs::config_dir), DB in dirs::data_local_dir/banto
└─ banto/               # bin: ratatui TUI + clap subcommands (banto, banto _wrap, ...)
```

## Opener spec

Two actions, mirrored by a TUI key (Enter = in-place, `s` = split) and by
`opener` in `config.toml` (default `"in-place"`; `"auto"` / `"tmux"` /
`"psmux"` / `"windows-terminal"` pick a split backend instead — the exact `s`-vs-`opener`
interaction when `opener` is left at its `"in-place"` default is an
implementation detail for the split-mode work, not fixed by this doc):

### In-place (default)

banto hands its own terminal to the session directly: tear down the TUI
(leave the alt screen, disable raw mode and mouse capture), run
`claude --resume <id>` (or plain `claude` in the target cwd for a new
session) as a direct child process, wait for it to exit, then reinitialize
ratatui and reload the list. No multiplexer, no pane/tab, no `_wrap`
wrapper — banto is already the direct parent and observes the exit itself.
Before spawning, the same liveness check `status` uses elsewhere (PID
alive?) guards against double-resume: if the session is already running
somewhere, refuse and show "already running" rather than forking its
history. Resume always starts in the session's original cwd.

### Split into a pane/tab (`s`)

Not deprecated — fully supported for users who want a multiplexer layout.
Priority: **1. psmux (tmux-compatible CLI) = primary target** 2. Windows
Terminal tab 3. future: Ghostty etc.
Auto detection (`opener = "auto"`) checks environment variables in the
order **`$TMUX` → `WT_SESSION`** (inside psmux both are set, so the order
matters). `$TMUX` says only *that* a multiplexer is hosting us, never which
one — it holds a socket path — so the platform resolves it: `psmux` on
Windows, real `tmux` everywhere else. That guess is wrong only for a
deliberately exotic install, which is what the explicit `opener = "psmux"` /
`"tmux"` values are for.

The two are not interchangeable behind one binary name. Measured against
tmux 3.6 on 2026-07-26 (docs/notes/psmux-spike.md records both sides): the
session-qualified pane target psmux *requires* — because it reuses window
and pane ids across sessions — is **rejected** by tmux, which reads
`<session>:<pane_id>` as "window `<pane_id>` of that session"
(`can't find window: %1`). tmux wants the bare, globally-unique
`<pane_id>`, which is in turn ambiguous on psmux. Each form is wrong on the
other CLI, so the flavor is carried explicitly
(`banto_io::opener::TmuxFlavor`), never inferred at call time.

- psmux/tmux: spawn with `split-window` / `new-window`, tag with a
  session-qualified `select-pane -t '<session>:<pane_id>' -T <title>`,
  match with `list-panes -F`, focus with a session-qualified
  `select-pane -t '<session>:<pane_id>'` alone.
  psmux confirmed to support all required commands
  ([compatibility.md](https://github.com/psmux/psmux/blob/master/docs/compatibility.md)),
  but — unlike real tmux — it reuses window/pane ids across sessions, so
  every target must be session-qualified (docs/notes/psmux-spike.md,
  2026-07-20). That spike also found `select-window -t 'session:@window_id'`
  fails outright and `switch-client` corrupted the live server badly enough
  to destroy a session, so neither is used; focus is a lone session-qualified
  `select-pane` (banto's own panes are splits within banto's own session, so
  no window/client switch is needed to surface one).
  `swap-pane` works, so a Desktop-like "sidebar + main" switcher is possible.
- Windows Terminal: spawn with `wt -w 0 new-tab`. **There is no API to
  enumerate or focus tabs**, so activating an existing tab is best-effort.
  When reliability is required, a "one session = one window" mode
  (SetForegroundWindow via HWND) is provided as a config option.
- Every split backend goes through
  `banto _wrap --session <id> -- claude --resume <id>`, which registers the
  PID, tracks liveness, detects exit, and prevents double resume
  (`wt.exe` detaches immediately and psmux panes run detached from banto's
  own process, so the wrapper is mandatory here — in-place needs none of
  this, see above).
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
rusqlite(bundled) / notify / serde, serde_json / clap / dirs /
sysinfo (PID liveness) / thiserror, anyhow.

## Phases

1. Indexer + search + TUI list (mouse support) — useful on its own — done
2. Opener (psmux / WT) + `_wrap` + double-resume prevention + focus — done
3. Activity dots + notify live updates — done
4. Groups / pins — done
5. In-place resume as the default action (Enter hands off banto's own
   terminal directly; `s` still splits into a psmux pane / WT tab) — in
   progress

Delivered alongside groups: a new-session modal (`n`), session archiving
(`d`, soft-hide only — the real jsonl file under `~/.claude` is never
touched), and an always-visible summary panel below the list.

## Risks

- JSONL format changes → contained by lenient parsing + fixtures
- WT tab focus limitations → window mode as fallback
- psmux-specific incompatibilities (claims tmux 3.3.6 compatibility but is an
  independent implementation) → flush out with spikes and on-device checks.
  Confirmed so far (docs/notes/psmux-spike.md): non-unique window/pane ids
  across sessions, and `switch-client` corrupting the live server — both
  are why split-mode targeting is session-qualified `select-pane` only,
  never `select-window` or `switch-client`. This non-uniqueness was also
  the motivation for making in-place the default (2026-07-20 decision
  above): it sidesteps split-target ambiguity entirely for the common case
