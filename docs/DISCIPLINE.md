# banto — Architecture Discipline (TEA / sans-IO)

Status: **adopted 2026-07-24**. Supersedes the conversational draft this grew
from. The feature set is frozen while the codebase migrates to this shape
(see §10); new work lands inside the discipline, not around it.

## 0. Purpose

Not beauty — **decisions made in advance**:

- New code has exactly one place to go (an AI collaborator never has to
  invent structure).
- Some violations are detected mechanically — the compiler catches a
  forbidden *dependency* (see §2). The rest — a forbidden *std* API like
  file, clock, process, or network access — cannot be caught that way and
  rely on review; §2 says which is which, and records a real one that got
  through.
- **No exceptions.** "Usually X, but it depends" is worse than no rule: it
  reintroduces the reasoning cost the rule existed to remove. The two scoped
  relaxations in §6 are *part of the rule*, stated once, not judgment calls.

## 1. The discipline, in one sentence

> **Every contact with the outside world has a name as an `Event`.
> The core is a pure function from `Event`s to `State` and `Cmd`s.**

Everything below is this sentence unpacked, plus how it is enforced.

## 2. Crate layout

Enforcement is split, and the two halves are not equally strong.

Cargo's dependency graph makes it fail to build for `banto-core`/`banto-tui`
to name a *crate* they don't depend on — `crossterm`, `rusqlite`,
`portable-pty`, `notify`, `sysinfo` are all absent from those two crates'
`Cargo.toml`, so using any of them is a compile error, not a review finding.
This is real, mechanical enforcement.

It does **not** cover §3's std-level prohibitions. `std::fs`, `std::time`,
`std::process`, and `std::net` ship with every Rust crate regardless of its
`Cargo.toml` — there is no dependency to remove, so there is nothing for the
compiler to reject. "No file reads or writes," "no clock access," "no
process spawning," and "no network" are maintained by review alone.

**This is not hypothetical.** R37 (2026-07-26) found `Path::is_dir()` sitting
in `banto-core/src/engine.rs`'s new-session confirm path — it compiled
cleanly and shipped through several prior cleanup rounds before anyone read
past it. It was fixed by moving the check to a `Cmd`/`Event` round trip
(§4); the point stands regardless: a std-level prohibition can be violated
and built successfully for as long as nobody happens to read that exact
line.

| crate | responsibility | crate-level dependency it must never carry |
|---|---|---|
| `banto-core` | `Event` → `State` transitions, `Cmd` production | `crossterm`, `rusqlite`, `portable-pty`, `notify`, `sysinfo` |
| `banto-tui` | rendering from `&State` | same (no queries during drawing) |
| `banto-io` | PTY/process spawning, jsonl reads, sqlite, clock, input events, fs watch, MCP stdio | `banto-core` internals (only `Event`/`Cmd`) |
| `banto-app` | wiring: `banto-io` ↔ `banto-core` ↔ `banto-tui` | — |

**Status: DONE (Phase 3, 2026-07-25).** The split is physical. `banto-core`
kept its name and purified in place (model, input, config types, age
bucketing, search, `App`, the emporium engine + `Screen`, key/paste
encoders — deps: serde, vt100, nucleo, ratatui-core);
`banto-io` and `banto-tui` are real crates; the bin package `banto` carries
the app role (the table's "banto-app" is that role, not a rename). Two
notes from the migration worth keeping:

- The umbrella `ratatui` crate enables its `crossterm` feature by default —
  core uses `ratatui-core` (the backend-free layout/buffer subset) and
  banto-tui turns the feature off. The `cargo tree` acceptance check is what
  caught this; keep running it.
- The chōba's `tui.rs` remains an un-migrated app-layer monolith by design
  (only the shared `render_modal` was extracted). Its future migration — or
  retirement — is a separate decision.

The `vt100::Parser` is itself a sans-IO state machine (bytes in, screen
state out); it lives in core as `State` (`banto_core::screen`), fed by
output-chunk `Event`s.

## 3. Prohibitions (core and tui)

None of the following is compiler-checked (see §2) — each relies on review.

- No process spawning.
- No file reads or writes.
- **No clock access** (`Instant::now()` / `SystemTime::now()` are forbidden;
  time arrives as an argument).
- No randomness (seeds arrive from outside).
- No network.
- No "updating state while we're here": every state change is an
  `Event → State` transition.
- No queries during drawing: `view` sees a `&State` snapshot only.
- Session history (jsonl) is read-only — banto never writes upstream history
  through any path. (This restates repo invariant 1; the discipline does not
  weaken it.)

**The hardest violation to spot: disguised inputs.** Something that looks
like an input but is actually derived from internal state — e.g. estimating
a session's last-update time from "when I last read it" instead of the
jsonl mtime. When enumerating `Event`s, hunt for these specifically.

## 4. The core signature

```rust
fn update(&mut self, ev: Event, now: Instant) -> Vec<Cmd>
```

- Even `now` is an argument; the core owns no clock.
- A `Cmd` is **plain data** — an instruction written down, not executed.
- Only `banto-app`/`banto-io` execute. Effect policy is this one sentence;
  there is no second mechanism.

Because *all* writes to a hosted session's stdin (key forwarding, mouse
reports, paste, the relay nudge and its delayed Enter) become
`Cmd::WritePty`-shaped data executed in one place, the single-writer
invariant stops being a convention and becomes structure. Type-level
enforcement (guard types with private constructors) remains the tool of
choice only for what `Cmd`s cannot express.

