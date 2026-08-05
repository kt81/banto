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

## Codex data sources (observed 2026-07-26, this machine's installed Codex CLI)

Nothing like Claude Code's layout: no `projects/`, no per-session `.jsonl`
directory tree banto walks itself. One sqlite database, one table.

| Source | Content |
|---|---|
| `$CODEX_HOME` (else `~/.codex`)`/state_5.sqlite`, `threads` table | One row per session. Columns banto reads: `id`, `title`, `cwd`, `rollout_path`, `first_user_message`, `updated_at_ms`. The filename (`state_5.sqlite`) is what this machine's installed version uses; a future Codex CLI could rename it |
| `threads.rollout_path` | The session transcript file itself, under a `sessions/` tree whose internal (date-partitioned) layout banto does not rely on — the path comes from the column directly, never from walking the tree |
| `threads.cwd` | Carries a Windows extended-length path prefix (`\\?\`) that a rollout file's own recorded cwd does not — normalized once, in `provider::codex`, at the point this becomes `SessionMeta.cwd` |
| `threads.updated_at_ms` | Milliseconds since the Unix epoch, application-reported by Codex — not a filesystem mtime the way Claude Code's is. `SessionMeta.mtime` says so |
| `$CODEX_HOME/logs_2.sqlite`, `logs` table | Process liveness (not read by discovery — the Codex-side double-resume guard, `codex_liveness::is_thread_alive`). Real schema, read from a real, live `~/.codex/logs_2.sqlite` on this machine (2026-07-27): `id, ts, ts_nanos, level, target, thread_id, process_uuid, ...`, with `CREATE INDEX idx_logs_thread_id_ts ON logs(thread_id, ts DESC, ts_nanos DESC, id DESC)` — the newest row per `thread_id` is exactly what that index is for |
| `logs.process_uuid` | `pid:<PID>:<suffix>`. Confirmed against that same real database: the newest row's pid for the session actually running matched a real, live process (cross-checked via `Win32_Process`); three finished sessions' newest rows named three dead pids |
| `logs.ts` | Unix **seconds** (confirmed by comparing a real row's value against the current clock — not milliseconds, unlike `threads.updated_at_ms`). Compared against `sysinfo::Process::start_time()` (also unix seconds, and available on every platform `sysinfo` supports — unlike Claude's Linux/WSL-only `/proc` ticks comparison) for the pid-recycling guard: a process that started strictly after this log row was written cannot be the one that wrote it |

**Reading a database Codex itself may be writing (measured directly against
both `state_5.sqlite` and, independently, `logs_2.sqlite` — including a read
against the real, live `logs_2.sqlite` on this machine while it was warm —
rather than assuming one database's answer transferred to the other;
rusqlite 0.40.1 bundled, the version this workspace pins).** Two open forms,
both able to read correctly, neither always safe alone:
- `mode=ro` is always correct — it sees every committed row, including a
  writer's in-flight commits — and on its own writes nothing, but opening it
  against a **cold** database (no `-wal`/`-shm` sidecars yet) creates those
  sidecars on first touch: a write into a directory banto promises never to
  write to.
- `immutable=1` never creates a sidecar, but read against an **actively
  written** database it can silently return a stale or incomplete snapshot —
  wrong, not just old.

The resolution: stat for the `-wal` sidecar first. Present -> `mode=ro`
(warm, correct, no new sidecar since one already exists). Absent ->
`immutable=1` (cold, correct, no sidecar created). This is correct in every
case and zero-write in every case but one: **crash residue** — `-wal`
present, its `-shm` sidecar absent (e.g. Codex was killed mid-write) — where
`mode=ro` recreates a fresh `-shm` on that first read, a single ~32KB write.
It is SQLite's own coordination index, not banto's data, and Codex's own
next run would create it regardless, but it is a write, so the README's
read-only section names it rather than smoothing it over.

One more edge, benign: a **TOCTOU** gap between the stat and the open — a
database that was cold a moment ago gets a writer between the two, and this
approach opens it `immutable=1` anyway, returning the pre-write snapshot
rather than an error. Stale for one poll cycle, not wrong, and
self-correcting the next time discovery stats and finds `-wal` now present.

Note: like Claude Code's format, this is undocumented and can change between
Codex CLI versions. Same defense: **lenient parsing** (a malformed row is
skipped, not fatal) plus synthetic-fixture tests. **Never bring a real
`~/.codex` database into the repository.**

**Live-reload watch:** `crate::watch` watches `CodexHome::rollout_dir`
(`<codex_home>/sessions/`, recursively) so a new session's rollout file
appearing triggers the same debounced reload a Claude Code change does. It
does not watch any of Codex's sqlite files. Measured against a synthetic
WAL-mode database shaped like `state_5.sqlite`: a write never touches the
main file's own mtime (everything lands in its `-wal`/`-shm` sidecars), and a
running session writes somewhere in `$CODEX_HOME` on every turn —
`logs_2.sqlite` on every log line — so a directory watch there would fire far
more often than the rollout tree's one event per turn, across databases
discovery doesn't even read.

**Busy/idle detection** (the relay engine's Codex-side signal —
`crate::codex_activity`; Claude Code publishes its own `status: busy` in
`sessions/<pid>.json`, which Codex has no equivalent of): no source above
records "is this session mid-turn right now" directly, and two timing-based
readings of `logs_2.sqlite`/the rollout file were tried and measured unsound
before landing on a third. Rollout mtime stayed pinned at its first-write
timestamp for an entire generation (file size grew 18KB -> 75KB across about
a minute of active writing; the reported mtime never moved). A
`logs_2.sqlite`-write-staleness threshold (120 seconds idle) was checked
against three real sessions' own `task_started`/`task_complete` markers as
ground truth and found wrong outright: the largest genuinely-within-a-turn
gap measured was 135 seconds, the smallest genuinely-between-turns gap was 1
second — those two distributions overlap completely, so no fixed threshold
can separate them. What is sound: tailing the rollout `.jsonl` itself for its
own `task_started`/`task_complete` event markers, a state read rather than a
clock — paired 1:1 with no exceptions across all 9 turns observed. The tail
window grows (`tail_turn_marker`/`TAIL_MAX_BYTES`) rather than staying fixed,
since a `task_complete` record was observed re-embedding the turn's full
final message on one line north of 11KB — a small fixed tail can land
entirely inside that one line and misreport idle right when the answer
matters most.

**Directory trust** (`crate::directory_trust`, advisory-only, read-only):
before typing anything into a freshly-spawned Codex Worker's pane (see
"Brigade" below), banto checks whether Codex already trusts that Worker's
cwd by reading `<codex_home>/config.toml`'s `[projects.'<path>']
trust_level` — the same record Codex itself writes when an operator answers
its own directory-trust prompt, keyed by a lowercased, backslash-separated
path. The module also reads Claude Code's equivalent
(`~/.claude.json`'s `.projects[<path>].hasTrustDialogAccepted`, keyed by
whatever exact string a past launch happened to pass as cwd, no
normalization — the same real directory was found stored under both
backslash and forward-slash forms disagreeing with each other). That half
gates nothing — a Claude Worker publishes its session id on its own the
moment it starts, so banto never types into its pane blind the way Codex's
kickoff (see "Brigade" below) does. It exists to explain a silence: a Worker
parked on that prompt starts nothing, is discovered by nothing, and would
otherwise sit there saying nothing at all, so the emporium's discovery poll
says so once in the status line. It fires only on a recorded refusal, not on
the absence of a record, because Claude writes an explicit `false` when an
operator declines — treating "never seen this directory" as a refusal would
have meant a warning on every directory opened for the first time.

## The `agents` setting

`Config.agents` (a plain string, default `""`) selects which agent products
banto discovers sessions for: `"all"` — also what an absent or empty string
means — or a comma-separated list of product names (`claude`, `codex`).
`banto_core::config::resolve_agents` resolves it into a `ResolvedAgents`
(`enabled: BTreeSet<AgentKind>`, plus `ignored`/`fell_back_to_all` — see
below); `"all"` means every product this build currently supports
(`AgentKind::ALL`), not a wildcard reserved for products that don't exist
yet. An unrecognized name is dropped, not rejected — this crate's config
layer is lenient by design (a broken setting must never prevent startup) —
but if *every* name in a non-empty setting goes unrecognized, `enabled`
falls back to `all` rather than an empty set that would silently discover
nothing with no visible cause.

**The ignored-name notice.** A dropped name still needs to be discoverable
by the operator, or a typo is indistinguishable from `all` with no way to
find out short of reading the source. `ResolvedAgents.ignored` records every
unrecognized name (deduplicated, first-seen order); `session::agents_ignored_notice`
turns that into one line, posted once via `App::set_status` right after
`App::new` in both `tui::run` and `embedded::run_emporium` — the two
entry points with a status line to put it in (the `list` subcommand has
none, and doesn't call this). Fires for a partial drop too, not only the
total fallback: an operator who kept a real, working filter is still owed
the fact that part of what they wrote silently did nothing.
`ResolvedAgents.fell_back_to_all` only changes the wording (naming what's
still enabled vs. saying the setting had no effect at all) — both cases
show the notice.

**Applies at discovery, not display — deliberately breaking precedent.**
Every other row filter in this codebase (`App::show_agents`,
`crate::tui::exclude_archived`) filters rows already read off disk.
`enabled_agents` instead gates which `SessionProvider` runs at all, inside
`session::discover_all` — the one place both providers' `discover()` calls
live. A disabled product's provider is never constructed, let alone called:
an operator who has switched Codex off gets banto never touching
`state_5.sqlite`, not banto reading it and discarding the result — the same
read-only discipline this file's Codex section had to earn a documented
exception for in the first place.

**The empty-list case:** a valid, non-restricting-looking setting (e.g.
`agents = "codex"` on a machine with no Codex sessions yet) can legitimately
discover zero rows, which looks identical to a genuinely empty machine.
`App::restricted_agents_label` (set once at startup via
`App::with_enabled_agents`, never re-filtered) lets
`banto_tui::view::render_list`'s existing empty-list placeholder say why —
`"No sessions found (agents = Codex)."` instead of the bare default — only
when `total_len() == 0`; a search query narrowing an otherwise-populated
list to nothing keeps its own unrelated "No matching sessions." message.

## Module layout

Four crates, split along the TEA / sans-IO boundary
(`docs/DISCIPLINE.md`) — `banto-core` is UI-free *and* I/O-free; the
provider/status/store/opener/config modules once sketched here as
`banto-core` submodules live in `banto-io`, not there:

```
crates/
├─ banto-core/          # pure: Event -> State + Cmd (TEA/sans-IO), no I/O — app/engine/model/status/screen/search/replay
├─ banto-io/            # the outside world: everything that touches a filesystem, spawns a process, or talks to sqlite
│  ├─ provider/         # SessionProvider trait + claude_code impl (JSONL) + codex impl (sqlite)
│  ├─ status/           # live state (sessions/<pid>.json + PID liveness) — Claude Code only so far
│  ├─ store/            # rusqlite: groups/pins/archived, brigades, session<->pane map
│  ├─ opener/           # Opener trait + tmux(psmux) / windows-terminal impls + auto detection
│  ├─ watch/            # filesystem watching (notify) for live TUI updates
│  ├─ claude_home.rs    # the Claude Code home root + its projects/sessions subdirs
│  ├─ codex_home.rs     # the Codex home root + its threads/logs db paths and rollout tree
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
spawned and read by banto itself (`portable-pty-psmux`), its output parsed with a VT
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
and non-Windows behavior beyond one dated Unix teardown follow-up
(`docs/notes/embedded-pty-spike.md`, 2026-07-25 addendum — a measured
comparison of graceful vs. force-kill teardown timing on that platform).

**Scrollback** was on that unverified list and turned out to fail outright: a
child that reserves a footer via DECSTBM (Codex does) never got a single line
into `vt100`'s scrollback tracking, because vanilla `portable-pty`'s ConPTY
session reinterprets the child's VT bytes through its own legacy console
buffer and re-serializes its own reconstruction — which for a DECSTBM footer
means a narrow scroll region synthesized unconditionally, regardless of what
the child actually drew. Fixed by requesting `PSEUDOCONSOLE_PASSTHROUGH_MODE`
(Windows 11 22H2+, build ≥22621) instead, which makes ConPTY relay the
child's bytes verbatim; banto-io depends on `portable-pty-psmux`, a fork that
requests this flag, rather than the crates.io `portable-pty`
(`crates/banto-io/src/pty.rs`'s `PortablePtyHost` doc). Measured 2026-08-02
with a controlled A/B — same Codex binary, same prompt, same geometry, only
the PTY crate swapped: scrollback stayed 0 without the flag, reached 191
lines with it.

## Brigade (Director/Worker cells)

A brigade is an internal operational cell of one Director session and one or
more Worker sessions, hosted together as tiled panes in the emporium
(`crates/banto-io/src/store/migrations.rs` v4 migration comment). It is a
separate concept from groups (the user's own project/phase filing): a brigade
is a live operational unit, not a filing category. A session belongs to at
most one brigade, and a brigade has exactly one Director — both are "layered
in code, not a schema constraint" (same comment, verbatim).

**A third role, Goinkyo, exists in `BrigadeRole` and the messaging rules
below** — the retired elder called back in to arbitrate a Director/Worker
disagreement, addressable by name but never a broadcast recipient (see
`send_to_peer` below). A Director calls one in with the `consult_goinkyo`
MCP tool (see "MCP mediation server" below), which files a written
consultation request and creates the member row; once a tick observes that
row with no session id yet, it is auto-spawned the same way a fresh Worker
is (Claude only — `goinkyo_model`/`goinkyo_effort`/`goinkyo_permission_mode`
below), briefed from `goinkyo_prompt` with `{request}` substituted for the
consultation file's path. The spawn is attempted at most once per
consultation (`EmporiumState::goinkyo_pane`, one guard per brigade, mapped
to the `SessionKey` that spawn used): a failed attempt is not retried
automatically.

`goinkyo_permission_mode` defaults to `"auto"`, not Claude's own default of
`"manual"`: nobody is at an unattended Goinkyo's keyboard to answer a
permission prompt, and `manual` (like `"plan"`, whose own design still exits
through a human approval at the end) reliably stops there. Not a guarantee
of unattended operation either — Claude Code falls back to its ordinary
confirmation flow after repeated denials even under `"auto"`, and that flow
stops the same way `"manual"` does.

The briefing alone is not a turn: Claude Code does nothing with a system
prompt until it receives one, so once the Goinkyo's own pane goes quiet
(`GOINKYO_KICKOFF_QUIET_PERIOD`), banto checks whether Claude has been told
to trust its cwd (`Cmd::CheckGoinkyoDirectoryTrust` — a different trust
registry from Codex's own directory-trust check, re-asked every tick until
trusted) and types a fixed kickoff line into it — the same bootstrap
problem, and the same shape of fix, `CODEX_WORKER_KICKOFF_LINE` solves for
a freshly-spawned Codex Worker. A Worker never needs this: the operator
gives it its own first turn by typing into its pane themselves, which
nobody does for an unattended Goinkyo.

**Ending a consultation** removes the Goinkyo's member row, which releases
the guard above and — the next time a tick observes a still-staged brigade
with no Goinkyo row at all — unstages and kills whatever pane was tracked
for it, the same way dismissing a Worker already closes its pane. Two ways
to end one: the Director's own `dismiss_goinkyo` MCP tool (see "MCP
mediation server" below), or the operator picking Dismiss from the
prefix-`x` kill-confirm dialog on the Goinkyo's own pane — the same choice
a Worker's pane already offered.

That dialog decides in two separate stages, not one, both role-based now:
*whether to offer* Dismiss at all reads `Stage::Brigade`'s own `director:
Option<SessionKey>` field directly (`director.is_some() &&
director.as_ref() != panes.get(focused)`) — not the focused pane's
position. `panes`' own order is display-only and best-effort (a convenience
`update_spawned` attempts on arrival, director-first when it can, never
load-bearing): `Store::brigade_members`' `ORDER BY` puts the Director
first and the common formation path preserves that into `panes`, but a
resume where the Director's own pane needs a fresh `Cmd::OpenEmbedded`
while another member's is already open can still append them out of that
order (see `engine.rs`'s `stage_brigade`/`update_spawned`) — this no longer
matters for correctness, only for which pane a fresh operator's eye lands
on first. *Whether a confirmed Dismiss actually deletes anything* is a
separate, later check on the member's real role from the store
(`update_membership_resolved`), which refuses for a Director regardless of
what the dialog showed — unchanged, and still the check that actually
guards deletion; the two stages agreeing is what closes the old UX gap
(Dismiss missing, or offered, on the wrong pane) that a position-derived
first stage used to risk. Disband does *not* reach the
brigade off stage *before* the row disappears, so the next tick's
observation is "not staged" rather than "no row" — the guard for a
disbanded brigade is simply never released, harmlessly, since that brigade
id is never staged (and so never observed) again. **Reopening a
brigade around an already-discovered Goinkyo (one that already has a
session id, resolvable to a real row) already works**, for free, through
the same resumed-member path a Director's or Worker's own closed pane
reopens through (`stage_brigade`) — a resume never carries `--model`
(unlike a fresh spawn), but does carry `--permission-mode`, since that flag
answers "who's here to click 'allow'", not "what should this session think
with" — unrelated to fresh-vs-resumed. A Goinkyo that was never discovered
(its pane died before it had a session id) does *not* reopen through that
path — `stage_brigade`'s undiscovered-member branch is Worker-only — but
stays a fresh-spawn candidate for the ordinary tick mechanism above for as
long as the brigade remains staged.

**A third case — a stranded Goinkyo — sits between those two.** A Goinkyo
can have a session id (its own discovery already ran once) that still
resolves to no row: Claude Code writes no `projects/*.jsonl` transcript
until a session's first turn, so a Goinkyo that never got as far as its own
kickoff (closed, or the operator's machine restarted, before it ran one)
has a real id but nothing `app.row_for_id` can find — permanently, not just
this tick. Left alone this reads as `missing` forever: the session id never
clears on its own, so the ordinary tick mechanism's `AwaitingSpawn` check
(which only fires on no session id at all) never sees this member as
spawnable again — a consultation the operator can't restart short of
dismissing it outright and filing a new one. `stage_brigade` now resets the
session id back to `None` for exactly this case, which lets the next tick's
`AwaitingSpawn` restart the very same consultation (its request file
outlives this, kept until dismissal) through the ordinary spawn path — the
Goinkyo analog of a Worker's own disposable, respawnable design. The one
trap this has to dodge: a pane can be alive under the id key already
(`Cmd::RekeyPty` renamed it) while `app`'s own row list simply hasn't caught
up yet with the not-yet-written transcript — that Goinkyo isn't stranded at
all, just momentarily unresolved, and resetting its session id in that
window would sever a *live* member from the row identifying it: its own
`_mcp` connection resolves who's calling through `brigade_of_session`,
keyed on that same session id, so clearing it would break `send_to_peer`/
`check_messages` for a Goinkyo still mid-consultation. `stage_brigade` tells
the two apart by checking whether a live pane already answers to that key;
if one does, it's reused, not reset.

**Formation.** `B` on a selected session appoints it Director and auto-spawns
`workers` (config, default 1, clamped 1..=8) fresh Workers beside it. `b` spawns
one more Worker into the currently-staged brigade. `B` on an existing Director
opens a disband confirmation instead (Workers cannot be promoted to Director
directly).

Forming (or adding to) a brigade can pass through up to three sequential
confirm gates before anything is actually formed (`crates/banto-core/src/
engine.rs`; at most one is ever open at once). Each is independently skipped
when its own condition doesn't apply, so most presses of `B` show none of
them:

1. **The Director-to-be's own pane is already open.** Brigade wiring (MCP
   config for Claude, `-c mcp_servers.banto.*`/hook overrides for Codex — see
   "MCP mediation server" below) only ever travels through that launch's own
   argv, so a pane opened before formation can't carry it.
   `Modal::ConfirmDirectorReopen` — Enter kills that pane and re-issues
   formation only once `Event::PtyExited` confirms it actually exited (any
   sooner risks `opener::decide_inplace_resume` seeing the not-yet-dead
   session as still live and refusing the reopen); Esc cancels the whole
   attempt. Placed first, ahead of the other two, because it is both the
   most disruptive of the three (running work in that pane is lost) and the
   cheapest to abort (nothing has been picked or written yet) — asking the
   operator to pick a Worker product first, only to then say the pane is
   about to restart anyway, would be backwards.
2. **`[brigade] worker_agent = "select"`.** `Modal::WorkerAgentPicker` asks
   which product (and model) the Workers run as before there is anything to
   form.
3. **The resolved Worker product is Codex, and banto's own hook doesn't look
   trusted.** `Modal::ConfirmCodexTrust` — Enter opens a solo pane running
   Codex's own trust-review startup and abandons this formation attempt
   outright, rather than waiting to resume it: Codex records trust as a hash
   of its own hook command, which banto has no way to reproduce or compare
   against (`banto_io::codex_trust`'s module doc), so it cannot tell whether
   an approval in that pane actually took — treating an unverifiable
   "probably fine" as ground truth is a mistake this project has paid for
   before. The operator presses `B` again once done. (Skipped, formation
   proceeding anyway with a status-line notice instead, on the one Codex
   installation banto cannot brief regardless of trust: its own executable
   path contains a space, which Codex's own hook launcher can't run a
   command from.)

A residual race the operator's own choices don't cover — something else
reopens the Director's pane in the gap between gate 1 confirming and the
store round trip actually forming the brigade — is handled without asking:
`update_brigade_formed` kills and rewires that pane unconditionally rather
than leaving a Director with no wiring at all.

**Member identity.** Each member gets a banto-owned `member_token`
(`"director"`, `"worker-1"`, `"worker-2"`, ...) rather than being keyed by its
session id — a Worker is formed by banto *before* its agent product assigns it
a session id (it's auto-spawned), so the id has to be a nullable, filled-in-later
column (`brigade_members.session_id`) rather than the primary identity
(`crates/banto-io/src/store/migrations.rs` v7/v12 migration comments — v7
named the column `claude_session_id`; v12 renamed it back to `session_id` once
Codex became a second product it has to hold). The token is stable for the
member's lifetime in the brigade; its session id is not (unknown until
discovered, never reused across brigades).

**A freshly-spawned Codex Worker has no session id to discover until it runs
a turn.** Unlike Claude Code, which publishes `sessions/<pid>.json` the
moment it starts, an idle Codex process with nothing typed into it never
fires its `SessionStart` hook, never gets a `threads` row, never writes a
rollout file — there is nothing for discovery to find yet. So banto starts
the first turn itself: once a fresh Worker's pane output has gone quiet for
`CODEX_KICKOFF_QUIET_PERIOD` (700ms, a threshold measured against this
product's own boot sequence) *and* Codex is confirmed to already trust the
Worker's cwd (the "Directory trust" reading above — typing blind into a pane
that might instead be showing that product's own directory-trust prompt
risks answering that prompt by accident), banto types a fixed, ASCII-only
status line naming itself, then a delayed `\r` in a second PTY write (the
same two-step "text, then submit" shape the auto-relay nudge uses, and
deliberately not that same nudge line, since no peer has actually sent mail
yet). The turn this forces is what fires the hook; the hook's own stdin
carries the real session id, which `banto _hook` records
(`store::record_briefing`) for `poll_discovery`'s `codex_briefed_session_id`
to pick up. If the cwd isn't trusted yet, banto keeps re-checking every tick
rather than typing blind or giving up — answering the prompt in the pane
directly lets the kickoff resume on its own next tick, with a status-line
notice posted once in the meantime.

**Lifecycle.** Killing a Worker's pane (prefix-`x`) lets it respawn fresh under
the same token next time its brigade is staged; *dismissing* one (a separate
choice on the same confirm dialog) removes it from the brigade for good —
membership, message cursor, and any mail addressed specifically to it, all
gone. Disbanding (`B` on a Director) removes the whole cell.

**Config** (`crates/banto-core/src/config.rs`, `[brigade]` in config.toml):

```rust
pub struct BrigadeConfig {
    pub workers: u32,               // auto-spawned per cell, clamped 1..=8, default 1
    pub worker_model: String,       // --model for an auto-spawned Claude Worker; "" = inherit; default "sonnet"
    pub worker_model_codex: String, // --model for an auto-spawned Codex Worker; independent default and escape hatch
    pub worker_agent: WorkerAgentSetting, // Inherit (default) | Select | Claude | Codex — see below
    pub relay: RelayMode,           // Auto | Manual, default Auto — see "Auto-relay" below
    pub director_prompt: String,    // role briefing template for the Director
    pub worker_prompt: String,      // role briefing template for each Worker
    pub goinkyo_prompt: String,     // role briefing template for a Goinkyo; also substitutes {request}
    pub goinkyo_model: String,      // --model for an auto-spawned Goinkyo; "" = no flag; default "fable"; Claude only
    pub goinkyo_effort: String,     // --effort for an auto-spawned Goinkyo; "" = no flag; default "max"; Claude only
    pub goinkyo_permission_mode: String, // --permission-mode for an auto-spawned Goinkyo; "" = no flag; default "auto"; Claude only
}
```

`worker_agent` decides which product a brigade's Workers run as, independent
of the Director's own product: `Inherit` (default) matches the Director's;
`Claude`/`Codex` fix it regardless; `Select` defers the choice to formation
gate 2 above. `worker_model`/`worker_model_codex` are two independent
`--model` overrides, since the two products don't share a model namespace —
one shared field could never default sensibly for both at once.

**Role briefing delivery differs by product.** `director_prompt`,
`worker_prompt`, and `goinkyo_prompt` all render the same template,
substituting `{brigade}` (the brigade id), `{token}` (this member's own
token), and `{peers}` (a comma-joined list of its addressable peers); a
Goinkyo's template also substitutes `{request}` (the path to the
consultation request `consult_goinkyo` filed — see "Brigade" above), left
alone in the other two roles' templates the same way any unrecognized
`{...}` is. An empty template means no briefing at all, deliberately a
*setting* and not a constant: without one, a cell exists only in banto's
data model and the operator's own screen, and a Director handed three MCP
tool names with no notion that it leads a cell mostly never uses them. Only
how the rendered text reaches the member differs by product:
- **Claude**: on the launch argv, `--append-system-prompt <rendered>`.
- **Codex has no equivalent flag.** Its briefing instead rides banto's own
  `SessionStart` hook: every Codex member's launch adds a `-c
  hooks.SessionStart=[...]` override naming `<banto executable> _hook` as
  the hook command — byte-identical across every member and every launch,
  deliberately, since Codex hashes that literal command string to decide
  whether to trust it, and a launch carrying member identity in the command
  itself would make every member's hook need its own separate approval.
  Identity instead travels through `BANTO_BRIGADE`/`BANTO_MEMBER`/
  `BANTO_ROLE` in the environment, which the hook process inherits.
  `banto _hook` (`crates/banto/src/hook.rs`) renders the same template
  (`crates/banto/src/briefing.rs`) and returns it as the turn's
  `additionalContext`, appending a fixed Codex-only addendum: banto's own
  MCP tools are deferred from Codex's own tool list until called by name,
  and Codex's own developer prompt otherwise casts the model as the primary
  agent of a different, unrelated multi-agent feature — measured end to end
  in `docs/notes/codex-briefing-spike.md`.

The shipped defaults tell a Director to delegate independent, parallelizable
work to its Workers via `send_to_peer` and keep genuinely sequential work
itself; that policy is intentionally a setting the operator can change, not a
fixed behavior.

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

**Codex carries the same server a different way — no config file at all.**
Codex has no `--mcp-config` equivalent; the launch instead adds `-c
mcp_servers.banto.command=...` and `-c mcp_servers.banto.args=[...]`
overrides naming the identical `<banto executable> _mcp --brigade <id>
--member <token> --role <role> [--session <id>]` invocation (structured TOML
fields Codex hands its own process spawner directly, not re-split by a
shell), plus `-c mcp_servers.banto.default_tools_approval_mode="approve"` to
bypass Codex's own per-tool approval prompt for this server — repeated on
every single launch rather than written once to `~/.codex/config.toml`, since a
`-c`-registered server can't persist that trust at all, and (measured
2026-07-28) the bypass degrades silently, no error and no warning, the
moment a launch leaves it off (`crates/banto/src/opener.rs`'s
`CodexBrigade::overrides` and the override functions it calls). This is a
different Codex prompt from the hook-trust one gated in "Brigade" below —
tool approval and hook trust are Codex's own two separate mechanisms, solved
two separate ways here.

The server shares banto's own sqlite store with the TUI process and exposes
five tools:
- `send_to_peer(text[, to])` — enqueues a message: a Director broadcasts to
  every Worker by default, or a Worker/Goinkyo sends to the Director;
  `to` names one specific member instead — the only way a Director reaches
  a Goinkyo, since a broadcast never does.
- `check_messages()` — pulls the messages addressed to this session's role
  that it hasn't seen yet, wrapped in framing that names them as relayed from
  another AI rather than a direct operator instruction.
- `brigade_status()` — this member's own identity plus a roster of its
  addressable peers, each with what it's doing right now and whether it's
  holding unread mail from this member. Replaces an earlier bare ping-style
  health check, added once dogfooding showed a Director launched with only
  the two message tools and no roster information mostly never used them.
- `consult_goinkyo(question, my_case, settled, unsettled, blind_spot[,
  their_case][, about])` — Director-only: files a written consultation
  request and creates the Goinkyo's member row (see "Brigade" above for what
  happens to that row next). `about` names the Worker the disagreement is
  with, which makes `their_case` required too; omit both for an impasse with
  no specific Worker. Refuses if a Goinkyo already exists for the brigade —
  only one consults at a time; `dismiss_goinkyo` ends the current one.
- `dismiss_goinkyo()` — Director-only: ends the brigade's active
  consultation by removing the Goinkyo's member row (see "Brigade" above for
  what that triggers). Refuses if no Goinkyo is currently part of the
  brigade.

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

Watch `projects/` and `sessions/` with `notify` for realtime updates — plus,
when a Codex home resolves, its rollout tree (see "Codex data sources" above
for why not its sqlite files too).

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
