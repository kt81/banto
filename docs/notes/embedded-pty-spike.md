# Embedded PTY multiplexer spike (2026-07-22)

**Question.** Can banto host a real, interactive `claude` session *inside* its
own ratatui TUI — spawn the child in a PTY, parse its output with a VT emulator,
render the grid + forward input — i.e. the "banto becomes a minimal
multiplexer" architecture (the general case of which in-place mode is the N=1
degenerate form)?

**Verdict: viable on Windows.** A real `claude` session renders inside the
embedded pane with essentially full fidelity (colors, boxed panels, the status
bar, wide/CJK glyphs) and multibyte (Japanese) input echoes correctly. One
ConPTY-specific gotcha was found and resolved (see below).

## Stack

- `portable-pty` 0.9, `vt100` 0.16.2, `ratatui` 0.30.2 (`ratatui-core` 0.1.2 +
  `ratatui-crossterm`), Rust 1.97 / edition 2024.
- The spike binary is **disposable** and lives outside the repo (session
  scratchpad), not in the workspace. Only these findings are kept.
- Rendering approach: each `vt100::Screen` cell → a ratatui `Span` (fg/bg via
  `Color::Indexed`/`Rgb`, bold/italic/underline/reverse), one `Line` per row,
  wide-char right halves (`is_wide_continuation`) skipped so widths stay
  correct; child cursor mirrored with `Frame::set_cursor_position`.

## Headless-verified (self-tests, agent-checkable without a terminal)

`--selftest` (vt100 plumbing), 11/11:

- SGR foreground/background colors, bold.
- Cursor addressing (CUP).
- Wide/CJK glyphs: left cell carries the glyph (`is_wide`), right cell is
  `is_wide_continuation`.
- **Alternate screen** enter/leave (`?1049h`/`?1049l`) preserves the primary
  screen — important because Claude Code runs on the alt buffer.

`--ptytest` (portable-pty spawn + read through ConPTY): a child is spawned and
its stdout is streamed back and read successfully.

## Interactively-verified (on the user's Windows Terminal, real `claude`)

- `claude` v2.1.217 renders inside the embedded pane: colored boxes, the
  "getting started" panel, the MCP warning line, the bottom status bar, the
  robot art; typing (including Japanese) echoes at the prompt.

### KEY FINDING — do NOT answer terminal queries on ConPTY

At startup ConPTY emits, into the output stream we read:

```
ESC[1t  ESC[6n  ESC[c  ESC[?1004h  ESC[?9001h  ESC[?7l
```

`ESC[6n` (DSR, cursor position) and `ESC[c` (DA1, device attributes) look like
queries that need answering, and a naive host writes replies (`ESC[1;1R`,
`ESC[?1;2c`) back to the PTY. **This is wrong on ConPTY.** ConPTY is *itself*
the terminal emulator for the child; those queries are ConPTY probing the
*outer, real* terminal (us) for calibration, not the child asking us. And
ConPTY passes host input straight through to the child's stdin, so our reply is
injected as if typed.

Observed harm when answering:

- A plain `pwsh` shows `[1;1R` sitting on its prompt (our DSR reply, ESC eaten,
  leaked as literal input).
- `claude` mis-paints on resume: the conversation history is blank until a
  manual scroll forces a repaint — consistent with claude receiving a bogus
  cursor-position answer and laying out against it.

**Fix: don't answer.** With answering disabled, `claude` renders correctly,
resume history shows immediately (no scroll needed), and the `pwsh` leak
disappears. Confirmed by A/B toggle (`--answer` reproduces the bad behavior;
default is off).

> Note: this is ConPTY-specific. On a raw Unix PTY there is no ConPTY layer, so
> a hosting emulator *is* responsible for answering the child's DSR/DA. If a
> cross-platform PTY path is ever added, the responsibility flips per platform.

## ConPTY gotchas found later, in production dogfooding

Recorded here so the full ConPTY quirk list lives in one place:

2. **Chunk boundaries carry meaning** (2026-07-24). The child's TUI decides
   "typed" vs "pasted" by read-chunk coalescing: nudge text + `\r` written
   back-to-back arrives as one chunk and reads as a paste (the `\r` becomes a
   newline in the input box, not a submit) — the relay nudge sends its Enter
   ~300ms behind the text for this reason. Symmetrically, a multi-line paste
   forwarded key-by-key submits line-by-line; it must be forwarded as one
   bracketed-paste chunk.
3. **A child's exit produces no EOF on the master** (2026-07-24). The
   pseudoconsole keeps the output pipe open after the child dies, so a reader
   blocked in `read()` never unblocks and a channel-disconnect exit signal
   never fires — a dead pane just freezes (the "zombie pane" dogfood find).
   Exit must be detected *actively*: a waiter thread blocks on `child.wait()`
   and reports through its own channel; the poll path drains remaining output
   first (one-tick grace) before declaring the session dead. On Unix both
   signals fire (EOF *and* `wait()`), which the dedupe tolerates.

## Phase 2 — multi-pane, banto-owned layout (verified interactively)

A second binary (`phase2`) hosts several contexts at once, each context owning
its own set of panes (each pane a real PTY child + `vt100`). Verified on the
user's Windows Terminal:

- **Multiple real children hosted simultaneously**, each keeping its own
  terminal state.