## 5. What this buys

1. Tests need no processes, no network, no terminal: `update` is arguments
   in, values out.
2. Time is forgeable — a 60-second cooldown is tested by passing a number,
   not by sleeping. (The relay engine's `should_nudge`/`tick_relay_decision`
   already work this way; every dogfood fix has been pushing the code toward
   this shape under bug pressure. The discipline is those fixes, generalized.)
3. Concurrency becomes deterministic: Director/Worker races replay as an
   *ordering of events*, which a test can choose. Mock injection cannot do
   this — with mocks, call order is decided by the code under test; with
   `Cmd`s, order is an input.
4. The hard parts shrink: async, lifetimes, and `Send` bounds are confined
   to `banto-io`; the core is enums and `match`.
5. The effect history *is* the return value — comparable and snapshotable
   wholesale, not scattered across mock recorders.

## 6. Scoped relaxations (part of the rule, not exceptions to it)

1. **The store executes synchronously.** sqlite is I/O and lives in
   `banto-io`, but `banto-app` may execute a store `Cmd` synchronously and
   feed the resulting `Event` back in the same iteration. The discipline
   governs *what* is an effect, not *when* the executor runs it.
   (Store swappability is served by the existing `Store` type boundary;
   `Cmd`-ization buys replay and ordering, not swapping.)
2. **Diagnostics may bypass.** Existing diagnostic channels
   (`BANTO_INPUT_LOG`-style logs) are `banto-io` concerns and do not need
   `Cmd` round-trips.

## 7. Multi-process reality

banto is not one program: the TUI, each `banto _mcp` server, and each
`banto _wrap` supervisor are separate processes sharing one sqlite file.
The discipline applies **per process** — each gets its own (possibly tiny)
core; `mcp::handle_line` is already nearly one. Between processes, the
**store is the boundary**: cross-process ordering is governed by sqlite
transactions and the busy timeout, not by `Event` ordering. Replay
determinism (§5.3) is an in-process guarantee; cross-process races are
tested by choosing the order in which store `Event`s are fed to each core.

## 8. Fixtures and replay (the oracle)

banto has no spec for upstream behavior; recorded inputs are the oracle.
Two fixture kinds, with different jobs:

