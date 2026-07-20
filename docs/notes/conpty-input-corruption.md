# Input corruption running a crossterm TUI inside psmux on Windows (ConPTY)

Field guide for anyone hitting garbled or dropped input from a crossterm-based
TUI nested inside psmux on Windows. banto hit real, silent corruption during
mouse-motion floods; everything below is the ground truth extracted from
`BANTO_INPUT_LOG` captures taken 2026-07-19/20 and the commit history that
fixed each mode (`8a5b798`..`92f4cbc`, see `crates/banto/src/tui.rs` —
`normalize_key_code`, `ESCAPE_GRACE`/`HEADLESS_GRACE`,
`resolve_escape`/`resolve_headless_bracket`, `end_interrupted_buffer` — and
`crates/banto/src/sgr.rs`). Claims are marked as either measured in a real
capture or a defensive/hypothetical mitigation; nothing here is asserted
beyond what a commit message, code comment, or test actually backs.

## Why this exists: `BANTO_INPUT_LOG`

Synthetic key injection (`psmux send-keys`) delivers a whole sequence
atomically in one shot, so it cannot reproduce the delivery races that only
show up under a real, continuously-moving mouse. `BANTO_INPUT_LOG` (added in
`fdbc1d5`) is banto's answer: set the env var to a file path and every raw
crossterm event, plus every escape/headless-resolution decision, is appended
to it with a millisecond timestamp (`Context::log` in `tui.rs`). To reproduce
a corruption mode, run banto inside psmux with the var set, drive a real
mouse around the TUI (motion/wheel/clicks), and read the log afterwards —
each line is `<unix-ms> <event-or-decision>`.

## Corruption modes observed

### 1. The leading ESC byte is dropped — the leak arrives "headless"

