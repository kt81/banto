# banto — Architecture Discipline (TEA / sans-IO)

Status: **adopted 2026-07-24**. Supersedes the conversational draft this grew
from. The feature set is frozen while the codebase migrates to this shape
(see §10); new work lands inside the discipline, not around it.

## 0. Purpose

Not beauty — **decisions made in advance**:

- New code has exactly one place to go (an AI collaborator never has to
  invent structure).
- Violations are detected mechanically (the compiler, not review).
- **No exceptions.** "Usually X, but it depends" is worse than no rule: it
  reintroduces the reasoning cost the rule existed to remove. The two scoped
  relaxations in §6 are *part of the rule*, stated once, not judgment calls.

## 1. The discipline, in one sentence

> **Every contact with the outside world has a name as an `Event`.
> The core is a pure function from `Event`s to `State` and `Cmd`s.**

Everything below is this sentence unpacked, plus how it is enforced.

## 2. Crate layout

Enforcement belongs to Cargo, not documentation: the dependency graph makes
violations fail to build.

| crate | responsibility | must never depend on |
|---|---|---|
| `banto-core` | `Event` → `State` transitions, `Cmd` production | process spawning, file I/O, clocks, `crossterm`, sqlite |
| `banto-tui` | rendering from `&State` | same (no queries during drawing) |
| `banto-io` | PTY/process spawning, jsonl reads, sqlite, clock, input events, fs watch, MCP stdio | `banto-core` internals (only `Event`/`Cmd`) |
| `banto-app` | wiring: `banto-io` ↔ `banto-core` ↔ `banto-tui` | — |

**Current reality and mapping.** Today there are two crates: `banto-core`
(UI-free but *not* IO-free: provider, store, status, opener all perform I/O)
and the `banto` bin (whose modules — `app`, `tui`, `view`, `embedded`,
`mcp` — are the de-facto layers). The migration reassigns, roughly:

- pure logic (`app.rs` list state, search ranking, status *classification*,
  the relay decision functions, `mcp::handle_line`) → new `banto-core`;
- provider/store/opener/status *reads*, `PortablePtyHost`, `LiveWatch`,
  terminal setup → `banto-io`;
- `view.rs` + the render halves of `tui.rs`/`emporium.rs` → `banto-tui`;
- the event loops → `banto-app`.

The `vt100::Parser` is itself a sans-IO state machine (bytes in, screen
state out); it lives on the core side as `State`, fed by output-chunk
`Event`s.

## 3. Prohibitions (core and tui)

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
- **Phase 1 — purity fixes in place.** Remove in-core clock reads (e.g.
  `App::set_status` currently calls `Instant::now()` internally), thread
  time as arguments. No structural moves.
- **Phase 2 — the emporium event loop becomes `update`.** Unify
  key/mouse/paste/relay/discovery handling into `Event` dispatch. This
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
- **Phase 3 — physical crate split** per §2; the compiler becomes the
  enforcer.
- **Phase 4 — record/replay infrastructure** per §8.

## Appendix A — initial I/O inventory (verify in Phase 0)

Process spawning: opener (psmux/WT via `CommandRunner`), in-place child,
`PortablePtyHost` (ConPTY), `_wrap` supervision, worker auto-spawn.
File reads: session jsonl (head + tail chunks), `sessions/<pid>.json`,
`config.toml`, dependency-free re-reads on live reload.
File writes: banto's own sqlite DB, per-member `--mcp-config` files,
diagnostic logs (all under banto's own config/data dirs — never `~/.claude`).
Clocks: `Instant::now()` (event loops, relay, click timing, status expiry),
`SystemTime::now()` (store timestamps, discovery `since`, live watch).
Input: crossterm key/mouse/paste/resize events; the ConPTY input quirks
(headless SGR mouse, `\r` for Enter, chunk-boundary paste semantics) live
at this boundary.
Watch: `notify` on `projects/` and `sessions/` (`LiveWatch`).
Processes: PID liveness via sysinfo (`ProcessProbe`).
MCP: stdio JSON-RPC in `_mcp` (per-connection).
Terminal control: raw mode, alt screen, mouse capture, bracketed paste,
terminal size.
Environment: `$TMUX` / `WT_SESSION` detection, `BANTO_*` diagnostics.
