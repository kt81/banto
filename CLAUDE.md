# banto

A resident TUI that searches and groups local Claude Code sessions and resumes them into psmux / Windows Terminal panes/tabs.
The canonical design/requirements document is `docs/REQUIREMENTS.md`. Always read it before starting work.
The architecture discipline (TEA / sans-IO: Event → State + Cmd, I/O at the edges) is `docs/DISCIPLINE.md` — new code must land inside it.

## Commands

- Build: `cargo build`
- Test: `cargo test --workspace`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Format: `cargo fmt --all`

## Layout

- `crates/banto-core` — UI-free logic (provider / status / store / search / opener / config)
- `crates/banto` — bin: ratatui TUI + clap subcommands

## Invariants (never violate)

1. Everything under `~/.claude` is **read-only**. banto may write only to its own config/data directories (`banto/` under dirs::config_dir / data_local_dir)
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