Measured (`0652019`): during a mouse-motion flood, SGR mouse reports
(`ESC [ < Cb ; Cx ; Cy (M|m)`) leak through to the application as key events
instead of being decoded into `Event::Mouse`, and their leading `ESC` byte is
dropped somewhere upstream (ConPTY or crossterm's Windows input path) before
banto ever sees it. What arrives is a plain, unmodified stream of `Char`
presses: `[`, `<`, digits, `;`, `M`/`m` — no `Esc` event, no ALT synthesis,
no ESC anchor of any kind. Byte gaps within one sequence were ~1-2ms in the
typical case, with one recorded burst showing a single 96ms gap between the
last digit and the terminator.

banto's fix: `sgr::parse_headless_prefix` matches the same grammar minus its
leading `ESC`. The event loop treats an unmodified `Char('[')` as a possible
sequence start (`is_headless_bracket`) and buffers via
`resolve_headless_bracket`, waiting up to `HEADLESS_GRACE` (120ms, chosen
with margin above the 96ms observed gap) between bytes.

### 2. An ESC-headed entry point is also kept, as a defensive fallback

banto keeps a second recognizer, `resolve_escape` (`ESCAPE_GRACE` = 30ms),
that buffers starting from a real `Esc` key event and matches the full
`sgr::parse_prefix` grammar (`ESC` included). This path is structurally
required regardless of any leak, since it's also how banto tells a genuine
standalone Esc keypress (cancel-search / quit) apart from the start of a
sequence.

Whether it's *also* still catching genuine ESC-headed leaks in this
environment is unconfirmed, and worth being precise about: `resolve_escape`
predates `BANTO_INPUT_LOG` (it shipped in `c464ddc`, against the standard SGR
wire format, before there was capture data to check the assumption against).
Once real captures were analyzed, every leak examined turned out to be
headless — `0652019`'s commit message states plainly that "every previous
defense hinged on an Esc anchor and therefore never engaged," and a
regression test (`headless_motion_sequence_from_the_real_log_is_swallowed_as_motion`)
notes zero `"esc:"` log lines fired during that entire capture session despite
the flood. So: the ESC-headed leak path is retained in case some other
delivery shape (different multiplexer, different terminal) does preserve the
byte, not because the *first* capture showed it doing useful work.

Update from a later capture (second motion run appended to the same log,
around timestamp `1784521146629`… `1784523146630`): `esc: swallowed complete
sequence` lines do appear there — the ESC-headed machinery actively
swallowing motion sequences, entered via the post-swallow drain path rather
than a top-level Esc press event. Delivery shape therefore varies run to run
in this environment, and the ESC-headed path has caught real sequences at
least once; both entry points are load-bearing.

### 3. Backspace and Esc key-*downs* corrupt at the top level during floods

Measured (`92f4cbc`, one motion-flood capture, 5026 lines): two specific keys
corrupt when their press event has to compete with an active mouse-motion
flood, and only at the top level — never for a key event already inside a
buffered sequence:

| Key | Press arrives as | Rate | Release |
|---|---|---|---|
| Backspace | `Char('\u{7f}')` (DEL) | 22/22 during motion (2/2 correct as `KeyCode::Backspace` while the mouse was stationary in the same session) | correct |
| Esc | *(dropped entirely)* | 6/6 | correct (arrives alone, no matching press) |

banto's fix:
- `normalize_key_code` maps stray `Char('\u{7f}')`/`Char('\u{8}')` (DEL/BS)
  back to `KeyCode::Backspace`, and a literal `Char('\u{1b}')` back to
  `KeyCode::Esc`, at every classification point. Only the DEL corruption was
  actually observed; the BS and ESC control-char cases are a defensive
  extension of the same mechanism. All three codepoints were already inert as
  query text (`App::push_char` drops control characters), so recovering the
  intended key costs nothing.
- A bare top-level `Esc` `Release` with no matching `Press` is now dispatched
  as a genuine `Esc` press. In the healthy path the recognizer consumes the
  matching Release internally (inside `swallow_one_sequence`), so an
  unmatched Release reaching the top-level `event_loop` is an unambiguous
  press-loss signal, not a spurious event to ignore.
- Inside a sequence buffer, a mid-buffer `Char` keeps its modifiers: only
  `KeyModifiers::CONTROL` routes it to the interrupting-event path (e.g.
  Ctrl+C during a buffered sequence still quits) — SHIFT does not, since it's
  already baked into which character arrived. This distinction matters for
  the test harness gotcha below.
- `end_interrupted_buffer` (for a buffer that gets cut off by an interrupting
  event): a leading buffered `Esc` always dispatches as the real action,
  discarding only the tail; a grown headless-bracket buffer (past its bare
  `[` seed) is discarded as a truncated leak rather than replayed as garbage
  text; a lone one-character seed is replayed so a real keystroke isn't lost.

### 4. Ctrl+C does not reach banto at all inside psmux

Reported/user-confirmed, not something `BANTO_INPUT_LOG` can positively
prove: no capture session ever shows a Ctrl+C event arriving during a flood,
which is consistent with psmux/ConPTY intercepting it below crossterm's event
layer — but an absent log line is only indirect evidence, not a direct
capture of the interception happening. What *is* confirmed is that banto's own
Ctrl+C handling is correct once a Ctrl+C event does reach `handle_key`
(`tui.rs`, unit-tested by
`ctrl_c_still_quits_when_dispatched_with_its_modifier_intact`), so this is an
environment/delivery gap outside banto, not an application bug. Because of
it, banto does not rely on Ctrl+C to quit: `q` and `Esc` are the real quit
keys (`handle_normal_key`).

## Grace periods

| Constant | Value | Applies to | Why this value |
|---|---|---|---|
| `ESCAPE_GRACE` | 30ms | Entry wait in `resolve_escape`; per-byte wait inside `swallow_one_sequence` for an Esc-headed buffer | A 0ms poll was tried first and misclassified a split-paced leaked sequence as a standalone Esc (proven on-device via psmux byte injection); 30ms is still far below human reaction time between two real keypresses |
| `HEADLESS_GRACE` | 120ms | Per-byte wait inside `swallow_one_sequence` for a headless bracket-headed buffer | Real gaps were ~1-2ms with one observed 96ms outlier; 120ms leaves margin above that. A human would have to type the exact `[<digits;digits;digitsM` grammar with every gap under 120ms to misfire this, which is negligible risk to real typing |

Outcome from the `92f4cbc` capture at `HEADLESS_GRACE` = 120ms: 317 sequences
swallowed via the grace-timeout replay path, 0 timeouts — i.e. nothing in
that session needed the timeout fallback to kick in.

## Test-harness gotchas

Findings from actually trying to reproduce these modes with synthetic input,
before falling back to `BANTO_INPUT_LOG` with a real mouse:

- **Byte injection from Git Bash gets mangled.** Running
  `psmux send-keys -l '...'` from Git Bash on Windows goes through MSYS's
  argv path-conversion, which corrupts the literal byte string being
  injected. Use PowerShell to invoke `psmux send-keys -l` instead.
- **`send-keys` synthesizes modifiers that don't match reality.** Injecting a
  literal `<` via `send-keys` arrives at crossterm with `SHIFT` set (confirmed
  by the injection harness), even though every real leaked-byte capture in
  `BANTO_INPUT_LOG` shows no modifiers at all on any character of the leak.
  This is why the modifier guard in `swallow_one_sequence` is CONTROL-only,
  not "any modifier": a SHIFT-only check would have broken on real leaks
  matched by injected-test expectations that don't reflect real delivery.
- **Synthetic injection is atomic; it can't reproduce coalescing or
  press-loss races.** `send-keys` delivers a whole sequence (or a whole
  burst) in one go, so it cannot reproduce the split-pacing, corrupted-key,
  or dropped-event behavior that only shows up under a real, continuously
  moving mouse. That gap is exactly why `BANTO_INPUT_LOG` exists (see above)
  — every corruption mode in this document was found through a real capture,
  not through synthetic injection.

## See also

- `crates/banto/src/sgr.rs` — the pure SGR grammar recognizer
  (`parse_prefix`/`parse_headless_prefix`), independent of the event loop.
- `crates/banto/src/tui.rs` — the buffering/dispatch loop that owns all of
  the above (`event_loop`, `resolve_escape`, `resolve_headless_bracket`,
  `swallow_one_sequence`, `end_interrupted_buffer`, `normalize_key_code`).
- [`psmux-spike.md`](psmux-spike.md) — psmux command-surface findings (a
  different layer: what psmux's control commands support, not input
  delivery).
