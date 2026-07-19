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
