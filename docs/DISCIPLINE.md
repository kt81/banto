# banto — Architecture Discipline (TEA / sans-IO)

Status: **adopted 2026-07-24**. Supersedes the conversational draft this grew
from. The migration described in §10 is complete; new work lands inside the
discipline, not around it.

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
encoders — deps: serde, vt100, nucleo, ratatui-core, unicode-width);
`banto-io` and `banto-tui` are real crates; the bin package `banto` carries
the app role (the table's "banto-app" is that role, not a rename). Two
notes from the migration worth keeping:

- The umbrella `ratatui` crate enables its `crossterm` feature by default —
  core uses `ratatui-core` (the backend-free layout/buffer subset) and
  banto-tui turns the feature off. The `cargo tree` acceptance check is what
  caught this; keep running it.
- Until 2026-11-13 the chōba's loop (`tui.rs`) stayed an un-migrated
  app-layer monolith by design (only the shared `render_modal` was
  extracted): it searched sessions and handed them to an in-place child or an
  external terminal, while the emporium hosted sessions in embedded PTYs. The
  chōba was retired as a mode that day (`docs/REQUIREMENTS.md`, "Architecture
  decision (2026-11-13)"), and with it the monolith: the one UI's event loop is
  the `update` engine this section describes, and no second loop is left to
  migrate.

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

**Reading the painted screen is allowed in one narrow form.** A hosted
pane's screen may be read for a fact the agent publishes nowhere else, only at
the moment a decision needs it, and only so that it closes when unsure: a
shape it does not recognise answers `Unrecognized`, and the caller does what
it did before it could read. It is never used to estimate or keep track of
state — that is the disguised input above in a new coat. The input-line
reader (`banto_core::input_line`) is one instance: it reads Claude's line for
text. Its only caller is the relay, which reads a Claude member's line on every
tick it runs (relay `auto`, the member's brigade not closing: the line is judged
for a draft whether or not a nudge is due), through one verdict per pane per tick
(below). A Codex member is not read at all:
it is woken through `codex queue`, not typed into (`docs/REQUIREMENTS.md`,
"Waking a Codex member").
Reading sends nothing; the operator approved the principle on 2026-11-10. The
next shortcut that wants the screen cites this paragraph's conditions, not the
instance.

**One reading per Claude pane per tick.** The relay's verdict on a Claude pane (what
the line reads, whether the unsent-draft fence holds a nudge, whether the reading
releases the operator's mark) is taken the first time anyone asks in a tick and handed
back unchanged to every later asker in that tick. The map holding it is built at the
start of the tick's relay work and dropped at its end, so a reading is never kept: it
is a reading, not state. The relay loop is its one asker today. It used to have a
second, the interrupt refinement that followed an Esc banto wrote for the Director's
`interrupt_worker`; that Esc is gone (a keystroke typed for the operator is the thing
this section forbids, and `interrupt_worker` now refuses the Worker's next tool calls
from a hook instead; "Auto-relay" in `docs/REQUIREMENTS.md`), and with it the second
asker. The verdict is still taken once per pane per tick, so a second asker added later
gets the same value. What crosses ticks is a set of members: no screen content. "The
relay is the one reader" is true of the relay, not of the whole engine: Codex reads the
line on its own.

Reading a program's own state report out of its output stream is not a read of
the painted screen: it is the child's explicit announcement (OSC 7501, for a
hosted Codex pane), arriving in an event that already exists (`PtyOutput`),
recorded and replayed like the rest, and it closes to the previous rule whenever
it is unsure (no report, or `clear`, and the earlier signal decides). It is not an
instance of the paragraph above, so a shortcut must not cite it for one. The Codex
relay gate reads it as one of its two conditions (a report that is not `blocked`,
and an empty prompt), and there a pane that has reported nothing is never at its
prompt: it holds, under the same exception as above. A pane's `done` or `error` is also latched
(`codex_turn_ran`) to say that a turn ran, which the notice for a first prompt
that was never identified reads. `working` is never latched or used that way:
Codex reports it while it starts as well.

## 4. The core signature

```rust
fn update(&mut self, ev: Event, now: Instant) -> Vec<Cmd>
```

- Even `now` is an argument; the core owns no clock.
- A `Cmd` is **plain data** — an instruction written down, not executed.
- Only `banto-app`/`banto-io` execute. Effect policy is this one sentence;
  there is no second mechanism.

Because *all* writes to a hosted session's stdin (key forwarding, mouse
reports, paste, and the relay nudge and its delayed Enter) become `Cmd`-shaped
data executed in one place (`Cmd::WritePty`), the single-writer invariant stops
being a convention and becomes structure. The Director's interrupt is not among
them: it writes nothing to a pane, and the refusal of a Worker's tool calls
that replaced the Esc is answered by that Worker's own `_mcp` process from a
hook, outside the core and outside the UI process's writers. Type-level
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
   (`BANTO_INPUT_LOG`-style logs, `BANTO_BELL_LOG`, and the pane lifecycle log
   `panes.log`; `docs/REQUIREMENTS.md`, "The emporium: hosted panes", says what
   each records) are `banto-io` concerns and do not need `Cmd` round-trips.
   The pane log is written by the dispatch loop beside `update`, from values the
   loop already holds. What a line says is decided in `banto_core::pane_log`,
   whose matches have no wildcard arm: a new Event or Cmd variant does not
   compile until someone has placed it in the written or the unwritten bucket.
   Writing a line never feeds anything back into `update`.
