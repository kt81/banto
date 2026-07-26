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
remote/SSH. ("Built-in PTY" was also listed here originally — superseded
2026-07-22 by the emporium mode; see the architecture decision below and
"Emporium mode" further down. The MVP itself was never revised to include
it: the emporium is later, additional scope layered on top, not a rewrite
of what "MVP" meant at the time this list was written.)

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

## Architecture decision (2026-07-22): the emporium — banto becomes a multiplexer

**An evolution of the 2026-07-19 decision above, not a reversal of it.** That
entry stays exactly as written above: it records what banto deliberately
started as. This entry records the later, deliberate decision to become a
multiplexer after all, once in-place resume (2026-07-20) had proven out the
underlying mechanism on the simplest possible case.

The question was posed precisely by the 2026-07-22 spike
(`docs/notes/embedded-pty-spike.md`): can banto host a real, interactive
`claude` session *inside* its own ratatui TUI — spawn the child in a PTY,
parse its output with a VT emulator, render the grid, forward input — "the
general case of which in-place mode is the N=1 degenerate form"? Verdict:
viable on Windows, full fidelity (colors, boxed panels, wide/CJK glyphs,
multibyte input), one ConPTY-specific gotcha found and resolved (never
answer the outer terminal's own DSR/DA queries — that traffic belongs to
ConPTY probing banto's real terminal, not the hosted child; answering it
leaks garbage into the child's stdin).

This is the **emporium** mode (大店, *oodana* — `banto --emporium` / `--oodana`):
a persistent sidebar plus one or more sessions hosted as live embedded
panes, up to and including brigade Director/Worker multi-session cells that
banto itself wires together over its own MCP server and keeps talking via
an auto-relay (see "Emporium mode", "Brigade (Director/Worker cells)", "MCP
mediation server", and "Auto-relay" below). The chōba is unaffected and
stays in-place-first per the 2026-07-20 decision; the emporium is a
separate, additional top-level mode (`--emporium`), not a replacement for
it — see the 2026-07-26 decision below, which freezes the chōba precisely
because new capability now belongs here instead.

Formalized architecturally as `docs/DISCIPLINE.md`'s TEA / sans-IO
discipline (adopted 2026-07-24): the emporium's event loop *is*
`engine::update`, a pure function from `Event`s to `State` and `Cmd`s
(`crates/banto/src/embedded/emporium.rs`'s own module doc). The resulting
four-crate physical split (`banto-core` / `banto-io` / `banto-tui` / `banto`)
completed 2026-07-25 (`docs/DISCIPLINE.md` §2's own status marker).

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
| `~/.claude/sessions/<pid>.json` | Live state of a running session: `pid`, `sessionId`, `cwd`, `status` ("busy" etc.), `kind` ("interactive"/"bg"), `name`, `updatedAt` — observed present on both Windows and Linux/WSL. `procStart` (the process's own kernel-reported start time, matching `/proc/<pid>/stat`'s `starttime` field) is Linux/WSL-only — absent on Windows — so `RawLiveSession` carries it as optional, falling back to a bare pid-liveness check without it, to tell a still-alive pid from one the OS has since recycled since a stale live-state file was written. (Observed directly on this machine's installed Claude Code version; not claimed for every version or platform.) |
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

Four crates, split along the TEA / sans-IO boundary
(`docs/DISCIPLINE.md`) — `banto-core` is UI-free *and* I/O-free; the
provider/status/store/opener/config modules once sketched here as
`banto-core` submodules live in `banto-io`, not there:

```
crates/
├─ banto-core/          # pure: Event -> State + Cmd (TEA/sans-IO), no I/O — app/engine/model/status/screen/search/replay
├─ banto-io/            # the outside world: everything that touches a filesystem, spawns a process, or talks to sqlite
│  ├─ provider/         # SessionProvider trait + claude_code impl (discovery/parsing)
│  ├─ status/           # live state (sessions/<pid>.json + PID liveness)
│  ├─ store/            # rusqlite: groups/pins/archived, brigades, session<->pane map
│  ├─ opener/           # Opener trait + tmux(psmux) / windows-terminal impls + auto detection
│  ├─ watch/            # filesystem watching (notify) for live TUI updates
│  ├─ claude_home.rs    # the Claude Code home root + its projects/sessions subdirs
│  ├─ lineage.rs        # auto-compaction parent-link resolution
│  ├─ pty.rs            # PTY host abstraction (portable-pty)
│  ├─ process.rs        # resumed-session process spawning
│  └─ config.rs         # config.toml (--config/BANTO_CONFIG/XDG/~/.config/dirs::config_dir), DB in dirs::data_local_dir/banto
├─ banto-tui/           # rendering from &State (ratatui, no terminal backend) — view/render_modal
└─ banto/               # bin: ratatui TUI + clap subcommands (banto, banto _wrap, banto _mcp, ...) — chōba's tui.rs + the emporium's embedded/
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

## Emporium mode

Introduced by the 2026-07-22 architecture decision above. `banto --emporium`
(alias `--oodana`) opens a persistent left sidebar (the same session list) plus
a right pane hosting the selected session **embedded** — the child's PTY is
spawned and read by banto itself (`portable-pty`), its output parsed with a VT
emulator (`vt100`) into a `Screen` the core owns, and rendered into a ratatui
pane. This differs from both chōba resume paths, which either hand banto's own
terminal to the child directly (in-place) or spawn it in a separate
psmux/Windows-Terminal pane/tab banto does not render (split): the emporium is
the only mode where banto itself is the terminal the session's output actually
paints into.

Launch dispatch: `crates/banto/src/main.rs` — `--emporium` selects
`embedded::run_emporium`, otherwise the chōba's `tui::run`. The two share
`App` (list state), the `view` renderers, the store-load helpers, and
`render_modal`, but have separate event loops (`crates/banto/src/embedded/`
vs `crates/banto/src/tui.rs`).

**Architecture.** The emporium's event loop is a thin shell around
`engine::update` (`crates/banto-core/src/engine.rs`) — see the TEA / sans-IO
discipline (`docs/DISCIPLINE.md`), formalized 2026-07-24. The shell gathers
facts about the outside world into `Event`s, calls the pure `update`, and
executes the `Cmd`s it returns (process spawning, PTY I/O, store reads/writes);
none of the *decisions* live in the shell. `BANTO_RECORD_EVENTS=<path>`
captures every `Event` fed into `update` as a replay stream (`docs/DISCIPLINE.md`
§8) — a captured file contains real session content and must never be
committed.

**Layout.** A solo session fills the whole pane. A staged brigade (see
"Brigade" below) tiles Director left / Workers stacked down the right —
"master + stack".

**Keys** (emporium; everything the chōba has except split, plus):

| Key | Action |
|---|---|
| `Enter` | Open embedded; on a Director, stage its whole cell |
| `B` | Appoint Director + auto-spawn Workers (on a Director: disband) |
| `b` | Spawn one more Worker into the staged cell |
| prefix chord (`Ctrl+B` default, `[keys] prefix` in config.toml), from sidebar or pane, then: | |
| … `o`/`Tab` | cycle focus through sidebar and panes |
| … arrows | directional pane navigation |
| … `1`-`9` | jump to pane N |
| … `s`/`Esc` | back to the sidebar |
| … `x` | kill the focused pane (confirm) |
| … `b`/`Ctrl+B` | send a literal prefix chord through to the child |

While the prefix is armed, the status bar shows the full binding table.
Multiline paste and file drag&drop into panes are synthesized into bracketed
pastes host-side, since the Windows console never reports pastes as pastes.

**ConPTY caveats** (`docs/notes/embedded-pty-spike.md`, 2026-07-22 spike plus
2026-07-24 dogfooding, all on Windows): never answer the outer terminal's own
DSR/DA queries (that traffic belongs to ConPTY probing banto's real terminal,
not the hosted child — answering leaks garbage into the child's stdin and
corrupts its repaint on resume); a chunk boundary can carry meaning (the
relay's nudge Enter is sent ~300ms after the nudge text rather than
back-to-back, see "Auto-relay" below); a child's exit produces no EOF on some
paths, so an active waiter thread is needed rather than relying on read
returning empty. The spike's own "not yet verified" list, stated plainly
rather than glossed over: mouse-forwarding into children, resize-under-stress,
scrollback, and non-Windows behavior beyond one dated Unix teardown follow-up
(`docs/notes/embedded-pty-spike.md`, 2026-07-25 addendum — a measured
comparison of graceful vs. force-kill teardown timing on that platform).

## Brigade (Director/Worker cells)

A brigade is an internal operational cell of one Director session and one or
more Worker sessions, hosted together as tiled panes in the emporium
(`crates/banto-io/src/store/migrations.rs` v4 migration comment). It is a
separate concept from groups (the user's own project/phase filing): a brigade
is a live operational unit, not a filing category. A session belongs to at
most one brigade, and a brigade has exactly one Director — both are "layered
in code, not a schema constraint" (same comment, verbatim).

**Formation.** `B` on a selected session appoints it Director and auto-spawns
`workers` (config, default 1, clamped 1..=8) fresh Workers beside it. `b` spawns
one more Worker into the currently-staged brigade. `B` on an existing Director
opens a disband confirmation instead (Workers cannot be promoted to Director
directly).

**Member identity.** Each member gets a banto-owned `member_token`
(`"director"`, `"worker-1"`, `"worker-2"`, ...) rather than being keyed by its
Claude session id — a Worker is formed by banto *before* Claude assigns it a
session id (it's auto-spawned), so the id has to be a nullable, filled-in-later
column rather than the primary identity
(`crates/banto-io/src/store/migrations.rs` v7 migration comment). The token is
stable for the member's lifetime in the brigade; its Claude session id is not
(unknown until discovered, never reused across brigades).

**Lifecycle.** Killing a Worker's pane (prefix-`x`) lets it respawn fresh under
the same token next time its brigade is staged; *dismissing* one (a separate
choice on the same confirm dialog) removes it from the brigade for good —
membership, message cursor, and any mail addressed specifically to it, all
gone. Disbanding (`B` on a Director) removes the whole cell.

**Config** (`crates/banto-core/src/config.rs`, `[brigade]` in config.toml):

```rust
pub struct BrigadeConfig {
    pub workers: u32,          // auto-spawned per cell, clamped 1..=8, default 1
    pub worker_model: String,  // --model for spawned Workers; "" = inherit operator default; default "sonnet"
    pub relay: RelayMode,      // Auto | Manual, default Auto — see "Auto-relay" below
    pub director_prompt: String, // --append-system-prompt template for the Director
    pub worker_prompt: String,   // --append-system-prompt template for each Worker
}
```

Each member is launched with a role briefing (`--append-system-prompt`)
substituting `{brigade}` (the brigade id), `{token}` (this member's own
token), and `{peers}` (a comma-joined list of its addressable peers); an empty
template launches with no briefing at all. This is deliberately a *setting*,
not a hardcoded constant: without one, a cell exists only in banto's data
model and the operator's own screen — a Director handed three MCP tool names
and no context about why mostly never uses them (`crates/banto-core/src/config.rs`
doc comments). The shipped defaults tell a Director to delegate independent,
parallelizable work to its Workers via `send_to_peer` and keep genuinely
sequential work itself; that policy is intentionally a setting the operator
can change, not a fixed behavior.

## MCP mediation server

An embedded `claude` session is launched with `claude --mcp-config <file>`
pointing at `banto _mcp --brigade <id> --member <token> --role <role>
[--session <id>]`; Claude Code spawns that as a stdio MCP server and speaks
JSON-RPC 2.0 to it (newline-delimited, no Content-Length framing — requests
carry an `id` and get a response, notifications don't). Because banto controls
the launch argv, the config file lives under banto's own data directory
(`dirs::data_local_dir()/banto/mcp/<brigade_id>-<token>.json`) and is never
installed into Claude Code's own configuration (`crates/banto/src/mcp.rs`
module doc; `crates/banto/src/embedded/emporium.rs`'s `write_mcp_config`).

The server shares banto's own sqlite store with the TUI process and exposes
three tools:
- `send_to_peer(text[, to])` — enqueues a message to the opposite role in the
  brigade (Director → every Worker, or Worker → Director); `to` narrows a
  broadcast to one specific member token.
- `check_messages()` — pulls the messages addressed to this session's role
  that it hasn't seen yet, wrapped in framing that names them as relayed from
  another AI rather than a direct operator instruction.
- `brigade_status()` — this member's own identity plus a roster of its
  addressable peers, each with what it's doing right now and whether it's
  holding unread mail from this member. Replaces an earlier bare ping-style
  health check, added once dogfooding showed a Director launched with only
  the two message tools and no roster information mostly never used them.

Delivery is a pull, never a stdin injection: even though the embedded banto is
the sole writer to a child's stdin, injecting a peer's message there would
forge operator input mid-turn; a tool result respects turn boundaries and
carries the firewall framing for free (`crates/banto/src/mcp.rs` module doc).

**Verified end to end against real Claude Code** (`docs/notes/mcp-spike.md`,
2026-07-23, follow-up 2026-07-25): `claude --strict-mcp-config --mcp-config
<file> --allowedTools "mcp__banto__banto_ping" -p "..."` round-tripped a real
handshake, tool list, and tool call against `banto _mcp` launched exactly as
production code launches it. The spike's own stated gaps, not glossed over:
multiple concurrent `_mcp` servers under real sqlite contention, and
non-Windows, were both explicitly marked "not yet verified" there. The
2026-07-25 follow-up is what motivated the role-briefing mechanism above and
the ping-to-`brigade_status` rename: a full day of dogfooding produced zero
Director-initiated messages, traced to a member having no idea a brigade
existed in its own context at all — an information gap, not a reliability one.

## Auto-relay

A Director↔Worker exchange over the MCP tools above is pull-based by design
(see above), which means a member sitting idle with unread mail will never
notice unless something nudges it. The auto-relay closes that loop: it
observes each staged brigade member's idle/busy status (the same
`sessions/<pid>.json` live-state read the Activity indicator uses) and unseen
message count, and once a member has been idle for `RELAY_IDLE_STREAK_REQUIRED`
consecutive observation ticks with mail waiting, types a fixed line into its
stdin — `"[banto relay] Your brigade peer sent you a message. Call the
check_messages tool now."` — followed by a submitting Enter roughly 300ms
later (`RELAY_SUBMIT_DELAY`; the delay exists because a chunk boundary can
carry meaning for the embedded PTY — see "Emporium mode" above). A nudge is
suppressed while the member's pane is focused and has just received forwarded
keystrokes (so the operator's own typing is never interrupted), is subject to
a cooldown between repeat nudges to the same member, and gives up after a
capped number of attempts on one unseen batch
(`crates/banto-core/src/engine.rs` relay constants).

`[brigade].relay` in config.toml (`RelayMode`, default `Auto`) toggles this
off (`Manual`) for an operator who would rather prompt `check_messages`
themselves.

## Activity indicator

1. `sessions/<pid>.json` exists, PID alive, and `status=busy` → **busy**
   (special color, highest priority)
2. PID alive (not busy) → **active** (idle)
3. Otherwise bucket by jsonl mtime: today / this week / older
   (thresholds and colors configurable)

Watch `projects/` and `sessions/` with `notify` for realtime updates.

## Stack

Rust workspace (edition 2024). ratatui + ratatui-core + crossterm / nucleo /
rusqlite(bundled) / notify / vt100 (VT emulation for the emporium's embedded
panes) / portable-pty (the embedded PTY host) / unicode-width / serde,
serde_json, toml / clap / dirs / sysinfo (PID liveness) / thiserror, anyhow.
Unix builds additionally depend on `libc`.

## Phases

1. Indexer + search + TUI list (mouse support) — useful on its own — done
2. Opener (psmux / WT) + `_wrap` + double-resume prevention + focus — done
3. Activity dots + notify live updates — done
4. Groups / pins — done
5. In-place resume as the default action (Enter hands off banto's own
   terminal directly; `s` still splits into a psmux pane / WT tab) — done
6. Emporium mode (2026-07-22 decision above): embedded multiplexer, brigade
   Director/Worker cells, MCP mediation, auto-relay (see "Emporium mode" /
   "Brigade" / "MCP mediation server" / "Auto-relay" above) — done. Its
   architecture was formalized separately as `docs/DISCIPLINE.md`'s TEA /
   sans-IO discipline — a five-phase migration (Phase 0 through Phase 4;
   Phases 2 and 3 each landed as two commits), all completed 2026-07-24
   through 2026-07-25 (`docs/DISCIPLINE.md` §10) — done.

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
