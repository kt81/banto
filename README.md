# banto (番頭)

[日本語](README.ja.md)

![The bantō at his counter, writing in a ledger whose rows glow like a
terminal list, while clerks carry boxed sessions between the shelves,
one box glowing mid-handoff](docs/assets/banto-hero.png)

**A full-screen terminal UI for Claude Code and Codex: every session on your
machine in one searchable list, several of them running side by side, and a Director
session that hands work to Worker sessions.**

*番頭 (bantō) is the head clerk of a traditional Japanese merchant house, who
stays on the premises and runs the shop.*

![banto on WSL: a new session is renamed and made a Director with three Workers, two
Claude Code and one Codex; the Director gives them role names, they report back over
banto's relay, the Director consults the Goinkyo, and you pick one of the options the
Director lays out](docs/assets/brigade-demo.gif)

*Recorded on WSL and sped up 3 to 5×, and 40× while the Goinkyo (an advisor session
the Director consults) is thinking.*

## What it does

The list on the left is the **chōba** (帳場, "the shop counter"): every session,
built read-only from the session files Claude Code writes under `~/.claude` and the
session index Codex keeps under `~/.codex`. Search it, pin, group and archive, and
start a new session in a directory you pick or type. Pick a session to resume it, no
id needed: it opens in a pane on the right (大店, the *emporium*), swapped like Vim
buffers and kept alive in the background. One already open is switched to. One
running anywhere else (another banto, a bare `claude`) is refused with a message and
never started twice, since resuming a running session forks its history.

### Director/Worker brigades

Press `B` on a session to make it the **Director**; banto spawns fresh **Worker**
sessions beside it. Their count, model and product are configurable, and Workers can
run Claude Code or Codex independently of the Director. A Worker's model and reasoning
effort are given only when its session is created. A restart keeps the model the
session last ran on. Codex keeps the effort too, but Claude Code takes it from your
own Claude settings, so only a level you saved there carries over (`max`, or a level
set for one session only, lasts until the restart).

banto relays messages between members over MCP, wiring each one to `banto _mcp` at
launch. Members get `send_to_peer`, `check_messages` and `brigade_status`. The
Director also gets:

- `consult_goinkyo` / `dismiss_goinkyo`
- `open_planner` / `close_planner`: the Director hands you to a Planner when the work
  spans several errands and is not yet settled (see below)
- `request_decision`: the Director needs a decision only you can make. banto tags it
  `▸ decision` until you press Enter in its pane, and does not stop it. The question
  is read on the reading screen (`F5`)
- `update_progress`: the Director records the state of the work (a stage line, tasks
  with their states, work outside the plan), replacing the previous record. The stage
  shows on the Director's tile and the rest on the reading screen (`F5`)
- `interrupt_worker`: delivers the Director's reason to a Worker as a message and
  has banto refuse a Claude Worker's next tool calls (up to 3) until it reads it, only
  with relay `auto`. It cannot stop a turn or cut a call that is running; with relay
  `manual`, and for a Codex Worker, it goes as ordinary mail and nothing is refused
- `name_member`: gives a Worker a display name for its standing role (the token
  stays the address)

The Goinkyo is the advisor session a Director consults with `consult_goinkyo`. It runs
as its Director's own product, so a Codex Director's Goinkyo is Codex by default
(`goinkyo_agent`); `goinkyo_agent = "claude"` keeps it on Claude. A Codex Goinkyo runs
read-only with no approval prompts, so it cannot build or run tests. The first
consultation in a directory Codex has not trusted waits at Codex's own trust question
until you answer it in the Goinkyo's pane.

The Planner is a Claude session the Director opens with `open_planner` when you ask for
work that spans several errands and whose scope or completion conditions are not settled
in your words. It works the Plan out with you in its own pane, reads the code with
`Read`, `Grep` and `Glob` only, and writes each draft with `write_plan`. Nothing is
settled until you read a draft on the reading screen (`F5`) and press `A` (Shift) to
approve the version on screen; if the Planner has written a newer version since you
opened the screen, banto refuses and tells you to read it first. Approval sends the
Director a message from banto, which also says whether the Plan goes to the Goinkyo for
review, by `goinkyo_consult`; the Director splits the approved Plan into errands.
Starting no Worker on a draft is an instruction in the `read_plan` tool's description,
not a check banto makes. The Director ends the Plan with `close_planner`, and banto
archives the Planner's session and keeps a Markdown export of the Plan. The Planner
always runs on Claude, whatever the Director is.