3. **A slow process may run off the loop's thread.** `codex queue` takes about
   0.2 s and is bounded at five, and the loop that runs it also draws the screen,
   so it runs on one worker thread (`banto`'s `embedded::queue_worker`). The
   thread never touches state: it turns a request into an `Event` and hands it
   back through a channel, and the loop applies it in a later iteration like any
   other event. The core owns what the request means: the mark that one is out,
   and the give-up when its answer never comes, so a replay that lacks the answer
   still ends. (`Cmd::HangUpPty` is the earlier case of the shell doing a slow
   thing away from the loop.) The catalog read stays on the loop's thread.

## 7. Multi-process reality

banto is not one program: the UI, each `banto _mcp` server, and each
`banto _hook` run are separate processes sharing one sqlite file.
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
  the app's old status setter called `Instant::now()` internally), thread
  time as arguments. No structural moves.
  **Status: DONE (2026-07-24, `8fd108b` "refactor(core): inject the clock
  into set_status and the summary view").**
- **Phase 2 — the emporium event loop becomes `update`.** Unify
  key/mouse/paste/relay/discovery handling into `Event` dispatch. One
  design decision was owed here (flagged by the Phase 0 sweep): the chōba
  loop's `Context` (retired 2026-11-13) mixed read-only dependencies (store,
  thresholds, claude_home) with `RefCell`-wrapped mutable state (`last_genuine_esc`, `input_log`,
  `pending_inplace`) behind an `&Context` that reads as configuration —
  that state must either join the real `State` or be explicitly assigned
  to the io layer, not survive the migration disguised as config. This
  phase deliberately carries the two features that motivated it:
  - **session termination** — `Event::PtyExited` (today a dead child's
    channel disconnect is silently swallowed by `pump()` and the pane just
    freezes) and `Cmd::HangUpPty` / `Cmd::KillPty` for deliberately stopping a solo session or
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

Status: verified 2026-07-24 by an exhaustive sweep of both crates; entries
for the external openers, in-place resume and `_wrap` were removed on
2026-11-13 when those paths were retired (`docs/REQUIREMENTS.md`,
"Architecture decision (2026-11-13)") — that edit was made by reading the
removal, not by repeating the sweep. This list defines `Event`: there is no I/O
beyond it. The sweep found no
randomness, no network, and **no disguised inputs** — notably, session
mtimes always come from real `fs::metadata().modified()`, never from
"when banto last read it" (§3's own cautionary example, already done
right).

Process spawning: `PortablePtyHost` (ConPTY), worker auto-spawn, and `codex debug
models --bundled` (`banto_io::codex_catalog`, behind the `CodexCatalogSource` trait;
added to the list on 2026-11-14 by reading the code, not by repeating the sweep).
*When:* only when a Codex Worker is being created with a non-empty model, so that a
family word such as `terra` can become a slug (`banto_core::codex_model`) — never
for a resume, which passes no model — and once for all the Workers waiting on it,
each of which is carried out only if it is still wanted when the answer arrives (the
brigade exists and is not being disbanded or closed, the member is on its roster and
is not open or being opened under any key; otherwise it is dropped with its
bookkeeping, and a successful disband or a confirmed close drops its held creations
at once). No creation is issued
for a member that is already open or being opened: a pane under any key, or an open
in flight (a creation or a resume, issued or held). *By whom:* the core emits
`Cmd::ReadCodexCatalog { round }` (each read a round, which the answer echoes and
which alone releases the creations waiting for it; an answer for a round with
nothing waiting changes nothing); the shell's executor runs the command through the
same binary a launch would use (`[agent_binaries]`, else `codex` on the path) and
answers with `Event::CodexCatalogRead { round, result }`, the output or the reason
there is none; the choice is made in the core from that text, so a replay re-decides
from the same event. *Bounds:* standard input closed, standard error dropped,
`CATALOG_TIMEOUT` (5 s) and 8 MiB of output, the child killed at the timeout; no
answer at all within 15 s (`CODEX_CATALOG_GIVE_UP`, counted by ticks, so a replay
that omitted the answer still finishes) is treated as an unreadable catalog; nothing
is cached. *Where:* on the event loop's thread, like a PTY spawn, so a hung binary
stands the screen still for up to the timeout and no longer. It takes about 0.1 s
and needs no network or auth; Codex itself creates and removes a temporary directory
under its own home on every start, as it does for every launch banto makes.

PTY exit waiting: `PortablePtyHost` waits on a dedicated thread. After a failed
wait it tries to send a nonterminal `Unknown` notification and retries after
250 ms, with at most one notification queued. After a subsequent wait error, a
dropped receiver stops further retries. Only a returned code or signal becomes
a live `PtyExited` event. After output loss the handle and
resume guard remain until that confirmation, including through persistent wait
errors. A historical recorded `PtyExited` event with `Unknown` still follows
the existing exit fold on replay. This entry was added by reading the current
code, not by repeating the inventory sweep.
File reads: session jsonl (head + tail chunks), `sessions/<pid>.json`,
`config.toml`, dependency-free re-reads on live reload. Codex's reads (its
`threads` sqlite lookup, read-only, and a tail of the rollout `.jsonl` for the
last turn marker) postdate this sweep and were added to the list on 2026-11-13
by reading the code, not by repeating it: once a tick the relay reads them for
every Codex brigade member, and the "is it idle?" sample for the quit question
and the brigade bell (`Event::QuiescenceSampled`) now reads them for every other
hosted Codex pane too — one sqlite lookup and one rollout tail per such pane per
tick. A hosted Claude pane adds no read of its own: the live-file scan and the
liveness pass are shared with the relay.
Codex discovery also reads the full `session_index.jsonl` name index on each
reload; individual lines are bounded, but total scan time and retained names
grow with the index ("Codex data sources" in `docs/REQUIREMENTS.md`).
For a fresh solo Codex pane, discovery also snapshots the read-only log
database's high-water mark before spawn and scans later rows once per tick,
in batches of at most 2,048, while checking PID start time and candidate
threads in the read-only threads database. It retains at most 1,024 associated
thread ids; an ambiguous or unavailable source leaves identity unresolved. It
refuses databases with a WAL but no shared-memory sidecar, and rejects a
replaced log file, rather than creating sidecars or trusting a changed source.
These bounds apply to log rows and retained associations, not necessarily the
number or cost of candidate-session queries.
File writes: banto's own sqlite DB, per-member `--mcp-config` files, diagnostic logs
(`BANTO_INPUT_LOG`, `BANTO_BELL_LOG`, one line per executed bell with its cause), and
the pane lifecycle log `panes.log` under `logs/` (rotated at 4 MiB to `panes.log.1`,
written by the UI process alone, content-free: `banto_core::pane_log` chooses the
lines) —
all under banto's own config/data dirs (the data dir is the operator's to name, never inside an agent's home), never `~/.claude`.
The single-instance lock: one more file the UI writes, `<database file>.lock` (an
exclusive advisory lock, empty, never deleted) and its `<database file>.lock.pid`
sidecar (the holder's process id, a hint for the refusal message only), both next
to the database. Taken in `main`'s UI arm, before the store opens, through
`banto_io::instance_lock`; the decision about what to do with the outcome is the
plain function `single_instance::claim_database`. Added by reading the code, not by
repeating the sweep.
The pre-migration copy: `<database file>.before-v<N>` (and, while it is being written,
`<database file>.before-v<N>.partial`), beside the database, written by `Store::open`
only. A finished copy is never overwritten or deleted by banto; a `.partial` is its own
working name, removed by the next start or by the attempt that failed. Same reading of
the code.
Clocks: `Instant::now()` (event loops, relay ticks, click timing, status
expiry),
`SystemTime::now()` (store timestamps, discovery `since` — both the
provider's and the emporium's `PendingNew` instance — live watch, and the
summary panel's relative-age display, whose read now sits at the draw-loop
boundary and is injected into the view).
Input: crossterm key/mouse/paste/resize events; the ConPTY input quirks
(headless SGR mouse, `\r` for Enter, chunk-boundary paste semantics) live
at this boundary. A hosted screen tracks its own Win32 input mode from child
output; the pure key encoder uses that state and the pane's agent identity for
the narrowly scoped Codex newline keys described in `docs/REQUIREMENTS.md`.
Watch: `notify` on Claude `projects/` and `sessions/`, the Codex rollout tree,
and filtered name-index events in the Codex home (`LiveWatch`; its debounce
is already a pure function of injected timestamps).
Processes: PID liveness via sysinfo (`ProcessProbe`).
MCP: stdio JSON-RPC in `_mcp` (per-connection).
Terminal control: raw mode, alt screen, mouse capture, bracketed paste, terminal
size, the host bell (`Cmd::RingHostBell { cause }`, a bare BEL — also what a brigade
going quiet rings through; the cause only names the deciding site for
`BANTO_BELL_LOG` and changes nothing written).
Environment: the variables startup reads (`BANTO_CONFIG`, `BANTO_DATA_DIR`,
`XDG_CONFIG_HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`), gathered into plain values before anything
decides from them — with the relay decision functions, the house pattern to
imitate; `BANTO_*` diagnostics, `std::env::current_exe()` (mcp-config argv
construction), `std::env::current_dir()` (cwd fallbacks in the UI).