1. **Raw-boundary fixtures** — synthetic jsonl files, scripted PTY byte
   captures, hand-written key sequences. These exercise the *parsers* and
   are the only layer that can detect upstream format drift. (This is
   today's provider-test practice, continued.)
2. **Event-stream fixtures** — sequences of `Event`s (with timestamps) fed
   to `update`, asserting the resulting `State`/`Cmd` history. These
   exercise the *core*, including ordering-sensitive brigade scenarios.

Both kinds are **authored for test, or captured from purpose-made scripted
throwaway sessions — never taken from working sessions.** Repo invariant 2
("never bring real session data into the repository") stands unmodified.
Fixture files carry a format-version marker so a replay format change is an
explicit migration, not silent breakage.

## 9. Costs, honestly

- `Event`/`Cmd` round-trips add boilerplate; one-line actions become three
  sites.
- Getting the `Event`/`Cmd` granularity wrong means redoing it; the Phase 0
  inventory exists to de-risk exactly this.
- "Just make it work" gets slower. That is the price of making "why did the
  relay double-fire at 2 a.m." a replayable test instead of an archaeology
  dig.

## 10. Adoption plan

Each phase lands green (`cargo fmt` / `clippy -D warnings` / full test
suite) before the next begins.

- **Phase 0 — inventory.** Enumerate every point where banto touches the
  outside world (Appendix A is the working list; verify against code). The
  finished list *defines* `Event`: "there is no I/O beyond this list."
  **Status: DONE (2026-07-24, `38146fd` "docs: mark the I/O inventory as
  verified").**
- **Phase 1 — purity fixes in place.** Remove in-core clock reads (e.g.
  `App::set_status` currently calls `Instant::now()` internally), thread
  time as arguments. No structural moves.
  **Status: DONE (2026-07-24, `8fd108b` "refactor(core): inject the clock
  into set_status and the summary view").**
- **Phase 2 — the emporium event loop becomes `update`.** Unify
  key/mouse/paste/relay/discovery handling into `Event` dispatch. One
  design decision is owed here (flagged by the Phase 0 sweep): `tui.rs`'s
  `Context` mixes read-only dependencies (store, thresholds, claude_home)
  with `RefCell`-wrapped mutable state (`last_genuine_esc`, `input_log`,
  `pending_inplace`) behind an `&Context` that reads as configuration —
  that state must either join the real `State` or be explicitly assigned
  to the io layer, not survive the migration disguised as config. This
  phase deliberately carries the two features that motivated it:
  - **session termination** — `Event::PtyExited` (today a dead child's
    channel disconnect is silently swallowed by `pump()` and the pane just
    freezes) and `Cmd::KillPty` for deliberately stopping a solo session or
    a whole brigade. `Stage` moves from raw indices to session keys; the
    append-only sessions invariant retires with it;
  - **the keymap layer** — `Event::Key` resolves through a configurable
    keymap into semantic `Action`s (default pane-switch: a tmux-style
    `Ctrl+B` prefix, double-tap to send a literal `Ctrl+B` through), which
    also retires the F-key dependency.
  **Status: DONE (2026-07-24, `10b91e9` "refactor(emporium): the event loop
  becomes a pure update() engine" (2a) + `62b2575` "feat(emporium): tmux-style
  prefix key, active kill, disband ends its workers" (2b)).**
- **Phase 3 — physical crate split** per §2; the compiler becomes the
  enforcer for the crate-level prohibitions §2's table lists (not the
  std-level ones in §3 — see §2's R37 note). **Status: DONE (2026-07-24,
  `1d873ba` "refactor(core): evacuate domain and input types ahead of the
  crate split" (3a) + `40a05ed` "refactor: the physical crate split — the
  compiler now enforces the discipline" (3b); §2's marker dates this
  2026-07-25, the day it was recorded there rather than the day it
  landed).**
- **Phase 4 — record/replay infrastructure** per §8. **Status: DONE
  (2026-07-25, `450bd9e` "feat(replay): record and replay event streams
  (Phase 4 — the roadmap closes)").**

**The migration this section plans is complete: all five phases landed
2026-07-24 through 2026-07-25** (2 and 3 in two commits each), closing the
adoption plan this section opened with.

## Appendix A — I/O inventory (verified against code in Phase 0)

Status: verified 2026-07-24 by an exhaustive sweep of both crates. This
list defines `Event`: there is no I/O beyond it. The sweep found no
randomness, no network, and **no disguised inputs** — notably, session
mtimes always come from real `fs::metadata().modified()`, never from
"when banto last read it" (§3's own cautionary example, already done
right).

Process spawning: opener (psmux/WT via `CommandRunner`), in-place child
(`ProcessRunner`), `PortablePtyHost` (ConPTY), `_wrap` supervision, worker
auto-spawn.
File reads: session jsonl (head + tail chunks), `sessions/<pid>.json`,
`config.toml`, dependency-free re-reads on live reload.
File writes: banto's own sqlite DB, per-member `--mcp-config` files,
diagnostic logs (`BANTO_WRAP_LOG` / `BANTO_INPUT_LOG`) — all under banto's
own config/data dirs, never `~/.claude`.
Clocks: `Instant::now()` (event loops, relay ticks, click timing, status
expiry, the Esc-vs-leaked-SGR disambiguation stamp in `tui.rs`),
`SystemTime::now()` (store timestamps, discovery `since` — both the
provider's and the emporium's `PendingNew` instance — live watch, and the
summary panel's relative-age display, whose read now sits at the draw-loop
boundary and is injected into the view).
Input: crossterm key/mouse/paste/resize events; the ConPTY input quirks
(headless SGR mouse, `\r` for Enter, chunk-boundary paste semantics) live
at this boundary.
Watch: `notify` on `projects/` and `sessions/` (`LiveWatch`; its debounce
is already a pure function of injected timestamps).
Processes: PID liveness via sysinfo (`ProcessProbe`).
MCP: stdio JSON-RPC in `_mcp` (per-connection).
Terminal control: raw mode, alt screen, mouse capture, bracketed paste,
terminal size.
Environment: `$TMUX` / `$TMUX_PANE` / `WT_SESSION` detection (already
injected as a closure in `detect_backend` — with the relay decision
functions, the house pattern to imitate), `BANTO_*` diagnostics,
`std::env::current_exe()` (mcp-config and `_wrap` argv construction),
`std::env::current_dir()` (cwd fallbacks in the emporium).