Each member also gets a role briefing naming its brigade, token and peers (appended
to the system prompt for Claude Code, delivered by banto's own `SessionStart` hook
for Codex). A relayed message is labeled as coming from another AI, never as
operator input. banto's auto-relay wakes idle members with unread messages by using a
Claude Code wake hook, or by queuing a short fixed nudge to a Codex thread with
`codex queue`, so a Director→Worker→Director round trip needs no human in between.
With `relay = "auto"` that wake starts a turn over the member's whole context, so a
message can cost an idle member a turn even when it asks for nothing. This repository
was largely built through that loop.

When every live member has been idle for 5 seconds after real work, the brigade
has gone quiet: your input is wanted. banto marks the Director (a waiting dot on
its list row, a magenta pane border) and rings the terminal bell, except while you
are looking at the Director's pane. `F4` rings even then (bell only, no mark).

A session that belongs to no brigade and sits open in a pane is told the same way:
once it has been idle for 5 seconds after real work, its list row gets the waiting
dot and the bell rings, except while you are in its pane. A session opened only to
read never rings. A Claude session whose turn has ended but whose background command
still runs counts as quiet; Codex cannot make that distinction, so a Codex pane with a
background terminal counts as working.

Before the first Codex brigade forms, banto opens a one-time approval pane: Codex
reviews banto's hook for trust once, at its own startup, and later brigades find it
trusted.

## Why not just tmux?

tmux is a window manager for terminals. banto is a lifecycle manager for Claude
Code and Codex sessions: it knows their ids, their directories, which are still
running, and how you filed them. Picking a row returns you to one specific
conversation, or to the process already running it, not to a shell that happens
to run `claude`. banto hosts the panes itself because a multiplexer cannot carry a
session list beside the panes, sessions that know their brigade, or relayed messages.

## Why not just herdr?

