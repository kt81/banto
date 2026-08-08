# banto

A resident TUI that searches and groups local Claude Code and Codex sessions and
resumes them — in-place in banto's own terminal by default, or into a psmux pane
/ Windows Terminal tab. Two UI modes share one core: the chōba (the list,
`crates/banto/src/tui.rs`) and the emporium (banto hosting the panes itself,
`crates/banto/src/embedded/emporium.rs`).

The canonical design/requirements document is `docs/REQUIREMENTS.md`. Always read it before starting work.
The architecture discipline (TEA / sans-IO: Event → State + Cmd, I/O at the edges) is `docs/DISCIPLINE.md` — new code must land inside it.

`CLAUDE.md` is a symlink to this file, not a second document — more than one
agent works in this repo and they look for different names. Edit this one.

## Commands

- Build: `cargo build`
- Test: `cargo test --workspace`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Format: `cargo fmt --all`

## Layout

Four crates. The dependency direction is the discipline, not a preference —
`docs/DISCIPLINE.md` §2 lists, per crate, the dependency it must never carry.

- `crates/banto-core` — pure: `Event` → `State`, `Cmd` production. No I/O of any
  kind, enforced by dependency (`crossterm` / `rusqlite` / `notify` / `sysinfo` /
  `portable-pty` / `dirs` are forbidden here)
- `crates/banto-io` — the outside world: filesystem, process spawning, sqlite,
  clock, input events, fs watch, MCP stdio (provider / store / opener / pty)
- `crates/banto-tui` — rendering from `&State`: pure `(frame, state, area)`
  widgets, no key handling and no terminal setup
- `crates/banto` — bin: clap CLI, raw crossterm, the event loop, and the wiring
  between the other three

## Invariants (never violate)

1. Every agent's own home is **read-only** — `~/.claude`, and `~/.codex` (or `$CODEX_HOME`) since Codex sessions are indexed the same way. banto may write only to its own config/data directories (`banto/` under dirs::config_dir / data_local_dir)
2. **Never bring real session data into the repository.** Tests must use hand-made synthetic fixtures
3. Parse JSONL leniently: ignore unknown record types and fields, skip broken lines instead of erroring
4. Never allow a double resume of the same session (it forks the session history)
5. Do not break cross-platform builds: isolate Windows-specific code behind cfg, use PathBuf for all path handling. **Gate the I/O, not the logic** — a pure helper reachable only from a `cfg(target_os)` arm is dead code on the other platform, and a `clippy -D warnings` run here can only judge the code its own cfgs left standing. CI's Windows job is the only gate that sees the other half

## Conventions

- Documentation, commit messages, identifiers, and code comments are all written in English
- Keep clippy clean with `-D warnings`
- Put every external process invocation (tmux etc.) behind an abstraction (trait) and mock it in unit tests
- **Comments earn their place.** Say what the code cannot: why it has this
  shape, a fact measured on a real machine, an incident that happened, a
  constraint an innocent edit would break, a rejected alternative someone
  would retry. Not the signature restated, not history git already holds,
  not what another comment in the crate already says. Length is proportional
  to surprise — obvious code gets none. Judge by content, not tense: the
  best comments here are past tense because their reason is a past event.
  Cite checked-in docs (`docs/DISCIPLINE.md §3`), never round numbers or
  card ids a reader cannot resolve. **A comment that has gone false costs
  more than a verbose one** — when behaviour lands, reread whatever
  described it as future.
