# banto (番頭)

![The bantō at his counter, writing in a ledger whose rows glow like a
terminal list, while clerks carry boxed sessions between the shelves —
one box glowing mid-handoff](docs/assets/banto-hero.png)

**banto knows which Claude Code and Codex sessions exist on this machine,
which are still alive, and which one you actually meant.**

`claude --resume` will reopen a session if you can produce its id. banto is
the part that finds it — a fuzzy-searchable ledger of every session on disk
(Claude Code's under `~/.claude`, Codex's under `~/.codex`), with your own
pinning, grouping and archiving layered on top — and the part that refuses
to open one twice, because resuming a session that is already running forks
its history.

It is one full-screen terminal UI: the ledger on the left, and on the right
the sessions you open, hosted by banto itself as live panes that keep running
while you switch between them — and that can be wired into Director/Worker
cells that talk to each other through banto.

Claude Desktop makes multi-session work feel like a workspace — sessions
side by side, their state legible at a glance, panes you arrange to suit the
job. banto is an attempt to have that against the real `claude`/`codex` CLI
sessions already on disk, without leaving the terminal to get it.

*Name origin: 番頭 (bantō) — the head clerk of a traditional Japanese
merchant house, who stays on the premises and runs the shop.*

**Cross-platform, raised on Windows.** Development and the heaviest
dogfooding happen on Windows Terminal + ConPTY, which makes Windows a
first-class citizen — the workshop, though, not the goal. The codebase
stays cross-platform (CI builds and tests both Windows and Linux), and
Linux dogfooding is underway.

## Why not just tmux?

tmux is a window manager for terminals. banto is a lifecycle manager for
Claude Code and Codex sessions: it knows their ids, the directories they were started
in, which are still running, and which of them you filed under what. Picking
a row does not open a shell that happens to run `claude` — it returns you to
one specific conversation, or to the process already having it.

banto does not drive a multiplexer. It hosts the panes itself, because a
multiplexer cannot carry what banto does with them: a ledger beside the
panes, sessions that know which cell they belong to, messages relayed between
them.

## What it does

The ledger on the left has a name: the **chōba** (帳場, "the shop counter") —
the list of every session, built by indexing the session files Claude Code
writes under `~/.claude` and the session index Codex keeps under `~/.codex`
— read-only, always; see below. Search it, pin, group and archive its
sessions, start a new one in a directory you pick or type.

