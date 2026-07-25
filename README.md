# banto (番頭)

A resident TUI that searches, groups, and resumes your local Claude Code
sessions — and, in its **emporium mode**, hosts them as live embedded panes,
including Director/Worker multi-session cells that banto itself wires
together and keeps talking.

*Name origin: 番頭 (bantō) — the head clerk of a traditional Japanese
merchant house, who stays on the premises and runs the shop.*

**Cross-platform, raised on Windows.** Development and the heaviest
dogfooding happen on Windows Terminal + ConPTY, which makes Windows a
first-class citizen — the workshop, though, not the goal. The codebase
stays cross-platform (CI builds and tests both Windows and Linux), and
Linux dogfooding is underway.

## What it does

banto indexes the session files Claude Code CLI writes under `~/.claude`
(read-only, always — see below), gives you a fast fuzzy-searchable list with
your own grouping, pinning, and archiving on top, and opens whatever you
pick — three ways:

- **In-place** (default, `Enter`): banto hands its own terminal to
  `claude --resume` and takes it back when the session exits. No
  multiplexer involved, full native fidelity.
- **Split** (`s`): into a separate psmux pane / Windows Terminal tab, for
  multiplexer-layout users.
- **Emporium** (`banto --emporium`, alias `--oodana`): banto becomes a
  minimal embedded multiplexer — a persistent sidebar plus sessions hosted
  in vt100-parsed panes, Vim-buffer-style swapping, sessions kept alive in
  the background across switches.

If a session is already running somewhere, banto focuses it instead of
resuming it a second time — a double resume forks the session history, so
this is enforced everywhere.

### Director/Worker cells (emporium)

Inside the emporium, `B` appoints the selected session as a **Director** and
auto-spawns fresh **Worker** sessions beside it (count and model
configurable). banto mediates a message channel between them over MCP — it
launches each member with `--mcp-config` pointing back at `banto _mcp`, so
members get `send_to_peer` / `check_messages` tools backed by banto's own
sqlite queue. Delivery is pull-based with per-member cursors and firewall
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
  (Windows: `%APPDATA%\banto\`)
- Data (sqlite): `dirs::data_local_dir()/banto/banto.db`
  (Windows: `%LOCALAPPDATA%\banto\`)

Even the MCP wiring honors this: member `--mcp-config` files are written
under banto's data dir and passed by argv, never installed into Claude's own
configuration.

## Install & run

Requires a recent stable Rust toolchain (edition 2024).

```sh
cargo build --release

# Classic list TUI (default action)
cargo run --

# Emporium mode: sidebar + embedded session panes
cargo run -- --emporium

# Plain-text session listing
cargo run -- list

# Point at a different Claude home
cargo run -- --claude-home /path/to/.claude
```

## Keys

### Classic list

| Key | Action |
|---|---|
| `j`/`k`, arrows, `PgUp`/`PgDn`, `Home`/`End` | Move selection |
| `Enter` / double-click | Resume in-place (or focus if already running) |
| `s` | Resume split into a psmux pane / WT tab |
| `n` / `N` | New session, in-place / split (pick or type a cwd) |
| `/` | Search (fuzzy, title + cwd) |
| `p` / `d` / `g` | Pin / archive (soft-hide) / join group |
| `Tab` | Toggle grouped view (Pinned / groups / Ungrouped) |
| `a` | Toggle showing agent-run sessions |
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

```toml
# "in-place" (default) | "auto" | "psmux" | "windows-terminal"
# — auto/psmux/windows-terminal pick the split backend `s` uses.
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