- **banto owns the layout.** Each context carries its own pane set; switching
  context (F2/F3 or a sidebar click) swaps the rendered layout buffer-style and
  does **not** carry the other context's arrangement — a 2-pane context and a
  3-pane context stay themselves across back-and-forth switching. (This is the
  "多ペインを引き継がない" requirement, and it is trivial precisely because
  banto owns the layout: it is just which context's pane list gets rendered.)
- **Hidden contexts keep running in the background** — off-screen tickers keep
  advancing; switching back shows the counter has moved on.
- **Keyboard routes to the focused pane** (F4 cycles focus; click focuses a
  pane; the focused pane's cursor is mirrored).
- **Strongest result:** `claude` launched *inside* a hosted shell pane retained
  full session state across repeated context switches — a heavy alt-screen TUI
  in a backgrounded pane survives being hidden and reshown. This is the
  resident-switcher behavior banto is built around.

**Conclusion.** The embedded-multiplexer direction (spike "case A") is validated
end-to-end: N=1 fidelity + N-pane banto-owned layout + background persistence.
The architecture fork is resolved in its favor. (Internal-only term coined by
the user: **"brigade"** = an operational formation of sessions deployed together
as split panes in the embedded view. This is **distinct** from banto's existing
`group` feature: a `group` (the `g` command) is the user's free organizational
filing by project/phase (implementing / QA / soaking); a brigade is a live
multi-pane deployment of a **Director + Worker(s) operational cell** (the
HANDOVER broker formation made visible as panes; brigadier = Director).
Arbitrary ad-hoc multi-pane of unrelated sessions is explicitly out of scope.
Internal term — never surfaced externally.)

## Not yet verified (next iterations)

- Resize follow-through under stress; heavy-output performance / flicker with
  many panes.
- Non-Windows behavior (raw Unix PTY; see the ConPTY note above).

## Two of the above landed

Mouse forwarding into the child and scrollback viewing were both on the list
above; both have since landed.

- **Mouse forwarding into the child** (SGR mouse translate):
  `Screen::wants_sgr_mouse` (`crates/banto-core/src/engine.rs:3610`) gates
  whether a mouse event reaches the child's own SGR mouse mode instead of
  banto's own pane control. Click/drag landed (2026-07-27); the wheel
  case — forward to the child vs. scroll banto's own pane — joined the same
  gate later (2026-08-02, the same day as scrollback below), tested by
  `wheel_over_a_pane_that_wants_sgr_mouse_is_forwarded_not_consumed`
  (`engine.rs:7680`).
- **Scrollback viewing** (2026-08-02): `Screen::scrollback()`/
  `Screen::scroll()` (`crates/banto-core/src/screen.rs:141`/`:169`), wired
  from mouse-wheel handling at `engine.rs:3625`, tested by
  `wheel_over_a_pane_that_does_not_want_sgr_mouse_scrolls_its_own_scrollback`
  (`engine.rs:7643`); the scrollback-*capture* fix this depends on
  (`portable-pty-psmux`) is in `docs/REQUIREMENTS.md`'s "Emporium mode"
  section.

## Implication for banto

- The embedded-multiplexer direction is de-risked enough to pursue as the path
  to the "left sidebar + swappable session pane" UX that no terminal
  multiplexer offers natively. in-place mode is its N=1 case.
- On Windows specifically: banto's embedded host must **not** implement a
  DSR/DA responder. Keep in-place (full native terminal, no embedding) as the
  fidelity escape hatch, and split (psmux/tmux) for users already living in a
  multiplexer.

## Unix follow-up (2026-07-25): the hangup ConPTY gives you for free

Filling in the "non-Windows behavior" gap listed above, found by dogfooding
the emporium on WSL: **quitting banto stalled for the full 5s shutdown grace
and then `SIGKILL`ed every pane.**

The teardown asks each child to close the way a terminal window closing would
(`PtyHandle::begin_graceful_close`): drop the writer and the master, then wait.
On ConPTY that *is* the request — dropping the master closes the pseudoconsole
and raises the console-close cascade. On Unix it is silent, and for a reason
that reads like a joke: the tty hangs up only when the **last** fd on the
master closes, and one is held by the reader thread, parked in the `read()`
that the hangup itself was going to release.

Measured against a real `claude` at its prompt, driven through banto's own
`PortablePtyHost` (so the reader thread and its cloned master are live, exactly
as in the emporium):

| teardown | outcome |
|---|---|
| drop writer + master only | still running at 8s; force-kill required |
| `killpg(pid, SIGHUP)` first, then the same drops | exits in **0.52s**, status 0 |

Also measured, with a bare `openpty` to isolate the mechanism: closing the sole
master fd kills `claude` in ~0.5s, while closing it with one dup still open
leaves the child running indefinitely — the dup, not `claude`, is what swallows
the hangup.

So the signal the tty would have sent is now sent by hand
(`banto_io::pty::Hangup`, a no-op on Windows where the master close already
means it). The process **group** is signalled rather than the lone pid: the
child is a session leader owning the pty (portable-pty does setsid +
TIOCSCTTY), so its pid is its pgid, and the group is what the tty driver itself
would hang up — which also reaches the `banto _mcp` server the session spawned
instead of orphaning it.

Worth keeping in mind beyond this bug: **"the master is closed" is a Windows
event and a Unix non-event.** Any future teardown path that leans on dropping
the master needs its own Unix answer.