[herdr](https://github.com/herdrdev/herdr) is a terminal multiplexer built for
coding agents: a background server owns the panes, you can detach and come back, and
it shows which of more than twenty agents is working, blocked or idle. If you want
many agents of any kind kept alive across detaches, machines and SSH, herdr does that
and banto does not. banto's subject is the session: it reads Claude Code's and
Codex's own files read-only, so it knows sessions started before banto or outside it,
and it will not resume one conversation twice. It also relays messages between
Director/Worker sessions itself, where herdr hands scripts and agents a CLI and socket
API. The trade: banto speaks two agents and has no server that outlives it.

## Status & caveats

- A personal tool, pre-1.0, moving fast. Interfaces and the on-disk schema migrate
  forward but nothing is promised yet. Before the UI migrates a database it copies
  it to `banto.db.before-v<N>` beside it (the database as it was at schema `N - 1`;
  `banto --version` shows a build's schema). An older build cannot open a database a
  newer one has migrated: restore a copy, or run the newer build.
- banto has no supervisor, by design. Sessions hosted in banto's panes end when banto
  does, each after an ordinary console-close so `claude` finalizes its history as on
  any window close. banto is a workbench you sit at, not a daemon that keeps work
  running behind you.
- Claude Code's session-file formats are undocumented and may change; banto
  parses leniently, but a future release could still surprise it.

## Read-only guarantee

**banto never writes anything under `~/.claude` or `~/.codex`.** Session files,
live-state files, history, Codex's `threads` and `logs` sqlite databases: all
strictly read-only. banto's own files live under:

- Config: `dirs::config_dir()/banto/config.toml` (Windows: `%APPDATA%\banto\`) by
  default; see [Configuration](#configuration) for the resolution order.
- Data (sqlite and working files): `dirs::data_local_dir()/banto/banto.db`
  (Windows: `%LOCALAPPDATA%\banto\`) by default; `data_dir` in the config or
  `$BANTO_DATA_DIR` names another place. A data directory or `db_path` inside
  `~/.claude` or `~/.codex` (by any name, links included) is refused: the UI and
  `banto _mcp` fail to start, and the agent hook silently opens no store.

One banto UI per database: a second `banto` that would open a database a running
one already uses refuses to start, naming the database and, when it can, the
holder's process id (the lock is `banto.db.lock` beside the database). To run a
second one, give it its own data directory or database (`BANTO_DATA_DIR`,
`data_dir`, or `db_path`).

There is one named exception. Reading a Codex sqlite database (`state_5.sqlite` for the
session index, `logs_2.sqlite` for liveness) while Codex might be writing it uses
SQLite's own read-only machinery: `mode=ro` when a `-wal` file exists, `immutable=1`
when it does not. Crash residue, a `-wal` left behind without its `-shm`, is the one
case that is not free: the first `mode=ro` open recreates a fresh `-shm`, a single
~32KB write. It is SQLite's own coordination index, not banto's data, and Codex's
next run would create it anyway. It is named here because "banto never writes"
should mean what it says everywhere.

## Install & upgrade

You need `claude` (Claude Code) and/or `codex` on your `PATH`. banto
hosts the real CLIs and does not bundle them. If either lives somewhere else,
name it in `config.toml`:

```toml
[agent_binaries]
claude = "C:/tools/claude.exe"
codex = "C:/tools/codex.exe"
```

banto runs on Windows x64, Linux x64 with glibc 2.35 or newer (the release is
built on Ubuntu 22.04, and the build fails if the binary needs a newer glibc;
musl systems such as Alpine build from source), and macOS on Apple silicon. Intel
Macs and Windows on ARM have no prebuilt binary.

macOS is in daily use. Since 2026-11-26 the maintainer's everyday banto has
been a build of main on an Apple silicon Mac, in Ghostty 1.4.0. Resume (across banto
restarts too), brigades with Claude and Codex Workers and a Goinkyo, the list
following session files, the pane keys (newline, word delete and move, the prefix),
copying text out of a pane, and the bell and the desktop notification work there;
Ghostty needs `bell-features = system` for the bell to make a sound. v0.1.0 and
v0.3.0 installed with `install.sh` with no Gatekeeper prompt. Unchecked: a
browser-downloaded binary (Gatekeeper), the installers' refusal to replace a running
banto, terminals other than Ghostty, and the pane hang-up and force-kill timings.

### One-line install

Windows: type this into an open PowerShell (Windows PowerShell 5.1 or PowerShell
7; no `-ExecutionPolicy` wrapper is needed):

```powershell
irm https://github.com/kt81/banto/releases/latest/download/install.ps1 | iex
```

Linux and macOS:

```sh
curl -fsSL https://github.com/kt81/banto/releases/latest/download/install.sh | sh
```

The installer downloads the archive for your platform and `SHA256SUMS` from the
release and installs nothing if they disagree. (The sums come from the same release,
so this shows the download arrived intact, not who built it; see the attestation
under [Install by hand](#install-by-hand).) It runs the new binary once before replacing anything,
so a build that cannot start here leaves your current banto untouched. It refuses to
overwrite a `banto` that is not a banto, and warns when more than one is on `PATH`:
only the first is replaced. It never edits your `PATH` or any shell profile; when
the directory is not on `PATH` it prints the one line that adds it.

banto lands where the `banto` on your `PATH` already is; failing that in
`BANTO_INSTALL_DIR`; failing that in `%USERPROFILE%\.local\bin\banto.exe` on Windows
or `~/.local/bin/banto` elsewhere. In PowerShell, set these two variables first in
the same window; in a Unix shell, put them on the `sh` side of the pipe:

```powershell
$env:BANTO_VERSION = 'v0.1.0'            # a release tag instead of the latest
$env:BANTO_INSTALL_DIR = 'C:\tools\banto' # where, if no banto is on PATH yet
irm https://github.com/kt81/banto/releases/latest/download/install.ps1 | iex
```

```sh
curl -fsSL https://github.com/kt81/banto/releases/latest/download/install.sh | BANTO_VERSION=v0.1.0 BANTO_INSTALL_DIR="$HOME/bin" sh
```

### Install by hand

Download the archive for your platform (`banto-<tag>-x86_64-pc-windows-msvc.zip`,
`banto-<tag>-x86_64-unknown-linux-gnu.tar.gz` or `banto-<tag>-aarch64-apple-darwin.tar.gz`)
and `SHA256SUMS` from the [Releases page](https://github.com/kt81/banto/releases).
Compare the archive's sha256 with its line in `SHA256SUMS` (`(Get-FileHash <archive>).Hash`
in PowerShell; `grep <archive> SHA256SUMS | sha256sum -c -` on Linux,
`grep <archive> SHA256SUMS | shasum -a 256 -c -` on macOS), then extract the single
`banto` / `banto.exe` into the one directory banto lives in. The binaries are not
signed: a `banto.exe` downloaded with a browser and double-clicked in Explorer meets
SmartScreen's "Windows protected your PC", and "More info" → "Run anyway" starts it.
Started from a terminal, or installed with `install.ps1`, v0.1.0 met no prompt on the
maintainer's machine.

Each archive also carries a build-provenance attestation that says which commit and
workflow built it, and it is the only check here that says where the file came from.
With the [GitHub CLI](https://cli.github.com/), run it before you unpack, since it
verifies the archive, not an exe extracted from it:

```sh
gh attestation verify banto-v0.1.0-x86_64-pc-windows-msvc.zip --repo kt81/banto
```

### Install from source

```sh
cargo install --git https://github.com/kt81/banto banto --locked
```

This needs a recent stable Rust toolchain (edition 2024) and a C compiler (the
bundled SQLite is compiled from C), and installs into `~/.cargo/bin`.

For development, from a checkout:

```sh
cargo build --release

# The UI: the session list on the left, banto-hosted panes on the right
cargo run --

# Plain-text session listing
cargo run -- list

# Point at a different Claude home
cargo run -- --claude-home /path/to/.claude
```

### One banto per machine

Keep exactly one banto binary, at one path, with no space in it. Codex trusts
banto's `SessionStart` hook by its full command, which contains banto's own path, so
a copy at another path is a different hook to Codex (what that does to the first
copy's trust is inferred from how the trust hash is built, not observed), and a path
with a space cannot be written into the hook at all (Codex members would launch
without their role briefing). That is why the installer always replaces in place, and warns about a
space or a second banto on `PATH` without refusing. Replacing at the same path kept
the trust in the one case watched (2026-11-18); the evidence is in
[the note](docs/notes/codex-briefing-spike.md#addendum-2026-11-18-replacing-banto-at-the-same-path-keeps-the-trust).

### Upgrading

Quit banto, run the same install line, start banto again (both installers refuse
while a banto is running from the target file; untested on macOS). The first start
migrates the database in place after copying it to `banto.db.before-v<N>` (see
[Status & caveats](#status--caveats)); going back needs the older build and that copy.

### Windows Defender

On 2026-11-06 Microsoft Defender flagged a locally built release `banto.exe` as
`Trojan:Win32/Bearfoos.A!ml` (a machine-learning detection) and remediated it, which
ended banto and every session it hosted ([account](docs/notes/windows-defender.md)).
If it flags a release you downloaded, compare its sha256 with `SHA256SUMS` and
[open an issue](https://github.com/kt81/banto/issues) with the detection name and
your definitions version. Please do not add an exclusion: it would also hide a file
that really had been tampered with.

### Uninstall

Delete the one `banto` binary (`where.exe banto` / `command -v banto` finds it). To
remove banto's data too, delete its config directory and its data directory
(`banto.db`, its lock files `banto.db.lock` and `banto.db.lock.pid`, the
`banto.db.before-v<N>` copies, and the working files); `data_dir`, `BANTO_DATA_DIR`
or `db_path` may have put them elsewhere. Sessions stay where their agents keep them,
and Codex's `trusted_hash` for banto's hook in `<codex home>/config.toml` is harmless.

## Keys

`q`/`Esc` quits at once when every hosted session is known to be idle, or none is
hosted. Otherwise it asks first, naming each session it counts and why in the status
bar, and a second `q`/`Esc` within 3 seconds quits, since quitting ends every hosted
process. Each session is then closed in turn and given up to 5 seconds to finish.

| Key | Action |
|---|---|
| `j`/`k`, arrows, `PgUp`/`PgDn`, `Home`/`End` | Move selection |
| `Enter` / double-click | Open the session in a pane (switch to it if it is already open); on a Director, stage its whole brigade |
| `n` / `/` | New session (pick or type a cwd) / search (fuzzy, title + cwd) |
| `p` / `d` / `g` / `D` | Pin / archive (soft-hide) / join group / restore the selected archived session |
| `Tab` / `a` | Toggle grouped view (Pinned / groups / Ungrouped) / cycle what the list reveals: ordinary sessions → also hidden ones (agent-run) → also archived ones |
| `B` / `b` | Appoint Director + auto-spawn Workers (on a Director: disband) / spawn one more Worker into the staged brigade |
| `x` | End what the selected row runs (confirm). On a Director: close its brigade: every process of the brigade ends, nothing is deleted, and Enter on the Director's row brings it all back. On a single session with an open pane: end its process. It never dismisses a Worker; use `Ctrl+B x` on that Worker's tile |
| `F2` / `F3` / `F4` | Move focus between the list and the panes / make the next pane of the staged brigade the current one / ring the bell even while the pane that went quiet is focused, a Director's or a lone session's (toggle) |
| `F5` | Read a brigade's open decision, Progress and Plan on a read-only screen (`A` approves the Plan's draft on screen), from the list (the selected row's brigade, else the staged one) or from a pane (the staged one). Arrows, `j`/`k`, `PgUp`/`PgDn`, `Home`/`End` scroll; `Esc` closes it; with a decision open, `Enter` closes it and goes to the Director's pane. Opening the screen starts and resumes nothing |
| `Ctrl+B` … | tmux-style prefix (configurable), from the list or a pane: focus, tile order, collapse, solo, jump to pane N. While it is armed, the status bar shows the full binding table |
| … `x` | dismiss the focused Worker/Goinkyo from its brigade (confirm; it leaves for good, its session is kept); on a Director or a single session, the same as `x` on the list |

In a pane, `Ctrl+J` is the newline that works in every terminal. `Shift+Enter` works where the terminal reports the Shift, and not where it sends a bare CR for it. `Alt+Enter` (`Option+Enter` on macOS) is a newline too, where the terminal sends it with the Alt.

With the mouse, drag a boundary between panes or tiles to resize, drag a stack tile
by its title to move it, and use `[S]` (solo) or `[_]`/`[+]` (collapse/open) in a
tile's title row. That row also shows the agent and whether it is busy, waiting or
idle.

## Configuration

`config.toml` is optional, and a missing one means defaults. A file that is there
must parse (valid TOML, and a known key given a value of the right type that it
takes: `relay = "typo"` is refused like `relay = 5`), or banto prints the error,
with its line and column, and does not start; unknown keys are ignored. The first hit wins:
`--config <path>` or `$BANTO_CONFIG` (both must also exist), then
`$XDG_CONFIG_HOME/banto/config.toml` and `~/.config/banto/config.toml` when they
exist, then the default `dirs::config_dir()/banto/config.toml` (`%APPDATA%\banto\`
on Windows).

```toml
claude_home = "C:/Users/you/.claude"   # optional override
db_path = "..."                        # optional override; an empty value is a startup error
# Where banto keeps its database and working files; `$BANTO_DATA_DIR` wins. Keep it
# above the first [section], or it is read as brigade.data_dir and silently ignored.
# Changing it moves nothing: banto opens an empty database in the new place.
data_dir = "D:/banto-data"

# "all" (default) | "claude" | "codex" | "claude,codex": which products banto indexes.
agents = "all"

[activity]
today_hours = 24   # activity-dot bucketing
week_days = 7

[brigade]           # Director/Worker brigades
workers = 1         # Workers auto-spawned per brigade (1..=8)
worker_agent = "inherit"   # "inherit" (the Director's product) | "claude" | "codex" | "select" (a picker: agent, model, effort)
# A Worker's auto-compact window, passed on every launch: "auto", an integer
# 100000..=1000000, or "" (no flag). Codex takes only a token count. Directors and Goinkyo never get it.
worker_autocompact = 250000
relay = "auto"      # "auto" | "manual" — the wake-up nudge engine
# Append every brigade message, in full, to <data dir>/logs/brigade-<id>.log. The
# file holds real conversation content and is never rotated or deleted: don't commit it.
message_log = false
# Role briefings for each member. {brigade}, {token} and {peers} are substituted;
# "" launches with no briefing. The defaults are policy you may change.
director_prompt = "..."
worker_prompt = "..."
# How readily a Director consults the Goinkyo unprompted: "off" (only when you ask)
# | "disputes" | "design" | "decisions", each adding to the one before. It is only
# an instruction in the Director's briefing, and does nothing if director_prompt is empty.
# A Claude Director needs a banto restart after an edit; a Codex Director's hook re-reads
# it at its next session start. A running Director never sees a change: tell it in its pane.
goinkyo_consult = "design"
# Which product a Goinkyo runs as: "inherit" (its Director's own) | "claude" | "codex".
# Its model and effort are goinkyo_model and goinkyo_effort under [brigade.codex].
goinkyo_agent = "inherit"
# When a Director opens a Planner: "auto" (when work spans several errands and its scope
# or completion is not settled in your words) | "off" (only when you ask). Like
# goinkyo_consult, it does nothing if director_prompt is empty.
planner = "auto"
# The Planner's role text, which replaces Claude's own system prompt along with a paragraph
# banto always adds; {brigade} is substituted, and "" leaves that paragraph alone. The
# default is in English and tells the Planner to answer in your language.
planner_prompt = "..."
# A Director's request_decision also marks its row and rings. false: the tag only.
request_attention = true

# What only one product reads lives in a table of its own, below every plain [brigade]
# key above: a plain key written under one belongs to that table, and banto refuses to
# start on one of its own keys written there. Model, effort and permission mode go to a
# NEW Worker or Goinkyo, only at creation; "" passes no flag. The old flat spellings under
# [brigade] still work, and the table wins when both are set.
[brigade.claude]
worker_model = "sonnet"          # --model
worker_effort = "high"           # --effort (low | medium | high | xhigh | max)
worker_permission_mode = "auto"  # --permission-mode
# Fable and max are opt-ins, not the built-ins: Fable bills usage credits on some
# plans, and Claude cannot save max, so a restart drops it.
goinkyo_model = "opus"
goinkyo_effort = "xhigh"
goinkyo_permission_mode = "auto"
planner_model = "opus"
planner_effort = "xhigh"
planner_permission_mode = "auto"
planner_tools = "Read,Grep,Glob"   # --tools: the built-in tools it gets, besides banto's own

[brigade.codex]
worker_model = "terra"           # a slug, or a family word resolved to its newest slug
worker_effort = "high"           # -c model_reasoning_effort
goinkyo_model = ""               # a Codex Goinkyo's model; "" = Codex's own default
goinkyo_effort = ""              # a Codex Goinkyo's -c model_reasoning_effort; "" = none

[keys]
prefix = "C-b"      # pane prefix chord; a backslash is written 'C-\' or "C-\\" in TOML
                    # on macOS/Linux, portable chords are a-z except i and m, space, \ ] ^ _ and their digit twins 4-7

[attention]
notify = "osc777"   # a desktop notification (OSC 777) with each bell; terminals without it ignore it. "off": the bell only
```

When banto writes a notification, the banner names the session or brigade by its title, and the terminal needs the OS's notification permission.

`--claude-home`/`--codex-home` and the `claude_home`/`codex_home` keys change which home banto lists and resumes from. They do not change what the agent in a pane reads, because banto never sets `CLAUDE_CONFIG_DIR` or `CODEX_HOME`: that agent uses the variable, or `~/.claude` / `~/.codex`. When the two differ, or the variable is a relative path, banto says so once at startup on the status line, naming both locations.

## Further reading

[docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) holds the design and requirements,
[docs/DISCIPLINE.md](docs/DISCIPLINE.md) the architecture discipline (a pure core,
I/O at the edges), [docs/notes/](docs/notes/) the on-device verification notes, and
[tools/measure/](tools/measure/README.md) Python scripts that measure what a team of
agents spends and how its changes fare in review (not part of the build).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
