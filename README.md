# banto (番頭)

![The bantō at his counter, writing in a ledger whose rows glow like a
terminal list, while clerks carry boxed sessions between the shelves —
one box glowing mid-handoff](docs/assets/banto-hero.png)

A resident TUI that searches, groups, and resumes your local Claude Code
sessions — and, in its **emporium mode** (大店, *oodana*), hosts them as
live embedded panes, including Director/Worker multi-session cells that
banto itself wires together and keeps talking.

*Name origin: 番頭 (bantō) — the head clerk of a traditional Japanese
merchant house, who stays on the premises and runs the shop.*

**Cross-platform, raised on Windows.** Development and the heaviest
dogfooding happen on Windows Terminal + ConPTY, which makes Windows a
first-class citizen — the workshop, though, not the goal. The codebase
stays cross-platform (CI builds and tests both Windows and Linux), and
Linux dogfooding is underway.

## What it does

banto indexes the session files Claude Code CLI writes under `~/.claude`
(read-only, always — see below) into the **chōba** (帳場, "the shop
counter" — the default view): a fast fuzzy-searchable ledger of your
sessions, with your own grouping, pinning, and archiving on top. Whatever
you pick there opens one of three ways:

- **In-place** (default, `Enter`): banto hands its own terminal to
  `claude --resume` and takes it back when the session exits. No
  multiplexer involved, full native fidelity.
- **Split** (`s`): into a separate tmux/psmux pane or Windows Terminal tab, for
  multiplexer-layout users.
- **Emporium** (大店 — `banto --emporium`, alias `--oodana`): banto becomes a
  minimal embedded multiplexer — a persistent sidebar plus sessions hosted
  in vt100-parsed panes, Vim-buffer-style swapping, sessions kept alive in
  the background across switches.

If a session is already running somewhere, banto focuses it instead of
resuming it a second time — a double resume forks the session history, so
this is enforced everywhere.

### Director/Worker cells (emporium)

![The emporium with a staged cell: the sidebar's ledger on the left — a
🤝 Director row pinned under a counted header — and three live panes: a
Director session flanked by two Workers reporting back](docs/assets/emporium-brigade.png)

*A live cell at work (WSL + tmux): the Director reviews and directs; two
Workers implement and report over banto's MCP channel. This screenshot is
itself dogfooding — the session shown is building banto.*

Inside the emporium, `B` appoints the selected session as a **Director** and
auto-spawns fresh **Worker** sessions beside it (count and model
configurable). banto mediates a message channel between them over MCP — it
launches each member with `--mcp-config` pointing back at `banto _mcp`, so
members get `send_to_peer` / `check_messages` / `brigade_status` tools backed
by banto's own sqlite queue. Each member is also launched with a role
briefing (`--append-system-prompt`) naming its brigade, its token, and its
peers — without one a cell exists only in banto's data model and the
operator's screen, and a Director handed three tool names and no context
mostly never uses them; `brigade_status` answers the follow-up question
(who is on my team, what are they doing, is anyone holding my mail). Delivery is pull-based with per-member cursors and firewall
framing (a relayed message is labeled as coming from another AI, never
mistaken for operator input), and banto's **auto-relay** watches for idle
members with unread messages and wakes them by typing a short fixed nudge
into their stdin — so a Director→Worker→Director round trip needs no human
ferrying at all. This repository was largely built through that loop.

## Status & caveats

- A personal tool, pre-1.0, moving fast. Interfaces and the on-disk schema
  migrate forward but nothing is promised yet.
- Claude Code's session-file formats are **undocumented and subject to
  change**; banto defends with lenient parsing (unknown records and broken
  lines are skipped, never fatal) and synthetic-fixture tests, but a future
  Claude Code release could still surprise it.
- The embedded mode speaks ConPTY, which has sharp edges (documented in
  [docs/notes/embedded-pty-spike.md](docs/notes/embedded-pty-spike.md):
  never answer DSR/DA, chunk boundaries carry meaning, child exit produces
  no EOF). These are handled — ConPTY is where the sharpest edges lived,
  which is why the Windows side gets such deliberate care.

## Read-only guarantee

**banto never writes anything under `~/.claude`.** Session `.jsonl` files,
live-state files, history — all strictly read-only. banto's own data lives
under its own directories:

- Config: `dirs::config_dir()/banto/config.toml`
  (Windows: `%APPDATA%\banto\`) by default — see
  [Configuration](#configuration) for the full resolution order
  (`--config`, `$BANTO_CONFIG`, XDG, `~/.config`)
- Data (sqlite): `dirs::data_local_dir()/banto/banto.db`
  (Windows: `%LOCALAPPDATA%\banto\`)

Even the MCP wiring honors this: member `--mcp-config` files are written
under banto's data dir and passed by argv, never installed into Claude's own
configuration.

## Install & run

Requires a recent stable Rust toolchain (edition 2024).

```sh
cargo build --release

# Chōba: the default searchable session list
cargo run --

# Emporium (oodana) mode: sidebar + embedded session panes
cargo run -- --emporium

# Plain-text session listing
cargo run -- list

# Point at a different Claude home
cargo run -- --claude-home /path/to/.claude
```

## Keys

### Chōba (the list)

| Key | Action |
|---|---|
| `j`/`k`, arrows, `PgUp`/`PgDn`, `Home`/`End` | Move selection |
| `Enter` / double-click | Resume in-place (or focus if already running) |
| `s` | Resume split into a tmux/psmux pane / WT tab |
| `n` / `N` | New session, in-place / split (pick or type a cwd) |
| `/` | Search (fuzzy, title + cwd) |
| `p` / `d` / `g` | Pin / archive (soft-hide) / join group |
| `Tab` | Toggle grouped view (Pinned / groups / Ungrouped) |
| `a` | Toggle showing hidden sessions (agent-run, superseded ancestors) |
| `q` / `Esc` | Quit |

### Emporium

Everything above (minus split), plus:

| Key | Action |
|---|---|
| `Enter` | Open embedded; on a Director, stage its whole cell |
| `B` | Appoint Director + auto-spawn Workers (on a Director: disband) |
| `b` | Spawn one more Worker into the staged cell |
| `Ctrl+B` … | tmux-style prefix (configurable), from sidebar or pane: |
| … `o`/`Tab` | cycle focus through sidebar and panes |
| … arrows | directional pane navigation |
| … `1`-`9` | jump to pane N |
| … `s`/`Esc` | back to the sidebar |
| … `x` | kill the focused pane (confirm) |
| … `b`/`Ctrl+B` | send a literal prefix chord to the child |

While the prefix is armed, the status bar shows the full binding table —
nothing to memorize. Multiline paste and file drag&drop into panes work
(synthesized into bracketed pastes host-side, since the Windows console
never reports pastes as pastes).

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
# "in-place" (default) | "auto" | "tmux" | "psmux" | "windows-terminal"
# — everything but in-place picks the split backend `s` uses. "auto" reads
# $TMUX first (real tmux, or psmux on Windows), then $WT_SESSION. Name the
# multiplexer explicitly for an unusual install: they take the same commands
# but address panes differently, so banto has to know which one it is
# talking to.
opener = "in-place"

claude_home = "C:/Users/you/.claude"   # optional override
db_path = "..."                        # optional override

[activity]
today_hours = 24   # activity-dot bucketing
week_days = 7

[brigade]           # emporium Director/Worker cells
workers = 1         # Workers auto-spawned per cell (1..=8)
worker_model = "sonnet"   # --model for spawned Workers ("" = inherit)
relay = "auto"      # "auto" | "manual" — the wake-up nudge engine
# Role briefings appended to each member's system prompt at launch.
# {brigade} / {token} / {peers} are substituted; "" launches with no
# briefing. The defaults tell a Director to delegate — that policy is
# yours to set, which is why it is a setting.
director_prompt = "..."
worker_prompt = "..."

[keys]
prefix = "C-b"      # emporium prefix chord
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
└─ banto/       # the bin: event loops, executors, the crossterm boundary
```

The boundary is compiler-enforced: `banto-core` cannot name crossterm,
rusqlite, a clock, or a file without failing to build. Event streams can be
recorded (`BANTO_RECORD_EVENTS`) and replayed deterministically
(`banto_core::replay`) — timeouts and debounces are tested by arithmetic,
not by sleeping.

Further reading: [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) (design/
requirements), [docs/notes/](docs/notes/) (on-device verification notes for
psmux, ConPTY, and MCP).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
