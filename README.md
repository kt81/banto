# banto (番頭)

[日本語](README.ja.md)

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
stays cross-platform (CI builds and tests on Windows, Ubuntu and macOS), and
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

## Why not just herdr?

[herdr](https://github.com/herdrdev/herdr) is a terminal multiplexer built for
coding agents: a background server owns the panes, you detach and come back
later, and it shows which of more than twenty agents is working, blocked or
idle. If what you want is many agents of any kind kept alive across detaches,
machines and SSH connections, it does that, and banto does not.

banto's subject is the session, not the terminal. It reads Claude Code's and
Codex's own files, read-only, so it knows sessions you started before banto
existed or outside it, which of them are still running, and which one you
meant; it never writes to either agent's home, and it will not resume one
conversation twice. herdr learns a session's id from the agent running in its
own pane, through a hook its integration installs in the agent's
configuration. banto also hosts Director/Worker cells in which it relays
messages between the sessions itself, where herdr hands scripts and agents a
CLI and socket API to drive each other. The trade: banto speaks two agents and
has no server that outlives it.

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

![banto with a staged cell: the ledger on the left — two pinned rows, a 🤝
Director and a Codex session, above sessions filed in groups — and the cell on
the right: the Director in the large pane, one Worker beside it, and two more
Workers collapsed to one-line strips that still show their latest
message](docs/assets/emporium-brigade.png)

*A live cell at work (on WSL): the Director lays out what each test protects
while a Worker reports its fix; two more Workers are collapsed to strips. This
screenshot is itself dogfooding — the cell shown is fixing a banto bug.*

`B` appoints the selected session as a **Director** and
auto-spawns fresh **Worker** sessions beside it (count, model, and even
product all configurable — a brigade's Workers can run Claude Code or Codex
independently of the Director's own product). A Worker's model and reasoning effort
are given when its session is created and never when it is reopened, so a change you
make inside a session survives banto's restarts. banto mediates a message
channel between them over MCP: it wires every member back to `banto _mcp` on
its own launch — Claude Code via `--mcp-config`, Codex via `-c
mcp_servers.banto.*` overrides — so members get `send_to_peer` /
`check_messages` / `brigade_status` tools backed by banto's own sqlite
queue (plus Director-only `consult_goinkyo` / `dismiss_goinkyo`,
`request_decision` (the Director is waiting on a decision only you can make: it
writes the question in its own pane and banto shows a `▸ decision` tag against it,
without stopping it, until you press Enter in its pane),
`interrupt_worker` (the Director has changed its mind about a Claude Worker's
task: it stops the Worker's running turn with one Esc and hands over the reason
as an ordinary message — banto presses only mid-turn and shows each press on the
status line; a Codex Worker gets the message but is not interrupted yet), and
`name_member`, which gives a Worker a display name for the standing role the
Director has given it — shown beside its token in the tile title, in
`brigade_status` and in message headers; the token stays the address). Each member is also handed a role briefing naming its brigade, its
token, and its peers — appended to the system prompt on launch for Claude
Code (on a resume, with `--system-prompt-snapshot off` when the session's own
record of its system prompt would otherwise be reused; that flag needs Claude
Code 2.1.267 or later), delivered by banto's own `SessionStart` hook for Codex,
which has no `--append-system-prompt` of its own — without one a cell exists
only in banto's data model and the operator's screen, and a Director handed
three tool names and no context mostly never uses them; `brigade_status` answers
the follow-up question (who is on my team, what are they doing, is anyone
holding my mail). A message's text is pulled, with per-member cursors and firewall
framing (a relayed message is labeled as coming from another AI, never
mistaken for operator input). banto's **auto-relay** watches for idle members
with unread messages and wakes them by typing a short fixed nudge into their
stdin — so a Director→Worker→Director round trip needs no human ferrying at all.
With the relay on (`relay = "auto"`), that wake starts a turn over the member's
whole context, so a message can cost an idle member a turn even when it asks for
nothing. This repository was largely built through that loop.

When every live member of a brigade (the Director, the Workers, a Goinkyo) has been
idle for 5 seconds after any of them did real work (starting up is not work) —
counted from the last member going idle or banto last prompting one — the brigade
has gone quiet — the moment your input is wanted. banto then marks the Director (the
waiting dot on its row in the list, counted in the sidebar's `waiting N`, and the
magenta border a waiting session gets on its pane, though a focused pane keeps its
focus colour) and rings the terminal bell. It stays silent while you are looking at
the Director's pane (that pane selected and banto's window focused); with the window
in the background it marks and rings. A member whose state banto cannot read keeps
it quiet, as does a new Codex Worker or Goinkyo that banto has yet to start, as does
a member with an unread report still on its way. A brigade that is not on screen
reaches you too: its fire marks its Director's row and rings, even while you work in
another brigade, and a member of it that is waiting on you (or whose child rang the
bell) marks that row and rings once. The mark ends when you reach that Director's
pane, and opening the brigade does not ring again. A picker you open yourself in a
Claude pane (a slash command such as `/model`) shows as waiting but does not ring,
and keeps its brigade from counting as quiet while it is open. `F4` switches on
"ring even while the Director's pane is focused" (the bell only, no mark); the
status bar shows `BELL ALWAYS` left of the counts while it is on, and the setting is
remembered across restarts.

Forming your first Codex brigade needs one thing from you: Codex reviews
banto's own hook for trust at its own startup, once — not per launch — so
banto opens a one-time approval pane for you to clear before that first cell
forms. Approve it once and every brigade launch after that finds it already
trusted.

## Status & caveats

- A personal tool, pre-1.0, moving fast. Interfaces and the on-disk schema
  migrate forward but nothing is promised yet. Before the UI migrates a database it
  copies it to `banto.db.before-v<N>` beside it (the database as it was at schema
  `N - 1`; `banto --version` shows the schema a build carries), and an older build
  cannot open one a newer build has migrated: restore a copy, or run the newer build.
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
- Data (sqlite and working files): `dirs::data_local_dir()/banto/banto.db`
  (Windows: `%LOCALAPPDATA%\banto\`) by default; `data_dir` in the config or
  `$BANTO_DATA_DIR` names another place (see [Configuration](#configuration)).
  A data directory or `db_path` inside `~/.claude` or `~/.codex` (reached by
  any name, links included) is refused: the UI and `banto _mcp` fail to start,
  and the agent hook silently opens no store.

Even the MCP wiring honors this: member `--mcp-config` files are written
under banto's data dir and passed by argv, never installed into Claude's own
configuration.

One banto UI per database: a second `banto` that would open a database a running
one already uses refuses to start, naming the database and, when it can, the
holder's process id (the lock is `banto.db.lock` beside the database and goes the
moment its holder exits, however it ends). To run a second banto, give it its own
data directory or database: `BANTO_DATA_DIR`, `data_dir` in the config, or
`db_path` (the hidden `--data-dir` / `--db-path` flags work too). A filesystem
that does no locks cannot be checked; banto then starts and says so on the status
line.

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

## Install & upgrade

**What you need.** `claude` (Claude Code) and/or `codex` on your `PATH` — banto hosts
the real CLIs and does not bundle them. If either lives somewhere else, name it in
`config.toml`:

```toml
[agent_binaries]
claude = "C:/tools/claude.exe"
codex = "C:/tools/codex.exe"
```

**Platforms.** Windows x64, Linux x64 with glibc 2.35 or newer (the release is built on
Ubuntu 22.04, and the build fails if the binary needs a newer glibc; musl systems such
as Alpine build from source), and macOS on Apple silicon. Intel Macs and Windows on ARM
have no prebuilt binary. Windows is where banto is developed and used every day.

**macOS is lightly checked.** On one Apple silicon Mac, v0.1.0 installed with
`install.sh` and nothing stopped it (no Gatekeeper prompt); sessions opened and closed in
panes, and text selected in a pane copied out. Resume, the list following session-file
changes, and input in detail have not been checked there yet, nor a binary downloaded
with a browser.

### One line

Windows — type this into an open PowerShell (Windows PowerShell 5.1 or PowerShell 7;
no `-ExecutionPolicy` wrapper is needed):

```powershell
irm https://github.com/kt81/banto/releases/latest/download/install.ps1 | iex
```

Linux and macOS:

```sh
curl -fsSL https://github.com/kt81/banto/releases/latest/download/install.sh | sh
```

The installer downloads the archive for your platform and `SHA256SUMS` from the release,
checks the archive against it, and installs nothing if they disagree. (`SHA256SUMS`
comes from the same release as the archive, so this check shows the download arrived
intact, not who built it; for that, see the attestation under [By hand](#by-hand).) It
runs the new binary once before it replaces anything, so a build that cannot start on
your machine (on Linux, a glibc older than 2.35) leaves the banto you had untouched. Any
`--version` run it makes is cut off after 10 seconds, with or without a `timeout` command on
the system, so a banto that hangs is refused rather than waited for. Staging files an earlier killed install left in the target
directory (`.banto.<random>`, `.banto-<hex>.exe`, over an hour old) are removed. It
refuses to overwrite a `banto` that is not a banto, whether it is the one on your `PATH`
or a file already at the install location (a wrapper script, a package manager's shim:
it must run and answer `--version` with `banto ...`), and warns when more than one banto
is on `PATH`: only the first is replaced and the others are left for you to remove. It
prints `banto --version` before and after (`(not installed)` before a first install). It never edits your `PATH` or
any shell profile: when the directory is not on `PATH` it prints the one line that adds
it.

Where banto lands, in this order:

1. the `banto` already on your `PATH`, replaced where it is;
2. `BANTO_INSTALL_DIR`, when no banto is on `PATH` yet;
3. `%USERPROFILE%\.local\bin\banto.exe` on Windows, `~/.local/bin/banto` elsewhere.

Two environment variables change what the line does. In PowerShell, set them first in
the same window; in a Unix shell, put them on the `sh` side of the pipe:

```powershell
$env:BANTO_VERSION = 'v0.1.0'            # a release tag instead of the latest
$env:BANTO_INSTALL_DIR = 'C:\tools\banto' # where, if no banto is on PATH yet
irm https://github.com/kt81/banto/releases/latest/download/install.ps1 | iex
```

```sh
curl -fsSL https://github.com/kt81/banto/releases/latest/download/install.sh | BANTO_VERSION=v0.1.0 BANTO_INSTALL_DIR="$HOME/bin" sh
```

### By hand

Download the archive for your platform from the
[Releases page](https://github.com/kt81/banto/releases) (`banto-<tag>-x86_64-pc-windows-msvc.zip`,
`banto-<tag>-x86_64-unknown-linux-gnu.tar.gz`, `banto-<tag>-aarch64-apple-darwin.tar.gz`)
together with `SHA256SUMS`. Compare the archive's sha256 with its line in `SHA256SUMS`
(`(Get-FileHash <archive>).Hash` in PowerShell; `grep <archive> SHA256SUMS | sha256sum -c -`
on Linux, `grep <archive> SHA256SUMS | shasum -a 256 -c -` on macOS), then extract the single `banto` / `banto.exe`
into the directory you chose. Make sure it is the one directory banto lives in (below).
The binaries are not signed, so a `banto.exe` downloaded with a browser and started by
double-clicking it in Explorer meets SmartScreen's "Windows protected your PC": "More info"
→ "Run anyway" starts it. Started from a terminal, or installed with `install.ps1`, v0.1.0
met no prompt on the maintainer's machine.

Each archive also carries a build-provenance attestation that says which commit and
workflow built it. With the [GitHub CLI](https://cli.github.com/):

```sh
gh attestation verify banto-v0.1.0-x86_64-pc-windows-msvc.zip --repo kt81/banto
```

That verifies the archive you downloaded, not an exe you extracted from it, so run it
before you unpack. It is the only check here that says where the file came from: the
`SHA256SUMS` check compares the archive with a list published in the same release, so it
only tells you the file arrived intact.

### From source

```sh
cargo install --git https://github.com/kt81/banto banto --locked
```

This needs a recent stable Rust toolchain (edition 2024) and a C compiler (the bundled
SQLite is compiled from C). It installs into `~/.cargo/bin`. A later run of the
installer above finds that copy on your `PATH` and replaces it where it is.

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

(`--emporium` and its alias `--oodana`, from when banto had two modes, are
still accepted, do nothing, and will be removed.)

### One banto per machine

Keep exactly one banto binary, at one path, with no space in it. Codex reviews banto's
`SessionStart` hook for trust by the hook's full command, which contains banto's own
path, so a copy of banto at another path is a different hook to Codex. What that does to
the trust of the first copy is inferred from how the trust hash is built, not observed
(see the note below). A path
containing a space cannot be written into the hook at all, so Codex members would
launch without their role briefing (Claude Code members are unaffected). Replacing the
file at the same path kept the trust in the one case that was watched (2026-11-18; see
[the note](docs/notes/codex-briefing-spike.md#addendum-2026-11-18-replacing-banto-at-the-same-path-keeps-the-trust)),
which is why the installer always replaces in place instead of installing a second copy.
The installer warns about a path with a space and about a second banto on `PATH`; it
refuses neither.

### Upgrading

Quit banto, run the same install line, start banto again.

- **Why quit first.** Both installers refuse while a banto is running from the target
  file. Windows would not let the file be replaced anyway. On Linux and macOS it can be
  replaced under a running banto, but that banto then refuses to open members
  (`banto was replaced on disk; restart banto`) until you restart it, because the
  helpers it launches would be a different build from the UI. (The release workflow
  exercises the running-banto refusal on Windows and Linux; on macOS that step is
  allowed to fail, because the `lsof`-based detection has not run there yet.)
- **The first start after an upgrade** migrates the database in place. Before it does,
  it copies the database to `banto.db.before-v<N>` beside it (the database as it was at
  schema `N - 1`; see [Status & caveats](#status--caveats)). An older build cannot open
  a database a newer one has migrated, so going back means the older build plus that
  copy. The upgrade to schema 19 also stops hiding the ancestors of compactions made
  before Claude Code 2.1.318: they show in the ordinary list beside their continuations.
- **One banto UI per database.** A second `banto` that would open a database a running
  one already uses refuses to start; see [Read-only guarantee](#read-only-guarantee)
  for how to run two.

### Windows Defender

On 2026-11-06 Microsoft Defender's real-time protection flagged a locally built release
`banto.exe` as `Trojan:Win32/Bearfoos.A!ml`, a machine-learning detection, and
remediated it. Remediation ends the process: banto and every session it was hosting
were closed at once. The debug build was not flagged, and the detection arrived with a
same-day definitions update. The full account is in [docs/notes/windows-defender.md](docs/notes/windows-defender.md).

What it means for you: a flag on a release build ends banto and its sessions, so it is
worth knowing it can happen. If Defender flags a banto you downloaded from a release,
compare its sha256 with `SHA256SUMS` (and, with `gh`, run the attestation check above),
and [open an issue](https://github.com/kt81/banto/issues) with the detection name and
your definitions version. Please do not "fix" it by adding an exclusion: an excluded
directory would also hide a file that really had been tampered with.

### Uninstall

Delete the one `banto` binary (`where.exe banto` / `command -v banto` finds it). To
remove banto's data as well, delete its config directory (`config.toml`) and its data
directory (the database `banto.db`, its lock files `banto.db.lock` and
`banto.db.lock.pid`, the `banto.db.before-v<N>` copies, and the working files: MCP
configs, consultation requests, message logs) — the locations are under
[Read-only guarantee](#read-only-guarantee) and [Configuration](#configuration);
`data_dir`, `BANTO_DATA_DIR` or `db_path` may have put them elsewhere. Sessions stay
where their agents keep them.

banto writes nothing into `~/.claude` or `~/.codex` (or `$CODEX_HOME`), with the one named
exception under [Read-only guarantee](#read-only-guarantee): reading a Codex database
can leave a ~32 KB `-shm` file beside it. One record there does concern banto, though not banto's doing: when you approve banto's Codex hook (the trust
prompt that appears when a brigade forms), Codex itself records the approval as a
`[hooks.state.'…<session-flags>…:session_start:0:0']` table with a `trusted_hash` in
`<codex home>/config.toml`. It is harmless to leave; it only says that hook was approved.
To remove it, delete that table by hand in Codex's `config.toml`. (Deleting it has not
been tried here, so what Codex then does is unverified; banto's own check would simply
read the hook as not yet approved.)

## Keys

One table. The first group is the list; the rest is what the panes add.

`q`/`Esc` quits at once when every hosted session is known to be idle, or none is
hosted. Otherwise it asks first, and a second `q`/`Esc` within 3 seconds quits,
since quitting ends every hosted process. The status bar names each session it
counts and why, for example `Esc again to quit — 2 not idle: レビュー (unknown), 実装
(busy)`. The reasons: `unknown` (nothing could be read of the pane, or it is on a
dialog), `input` (given a key or a paste by you, or something typed by banto, within
the last 3 seconds; clicking or scrolling does not count), and `busy` (read as
working or waiting, or as a status banto does not recognise). A Claude Code session
whose turn has ended but that left a command running in the background is counted
too: it is `(bg)` beside sessions that are not idle (`2 not idle, 1 in background:
a (unknown), b (busy), c (bg)`), and `2 in background: x, y` when it is alone. The
count comes first, then the names: unknown first, because no tile marks an unknown
pane, then input, busy and background, each group by name. A Director is named by
its list row's title, else its token; a Worker or Goinkyo by its display name, else
its token; any other pane as the list shows it (a placeholder not yet resolved to a
session reads `new session`). A name is cut to 11 columns and gets a `#…` suffix when
two would read the same (the whole-id suffix, the last resort, may go past 11). The
question is one status line, so names past the first may be cut off; the count in
front is always the whole
count. A session is counted once, as not idle when it was given work in the last 3
seconds. A Claude Code session reports its own state, and so does a Codex one:
banto reads the status report the Codex interface writes to its terminal (working,
blocked on you, done or idle) and, until a session has made one, its last turn in
the rollout. Codex records nothing about a command it left running in the
background, so a Codex session that has not reported reads as idle. Once quitting is
decided the status bar reads `Closing — ending N session(s)…` (`Closing…` with none)
until banto exits: each session is closed in turn and given up to 5 seconds to
finish.

| Key | Action |
|---|---|
| `j`/`k`, arrows, `PgUp`/`PgDn`, `Home`/`End` | Move selection |
| `Enter` / double-click | Open the session in a pane (switch to it if it is already open); on a Director, stage its whole cell |
| `n` | New session (pick or type a cwd) |
| `/` | Search (fuzzy, title + cwd) |
| `p` / `d` / `g` | Pin / archive (soft-hide) / join group |
| `Tab` | Toggle grouped view (Pinned / groups / Ungrouped) |
| `a` | Cycle what the list reveals: ordinary sessions → also hidden ones (agent-run) → also archived ones |
| `D` | Restore (un-archive) the selected session, while archived ones are revealed |
| `q` / `Esc` | Quit (a second press is needed while a session is not idle or is running a background command, see above) |
| `B` | Appoint Director + auto-spawn Workers (on a Director: disband) |
| `b` | Spawn one more Worker into the staged cell |
| `x` | End what the selected row runs (confirm). On a Director: **close its brigade**, on stage or not — every process of the cell ends and nothing is deleted, so Enter on the Director's row brings it all back. On a single session with an open pane: end its process. It never dismisses a Worker; use `Ctrl+B x` on that Worker's tile |
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
| … `x` | dismiss the focused Worker/Goinkyo from its brigade (confirm; it leaves for good, its session is kept); on a Director, close the whole brigade (confirm; every process ends, nothing is deleted, Enter on the Director's row resumes it); on a single session, end its process |
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

Every tile's title row says what its session is doing, minimised or not: **busy** (a
turn is running), **waiting** (it needs you) or **idle** (running, nothing to do),
then how long ago it last wrote. A Claude Code session whose turn has ended but that
has a command still running in the background reads `idle · bg`: the session list
still draws it as busy, and quitting still asks about it. A session banto has read
nothing of yet shows no word. It is the same for Claude and Codex: a session waiting
at its prompt reads idle. A Codex session's word comes from the status report its
interface writes to its terminal; a Claude Worker that has not run a turn yet shows
its state from its live file. On a narrow tile the title gives way: the agent name
goes first, then the age is shortened (`2m`) and finally dropped; the word stays
(with its `bg`, when it has one), and the label never gives way, so at the default
minimised width a long display name
leaves room for the word only. A minimised tile is four rows (its title row, the
last message it was sent and the last it sent, and the bottom border), so five fit
on a 24-row terminal. A tile too small for a screen shows the top-left corner of a
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
# Where banto keeps its database and working files (MCP configs, consultation
# requests, message logs). Optional; an empty value is a startup error, and so
# is a path inside the Claude or Codex home. `$BANTO_DATA_DIR` wins over it.
# It must stay up here, above the first [section]: under [brigade] it would be
# read as brigade.data_dir and silently ignored. Changing it moves nothing —
# banto opens an empty database in the new place.
data_dir = "D:/banto-data"

# "all" (default) | "claude" | "codex" | "claude,codex" — which agent
# products banto discovers sessions for. A disabled product's own files
# (Codex's sqlite database, in particular) are never even opened.
agents = "all"

[activity]
today_hours = 24   # activity-dot bucketing
week_days = 7

[brigade]           # Director/Worker cells
workers = 1         # Workers auto-spawned per cell (1..=8)
worker_agent = "inherit"   # "inherit" (the Director's product) | "claude" | "codex" | "select" (a picker: agent, model, effort)
# Model and effort of a NEW Worker. They are passed only when a Worker's session is
# created, never when it is reopened: a reopened session keeps what it last used,
# including what you changed inside it. Values are trimmed; "" (or only spaces)
# passes no flag. The defaults shown name no version.
worker_model = "sonnet"          # Claude: --model
worker_effort = "high"           # Claude: --effort (low | medium | high | xhigh | max)
worker_model_codex = "terra"     # Codex: a slug, or a family word resolved to its newest slug
worker_effort_codex = "high"     # Codex: -c model_reasoning_effort
# A Worker's auto-compact window, passed on EVERY launch, a resume included (unlike
# the model and effort above): it is a launch flag, and banto does not count on a
# resumed session remembering it. "auto", an integer 100000..=1000000, or "" (no
# flag). Directors and Goinkyo never get it.
# Claude: --autocompact; Codex: -c model_auto_compact_token_limit (a token count
# only; "auto" gives Codex nothing).
worker_autocompact = 250000
relay = "auto"      # "auto" | "manual" — the wake-up nudge engine
# For banto developers: append every brigade message, in full, to
# <data dir>/logs/brigade-<id>.log. Off by default. The file holds real
# conversation content; banto never rotates or deletes it — don't commit it.
message_log = false
# Role briefings appended to each member's system prompt at launch.
# {brigade} / {token} / {peers} are substituted; "" launches with no
# briefing. The defaults tell a Director to delegate (and that it can name a
# Worker with name_member) and a Worker to say which source each claim about the
# repository rests on (committed code from `git show HEAD:<path>`, the working
# tree only for files the Director asked it to edit or review) — that policy is
# yours to set, which is why it is a setting. The default Director briefing also
# says when to call request_decision; your own director_prompt does not get that.
director_prompt = "..."
worker_prompt = "..."
# How readily a Director consults the Goinkyo on its own initiative, appended after
# director_prompt (yours included): "off" (only when you ask) | "disputes" (a
# disagreement it cannot settle) | "design" (also before a lasting change is shaped) |
# "decisions" (also whenever it chooses between courses). At every level it also
# consults when you ask. banto enforces none of them: each is an instruction in the
# Director's briefing, off included, and consult_goinkyo works the same at every
# level. "" appends nothing; with an empty director_prompt the setting does nothing.
# banto reads this once at start, and a Claude Director gets it in its launch
# arguments, so restart banto after editing. A Codex Director's hook re-reads the
# file at each SessionStart, so it picks an edit up without a restart. A running
# Director never sees a change: tell it in its pane.
goinkyo_consult = "design"
# A Director's request_decision also marks its row and rings, like a brigade going
# quiet (read at start). false: the tag only.
request_attention = true

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
not by sleeping. `BANTO_BELL_LOG=<file>` appends one line per bell banto rings, with
the decision that rang it (ids and counts only, no session text).

Further reading: [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) (design/
requirements), [docs/notes/](docs/notes/) (on-device verification notes for
the embedded ConPTY host, MCP, and Codex briefing delivery).
[tools/measure/](tools/measure/README.md) holds Python scripts that measure how
much a team of agents spends and how its changes fare in review; they are not part
of the build.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