Whatever you pick there opens in a pane on the right (大店, the *emporium*:
banto's own hosted-pane view). banto is a minimal embedded multiplexer — a
persistent sidebar plus sessions hosted in vt100-parsed panes, Vim-buffer-style
swapping, sessions kept alive in the background across switches.

If a session is already open in one of banto's panes, picking it switches to
that pane. If it is running anywhere else — another banto, a bare `claude` in
some other window — banto refuses to start a second one and says so. That is
the guarantee from the top of this page: resuming a session that is already
running forks its history.

### Director/Worker cells

![banto with a staged cell: the ledger on the left — a
🤝 Director row pinned under a counted header — and three live panes: a
Director session flanked by two Workers reporting back](docs/assets/emporium-brigade.png)

*A live cell at work (on WSL): the Director reviews and directs; two
Workers implement and report over banto's MCP channel. This screenshot is
itself dogfooding — the session shown is building banto.*

`B` appoints the selected session as a **Director** and
auto-spawns fresh **Worker** sessions beside it (count, model, and even
product all configurable — a brigade's Workers can run Claude Code or Codex
independently of the Director's own product). banto mediates a message
channel between them over MCP: it wires every member back to `banto _mcp` on
its own launch — Claude Code via `--mcp-config`, Codex via `-c
mcp_servers.banto.*` overrides — so members get `send_to_peer` /
`check_messages` / `brigade_status` tools backed by banto's own sqlite
queue (plus Director-only `consult_goinkyo` / `dismiss_goinkyo`, and
`name_member`, which gives a Worker a display name for the standing role the
Director has given it — shown beside its token in the tile title, in
`brigade_status` and in message headers; the token stays the address). Each member is also handed a role briefing naming its brigade, its
token, and its peers — appended to the system prompt on launch for Claude
Code, delivered by banto's own `SessionStart` hook for Codex, which has no
`--append-system-prompt` of its own — without one a cell exists only in
banto's data model and the operator's screen, and a Director handed three
tool names and no context mostly never uses them; `brigade_status` answers
the follow-up question (who is on my team, what are they doing, is anyone
holding my mail). Delivery is pull-based with per-member cursors and firewall
framing (a relayed message is labeled as coming from another AI, never
mistaken for operator input), and banto's **auto-relay** watches for idle
members with unread messages and wakes them by typing a short fixed nudge
into their stdin — so a Director→Worker→Director round trip needs no human
ferrying at all. This repository was largely built through that loop.

When every live member of a brigade (the Director, the Workers, a Goinkyo) has
been idle for 5 seconds after any of them did real work (starting up is not
work) — counted from the
last member going idle or banto last prompting one — the brigade has
gone quiet — the moment your input is wanted. banto then marks the Director's
pane (the magenta border a waiting session gets, though a focused pane keeps its
focus colour) and rings the terminal bell.
It stays silent while you are looking at the Director's pane (that pane
selected and banto's window focused); with the window in the background it
marks and rings. A member whose state banto cannot read keeps it quiet, as does a
new Codex Worker or Goinkyo that banto has yet to start, as does a member with an
unread report still on its way. A brigade that is not on screen does not notify you
when it goes quiet; a member of it that is waiting on you is shown when you open it.
A picker you open yourself in a Claude pane (a slash command such as `/model`) shows
as waiting but does not ring, and keeps its brigade from counting as quiet while it
is open.
`F4` switches on "ring even while the Director's pane is focused" (the
bell only, no mark); the status bar shows `BELL ALWAYS` left of the counts
while it is on, and the setting is remembered across restarts.

Forming your first Codex brigade needs one thing from you: Codex reviews
banto's own hook for trust at its own startup, once — not per launch — so
banto opens a one-time approval pane for you to clear before that first cell
forms. Approve it once and every brigade launch after that finds it already
trusted.

## Status & caveats

- A personal tool, pre-1.0, moving fast. Interfaces and the on-disk schema
  migrate forward but nothing is promised yet: banto takes no backup before
  migrating its database, and an older build cannot open one a newer build has
  migrated.
- **No supervisor, by design.** Sessions hosted in banto's panes end when
  banto does. Each one gets an ordinary console-close first, so `claude`
  finalizes its history exactly as it would on any window close and the
  session resumes cleanly next time — but banto is a workbench you sit at,
  not a daemon that keeps work running behind you.
- Claude Code's session-file formats are **undocumented and subject to
  change**; banto defends with lenient parsing (unknown records and broken
  lines are skipped, never fatal) and synthetic-fixture tests, but a future
  Claude Code release could still surprise it.
- banto's own pane hosting speaks ConPTY, which has sharp edges (documented in
  [docs/notes/embedded-pty-spike.md](docs/notes/embedded-pty-spike.md):
  never answer DSR/DA, chunk boundaries carry meaning, child exit produces
  no EOF). These are handled — ConPTY is where the sharpest edges lived,
  which is why the Windows side gets such deliberate care.

## Read-only guarantee

**banto never writes anything under `~/.claude` or `~/.codex`.** Session
`.jsonl` files, live-state files, history, Codex's `threads` and `logs`
sqlite databases (session index and liveness, respectively) — all strictly
read-only. banto's own data lives under its own directories:

- Config: `dirs::config_dir()/banto/config.toml`
  (Windows: `%APPDATA%\banto\`) by default — see
  [Configuration](#configuration) for the full resolution order
  (`--config`, `$BANTO_CONFIG`, XDG, `~/.config`)
- Data (sqlite): `dirs::data_local_dir()/banto/banto.db`
  (Windows: `%LOCALAPPDATA%\banto\`)

Even the MCP wiring honors this: member `--mcp-config` files are written
under banto's data dir and passed by argv, never installed into Claude's own
configuration.

**One named exception.** Reading a Codex sqlite database (`state_5.sqlite`
for the session index, `logs_2.sqlite` for liveness — the same strategy was
independently re-verified against each, not assumed to carry over) while
Codex itself might be writing it needs SQLite's own read-only machinery,
chosen by which sidecar files are present: `mode=ro` when a `-wal` file
exists (a database Codex may be actively writing), `immutable=1` when it
does not (a cleanly-closed database, where `immutable=1` alone could
otherwise return a stale snapshot if a writer started between the check and
the open). The one case this doesn't cover for free is crash residue — a
`-wal` left behind without its `-shm` sidecar (e.g. Codex was killed
mid-write) — where the first `mode=ro` open recreates a fresh `-shm`: a
single ~32KB write, SQLite's own coordination index rather than banto's
data, and one Codex's own next run would create regardless. Named here
rather than smoothed over, because "banto never writes" should mean exactly
what it says everywhere else.

## Install & run

Requires a recent stable Rust toolchain (edition 2024).

```sh
cargo build --release

# The UI: the session list on the left, banto-hosted panes on the right
cargo run --

# Plain-text session listing
cargo run -- list

# Point at a different Claude home
cargo run -- --claude-home /path/to/.claude
```

(`--emporium` and its alias `--oodana`, from when banto had two modes, are
still accepted, do nothing, and will be removed.)

## Keys

One table. The first group is the list; the rest is what the panes add.

`q`/`Esc` quits at once when every hosted session is known to be idle, or none
is hosted. Otherwise it asks first: the status bar shows `Esc again to quit —
N session(s) not idle`, and a second `q`/`Esc` within 3 seconds quits, since
quitting ends every hosted process. N counts the sessions that are working,
waiting on you, of unknown state, or that were typed into (by you, or by banto
itself) within the last 3 seconds. A Claude Code session reports its own state,
and so does a Codex one: banto reads the status report the Codex interface writes
to its terminal (working, blocked on you, done or idle) and, until a session has
made one, its last turn in the rollout. Codex records nothing about a command it
left running in the background, so a Codex session that has not reported reads
as idle. Once quitting is decided the status bar reads `Closing —
ending N session(s)…` (`Closing…` with none) until banto exits: each session
is closed in turn and given up to 5 seconds to finish.

| Key | Action |
|---|---|
| `j`/`k`, arrows, `PgUp`/`PgDn`, `Home`/`End` | Move selection |
| `Enter` / double-click | Open the session in a pane (switch to it if it is already open); on a Director, stage its whole cell |
| `n` | New session (pick or type a cwd) |
| `/` | Search (fuzzy, title + cwd) |
| `p` / `d` / `g` | Pin / archive (soft-hide) / join group |
| `Tab` | Toggle grouped view (Pinned / groups / Ungrouped) |
| `a` | Cycle what the list reveals: ordinary sessions → also hidden ones (agent-run, superseded ancestors) → also archived ones |
| `D` | Restore (un-archive) the selected session, while archived ones are revealed |
| `q` / `Esc` | Quit (a second press is needed while a session is not idle, see above) |
| `B` | Appoint Director + auto-spawn Workers (on a Director: disband) |
| `b` | Spawn one more Worker into the staged cell |
| `F2` | Move focus between the list and the panes |
| `F3` | Make the next pane of the staged brigade the current one (collapsed tiles are skipped); pressed from the list, the keyboard stays on the list |
| `F4` | Switch the brigade bell (see "Director/Worker cells") between silent-while-you-watch and ringing even while the Director's pane is focused; one keystroke from the list or from a pane, taken by banto and not sent to the agent; the status bar shows `BELL ALWAYS` while the second is on |
| `Ctrl+B` … | tmux-style prefix (configurable), from the list or a pane: |
| … `o`/`Tab` | cycle focus through sidebar and panes |
| … arrows | directional pane navigation |
| … `Shift`+`Up`/`Down` | move the focused stack tile one slot up/down (focus stays on it) |
| … `m` | collapse the focused stack tile to a strip, or open it again |
| … `z` | solo the focused tile (the master included; every stack tile but the soloed one is collapsed), or turn its solo off |
| … `M` | open every collapsed tile and turn the solo off |
| … `=` | put the staged brigade's tile sizes back to the defaults (order, collapsed tiles and the sidebar width are kept) |
| … `1`-`9` | jump to pane N |
| … `s` | back to the sidebar |
| … `Esc` | cancel the prefix (focus stays; nothing is sent to the child) |
| … `x` | dismiss the focused Worker/Goinkyo from its brigade (confirm; it leaves for good, its session is kept); on a Director or a single session, end its process |
| … `b`/`Ctrl+B` | send a literal prefix chord to the child |

While the prefix is armed, the status bar shows the full binding table —
nothing to memorize; otherwise its hints follow the focus (the list keys when the
list is focused, just the prefix chord and `F2`/`F3`/`F4` when a pane is). Multiline paste and file drag&drop into panes work
(synthesized into bracketed pastes host-side, since the Windows console
never reports pastes as pastes).

With the mouse: drag the boundary between the list and the panes, between the
master tile and the stack, or between two stack tiles to resize; drag a stack
tile by its title to move it; `[S]` in any tile's title row solos it, and a
stack tile also has `[_]` to collapse it to a strip (`[+]` opens it again). Each brigade's
arrangement, and the sidebar's width, is remembered across restarts.

A tile's title row, and a minimised tile's strip, say what its session is doing:
**busy** (a turn or a background command is running), **waiting** (it needs you),
**alive** (running, nothing to do) or **idle** (not running), then how long ago it
last wrote. A Codex session's word comes from the status report its interface writes
to its terminal. A tile too small for a screen shows the top-left corner of a
2-row-by-2-column one: panes are never made smaller than that.

## Configuration

`config.toml` is optional; missing or broken config falls back to defaults.
Its location is resolved in this order, first hit wins:

1. `--config <path>` — must exist and parse, or startup fails
2. `$BANTO_CONFIG` — same strictness
3. `$XDG_CONFIG_HOME/banto/config.toml`, when the variable is set, non-empty,
   and the file exists (every platform, including Windows, for dotfiles
   setups)
4. `~/.config/banto/config.toml`, if it exists
5. `dirs::config_dir()/banto/config.toml` (`%APPDATA%\banto\` on Windows) —
   the default, used unconditionally as the last resort

Only (1)/(2) are strict; (3)-(5) are existence-gated or unconditional
fallbacks and stay lenient — a missing or broken file there just means
defaults, same as today.

```toml
claude_home = "C:/Users/you/.claude"   # optional override
db_path = "..."                        # optional override; an empty value is a startup error

# "all" (default) | "claude" | "codex" | "claude,codex" — which agent
# products banto discovers sessions for. A disabled product's own files
# (Codex's sqlite database, in particular) are never even opened.
agents = "all"

[activity]
today_hours = 24   # activity-dot bucketing
week_days = 7

[brigade]           # Director/Worker cells
workers = 1         # Workers auto-spawned per cell (1..=8)
worker_model = "sonnet"   # --model for spawned Workers ("" = inherit)
relay = "auto"      # "auto" | "manual" — the wake-up nudge engine
# For banto developers: append every brigade message, in full, to
# <data dir>/banto/logs/brigade-<id>.log. Off by default. The file holds real
# conversation content; banto never rotates or deletes it — don't commit it.
message_log = false
# Role briefings appended to each member's system prompt at launch.
# {brigade} / {token} / {peers} are substituted; "" launches with no
# briefing. The defaults tell a Director to delegate (and that it can name a
# Worker with name_member) — that policy is yours to set, which is why it
# is a setting.
director_prompt = "..."
worker_prompt = "..."

[keys]
prefix = "C-b"      # pane prefix chord
```

## Architecture

Four-crate workspace under a strict TEA / sans-IO discipline
([docs/DISCIPLINE.md](docs/DISCIPLINE.md)) — in one sentence:

> Every contact with the outside world has a name as an `Event`.
> The core is a pure function from `Event`s to `State` and `Cmd`s.

```
crates/
├─ banto-core/  # pure: Event -> State + Cmd. deps: serde, vt100, nucleo — that's the point
├─ banto-io/    # the outside world: jsonl provider, sqlite, PTY host, watchers, probes
├─ banto-tui/   # rendering from &State (ratatui, no terminal backend)
└─ banto/       # the bin: the event loop, executors, the crossterm boundary
```

Part of the boundary is compiler-enforced: `banto-core`'s `Cargo.toml`
carries no dependency on crossterm, rusqlite, or a terminal/PTY backend, so
naming any of them fails to build. It is not enforced against a clock or a
file the same way — `std::time` and `std::fs`/`std::path` are always in
scope for any Rust crate regardless of its dependency list — so those
prohibitions are maintained by review (see `docs/DISCIPLINE.md` §2 for the
full breakdown, including a real breach that survived it). Event streams can
be recorded (`BANTO_RECORD_EVENTS`) and replayed deterministically
(`banto_core::replay`) — timeouts and debounces are tested by arithmetic,
not by sleeping.

Further reading: [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) (design/
requirements), [docs/notes/](docs/notes/) (on-device verification notes for
the embedded ConPTY host, MCP, and Codex briefing delivery).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
