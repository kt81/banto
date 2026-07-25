# psmux write-command spike results (2026-07-19, psmux winget build / claims tmux 3.3.6 compatibility)

Non-destructive verification inside the live psmux session using `new-window -d`.
All results measured on the actual binary.

## Confirmed working

| Command | Result |
|---|---|
| `new-window -d -n <name> -P -F '#{window_id}'` | OK. Returns window_id (`@3`) |
| `split-window -t <win> -h -P -F '#{pane_id}'` | OK. Returns pane_id (`%8`) |
| `select-pane -t <pane> -T '<title>'` | OK. Readable via `list-panes -F '#{pane_title}'` |
| `send-keys -t <pane> '<cmd>' Enter` | OK. Executes if we wait for the shell to start |
| `capture-pane -t <pane> -p` | OK. Output can be captured |
| `swap-pane -s <p1> -t <p2>` | OK (exit 0) → sidebar + main switching is implementable |
| `kill-window -t <win>` | OK |

## Not supported (important)

- **Pane user options are effectively unusable**:
  `set-option -p -t <pane> '@banto_session' <val>` exits 0, but
  `display-message -p '#{@banto_session}'` returns empty (format expansion
  not implemented).

## Impact on banto's design

- Tag panes with **titles via `select-pane -T`** (doubling as a human-visible
  label) + **our own store's pane_id ↔ session map** (source of truth).
  Do not rely on user options
- pane_id / window_id are reliably obtained at creation time via `-P -F`,
  so a create-then-record flow is sufficient
- Remaining unverified items: the direct `split-window <command>` spawn form
  (instead of going through send-keys), and `respawn-pane`. Check while
  implementing the opener

## Follow-up spike (2026-07-19, opener implementation)

Verified on-device, non-destructively (`new-window -d` + `kill-window`):

| Item | Result |
|---|---|
| `psmux` binary accepts tmux subcommands directly | OK (`psmux new-window ...` — no need for the `tmux` alias binary) |
| Combined `-P -F '#{window_id}:#{pane_id}'` | OK. Returns e.g. `@2:%7` on both `new-window` and `split-window` |
| Direct command spawn: `split-window ... <argv...>` (multi-arg, no send-keys) | OK. `powershell -NoExit -Command Get-Location` ran as passed |
| `-c <cwd>` on `split-window` | OK. Spawned shell's cwd matched the `-c` argument |
| `select-pane -T` + `list-panes -F '#{pane_id} #{pane_title}'` | OK (re-confirmed) |

Still unverified: `respawn-pane` (not needed by the current opener design).

See also [`conpty-input-corruption.md`](conpty-input-corruption.md) for a
different layer of psmux findings: input delivery corruption (dropped ESC
bytes, corrupted keys) observed running banto's TUI inside psmux on Windows.

## Non-unique window/pane ids across sessions (2026-07-20, on-device)

Unlike tmux — where `@window_id` / `%pane_id` are unique per *server* — psmux
**reuses these ids across sessions**. Confirmed on a live server: pane `%2`
existed in both session `play` and session `test` simultaneously; `%2`
appeared twice in `list-panes -a -F '#{pane_id}'`.

Consequences and the verified session-qualified forms banto must use:

| Target form | Result |
|---|---|
| `display-message -t %2` (bare, id in two sessions) | Ambiguous — resolved to the *current* session (not an error, not random, but not guaranteed) |
| `select-window -t '<session>:<@window_id>'` | **FAILS**: `can't find window: @3`. Must use `<session>:<index>` or `<session>:<name>`, NOT the `@id` |
| `select-pane -t '<session>:<%pane_id>'` | **OK** — session + bare pane id, no window component needed; this is the reliable focus form |
| `select-pane -t '<session>:<%pane>' -T <title>` | OK (tagging) |
| `split-window`/`new-window` `-P -F '#{session_name}:#{window_id}:#{pane_id}'` | OK — captures the creating session, so the pane record can be session-qualified |
| `display-message -p '#{session_name}'` (no `-t`) | OK — the reliable way for banto to learn its OWN session name |
| `$TMUX` env third field (session-id number) used as `-t '$0'` | **FAILS** / unreliable — the real `#{session_id}` did not match it. Do not use the env route |

### `switch-client` is DESTRUCTIVE on this psmux build — never call it

Testing cross-session focus, `switch-client -t <session>` corrupted the
server's client/session accounting: it hijacked the user's attached client
away from its session, then a subsequent (unrelated, normally-safe)
`kill-window` of a temp spike window destroyed a whole session that was not
being killed, and the remaining sessions ended up merged/renamed
incoherently. **banto must never run `switch-client`.**

