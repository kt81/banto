# banto

A resident TUI that searches and groups local Claude Code sessions and resumes them into psmux / Windows Terminal panes/tabs.
The canonical design/requirements document is `docs/REQUIREMENTS.md`. Always read it before starting work.

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
5. Do not break cross-platform builds: isolate Windows-specific code behind cfg, use PathBuf for all path handling

## Conventions

- Documentation, commit messages, identifiers, and code comments are all written in English
- Keep clippy clean with `-D warnings`
- Put every external process invocation (tmux etc.) behind an abstraction (trait) and mock it in unit tests