Design consequence: banto opens every resumed/new session as a **pane split
in banto's OWN session** (session-qualified anchor), and focuses only with
`select-pane -t '<own_session>:<%pane_id>'` — same session, no window switch,
no client switch. Because the pane always lives in banto's own session,
focus never needs to cross sessions, so `switch-client` (and the failing
`select-window -t <session>:@id`) are never needed.

### `display-message` takes the format positionally, NOT via `-F`

`display-message -p -t <pane> -F '<format>'` does NOT work — psmux echoes the
literal `-F ` into stdout (`"-F test:@1"`), which silently corrupted a
recorded pane target to `-F test:@1:%8` and broke focus. The format is a bare
positional after `-p`: `display-message -p -t <pane> '<format>'`. (The `-F`
flag IS correct for the *other* command family — `new-window`/`split-window
-P -F '<format>'` — so don't confuse them.)

## Diagnostic logging (kept in the binary; inert unless enabled)

These were built to diagnose the ConPTY input corruption and the psmux
pane-tracking bugs, and are retained because those classes of problem only
reproduce on a real psmux/ConPTY and are otherwise invisible. All are no-ops
unless explicitly turned on.

| Toggle | Process | Captures |
|---|---|---|
| `BANTO_INPUT_LOG=<file>` (env) | the TUI | every raw crossterm event + escape/SGR-resolution decision + banto's own `$TMUX`/`$TMUX_PANE`; lines prefixed `tui:` |
| `BANTO_WRAP_LOG=<file>` (env) | `banto _wrap` (resume path) | `_wrap`'s `$TMUX`/`$TMUX_PANE`, session id, child exit; lines prefixed `wrap:` |
| `--wrap-log <file>` (argv) | `banto _wrap --new-session` | the full new-session flow: `resolve_own_pane` (incl. the `display-message` argv + output), each `find_new_session` poll, and whether/why a pane record was written |

Note the `--wrap-log` argv route exists because a psmux pane's process is
spawned by the psmux *server*, not by banto, so it does NOT inherit banto's
environment — an env-var toggle never reaches `_wrap`. banto reads its own
`BANTO_WRAP_LOG` and forwards it to the new-session `_wrap` as `--wrap-log`.
Both `BANTO_INPUT_LOG` and `BANTO_WRAP_LOG` may point at the same file; the
`tui:`/`wrap:` prefixes keep the origins unambiguous.

## tmux (2026-07-26): the same commands, a different pane target

banto grew a real-tmux backend when the chōba's `s` key turned out to invoke
a `psmux` binary that does not exist on Linux. Probed against **tmux 3.6** on
a dedicated socket (`tmux -L bantoprobe`, so no live session was touched),
every command form this note verified on psmux works unchanged — with one
exception, and it is the one that matters.

| form | psmux | tmux 3.6 |
|---|---|---|
| `split-window -h -t <anchor> -c <cwd> -P -F '#{session_name}:#{window_id}:#{pane_id}' <argv…>` | works | works, returns `probe:@0:%1` |
| `new-window -d -n <title> -c <cwd> -P -F …` | works | works |
| `select-pane -t '<session>:<pane_id>'` | **required** (ids are reused across sessions) | **fails**: `can't find window: %1` |
| `select-pane -t '<pane_id>'` | ambiguous — could hit another session's pane | **works**; `-T <title>` sticks |
| `select-pane -t '<session>:<window_id>.<pane_id>'` | untested | works |

tmux parses `<session>:<pane_id>` as *window* `<pane_id>` of that session, so
the qualification psmux needs is exactly what tmux refuses. Neither form is
merely preferred: each is wrong on the other CLI. Hence
`banto_io::opener::TmuxFlavor` — the flavor is carried, not guessed.

Two smaller findings while there:

- tmux **sanitizes** `:` out of session names (`new-session -s 'a:b'` yields
  `a_b`), so the `:`-joined `-P -F` output this note's parser splits on stays
  unambiguous. No hardening needed.
- `select-pane` on a pane in another *window* succeeds but does not switch
  the active window — the same reach limitation already documented for psmux,
  and the reason banto keeps its panes as splits of its own window.

Verified end to end afterwards with banto's real `TmuxOpener` +
`SystemCommandRunner` (not the mock) against a probe server, with `$TMUX`
pointed at its socket: pane created, `sleep 120` running in it, title
`banto e2e` applied, focus accepted.
