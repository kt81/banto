# banto — Requirements & Design

A resident TUI tool that manages local Claude Code and Codex session
history with Claude-Desktop-like listing and grouping, and resumes a
selected session in a pane banto hosts itself: the session list (the chōba,
the ledger) on the left, live embedded sessions on the right. One UI, no
other way of opening a session. Windows-first.

Name origin: 番頭 (bantō) — the head clerk of a traditional Japanese shop, who
stays on the premises and directs and watches over the guests (sessions).

## MVP requirements

- Fast search over local session history (Claude Code and Codex CLI)
- Grouping (a session belongs to at most one group), pinning, and archiving
  (soft-hide) — all stored by banto itself, never writing to either agent's
  own files
- Enter on a search result opens the session in a pane on the right, hosted by
  banto itself: banto spawns the agent in a PTY it owns, parses its output
  with a VT emulator and renders the grid (see "The emporium: hosted panes"). A
  session that is already open in one of banto's panes is switched to, not
  opened a second time
- A dedicated dialog also launches a brand-new session (pick or type a
  working directory), not just resumes an existing one — `n`
- If a session is already resumed, refuse to start a second one instead
  (a double resume forks the session history and is therefore forbidden): a
  session open in one of banto's panes is switched to; one running anywhere
  else (another banto, an agent started by hand) is refused with "already
  running elsewhere"
- Activity indicator (colored dot) in the list. Busy sessions get special
  treatment; the rest are bucketed by time since last update
- Mouse support including wheel scrolling
- Runs on Windows; keeps a structure that also builds on macOS / Linux

Out of MVP scope: Claude Desktop (claude.ai) history, remote/SSH. ("Built-in
PTY" was out of the original MVP and came in with the emporium, 2026-07-22.
"Other agents" left the list when Codex support landed (`CodexProvider`,
`AgentKind::Codex`). Neither was an MVP revision; both are later scope layered
on top.)

## Architecture decision (2026-07-19)

*Status (2026-11-13): superseded by the 2026-07-22 entry (banto became a
multiplexer after all) and, for its "external terminal control", retired by the
2026-11-13 entry. Kept as the record of what banto started as.*

**TUI launcher + external terminal control.** No built-in multiplexer.
banto does no terminal emulation of its own; resuming is delegated to a real
terminal (psmux / Windows Terminal). `banto-core` (lib) and `banto` (bin:
TUI/CLI) are separated so that a future Tauri GUI or a "single-screen
switcher" built-in view (portable-pty + tui-term) can evolve on the same core.

## Architecture decision (2026-07-20): in-place as the default action

*Status (2026-11-13): superseded — in-place resume and split-into-a-pane/tab
no longer exist; see the 2026-11-13 entry below. Kept as the record of why
they were chosen.*

**In-place resume is the default (Enter); split-into-a-pane/tab remains a
first-class alternate (`s` / `opener` config), not deprecated.** In-place
still needs no PTY emulation and no multiplexer, consistent with the
2026-07-19 decision above — it's the simplest possible case, banto's own
terminal handed straight to a direct child process. Motivated by psmux's
non-uniqueness of window/pane ids across sessions
(the psmux spike note, deleted 2026-11-13 with the code it described; it
remains in git history) making split-mode targeting inherently more
fragile than just running the session where banto already is.

## Architecture decision (2026-07-22): the emporium — banto becomes a multiplexer

*Status (2026-11-13): the wording below that makes the emporium a "mode"
chosen with a flag, and the paragraph saying that the chōba stays
in-place-first and that the emporium is a separate, additional top-level mode,
no longer hold — see the 2026-11-13 entry. The rest stands.*

**An evolution of the 2026-07-19 decision above, not a reversal of it.** That
entry stays exactly as written above: it records what banto deliberately
started as. This entry records the later, deliberate decision to become a
multiplexer after all, once in-place resume (2026-07-20) had proven out the
underlying mechanism on the simplest possible case.

The question was posed precisely by the 2026-07-22 spike
(`docs/notes/embedded-pty-spike.md`): can banto host a real, interactive
`claude` session *inside* its own ratatui TUI — spawn the child in a PTY,
parse its output with a VT emulator, render the grid, forward input — "the
general case of which in-place mode is the N=1 degenerate form"? Verdict:
viable on Windows, full fidelity (colors, boxed panels, wide/CJK glyphs,
multibyte input), one ConPTY-specific gotcha found and resolved (never
answer the outer terminal's own DSR/DA queries — that traffic belongs to
ConPTY probing banto's real terminal, not the hosted child; answering it
leaks garbage into the child's stdin).

This is the **emporium** mode (大店, *oodana* — `banto --emporium` / `--oodana`):
a persistent sidebar plus one or more sessions hosted as live embedded
panes, up to and including brigade Director/Worker multi-session cells that
banto itself wires together over its own MCP server and keeps talking via
an auto-relay (see "The emporium: hosted panes", "Brigade (Director/Worker cells)", "MCP
mediation server", and "Auto-relay" below). The chōba is unaffected and
stays in-place-first per the 2026-07-20 decision; the emporium is a
separate, additional top-level mode (`--emporium`), not a replacement for
it — see the 2026-07-26 decision below, which freezes the chōba precisely
because new capability now belongs here instead.

Formalized architecturally as `docs/DISCIPLINE.md`'s TEA / sans-IO
discipline (adopted 2026-07-24): the emporium's event loop *is*
`engine::update`, a pure function from `Event`s to `State` and `Cmd`s
(`crates/banto/src/embedded/emporium.rs`'s own module doc). The resulting
four-crate physical split (`banto-core` / `banto-io` / `banto-tui` / `banto`)
completed 2026-07-25 (`docs/DISCIPLINE.md` §2's own status marker).

## Architecture decision (2026-07-26): the chōba is feature-frozen

*Status (2026-11-13): moot — there is no chōba mode left to freeze; see the
2026-11-13 entry below. Kept because it records an incident about how a
policy sentence was misread.*

**The chōba (formerly the "classic" list mode; `banto` with no flags) takes no
work done *for the chōba*.** New capability aimed at it belongs in the
emporium instead, which is where the hosted-pane work is going. Bug fixes and
platform parity remain in scope on their own.

**What this does not exclude: a change made for the emporium that lands in
the chōba because the two share the code it lives in** — a shared modal,
shared `App` state, a shared confirm. That is the intended outcome, not an
exception to be argued for case by case. The freeze is about where effort is
aimed, not about holding the two modes apart.

That sentence is here because the wording it replaces did not say it, and
misled two readers in one afternoon (2026-08-08): both a Director and a
Goinkyo read "new behavior belongs in the emporium" as covering anything that
reaches the chōba at all, and treated a shared-modal change as an exception
needing the operator's sign-off. It was not one.

"Platform parity" is what admitted the tmux backend above under the freeze:
`s` invoking a `psmux` binary that does not exist on Linux is a mode that
does not work off Windows, not a feature it lacks. The same reading covers
the input-path fix that preceded it. What is out of scope is work whose
reason is the chōba — making it do something new, for its own sake.

## Architecture decision (2026-11-13): one UI — the chōba is retired as a mode

**banto has one UI.** `banto` with no subcommand starts the session list on
the left and banto-hosted panes beside it. The chōba as a mode — the default
list, which opened a session by handing over banto's own terminal ("in-place"),
or into a tmux/psmux pane or a Windows Terminal tab through the `_wrap` helper
— is retired, together with every way of opening a session outside banto's own
panes. This supersedes the 2026-07-20 decision (in-place as the default
action), the reading of the 2026-07-22 decision that made the emporium "a
separate, additional top-level mode", and the 2026-07-26 feature freeze, which
has nothing left to freeze. Those entries stay above as the record of what
banto was.

*Gone:* in-place resume; split/tab opening (tmux, psmux, Windows Terminal) and
the `_wrap` subcommand that supervised those panes; the `[opener]` config
section and `BANTO_WRAP_LOG`; the `panes` table (dropped by store migration v17: see "Module layout");
the chōba's key table as a separate thing. *Unchanged:* the list's own features
(search, grouping, pinning, archive/reveal, new session), everything about
panes, brigades and MCP, the hand-off of resolved paths to `_mcp` and `_hook`,
the message log, `banto list` and `banto versions`.

"Chōba" remains only as the nickname of the session list on the left (the
ledger); "emporium" as the name of the hosted-panes UI. `--emporium` and its
alias `--oodana` are still accepted, do nothing, are hidden from `--help`, and
are slated for removal.

*Why* (the operator's reasons, 2026-11-13):

- Tmux-like multiplexers could not carry what the emporium does, so banto ended
  up hosting panes itself anyway; driving an external multiplexer was a second,
  weaker road to the same place.
- In-place resume had no demand: the agents' own pickers already list sessions
  across projects.
- banto does not intend to support many agent products, so "one list across
  every agent" is not a demand it can answer.
- Nobody used the external paths, and every cross-cutting change had to carry
  them untested.

## Decision (2026-11-13): quitting asks about what could be lost; a quiet brigade rings

Three decisions of the operator, made the same day, about when banto speaks up, and the steps that
followed them (4 to 12).
Their behaviour is written out in "The emporium: hosted panes" ("Is it idle? (the
quit question)", "The closing line", "Attention", "The quiet brigade's bell"); this
entry records what was decided and why.

1. **Quitting from the list asks only when a hosted session is not known to be
   idle.** `q`/`Esc` quits on the first press when every hosted session is known to
   be idle, or none is hosted; otherwise the prompt names each session it counts
   and why (`Esc again to quit — 2 not idle: a (unknown), b (busy)`), counting
   sessions that are working, waiting, of unknown state, or given input in the last
   3 seconds. The window, the second key and the
   ways the prompt is dropped are unchanged. (Since 2026-11-15 it also asks for a
   session whose only reason is a background command, and says so: "Decision
   (2026-11-15): a background command is named, not hidden".) This replaces the
   earlier rule (ask whenever any session is hosted, `N session(s) running`). *Why:*
   he quits when everything is idle and was made to press twice every time. Unknown
   asks rather than quits, since the question fails safe.
2. **Once quitting is decided, the status line says so.** `Closing — ending N
   session(s)…` (`Closing…` with none), with priority over everything else on the
   line, for every quit path, until the process exits. *Why:* the shutdown sweep
   closes each pane and waits for the children (up to 5 s) with no further frame
   drawn, and until now the line had already returned to its hints, so for that
   second nothing on screen said banto was closing.
3. **A brigade that has gone quiet marks its Director and rings.** When every live
   hosted member has been idle for 5 seconds after real work (a member still
   starting up does not count), and no member has unread mail the relay will still
   deliver — counted from the last member going idle or banto last prompting one —
   banto marks the Director (its row in the list, and its tile when the brigade is
   on stage) and rings the host bell, whether or not the brigade is on stage and
   whatever the operator is doing elsewhere, except while the Director's pane is
   effectively focused (it marks and rings with banto's window unfocused). *Why:* a
   brigade going quiet is the moment his input is wanted, and it is rare enough to
   deserve a sound. *Status (2026-11-14):* until then the fire latched the
   Director's pane, which only a staged pane draws, so it reached him only for a
   staged brigade; see "Decision (2026-11-14): an off-stage brigade speaks through
   its Director's row". A runtime switch (the function key `F4`, badge `BELL
   ALWAYS`, persisted as `bell_when_focused`, off by default) makes the bell ring
   even with the Director's pane focused. *Why a runtime switch and not a config
   key:* whether he wants the sound depends on what he is doing, and he wants to
   change it while running. *Why F4 and not a prefix chord:* he wants to flip it
   casually, in one keystroke, while working.

Alongside, banto now asks Codex to BEL for `async-question` and `plan-mode-prompt`
as well as `approval-requested` ("Attention"), so that a question the model asks
and the plan-implementation prompt ring too, not only an approval.

*What "idle" means, per agent* (the facts behind decision 1 and 3):

- **Claude Code:** the live file's `status` is `idle`. `busy`, `waiting` and
  anything else are not idle, and so is `shell`, which since 2026-11-15 is told
  apart as *background*, not idle for one reason only ("Decision (2026-11-15): a
  background command is named, not hidden"), except `waiting` with `waitingFor`
  exactly `dialog open`, which is unknown (item 9); no usable live entry is unknown.
  `shell` was measured on Claude Code 2.1.323 on 2026-11-13: the turn has ended but
  a background shell command is still running (see "Data sources").
- **Codex:** first, what the pane itself reports (item 4 above): idle, done and
  error → idle; working and blocked → not idle. With no report (or after its
  `clear`), the last turn marker in the rollout says the turn ended
  (`task_complete`, `turn_aborted`, and the `turn_*` alias spellings); a started
  turn is not idle; no marker is unknown. These marker names are from the Codex
  source at tag `rust-v0.168.0`, not measured here (see "Codex data sources").
  **Known limit of the rollout alone, measured 2026-11-13** (installed `codex-cli
  0.168.0`): Codex records nothing about a command it left running after the turn
  ended — the process was alive for 108 s after `task_complete`, with no rollout or
  sqlite change and no re-invocation of the model at its end — so a Codex session
  with no report reads as idle, and a quit can end it without asking. The
  `brigade_status` words for a Codex peer read this same marker (see
  `brigade_status()` in the tool list), so they share the limit.

4. **Later the same day: a hosted Codex pane's own state report is read.** banto
   reads it out of the pane's output: OSC 7501, the published Program Status
   Protocol (spec: <https://www.superlogical.com/rex/docs/build/program-status>,
   proposed by Mitchell Hashimoto,
   <https://mitchellh.com/writing/program-status-osc7501>; another member read the
   spec and Codex's source, banto's code reads only the sequences). For a pane known
   to be Codex that has reported, it decides the quit question and the brigade bell
   instead of the rollout marker; `blocked` is shown and rung for like a Claude
   session in `waiting`; `working` shows busy in the list. The relay still reads the
   rollout marker. *Why:* the rollout cannot see work a Codex session left running
   after its turn, nor a prompt waiting for the operator, and the pane's own
   announcement can. *Measured 2026-11-13 on this machine:* the sequence reaches
   banto's reader through ConPTY, and over six turns its working/done/idle
   transitions led or matched the rollout-based sample by at most about a second.
   `blocked` and `working` for background work were **not yet observed on a real
   session**; what they mean is read from the Codex source. The facts and limits are
   in "Codex data sources" ("What the markers mean, and where the signal stops").
5. **Later the same day: an expanded tile says what its pane is doing.** Its title
   row shows `Agent · word · output age` after the label, in the strip's own words
   and colours ("The title row of a tile"). *Why:* the operator asked: the clear
   status word was visible only on a minimised tile.
6. **Every surface reads the same activity, from one source.** For a hosted pane it
   is a believed Codex report first (`working` is busy, `blocked` is waiting), else
   the pane's list row, else what the member's live file said this tick (item 12),
   else none (`EmporiumState::displayed_activity`). The list rows, the counts and
   the marks get the report laid over the rows; the strip and the title row read it
   directly (until 2026-11-15, when the word became `pane_word`, which takes its
   waiting from it and from the latest tick's live file: "Decision (2026-11-15)").
   So a hosted pane with no row in the list yet still shows what it reported, and a
   `blocked` one is marked, counted once and rung for once like a pane with a row.
   *Why:* the collapsed strip first read the raw rows and showed "alive" for a Codex
   pane reporting `working` (found in the operator's trial); then the strip and the
   title took the activity only from the row, so a row-less Codex pane showed no
   word while the quit question already treated it as not idle (found in review).
7. **A pane's screen and PTY are never smaller than 2 rows × 2 columns.** A smaller
   tile keeps a floor-sized screen and child and is drawn clipped ("Size floor").
   *Why, measured:* the pinned vt100 panics on wrapped text in a one-row screen, in
   release builds too, and a one-row tile is reachable by shrinking the terminal to
   4 rows or stacking 6 Workers on a 24-row terminal. The published 0.16.13 no
   longer has that panic, but moving to it was backed out for its emoji widths
   ("Size floor"), so the fork stays pinned.
8. **`shell` shows busy.** A live Claude session whose status is `shell` (the turn
   ended, a background command is still running) is displayed as busy in the session
   list and `banto list`, and through the list in the counts and marks, matching
   what the quit question and the watch already counted; the relay still nudges it.
   *Why:* the operator restarted banto without knowing a command was still running
   in a pane that said "alive". (A tile read `shell` as busy through the sample from
   2026-11-15, saying "idle" where it had said "alive"; later the same day a tile
   says `idle · bg`, the quit prompt names the command and `brigade_status` says
   what it is: "Decision (2026-11-15): a background command is named, not hidden".
   The list is unchanged.)
9. **A dialog the operator opens does not ring and does not arm.** A Claude session
   that is `waiting` with `waitingFor` exactly `dialog open` is shown and counted as
   Waiting everywhere but is left out of the ring set (it rings once if the reason
   later becomes anything else while still waiting); the relay never nudges a
   waiting session; for the quit question and the brigade watch it is unknown (quit
   asks, the quiet stretch is held, a dialog alone never arms the bell). *Measured
   2026-11-13 on 2.1.323:* `waitingFor` was `input needed` for the ask-the-user
   picker and `dialog open` for a slash-command picker the operator opened himself.
   *Why:* banto rang for the dialog the operator opened, and opening and closing
   `/model` in an idle brigade rang the quiet bell five seconds later. Known limit:
   a brigade with a dialog left open cannot go quiet until it is closed.
10. **One title row for every tile.** The operator showed a screenshot in which a
    collapsed tile had its status on a row of its own under the title and an
    expanded tile had it on the title row: "inconsistent, and for no reason; unify
    the code path and the look", and he noted it would free a precious row per
    collapsed tile when there are many Workers. Now one function
    (`paint_tile_title`) draws the title row of every staged tile, collapsed or
    open: label, status, buttons. The collapsed strip's status row is gone; its body
    is only the two message rows, and a collapsed tile is four rows instead of five
    (`COLLAPSED_TILE_HEIGHT`; on a 24-row terminal 5 collapsed Workers fit instead
    of 4, on 40 rows 9 instead of 7). Nothing persisted depends on the height.
11. **The status sheds the same way in both states, and the age outlives the
    agent.** Full `Agent · word · output 2m ago`, then without the agent, then the
    age shortened (`word · 2m`), then the word alone, then nothing; the word is
    never cut and the label never sheds ("The title row of a tile"). *Why the short
    age:* on a collapsed tile the pane cannot be seen, and for Codex the output age
    is the only liveness signal, so it should outlive narrow tiles. *Cost, stated
    plainly:* the label never sheds, so at the default collapsed width (36 columns)
    a long display-named label leaves room for the word only.
12. **A member with no list row yet shows its live file's word.** A Claude member
    that has not run a turn has no transcript and so no session-list row; its tile
    showed no status word (the operator saw it on a freshly added Worker). The
    display's decision is now a believed Codex report, else the list row, else what
    the member's live file says as the relay observed it this tick (the list's own
    classification), else nothing. A row-less member that is waiting is marked,
    counted and rung for like any other; one waiting only on a dialog is shown and
    counted, not rung (item 9). The word is replaced every tick, absence included: a
    tick that did not observe the member leaves it no word. The quit question, the
    brigade watch and the relay do not read it.

*Not done:* answering the `?` query of the same protocol (which is what would make
Claude Code report over it too — from the 2.1.323 bundle, read and not run); keeping
sub-records (reports naming an `id=` are parsed and dropped); and sub-agent activity
is not tracked.

## Decision (2026-11-14): a model and an effort are given only when a session is created

Decisions of the operator, 2026-11-14, about what banto tells an agent to run on.
Their behaviour is written out under "Brigade" ("Formation", "Config": "Model and
effort: only when a session is created"); this entry records what was decided, why,
and what it replaces.

*What prompted it.* He changes the model inside his Codex sessions to a newer one,
and every banto restart put them back on `gpt-5.6-terra`: banto passed `-m` on every
launch, a resume of an existing session included, from a default
(`worker_model_codex`) written into its code in August.

1. **A default value has two levels:** the config file's value if the file sets one,
   else banto's built-in. Each key stands alone: setting one product's key leaves
   the other product's built-in as it was.
2. **A model and an effort are passed only when a member's session is created.**
   Re-opening an existing session passes neither, for either product, without
   exception. *Why:* a flag-less resume restores what the session last used,
   including what he changed inside it, and a flag on a resume would overwrite that.
   He considered an exception for Claude's effort (Claude does not remember it per
   session) and decided against it after learning that an Enter in Claude's
   `/effort` or `/model` picker saves to his own Claude settings: what a session is
   on is then his to say, not banto's.
3. **The built-ins name no version.** Claude Worker `sonnet` / `high`, Codex Worker
   `terra` (the family) / `high`, Goinkyo `fable` / `max`; the Director gets nothing
   (`BrigadeConfig` has no `director_*` setting: that session is the operator's own,
   started and restarted outside banto's control). *Why:* a version written into the
   code goes stale, as `gpt-5.6-terra` did when 6.1 came out. A test keeps a digit
   out of the three model built-ins.
4. **A Codex model may be given as a family word,** and banto resolves it to the
   newest slug of that family when the Worker is created. *Why:* Codex has no alias
   or "latest" for `-m` (below), so without this a family could only be named by a
   version.
5. **With `worker_agent = "select"` the picker (agent, model, effort) opens on the
   agent chosen last time, with that agent's defaults,** and its words say whether a
   brigade is being formed or a Worker added. With any other value there is no
   picker.

*Status (2026-11-14): the rule quoted here is superseded by this entry, as far as
the model and the effort go.* It was recorded in the commit messages of `46f2b8a`
(2026-07-25, "worker model on resume") and `c34512f` (2026-08-06, "let a resumed
member keep the model and effort its role is given") and in a comment on the
engine's `RoleLaunch`, not in this document; the sentence is kept as `c34512f` wrote
it: "Both are properties of the role, and banto's configuration is authoritative for
what a member is on every launch, the same as `permission_mode` and
`disallowed_tools` already were." What prompted it: on 2026-07-25 a resumed Worker
came up on the operator's own default model instead of the brigade's configured one
(Opus 5 Workers on WSL), and on 2026-08-06 a Goinkyo configured for `max` reported
`xhigh` after a restart, because nothing passed its effort again. Measured then,
claude 2.1.223: `--effort low` once, then resumed with no flag, reverted to the
operator's global effort setting; `--model haiku` once, then resumed with no flag,
stayed on Haiku. `permission_mode` and `disallowed_tools` are not superseded: they
are the role's, are still passed on every launch, a resume included.

*Facts behind it, by where they come from.*

- *Read in Codex's source at tag `rust-v0.168.0` (not run):* a flag-less `codex
  resume` restores the model and the reasoning effort recorded for the thread (the
  `threads` row, `model` and `reasoning_effort`); `-m` on a resume overrides that,
  and core then writes the new settings into the rollout at once, so the in-session
  choice is gone from the record; an effort-only `-c model_reasoning_effort` on a
  resume also counts as an override and drops the thread's model; there is no alias,
  family name or "latest" for `-m`: the string must start with a slug in Codex's
  model catalog, and an unknown one is sent to the API as it stands (a warning in
  Codex's log, no error at start); `/model`'s Enter writes Codex's own config.toml
  (`model` and `model_reasoning_effort`, a new global default), while `s` in the
  picker and `/model <name>` change the session only.
- *Stated in Claude Code's docs and read in its 2.1.323 bundle (not run):* a resumed
  session keeps the model its transcript last used (the bundle's resume module
  exports `lastTranscriptModel` and `restoreModelFromSession`); effort is not
  restored from the session: it comes from the user's own Claude settings, saved per
  model, then the model's default; an Enter in `/effort` or the `/model` picker
  saves the level to those settings, `s` applies it to the session only, and
  `--effort` is for one run. The one measurement already in the repo (the paragraph
  above) agrees.
- *Measured on this machine, 2026-11-14:* `codex debug models --bundled` exits 0 in
  about 50 ms (84 to 107 ms in four later runs, one of them with an empty
  `CODEX_HOME` and an unreachable proxy and the same output, so it needs no network
  and no auth) and prints one line of JSON, a `models` array of 12 objects carrying
  `slug`, `visibility` and `priority` among many other fields; with it `terra`
  resolves to `gpt-6.1-terra`. And after this build was started, every member of the
  running brigade was resumed with no model flag and no effort flag (seen in the
  process list).

*Consequences, stated plainly.*

- A Goinkyo resumed after a banto restart no longer gets `--effort max`: its effort
  then comes from the operator's own Claude settings, as the session's model comes
  from its transcript. The `max` it is created with lasts as long as the process.
- The picker's choice is used once and is not stored (only the agent is). A Worker
  whose process died before its session existed is created again from the configured
  defaults, not from what the picker once said.
- Members that existed before this build keep whatever their own records say until
  it is changed in the session: nothing resets them.
- The 2026-07-25 incident above (a resumed Worker on the operator's own default
  model) is not re-tested by this change. Whether Claude Code restores the creation
  model for every session that is resumed is unknown; the docs say a resume keeps
  the transcript's model.

*Not done:* banto keeps no record of what a member was created with, so there is
nothing for a recreated Worker to be recreated from but the configuration; and the
Director has no model or effort setting.

## Decision (2026-11-14): an off-stage brigade speaks through its Director's row

Decisions of the operator, 2026-11-14, about how a brigade that is not on stage
reaches him. His own words were these: off-stage brigades should speak through the
Director's row in the list; an off-stage quiet fire should ring even while he works
in another brigade ("please ring it"); and he runs several brigades at once. The
reasons below beyond that are the Director's and the implementers', not his. This is
the first stage of a larger change to how brigades get his attention; only what is
built is described. The behaviour is written out under "The emporium: hosted panes"
("Attention", "The quiet brigade's bell"); this entry records what was decided and
why.

*What prompted it.* The quiet brigade's bell had a known defect (recorded in "The
quiet brigade's bell" until now): for a brigade that was not on stage nothing
reached him. Its fire latched a mark on the Director's pane, only a staged pane
draws one, and staging the brigade put the focus on that Director's tile, which
cleared the latch in the same update, so with banto's window focused there was no
mark and no bell at all.

1. **An off-stage brigade speaks through its Director's row.** The Director's row is
   in the list whether or not the brigade is on stage; it gets the waiting-colour
   dot and counts in `waiting N`. A hosted member whose pane is not on stage has no
   row of its own (members are hidden from the list), so its Director's row speaks
   for it: Claude `waiting`, Codex `blocked`, or a child BEL marks the row and rings
   once; a dialog the operator opened (`dialog open`) does neither.
2. **An off-stage quiet fire rings, even while he works in another brigade.** It is
   suppressed only while he is effectively on that Director's pane. *Why (his):* he
   runs several brigades at once and asked for it to ring. *(The implementers' added
   reasoning: it is the reason of the 2026-11-13 decision, a brigade going quiet is
   the moment his input is wanted, and a bell that waited for the brigade to be
   staged never came.)*
3. **One event, one ring.** The bell is rung when an event first happens (a wait
   first announced, a quiet fire first recorded), not when a row mark appears.
   Staging the brigade later does not ring again: the mark moves from the row to the
   tile of the member that is still waiting (a quiet fire's mark ends, since staging
   puts him on the Director's pane); nor does leaving a brigade for another with a
   wait that began on stage still standing, which marks the row without a ring. A
   second member's wait rings even while the Director's row is already marked; a
   second quiet fire under a quiet mark that is still standing does not.
4. **A pane on stage speaks only through its own tile.** A staged brigade's panes,
   and a pane staged alone, never also mark their Director's row. *Why (the
   implementers' reasoning, not his words):* counting both would count one waiting
   member twice.
5. **The join point of a member is a message id, not a time** (schema v18,
   `brigade_members.joined_after_id`). *Why (the implementers' reasoning, not his
   words):* clocks differ between processes and can step back; message ids are never
   reused and are ordered across processes. It bounds the two message rows of a
   collapsed tile, so a member no longer shows a broadcast sent before it joined and
   a new Goinkyo no longer shows the previous one's lines, even before its first
   observation (its token is reused by every consultation, so what was read about
   the previous one is forgotten when a consultation ends and when the next is
   summoned).

*Consequences and limits.* The row's mark is display only: it is a set kept apart
from the session's own activity, so the tile's word, the quit question, the quiet
watch and the relay never read it. It is memory only, so a restart starts with none.
A member that joined before v18 has no join point and shows what it showed before.
Schema v18 also adds `brigades.request_text`, `request_seq` and `request_at_ms`, a
Director's open decision request: `request_text` holds the constant `open` while one
is open and NULL otherwise (never anything an agent wrote), `request_seq` counts
requests ("Decision (2026-11-15)").

## Decision (2026-11-14): the default Worker briefing says where each claim rests, not that the working tree is forbidden

The operator approved a rewording of one sentence of the default Worker briefing
(`DEFAULT_WORKER_PROMPT` in `crates/banto-core/src/config.rs`; `worker_prompt` in
config.toml replaces the whole default, and a briefing the operator wrote is not
touched).

- *Before:* "Ground a claim about this repository on `git show HEAD:<path>`, never
  on the working tree — a tree can be mid-write, and reading one has misled this
  brigade before."
- *Now:* "Say which source each claim about this repository rests on: `git show
  HEAD:<path>` for what is committed, and the working tree only for files the
  Director has asked you to edit or review — a tree can be mid-write, and reading
  one as if it were settled has misled this brigade before."

*Why.* The absolute "never" made a Codex Worker refuse to review an uncommitted
change that the Director had pinned by sha256; reviewing before committing is how a
brigade works. The reasoning about why the operator could not simply override it is
the Director's diagnosis from the Worker's own quoted refusal, not the operator's
words: Codex receives the briefing as developer instructions, so what he typed could
not lift it. The wording was approved by the operator. A test pins what the sentence
must keep saying (the "which source" opening, `git show HEAD:<path>` for what is
committed, the working tree for files the Director asked for) and that "never on the
working tree" is gone.

## Decision (2026-11-14): work given to a session is a key or a paste, not every event forwarded to it

Two records of operator input exist since 2026-11-14, because one notion was doing
two jobs.

- *Forwarded to the pane:* every key, paste and mouse report the child received, a
  wheel turn, a drag, a press and a release included. Its reader is the relay, which
  keeps out of the operator's way: every forwarded event, mouse included, still
  holds a nudge (see "Auto-relay" and "Mail on the way") and feeds its judgement of
  unsent input.
- *Work given to the session:* a key forwarded to the pane (one that encodes to no
  bytes included, and the prefix sent through literally) or a paste. No mouse event
  is work given: a press, a release, a drag and a wheel turn are not in it (a bare
  pointer move was never forwarded). Its readers are the quit question's
  recent-input window and the quiet watch's "was given work".

*The incidents, in order.* (a) The operator reads a pane's history with the mouse
wheel. Having scrolled the Director's pane, he clicked the list and pressed Esc, and
banto asked "1 session(s) not idle" for three seconds with nobody working. The same
over-broad notion let a wheel notch over a freshly resumed pane count as work given,
so that process's first not-idle reading armed the quiet watch: established by a
test at the engine level. Whether that is what rang the bell he heard after a
restart is not established: what a resumed pane reports at start is unmeasured. (b)
The first fix kept a button press as work given, and he then got the same question
every time he clicked into the Director's pane and back to the list: a click is how
he moves into a pane and out of it, not an instruction to the agent. So no mouse
event counts.

## Decision (2026-11-14): start-up is not work

For 10 s after a member's process started (`PROCESS_START_GRACE`), a not-idle
reading does not arm the quiet watch through the "sampled idle before" rule. Being
given work (the operator's key or paste, banto's own nudge or kickoff) arms at any
age; work still going when the grace ends arms at the first reading past it; a
relaunch under the same key gets a new grace, and on `Spawned` the stamps of work
given to the previous process under that key (the operator's keys and pastes,
banto's own nudges and kickoffs) are dropped, so work given to a process that was
replaced never counts for the new one, including when both fall in the same
event-loop drain (the relay's record of forwarded input is kept); a rename keeps the
process's start; the Director is covered like any member; a pane with no recorded
start has no grace to wait out.

*What was measured.* On the operator's trial the bell rang 9.4 s after a restart
with nothing asked of anyone (bell log: `quiet_focused`, a quiet fire with the
Director's pane focused and the "bell always" switch on); a replay of the same run's
recording reproduced it to the millisecond. The Codex Worker's pane read idle on the
shell's first sample at 1.05 s, reported `working` by itself at 2.06 s and `idle` 75
ms later, and that report was taken for work. In another recorded run the same
report lasted 2.34-2.97 s and fell between samples, so nothing armed: the bell rang
only when a sample landed inside the window. The start-up reports ended 1.4 and 2.0
s after the spawn in those two runs and the Claude panes settled within about 3.5 s;
10 s is a margin, not a derived value. *Unexplained:* one earlier ring at a resume.
That run had no bell log, and a replay of its recording shows no ring near the
resume.

## Decision (2026-11-15): a tile's word is busy, waiting or idle, never alive

The operator's decision, 2026-11-15, in his words: "alive" is a word in contrast to
"dead"; what he wants from a tile is whether the session is working or idle, so the
words converge on "idle" — a hosted session waiting at its prompt reads "idle"
whichever agent it is.

*The incident.* After a restart the four Claude Workers' tiles read "alive" and the
Codex Worker's tile, open and waiting at its prompt with its own report saying idle,
read "idle" (there meaning "no live process proven") for about a minute and then
"alive" when an unrelated list reload happened. Measured from a recording: the title
flipped 59.9 s after the spawn, at the first reload after the operator's Enter in
another pane. The Codex liveness check (of its logs database) runs only on a list
reload, and an idle Codex pane causes none.

*What the code does.* The word of a hosted pane's title row is `waiting`, `busy`,
`idle` or nothing, from a word-only decision (`EmporiumState::pane_word`): waiting
from the existing waiting sources or from the member's live file having said
`waiting` in the latest relay tick (found in review: a hosted Claude Worker whose
row was last loaded as live read busy until the next reload; for that short window
the tile can say waiting before the mark appears), else the single idle decision the
quit question reads, with nothing read yet showing no word and the quit question's
3-second recent-input window not moving it ("The emporium: hosted panes", the strip
and title paragraph). The marks, the count, the bell and the relay still read the
older decision (`displayed_activity`) and are unchanged; the code comment on
`pane_word` gives the reason they are kept apart (a renamed pane whose row has not
arrived is unknown to them, which keeps an announced wait from ringing twice). In
the session list, a row whose session is hosted here but whose own files prove no
live process is shown as live (`App::set_hosted_ids`): a busy or waiting activity
stands, ending the pane gives the row its own state back, a reload does not lose it,
and the rows of sessions not hosted here are unchanged. `banto list` keeps "alive"
for a live process, and `brigade_status` for a Codex peer now reads the rollout's
last turn marker (its words are under `brigade_status()`). A background command
(`shell`) is a not-idle reading that this decision drew busy on a tile and the next
entry draws idle with a mark.

## Decision (2026-11-15): a Director can tell the operator it is waiting, without stopping

Decisions of the operator, settled over 2026-11-13 to 2026-11-15, written from the
code as it is in the working tree of 2026-11-15 (still under review): a Director can
tell the operator that it is waiting on a decision only he can make, and carry on.
The reasons marked as his are the ones he gave; the rest are the Director's or the
implementer's.

1. **An MCP tool, `request_decision()`, Director-only, that does not block.** It is
   not an ask, and its result does not claim anyone was notified. *His reason:*
   stopping to ask is what the agent's own question picker is for, and a Director
   that is `waiting` cannot be nudged, so a request must not make it wait.
2. **Two lifetimes.** *His reason:* the mark and the bell mean "not looked yet" and
   the tag means "not answered yet". The mark ends when the operator reaches the
   Director's pane; the tag stays until he answers.
3. **The tag is cleared by Enter, not by any key.** *His reason:* a keystroke in a
   draft he was already typing would otherwise erase a request he had not read.
4. **The switch is a config key, `[brigade] request_attention` (default on), and F4
   stays two-state.** *His reason:* with F4 off and the operator in the Director's
   pane a request does not ring, the same rule as the quiet bell. With the key off a
   request is the tag only.
5. **No text: a short fixed tag only (`▸ decision`).** *His decision, after trying
   the first design:* the behaviour was right in all three situations (in the
   Director's pane, elsewhere in the brigade, the brigade off stage), but neither
   the title row nor the list row is wide enough to read a sentence; the real
   question is on the Director's own screen; and a place wide enough for the full
   text would need list space set aside for it, or scrolling, which he judged not
   worth it. *History:* the first design took one line of up to 200 characters, made
   safe by banto (control and format characters dropped, whitespace folded, cut on a
   cluster boundary), stored on the brigade, carried in the tick and drawn on the
   Director's list row and title row; it was tried and dropped for those reasons,
   with its sanitising, its caps and the text it carried.
6. **The default Director briefing's second point:** when the operator alone can
   decide and work stops on it, write the question in your own output and then call
   `request_decision`; it shows the operator that you are waiting, not the question,
   and does not block. An operator's own `director_prompt` is not touched.

*What the code does.* The tool takes no parameters and records only that the brigade
is waiting on the operator's decision; the Director writes the question in its own
pane first, and the tool's description and result say so. A caller that still sends
`text` (an agent briefed by an older prompt) has it ignored, and nothing an agent
wrote is stored, carried or drawn through this path. It resolves the caller by the
store's live membership (a Worker or a Goinkyo gets a tool error; outside a brigade,
or in a disbanded one, the usual refusal), and `Store::set_request` re-checks inside
its own transaction that the caller's token is still the brigade's Director, and
writes nothing otherwise. The store keeps it on the brigade: `brigades.request_text`
holds the constant `open` while a request is open and NULL otherwise, `request_seq`
counts requests (so a second call is a new request though nothing in the row
differs), `request_at_ms` stamps the time, there is no migration, and an open
request is "not NULL", so a hand-edited value cannot fail the read;
`Store::clear_request` clears only while the sequence is still the one answered, so
a newer request is never wiped, and the sequence stays. It reaches the core in the
per-tick brigade facts as its sequence number alone (`BrigadeNameFacts::request`, a
defaulted field, so an older recording still parses, and a recorded Tick holds none
of the Director's words); a store read error leaves the brigade out of that tick's
facts and the core keeps what it had. The open state is adopted at every tick. A
request is *new* only when its sequence is above the highest the core acted on; a
request that already stood when this run first saw a brigade is silent: the first
tick that visits it seeds that number without effect, so a stored request is drawn
again after a restart (on its Director's list row even for a brigade nothing hosts
yet) and neither rings nor marks. A brigade formed in this run has seen nothing, so
a request stored before the first tick that visits it is new and marks and rings
under the usual rules (found by the Codex reviewer and confirmed by a test: the tag
showed but nothing rang or was marked). A second request is new again and rings
again; the same sequence seen again does nothing; a sequence at or below the one the
operator answered is not brought back by a late or failed clear.

A new request with `request_attention` on and the Director hosted spends the
brigade's next quiet point: the next moment every hosted member has been idle for
the settle time passes in silence (no ring, no record, no mark), whether or not a
fire was due, and after it the watch works as before. The spending survives the
not-idle readings of the stretch in progress and banto's own nudges, and ends early
when the operator sends a key or a paste (the Enter that answers included) into a
pane of that brigade: from then on the stretch is his and its end rings. A second
request spends again; a Director that is not hosted spends nothing, so no spending
is left to eat a later, unrelated quiet. The spending goes with the quiet watch:
when no Director pane of the brigade is hosted (its process ended and it was not yet
reopened) nothing stays spent, so a reopened Director's first stretch of work is
announced as usual (found by a reviewer's probe: a request, the Director's pane
exiting, a reopen, and the quiet after the reopened Director's work was swallowed);
a Worker's relaunch leaves the Director hosted and keeps the spending. It is the
quiet point that is spent and not the watch's armed flag, because a Director calls
the tool in the middle of a turn and its own not-idle readings re-armed the watch in
the same tick (found by both reviewers and reproduced: the brigade rang twice,
`request` and then `quiet_focused` 8 s later). The request then does what a quiet
fire does under its own bell cause (`request`): with the operator effectively
focused on that Director's pane there is no mark and the bell rings only if F4 is
on; anywhere else the brigade is recorded (the Director's row marked, staged or off
stage), and the bell rings whatever the operator is doing elsewhere. It is one
record per brigade: a quiet fire while a request's mark is still unlooked at does
not ring again, and one after the mark is looked at rings as usual while the tag
stays. The mark counts in `waiting N` like a quiet fire's; the tag alone, once the
mark is gone, does not. With the key off, a request is the tag only: no record, no
mark, no bell, and no quiet point is spent. There is no rate limit beyond the bell
cooldown.

The tag is drawn on the Director's list row whenever its brigade has an open
request, on stage or off, and also on the Director's tile title row while the
brigade is staged ("The title row of a tile" for the layout). The operator's remark
on the trial was that showing it on the list off stage but not on stage was
inconsistent; the Director's reasoning, not his: the list is the overview of every
brigade and what a row says should not depend on which brigade is staged, and "a
pane on stage speaks only through its own tile" is about marks and the `waiting N`
count, which the tag is not, so showing it on the row and on the tile double-counts
nothing. It stays, even while the Director's pane is focused, until the operator
sends Enter into that pane: the key that submits what was typed, which reaches the
child as a carriage return. Another key, a paste, any mouse event, a focus change,
Shift+Enter, Ctrl+J, an Enter into another pane, and banto's own
nudge and its submitting Enter do not clear it. Clearing removes the tag from the
core and issues the conditional store clear. Disbanding drops all of it with the
brigade's row. There is no collapsed-body rule: a collapsed Director cannot exist
today.

*Evidence (bell log, 2026-11-15, the operator's trial).* One `request` ring per call
in each of the three situations, and no quiet ring after the Director's turn; after
the operator's answering Enter, the end of the Director's next stretch rang as a
quiet fire.

*Known limit, not about requests.* Texts banto did not write that go through the
column-shortening helper (brigade names, the first line of a message) can still
carry an unbounded run of combining marks: a follow-up.

## Decision (2026-11-15): a background command is named, not hidden

Written from the code as it is in the working tree of 2026-11-15 (HEAD plus an
uncommitted change, still under review). A Claude session whose live file says
`shell` (the turn has ended, a background command of the session still runs; "Data
sources") was drawn busy by every surface until this change. Two decisions met on
it: item 8 of the first decision of 2026-11-13 made it show busy, so that a command
left running is never overlooked, and the tile decision above says a tile answers
whether the session is working or idle, where "the turn has ended" is idle. *His
words, translated, where there are any:* on the mark, `bg` is fine, "I understand
it"; on keeping the quiet watch as it is, "since the Director can now actively tell
me it is waiting (`request_decision`), the times I want to sense that the brigade
has gone quiet have become relatively fewer". *Not his words:* the choice of `idle`
over `busy` for the tile's word is read off the two decisions above, and no reason
of his for it is recorded here.

1. **A tile says `idle · bg`.** The mark `bg` follows the word, in the age text's
   colour, and sheds with it (`Claude · idle · bg · output 2m ago`, then without the
   agent, then `idle · bg · 2m`, then `idle · bg`, then nothing); it never appears
   or disappears without the word, and with a Director's decision tag the pairing
   holds (`idle · bg · ▸ decision`).
2. **The quit question still counts the pane, and says why.** Each counted pane is
   named with its reason: `Esc again to quit — 2 not idle: a (unknown), b (busy)`;
   only background commands: `Esc again to quit — 2 in background: x, y`; both:
   `Esc again to quit — 2 not idle, 1 in background: a (unknown), b (busy), c (bg)`.
   A pane that took input in the last 3 seconds is counted as not idle, and no pane
   is counted twice.
3. **`brigade_status` says what it is.** A shell peer prints as `idle (background
   command running)`; a Codex peer prints `busy (turn in progress)` or `idle (turn
   ended; background work not visible)` from its rollout, or `running` or `not
   running` (see `brigade_status()`).
4. **Unchanged.** The list row is still busy (the green dot) and so is `banto
   list`'s tag; the marks, counts and bells; the quiet watch and a request's
   spending of it, which treat `shell` as they treat busy; a shell member is still
   nudgeable; the input-line notice; and Codex, which has no such value.

*What the code does.* The quit question's fact gains a value,
`Quiescence::Background`, beside idle, not idle and unknown; it is a kind of not
idle, and every reader that asks whether ending the session loses something asks
`Quiescence::is_working`, so the quit count and the brigade watch hold it exactly as
they hold not idle. It is decided where the live file is read (`claude_quiescence`):
`shell` is background only when it is the whole reason the session is not idle, so a
`shell` entry beside a busy one, any waiting one (a dialog included) or an
unrecognised status is not idle, beside a live idle entry it is background, and a
dead entry says nothing, in whatever order the entries come. A Codex pane's own
report and the rollout never produce it. The tile's word is still
`EmporiumState::pane_word` (background reads idle, a waiting row or live file
outranks it), and the mark is `EmporiumState::pane_background`, true only where
`pane_word` says idle and the sample says background, so waiting never carries it.
The quit count (`EmporiumState::quit_counts`) keeps two numbers from one pass over
the hosted panes. A recording made before this change holds `shell` as not idle and
replays as that, so it cannot show the mark or the new prompt; the new value is
written into recordings made from here on.

*Not measured.* All of it is read from the code and its tests; no observation of it
on a running pane is recorded here (the status `shell` itself was measured on
2026-11-13, "Data sources"). The widths at which the title row sheds are in the
tests, not here.

## Data sources (measured 2026-07-19, Claude Code 2.1.215)

| Source | Content |
|---|---|
| `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl` | Session body. One JSON per line |
| First few records of the jsonl | `{"type":"custom-title","customTitle":...}` / `{"type":"ai-title","aiTitle":...}` — the title can be extracted by reading only the head chunk. Older formats fall back to the first user message |
| First record of the jsonl | `{"type":"agent-setting",...}` marks a session run by a spawned agent (subagent / Agent-Teams teammate); interactive sessions open with a `mode` record instead. Sets `SessionMeta.is_agent`, persisted in the store (schema v2) |
| `~/.claude/sessions/<pid>.json` | Live state of a running session: `pid`, `sessionId`, `cwd`, `status` (`busy`, `waiting`, `idle`; and, measured on Claude Code 2.1.323, 2026-11-13, `shell` — the turn has ended but a background shell command of the session, or of one of its sub-agents, is still running; when that work ends Claude is re-invoked, reads `busy` again and then `idle`), and `waitingFor` beside `waiting` (measured the same day on 2.1.323: `"input needed"` while the session sat in the picker of a question the model had asked, `"dialog open"` while the OPERATOR had a slash-command picker such as `/model` open that he had opened himself, after which `status` went back to `idle`; **not measured:** the value for a permission prompt, for a dialog the product opens on its own, and what `status` says when a dialog is opened over a running turn; a table of reason strings — `permission prompt`, `sandbox request`, `worker request`, `goal proposal`, `input needed`, `dialog open` — was only read in the Claude Code bundle; banto reads the field leniently, a value that is not a string is ignored and keeps the record, and only the exact string `dialog open` means anything), `kind` ("interactive"/"bg"), `name`, `updatedAt` — observed present on both Windows and Linux/WSL. `procStart` is the process's own kernel-reported start time: Linux/WSL records `/proc/<pid>/stat`'s raw `starttime` ticks, while Windows records the raw creation FILETIME (each matched directly against a live process on this machine, 2026-11-10). `RawLiveSession` carries it as optional, so an absent or unrecognized value falls back to bare pid liveness; when it is comparable, banto rejects a recycled PID that belongs to a different process. (Observed directly on this machine's installed Claude Code version; not claimed for every version or platform.) |
| `~/.claude/history.jsonl` and others | Unused in the MVP |

**Timing (measured 2026-07-25, Claude Code 2.1.219).** The session-body and
live-state files do not appear together: `sessions/<pid>.json` is written at
**startup**, while the session's `<uuid>.jsonl` is only created at its
**first recorded activity** — a turn, a slash command, a `/rename`. An
untouched session therefore has an id and no history file at all,
indefinitely (observed: a Worker sitting at its prompt for minutes with no
jsonl anywhere).

That is why id discovery for a freshly-spawned session (emporium brigade
Workers) matches `sessions/<pid>.json` by the pid banto itself spawned, and
falls back to scanning session files only when the direct child isn't
`claude` itself. Waiting on the jsonl alone deadlocks: an unidentified Worker
is invisible to the relay engine, so it can never be nudged into the first
turn that would create the very file discovery is waiting for.

**A hosted Claude pane follows its process to another session id.** `/clear`, `/branch`
and an in-pane `/resume` move a running `claude` to a different session id while its pid
stays, and its live file starts naming the new id (auto-compaction does not: 2.1.318 and
later compact in place, same file and id). Each tick banto reads, once for the whole tick,
the roster and the live files, and for every hosted Claude pane whose own process
(its pid, or the nearest `claude` below it) reports an id other than its key: a brigade
member is renamed and its row moved (`MemberSessionForked`, the name the event keeps in
the record/replay format and the pane log); any other pane is renamed with no store write
(`PaneSessionMoved`). Without it the pane would read unknown (no live file names its key),
the quit confirmation would count it, and its next resume would open the conversation it
left. A move onto an id **another pane already holds** (an in-pane `/resume` of a session
open elsewhere) changes nothing, the store included, since renaming would put two panes
on one key and writing it would resume the conversation twice (invariant 4); the status
line says so once per pane and id. "Already holds" covers a pane being opened and another
member's row whose pane is closed (`SetMemberSession` moves a session id between rows); it
covers a member whose own pane is gone too, since that write would take the id as well. The
refusal is not final: the move is sent again every few ticks while the process still reports
the new id, so the pane is renamed once what held the conversation lets go (the open fails,
the other pane closes, the other row changes). A move is believed only after the same one has
held for two consecutive ticks (a live file missing or torn for one tick while a nested
`claude -p` runs must not rename the pane onto the nested session and back), and only from a
live file whose process is alive, a dead file naming the pane's own key included. A tick that
could not read every brigade's members names no move at all, member or solo: the missing
brigade could be where the pane belongs or the row that holds the id. A Codex pane has no
live file, so none of this reads it; a Codex *member* is followed another way, next.

**A hosted Codex member follows its pane to another thread, from what its hook recorded.**
Codex switches thread inside one process, same pid, with `/new`, `/clear`, `/fork` and an
in-pane `/resume` (measured on Codex 0.169.0, whose log database attributes the new thread
to that pid). The SessionStart hook then fires once, at the next typed turn, with the new
thread's `session_id`, and `banto _hook` records it in the member row's
`briefed_session_id` and `briefed_at`. When that id is not the row's `session_id` and was
recorded at or after the moment the pane's process was spawned (the column is never cleared,
so an older value is a previous process's) the gather proposes the same `MemberSessionForked`
as for Claude, through the same watch: believed after two ticks, sent again while the core
refuses it, held ids refused as above (another member's row, a pane, an open in flight),
nothing changed and the store unwritten. The hook's word alone is not proof, so two more
things must be confirmed first: the newest row of the new thread in `logs_2.sqlite` names the
pane's own child pid, with the start-time check that guards against a recycled pid
(`codex_liveness::is_thread_owned_by`), and the new thread is a listed Codex session. The
listing is the only guard against a sub-agent thread of the same process: such a thread may
run the hook and its log rows would name the same pid, but neither has been observed (a
read-only count on the development machine found eight recorded hook rows, none naming a
sub-agent thread, while most of its threads were sub-agent threads). The guard rests on the
provider recognising the sub-agent `source` shape, so a Codex release that changed that
shape would let a pane be re-pointed onto a sub-agent's thread: the wrong identity, though
never a double open. An unconfirmed change moves nothing and is told once per stretch, after
thirty ticks, because neither the log row nor the reload that lists the new rollout has been
timed against the hook and a notice about a thread that is confirmed seconds later would read
as a verdict (`<member> of "<brigade>": Codex switched to session <id>, which banto cannot yet
tie to this pane (it is not a listed Codex session | Codex's log does not show this pane's
process writing it); not following it`); once it is confirmed the pane moves with no word.
The count is kept across a tick that could not read the roster, so such a tick neither
restarts it nor lets the notice be said twice. A move already admitted is not checked against the log
again while the core refuses it. Which command switched the thread (`source`) is neither
stored nor read; it is not needed to follow the pane.

Consequences, none repaired by banto:
- A brigade is named by its Director's current session title, so after a Director's `/new` it
  is named by the new thread's title.
- The old thread reads as running while the process lives, since its newest log row still
  names that pid, so banto refuses to open it in another pane. That fails safe.
- `/new` in a git working directory can put the new thread in a worktree, and staging takes
  the Director row's cwd for fresh Workers and the Goinkyo, so those start in the new
  thread's directory.
- Between the command and the next typed turn nothing says the pane moved: the hook has not
  fired. That gap is benign, since a nudge that arrives in it becomes the new thread's first
  turn, which fires the hook.
- **Not covered:** a solo Codex pane. No hook runs in it and it has no row to record the
  change in, so it keeps the key of the thread it was opened on.

Note: the format is undocumented and subject to change. Defend with **lenient
parsing** (ignore unknown records/fields, skip broken lines) plus tests against
synthetic fixtures. **Never bring real session data into the repository.**

## Codex data sources (observed 2026-07-26, this machine's installed Codex CLI)

Nothing like Claude Code's layout: no `projects/`, no per-session `.jsonl`
directory tree banto walks itself. Discovery reads a sqlite table and a name
index; the behavior below is from banto's implementation unless marked observed.

| Source | Content |
|---|---|
| `$CODEX_HOME` (else `~/.codex`)`/state_5.sqlite`, `threads` table | One row per session. Columns banto reads: `id`, `title`, `cwd`, `rollout_path`, `first_user_message`, `updated_at_ms`, `archived`, `source`, and, when present, `history_mode` and `name`. The filename (`state_5.sqlite`) is what this machine's installed version uses; a future Codex CLI could rename it |
| `$CODEX_HOME/session_index.jsonl` | Legacy names used by discovery: for each id, the last valid physical record with a nonblank `thread_name`, trimmed; `updated_at` does not choose the winner. Malformed, invalid UTF-8 and over-1 MiB lines are skipped; a valid final line need not end in a newline. The limit includes the newline when present. Synthetic tests cover these choices; the ordering agrees with the batch reader in Codex source tag `rust-v0.169.0`, not an observation of the installed binary |
| `threads.rollout_path` | The session transcript file itself, under a `sessions/` tree whose internal (date-partitioned) layout banto does not rely on — the path comes from the column directly, never from walking the tree |
| `threads.cwd` | Carries a Windows extended-length path prefix (`\\?\`) that a rollout file's own recorded cwd does not — normalized once, in `provider::codex`, at the point this becomes `SessionMeta.cwd`. It is also the *resolved* path in the on-disk letter case (measured, Codex 0.169.0), so tying a freshly started pane to its thread compares the row's raw `cwd` column (the normalization above is for `SessionMeta.cwd` only; the comparison writes the verbatim prefixes plainly itself) with the **spellings of the spawn directory** (`file_identity::DirectorySpellings`: the typed spelling and its `canonicalize`d form, verbatim `\\?\` and `\\?\UNC\` written plainly, letter case ignored only on a volume probed to fold it), never with `==` and never by opening the reported path: the spellings are measured once, on the directory banto has just spawned in, and a reported path (a transcript cwd, a registry key) is only text, because opening one on a dead share blocked the UI for 21 s (measured). The Claude live file and transcript head are matched the same way — Claude records a symlink's target in the case typed — and so is the directory-trust key check. Not covered by spellings: a path only partly resolved, a bind mount or a hard-linked directory, a short (`PROGRA~1`) name; none has been seen reported |
| `threads.updated_at_ms` | Milliseconds since the Unix epoch, application-reported by Codex — not a filesystem mtime the way Claude Code's is. `SessionMeta.mtime` says so |
| `threads.source` | Who started the thread. Measured (Codex 0.147 and 0.166–0.167.1): an ordinary session is plain text — `cli`, or `exec` for `codex exec` — while Codex's own automatic-review sub-thread of a session (title `Guardian review`, model `codex-auto-review`, one appearing beside every launch or resume) is a JSON object in the same TEXT column, `{"subagent":{"other":"guardian"}}`. `provider::codex` **does not list** a row whose `source` parses as a JSON object with a `subagent` key — in both `discover` and `find_new_sessions`, so no list, no Worker discovery and no liveness decoration can take one for a session. Everything else stays listed: plain text of any value (including ones never seen), empty, an object without that key, unparsable text, a non-text value, text that is not valid UTF-8, and a schema with no `source` column at all — only a positively recognised sub-agent is hidden, since hiding a real session costs more than showing a stray thread. Decided by `source`, never by `threads.thread_source` (newer, nullable, its value for the same thread changed between those versions) and never by title. Anything banto keys by such a thread's id (pins, groups, archive marks) simply no longer matches a listed row |
| `$CODEX_HOME/logs_2.sqlite`, `logs` table | Process liveness (the Codex-side double-resume guard, `codex_liveness::is_thread_alive`), fresh solo Codex attribution during discovery, and the check that a Codex member's new thread is its pane's own (`codex_liveness::is_thread_owned_by`, under "A hosted Codex member follows its pane"). Real schema, read from a real, live `~/.codex/logs_2.sqlite` on this machine (2026-07-27): `id, ts, ts_nanos, level, target, thread_id, process_uuid, ...`, with `CREATE INDEX idx_logs_thread_id_ts ON logs(thread_id, ts DESC, ts_nanos DESC, id DESC)` — the newest row per `thread_id` is exactly what that index is for |
| `logs.process_uuid` | `pid:<PID>:<suffix>`. Confirmed against that same real database: the newest row's pid for the session actually running matched a real, live process (cross-checked via `Win32_Process`); three finished sessions' newest rows named three dead pids |
| `logs.ts` | Unix **seconds** (confirmed by comparing a real row's value against the current clock — not milliseconds, unlike `threads.updated_at_ms`). Compared against `sysinfo::Process::start_time()` (also unix seconds, and available on every platform `sysinfo` supports — unlike Claude's Linux/WSL-only `/proc` ticks comparison) for the pid-recycling guard: a process that started strictly after this log row was written cannot be the one that wrote it |

**Reading a database Codex itself may be writing (measured directly against
both `state_5.sqlite` and, independently, `logs_2.sqlite` — including a read
against the real, live `logs_2.sqlite` on this machine while it was warm —
rather than assuming one database's answer transferred to the other;
rusqlite 0.40.1 bundled, the version this workspace pins).** Two open forms,
both able to read correctly, neither always safe alone:
- `mode=ro` is always correct — it sees every committed row, including a
  writer's in-flight commits — and on its own writes nothing, but opening it
  against a **cold** database (no `-wal`/`-shm` sidecars yet) creates those
  sidecars on first touch: a write into a directory banto promises never to
  write to.
- `immutable=1` never creates a sidecar, but read against an **actively
  written** database it can silently return a stale or incomplete snapshot —
  wrong, not just old.

The resolution: stat for the `-wal` sidecar first. Present -> `mode=ro`
(warm, correct, no new sidecar since one already exists). Absent ->
`immutable=1` (cold, correct, no sidecar created). This is correct in every
case and zero-write in every case but one: **crash residue** — `-wal`
present, its `-shm` sidecar absent (e.g. Codex was killed mid-write) — where
`mode=ro` recreates a fresh `-shm` on that first read, a single ~32KB write.
It is SQLite's own coordination index, not banto's data, and Codex's own
next run would create it regardless, but it is a write, so the README's
read-only section names it rather than smoothing it over.

One more edge, benign: a **TOCTOU** gap between the stat and the open — a
database that was cold a moment ago gets a writer between the two, and this
approach opens it `immutable=1` anyway, returning the pre-write snapshot
rather than an error. Stale for one poll cycle, not wrong, and
self-correcting the next time discovery stats and finds `-wal` now present.

Note: like Claude Code's format, this is undocumented and can change between
Codex CLI versions. Same defense: **lenient parsing** (a malformed row is
skipped, not fatal) plus synthetic-fixture tests. **Never bring a real
`~/.codex` database into the repository.**

Each discovery reads the whole name index and keeps the last usable name for
each distinct id in memory. The per-line bound limits a single allocation,
not total scan time or the number of names retained; both grow with the index.
For a legacy row (including a missing or unrecognized `history_mode`), a
nonblank `threads.title` distinct from the trimmed first user message wins;
otherwise an indexed name wins, followed by the original database title. For
`paginated`, banto uses the trimmed nonblank `threads.name`, then the trimmed
nonblank first user message, then the session id if neither exists; it does not
use an older title or index name. The first-message fallback is banto's display
choice. An unreadable or absent index leaves the other fallbacks in use, and a
read error partway through leaves the names already read in use. The mode-based
name selection is guided by the list reader in Codex source tag `rust-v0.169.0`
and exercised with synthetic databases; the full cause of any observed missing
session remains unverified.

**Live-reload watch:** `crate::watch` watches `CodexHome::rollout_dir`
(`<codex_home>/sessions/`, recursively) so a new session's rollout file
appearing triggers the same debounced reload a Claude Code change does. It also
watches an existing Codex home non-recursively for create, modify and remove
events naming exactly `session_index.jsonl`, including creation and replacement.
Access events and unrelated paths, including sqlite and log files, do not trigger
reloads through this home watch. A home missing at startup is not watched later;
failure to register the optional home watch leaves the other watches running.
Synthetic watcher tests cover index creation and replacement; they do not prove
what events an installed Codex emits for a rename. Measured against a synthetic
WAL-mode database shaped like `state_5.sqlite`: a write never touches the
main file's own mtime (everything lands in its `-wal`/`-shm` sidecars), and a
running session writes somewhere in `$CODEX_HOME` on every turn —
`logs_2.sqlite` on every log line — so an unfiltered directory watch would
fire far more often than the rollout tree's one event per turn, across
database writes that do not require a session-list reload.

**Busy/idle detection** (the relay engine's Codex-side signal —
`crate::codex_activity`; Claude Code publishes its own `status: busy` in
`sessions/<pid>.json`, which Codex has no equivalent of): no source above
records "is this session mid-turn right now" directly, and two timing-based
readings of `logs_2.sqlite`/the rollout file were tried and measured unsound
before landing on a third. Rollout mtime stayed pinned at its first-write
timestamp for an entire generation (file size grew 18KB -> 75KB across about
a minute of active writing; the reported mtime never moved). A
`logs_2.sqlite`-write-staleness threshold (120 seconds idle) was checked
against three real sessions' own `task_started`/`task_complete` markers as
ground truth and found wrong outright: the largest genuinely-within-a-turn
gap measured was 135 seconds, the smallest genuinely-between-turns gap was 1
second — those two distributions overlap completely, so no fixed threshold
can separate them. What is sound: tailing the rollout `.jsonl` itself for its
own `task_started`/`task_complete` event markers, a state read rather than a
clock — paired 1:1 with no exceptions across all 9 turns observed. The tail
window grows (`tail_turn_marker`/`TAIL_MAX_BYTES`) rather than staying fixed,
since a `task_complete` record was observed re-embedding the turn's full
final message on one line north of 11KB — a small fixed tail can land
entirely inside that one line and misreport idle right when the answer
matters most.

**What the markers mean, and where the signal stops.** The same tail is what the
emporium's "is it idle?" question asks of a hosted Codex session (see "The emporium:
hosted panes" below). A started turn is `task_started` (alias `turn_started`); a
turn that ended is `task_complete` (alias `turn_complete`) or `turn_aborted` (an
interrupt, a replaced turn, an ended review, a budget stop); the last such record in
the tail decides (a start after an abort is mid-turn again; what follows a
`replaced` abort is not among the facts below, so nothing is assumed about it), and
no marker at all is "unknown", never "idle". The relay's Codex-side reading is the
same function, so an interrupted Codex member now counts as idle for nudging too;
before, it read as mid-turn until its next turn. *Read in the Codex source at tag
`rust-v0.168.0` (not measured here):* all three are written to the rollout in both
of Codex's history modes; an errored turn ends in a `task_complete` that carries an
`error`; the requests that block a turn on the operator (approvals, questions,
elicitations) are never written to it; and a process that dies mid-turn leaves a
`task_started` with no end. **Known limit of the rollout:** Codex records nothing
about a command it left running after the turn ended. *Read in the same source:* a
background terminal survives the end of the turn and is stopped only by `/stop` or
at session shutdown, its exit is not durable in the legacy history mode, and nothing
re-invokes the model when it ends. *Measured 2026-11-13 on this machine's installed
Codex (`codex-cli 0.168.0`):* a command left running stayed alive for 108 s after
`task_complete`, with no rollout or sqlite change and no re-invocation at its end.
Read from the rollout alone, such a session reads as idle.

**Limitation, read from the code and not observed:** a Codex process that dies inside
a turn leaves `task_started` with no end in its rollout. While no process runs on that
session nothing reads the stale marker: the pane's screen is gone (`update_pty_exited`),
so the relay's gather skips the member and `mail_pending` holds no bell for it, and
`brigade_status` checks that the thread is alive first and says `not running`. It
bites once a process runs on that session again (an in-place relaunch, or a
re-stage) and until that session's next turn ends: `is_thread_idle` then reads the
rollout alone, so the relay reads the member as busy and never nudges it,
`brigade_status` says `busy (turn in progress)`, and its unread mail holds the
brigade-quiet bell (`mail_pending`: mail the relay is still going to deliver does
not count as the brigade waiting on the operator). Not seen: on this machine, a count
of 773 Codex turns on 2026-11-19 found none cut off, so the consequences above
follow from how the readers are written and have not happened.

**The pane's own report (OSC 7501).** The Codex TUI announces its lifecycle state in
its terminal output, and banto reads that announcement. The format is the Program
Status Protocol, a published protocol and not Codex's own: `ESC ] 7501 ;
state=<idle|working|blocked|done|error>:app=codex[:kind=<permission|question|auth>]
ST` (ST is `ESC \` or BEL; spec
<https://www.superlogical.com/rex/docs/build/program-status>, proposed by Mitchell
Hashimoto in <https://mitchellh.com/writing/program-status-osc7501>; it also has
`id=` sub-records, `progress`, `title`, `msg` and a `?` feature query). How it is
read: the pane's `vt100` parser hands every OSC it does not implement to
`Callbacks::unhandled_osc` once the sequence is complete, whichever chunks it
arrived in; `banto_core::program_state::parse_osc` reads the one it cares about (at
most 4096 bytes; an unknown state, a missing `state=` or the `?` form is not a
report, except for the cut-short case below; unknown keys are ignored). The terminal
bytes the parser sees are untouched, and the report arrives inside
`Event::PtyOutput`, an event that already exists, so it is recorded and replayed
with everything else. Only the **root record** is kept; a report naming a sub-record
(`id=`) is parsed and dropped, and the `?` query is **not answered**. `clear`
removes the pane's report, which sends it back to the rollout rule. It is held per
process (a new process starts with none) and read only for a pane known to be Codex
*now*: the product is read from the current session list row and from the relay's
current facts about its member, each time it is asked, and never kept. If the two
both speak and disagree the pane is not known to be any product, and neither is a
pane they both leave silent; for such a pane the rollout decides, as for one that
never reported, and nothing is laid over the list (what the pane said is kept and
counts again if it becomes known to be Codex). A pane of another product is never
read this way. Why (found in review): a remembered "is Codex" could be up to a tick
stale after a list reload and let an old report decide. Two parser details. The
terminal parser keeps the sequence number and fifteen more `;`-separated parameters,
so text after the fifteenth `;` of a payload never arrives (a conforming report has
no `;` outside `id` and unknown keys; Codex writes `state` first). A payload that
fills every parameter the parser keeps, in which no usable `state=` arrived and that
is not a `?` query, is taken as possibly cut short and ends the pane's reported
state as `state=clear` does (the rollout rule decides until the next report), unless
`id=` arrived (then it was about a sub-record and the root is untouched). Why: a
report that may have been cut could be the one that ended a working or blocked
state, so the reader closes to the previous rule when unsure rather than keep
believing the old state. A sequence cut short by CAN after a complete state still
counts, by the parser's own behaviour. Cost: processing output does the list-overlay
work only when that pane's reported state changed in that chunk.

What the report is worth. *Read in the Codex source at tag `rust-v0.168.0`
(`tui/src/terminal_program_status.rs`, `chatwidget/status_surfaces.rs`,
`chatwidget/turn_runtime.rs`, `app.rs`):* it is written when the TUI's lifecycle
state changes (only then) and no setting turns it off; `state=clear` goes out when
the TUI exits or is suspended. `blocked` is an approval, question or verification
prompt on screen (kind permission, question or auth). `working` is a turn running or
pending, **background terminals of the session still running**, a goal active after
a turn, and also its MCP servers starting — which is the `working` then `idle` a
pane shows at start-up. `done` and `error` follow a completed or failed turn and
revert to `idle` only when the operator interacts with the pane (a key, a non-empty
paste, a non-motion mouse event; focus reports, resize and mouse motion do not), so
a pane can sit in `done` for good, and nothing in the stream tells who produced the
input: a host's own writes acknowledge it the same way. An interrupt goes straight
to `idle`. A resumed thread starts `idle` (replayed history never produces `done`).
*Measured 2026-11-13 on this machine (`codex-cli 0.168.0`):* the sequences arrive in
`PtyOutput` chunks, so ConPTY passes them through, and over six turns the `working`,
`done` and `idle` transitions led or matched the rollout-based sample by at most
about a second. **Not yet observed on a real session:** `blocked`, and `working` for
work left running after a turn.

**Limits of the report.** It describes the thread the TUI *displays*: a TUI showing
another thread than the member's would mislead, and nothing in the stream says
which. Sub-records are dropped, so sub-agent activity is still not tracked (a Codex
parent's turn ends without waiting for its children). The `?` query is not answered;
that is the one thing that would make Claude Code report over the same protocol
(from the 2.1.323 bundle, read and not run; see the decision entry's "Not done").

**Directory trust** (`crate::directory_trust`, advisory-only, read-only): banto
reads Claude Code's own record of whether it has been told to trust a directory
(`~/.claude.json`'s `.projects[<path>].hasTrustDialogAccepted`, keyed by whatever
exact string a past launch happened to pass as cwd, no normalization — the same
real directory was found stored under both backslash and forward-slash forms
disagreeing with each other). It **gates nothing**: no keystroke is typed or held
on its word. It exists to explain a silence: a Claude Worker parked on that prompt
starts nothing, is discovered by nothing, and would otherwise sit there saying
nothing at all, so the emporium's discovery poll says so once in the status line. It
fires only on a recorded refusal, not on the absence of a record, because Claude
writes an explicit `false` when an operator declines — treating "never seen this
directory" as a refusal would have meant a warning on every directory opened for the
first time.

It used to be the gate on the first line banto types into a Codex Worker (Codex's
own `config.toml` record) and into a Goinkyo (this one), and it was the wrong
witness for both: Claude never records trust for the home directory, Codex was seen
to write no trust key outside a git repository (so an already-trusted pane waited
for ever), and a stale `true` typed the line into a screen that was not a prompt.
Both kickoffs now ask the pane itself — see "Brigade" below.

## The `agents` setting

`Config.agents` (a plain string, default `""`) selects which agent products
banto discovers sessions for: `"all"` — also what an absent or empty string
means — or a comma-separated list of product names (`claude`, `codex`).
`banto_core::config::resolve_agents` resolves it into a `ResolvedAgents`
(`enabled: BTreeSet<AgentKind>`, plus `ignored`/`fell_back_to_all` — see
below); `"all"` means every product this build currently supports
(`AgentKind::ALL`), not a wildcard reserved for products that don't exist
yet. An unrecognized name is dropped, not rejected — this crate's config
layer is lenient by design (a broken setting must never prevent startup) —
but if *every* name in a non-empty setting goes unrecognized, `enabled`
falls back to `all` rather than an empty set that would silently discover
nothing with no visible cause.

**The ignored-name notice.** A dropped name still needs to be discoverable
by the operator, or a typo is indistinguishable from `all` with no way to
find out short of reading the source. `ResolvedAgents.ignored` records every
unrecognized name (deduplicated, first-seen order); `session::agents_ignored_notice`
turns that into one line. It is one of up to two **startup notices**; the other
is the Codex hook-trust notice (see "Brigade", formation gate 3). At startup the
shell computes whichever apply and hands them to the core as one event
(`Event::StartupNotices`; nothing is sent when neither applies). The core shows
them on the status line one after the other — each for the status line's normal
duration, the next taking the line when it becomes free, a notice waiting
rather than replacing some other status already showing — and they block no
input. The `list` subcommand has no status line, and computes neither. The
ignored-name notice fires for a partial drop too, not only the
total fallback: an operator who kept a real, working filter is still owed
the fact that part of what they wrote silently did nothing.
`ResolvedAgents.fell_back_to_all` only changes the wording (naming what's
still enabled vs. saying the setting had no effect at all) — both cases
show the notice.

**Applies at discovery, not display — deliberately breaking precedent.**
Every other row filter in this codebase (`App::show_agents`, the
archive/reveal filter) filters rows already read off disk.
`enabled_agents` instead gates which `SessionProvider` runs at all, inside
`session::discover_all` — the one place both providers' `discover()` calls
live. A disabled product's provider is never constructed, let alone called:
an operator who has switched Codex off gets banto never touching
`state_5.sqlite`, not banto reading it and discarding the result — the same
read-only discipline this file's Codex section had to earn a documented
exception for in the first place.

**The empty-list case:** a valid, non-restricting-looking setting (e.g.
`agents = "codex"` on a machine with no Codex sessions yet) can legitimately
discover zero rows, which looks identical to a genuinely empty machine.
`App::restricted_agents_label` (set once at startup via
`App::with_enabled_agents`, never re-filtered) lets
`banto_tui::view::render_list`'s existing empty-list placeholder say why —
`"No sessions found (agents = Codex)."` instead of the bare default — only
when `total_len() == 0`; a search query narrowing an otherwise-populated
list to nothing keeps its own unrelated "No matching sessions." message.

## Module layout

Four crates, split along the TEA / sans-IO boundary
(`docs/DISCIPLINE.md`) — `banto-core` is UI-free *and* I/O-free; the
provider/status/store/config modules once sketched here as
`banto-core` submodules live in `banto-io`, not there:

```
crates/
├─ banto-core/          # pure: Event -> State + Cmd (TEA/sans-IO), no I/O — app/engine/model/status/screen/program_state/codex_model (a family word → a Codex slug)/search/replay
├─ banto-io/            # the outside world: everything that touches a filesystem, spawns a process, or talks to sqlite
│  ├─ provider/         # SessionProvider trait + claude_code impl (JSONL) + codex impl (sqlite)
│  ├─ status/           # live state (sessions/<pid>.json + PID liveness) — Claude Code only so far
│  ├─ store/            # rusqlite: groups/pins/archived, brigades, UI preferences; migrations, and backup.rs (the copy taken before one)
│  ├─ watch/            # filesystem watching (notify) for live TUI updates — `LiveWatch` (debounced reload trigger) + the raw change source
│  ├─ data_dir.rs       # banto's own data directory (default `dirs::data_local_dir()/banto`, or the one the operator names): where each working file lives, the database included, and the sweep of ended members' files
│  ├─ instance_lock.rs  # the exclusive lock beside the database that keeps a second banto UI off it, and its pid sidecar
│  ├─ file_identity.rs  # which directory or file a path leads to by the filesystem's own identity: the agent-home check, and the executable a UI resolved at start
│  ├─ claude_home.rs    # the Claude Code home root + its projects/sessions subdirs
│  ├─ codex_home.rs     # the Codex home root + its threads/logs db paths and rollout tree
│  ├─ codex_catalog.rs  # the model catalog compiled into the installed Codex: `codex debug models --bundled` behind a trait, bounded by a timeout
│  ├─ pty.rs            # PTY host abstraction (portable-pty)
│  └─ config.rs         # config.toml: loading it, and where it is looked for (--config/BANTO_CONFIG/XDG/~/.config/dirs::config_dir)
├─ banto-tui/           # rendering from &State (ratatui, no terminal backend) — view (session list, summary), paint (vt100 screen → buffer cells), render_modal, strip (the title status of every tile; the message rows of a collapsed one), hints, text
└─ banto/               # bin: clap subcommands (banto, banto _mcp, banto _hook, banto list, ...), the one UI's event loop and executors
   ├─ main.rs           # clap CLI and dispatch
   ├─ exe.rs            # the banto executable, resolved once at UI start with its file identity, and re-checked at each member open
   ├─ build_info.rs     # what `banto --version` prints: package version, build commit, database schema
   ├─ single_instance.rs # the claim `main`'s UI arm makes on the database before opening the store, and what a refusal or an unchecked claim says
   ├─ startup.rs        # where config, database, data dir and the agent homes are (and what a child is handed)
   ├─ session.rs        # discovery + activity classification into list rows, and the store loaders the list needs (pins, groups, archive, hidden members, Directors)
   ├─ launch.rs         # how an agent is started: AgentLaunch argv per product (`SessionStart`: a resume carries no model or effort, a creation may), CodexBrigade's `-c` overrides, [agent_binaries] selection, OpenContext, and `is_live` — the double-resume guard
   ├─ embedded/         # the UI: event loop + executors (emporium.rs), per-pane PTY handling, crossterm conversion, paste synthesis
   ├─ mcp.rs            # `banto _mcp`, the brigade mediation server
   ├─ hook.rs           # `banto _hook`, Codex's SessionStart hook (with briefing.rs, the shared role-briefing renderer)
   └─ codex_trust.rs    # the argv of the one-time approval pane for banto's own Codex hook
```

**The store's schema** is versioned by `PRAGMA user_version`; `store/migrations.rs`
holds one script per version (19 today) and the UI's open applies what is pending,
under the write lock, after a copy of the database (see "Upgrading banto"). An older
build cannot open a database a newer one has migrated: it reports `unsupported database
schema version <n>` rather than guess, with the way out in the same line. v17 dropped `panes`, the map of a session to
its external-terminal pane, which nothing records or reads once banto opens sessions
only in its own panes; any rows went with it. v19 dropped `session_lineage`, the
child → parent links of auto-compaction continuations: Claude Code 2.1.318 and later
compacts in place, in the same file under the same session id, so there is no
continuation to link, and the list no longer hides "superseded" ancestors, so after the
upgrade the ancestors of compactions made before 2.1.318 show in the ordinary list
beside their continuations (the migration took `banto.db.before-v19` first, which still
holds the links). v18 added
`brigade_members.joined_after_id`, the highest message id that existed when the
member joined (0 for a member that joined before v18, which means no bound; it lives
on the member's own row, so a dismissed member's value goes with it, and a migration
that rebuilds `brigade_members` must copy it), and, on `brigades`, `request_text`,
`request_seq` and `request_at_ms`, a Director's open decision request (NULL, 0 and NULL
are "none"; `request_text` holds the constant `open` while one is open; one per
brigade, on `brigades` so that disbanding deletes it; `request_decision`, "Decision
(2026-11-15)").

## The emporium: hosted panes

Introduced by the 2026-07-22 architecture decision above; since the 2026-11-13
decision it is the whole UI, not a mode of it. `banto` with no subcommand opens
a persistent left sidebar (the session list, the chōba) plus a right pane
hosting the selected session **embedded** — the child's PTY is spawned and read
by banto itself (`portable-pty-psmux`), its output parsed with a VT emulator
(`vt100`) into a `Screen` the core owns, and rendered into a ratatui pane.
banto itself is the terminal the session's output paints into: no session is
handed banto's own terminal, and none is opened in a pane or tab banto does not
render.

The sidebar's list state is `App`, drawn by the `view` renderers
(`banto_tui`); its modals go through `render_modal`. `main` dispatches a bare
`banto` to the UI's entry point and the other subcommands (`list`, `versions`,
`_mcp`, `_hook`, …) to theirs.

**Architecture.** The emporium's event loop is a thin shell around
`engine::update` (`crates/banto-core/src/engine.rs`) — see the TEA / sans-IO
discipline (`docs/DISCIPLINE.md`), formalized 2026-07-24. The shell gathers
facts about the outside world into `Event`s, calls the pure `update`, and
executes the `Cmd`s it returns (process spawning, PTY I/O, store reads/writes);
none of the *decisions* live in the shell. `BANTO_RECORD_EVENTS=<path>`
captures every `Event` fed into `update` as a replay stream (`docs/DISCIPLINE.md`
§8) — a captured file contains real session content (keystrokes, pasted text,
PTY output, the first line of each live member's newest brigade messages, which
the relay `Tick` carries for the collapsed tiles) and must never be committed. (A
Tick also carries each brigade's open decision request as its sequence number alone:
no agent-written text.)

`BANTO_INPUT_LOG=<file>` is a lighter diagnostic of the same kind, for
"what did the terminal hand banto, and what did banto make of it". When the
variable is set (to a path), the UI opens that file once at startup — created
if missing, appended to, never truncated or rotated; an open that fails just
leaves the log off — and writes one line per entry, `<unix ms> emporium:
<message>`, synchronously on the UI's own thread. Per raw terminal event it logs
the event as crossterm delivered it — a key as its code, kind and modifiers (so
every typed character is in the file), a mouse event as its kind and cell, a
paste as its length only, never its text, a resize as its size, window focus
gained or lost — and, for a key, a mouse event or a paste, a second line with
what banto converted it into; and, when a run of keys is joined into a paste
(the Windows console never reports pastes as pastes), its length and the number
of Enters. What a child prints and the brigade messages are never written. The path comes only from the variable; there
is no config key or flag. Nothing in banto reads the file back, and the pure
core never sees it (`docs/DISCIPLINE.md` §6.2). **It holds the operator's real
keystrokes: it must never be committed.**

`BANTO_BELL_LOG=<file>` answers "why did it ring?". When the variable is set (to a
path), the UI opens that file once at startup, created if missing, appended to,
never truncated or rotated, and appends one line for each bell banto executes,
`<unix ms> emporium: bell +<ms since the run started>ms cause=<cause> event=<event
variant> <written|failed=<error kind>>`. An open that fails leaves the log off and a
failed write is ignored, so a diagnostic never breaks the run; unset, no file is
opened and no work is done. The `+ms` shares its zero with the offsets of
`BANTO_RECORD_EVENTS`, so a line can be set beside a recording. `event` is the name
of the `Event` variant whose update produced the ring and nothing else of it;
`written` or `failed=<kind>` is the outcome of the write to the terminal. The cause
names the decision that rang and what it was about, as ids and counts only, never
session text, message text or screen content: `quiet_focused` (brigade, Director: a
quiet fire with the Director's pane focused and the "bell always" switch on, the one
ring that leaves no mark), `quiet_recorded` (brigade, Director), `request` (brigade, Director: a Director's
decision request rang, in the same two forms as a quiet fire; it is the cause that
leaves no mark when the operator is in the Director's pane with F4 on),
`staged_pane_bell`
(pane, others: a staged pane's BEL), `staged_pane_wait` (pane, others),
`list_row_waiting` (session, others: a list row that began Waiting and is on no
tile), `off_stage_member_bell` and `off_stage_member_wait` (member, brigade,
others); where several waits gained a place in the same update the cause names one
and `others` counts the rest. A ring the cooldown swallowed is never a bell and is
not logged. The cause changes no decision: it is carried on `Cmd::RingHostBell` for
this log and the shell writes the same single BEL whatever it says. Ids in a cause
are rendered for logging: a new plain session's placeholder key appears as
`new::<n>` without its working directory, a member's placeholder
(`new-worker::<brigade>::<token>`) as it is, and in any other key control characters
and whitespace are replaced by `_` and the id is bounded in length (80 characters).
The path is opened and written on the UI thread like the other diagnostic logs, so
it should name a regular file: a FIFO with no reader would block the event loop. The
path comes only from the variable. *Why it exists:* three days of answering "why did
it ring?" by inference were settled by the first line it wrote.

`<data dir>/logs/panes.log` is always on and needs no variable or key: the pane
lifecycle log, for "what happened to this pane?" after the fact. Only the UI
process appends to it. Each file begins with a `start` line, with the banto version,
the build commit (`crates/banto/build.rs`, `unknown` when git cannot say), the
store's schema version and the UI's process id. After it, one line per Event or Cmd that
is written, `<unix ms> panes +<ms since start>ms <Kind> <name=value ...>`:
`Spawned`, `SpawnFailed` (a refusal word per refusal site and the OS error number,
never the error text), `PtyExited` (the exit code or signal, `asked=yes|no`, the
member, `was=idle|not_idle|background|unknown`, and `input_ago_ms=<n>|none`), `PtyOutputLost`, `PtyWriteDropped`, `Reaped` (a released handle and its
pid), `DiscoveryResult`, `MemberSessionForked` and `PaneSessionMoved` (a brigade member's
or a solo pane's own process now reports another session id; `held_by_row=yes|no`), `MembershipResolved` and
`MembershipRoster` (tokens, roles and session ids), `BrigadeFormed`, `WorkerAdded`,
`WorkerDismissed`, `ExitedWorkerDismissed`, `Disbanded`; and the Cmds `OpenEmbedded`
(with `replaced=yes` when it displaced a live handle under its key), `OpenCodexTrustPane`
(the same, with `pid`), `KillPty` and
`RekeyPty` (with `handle=yes|no`), and the store intents that set, clear or remove a
member's session or row. Every Cmd line carries `by=<the Event that produced it>`.
Input, pane output, the bytes sent to a pane, ticks, quiescence samples and row loads
are never written, nor is any Event or Cmd not named here. A Goinkyo line,
`goinkyo brigade=<id> row=… director=… roster=… guard=… pane=… opening=…`, is written
per brigade only when its value changes; it is the same value the spawn decision of
that tick is made from (`banto_core::engine::decide_goinkyo`). When the brigade's
roster could not be read the line says `row=unread director=unknown` rather than
claiming either value. `was=` is what the pane's activity read as just before the
exit, by the reading the quit question and the tiles use (a Codex pane's own report
first, otherwise the shell's sample; `not_idle` covers both working and waiting on
the operator, which the reading does not tell apart, and `unknown` is nothing usable
read). `input_ago_ms=` is the time since the operator last
gave that pane a key or a paste, `none` when there is no such record (never typed
into, or the pane was unstaged first). Both are fixed words and an integer: they
answer "was it mid-turn when it died, and was someone typing" without any pane text.

Nothing in a line is operator or agent text. A line is built from fixed words,
numbers, flags and identifiers only: no pane bytes, keys, pasted text, message
bodies or glances, consultation text, titles, previews, display or brigade names,
working directories, argv or error messages. A test feeds each text-carrying Event
and Cmd a marker string and asserts that it reaches no line.

Bounds: a line is at most 300 bytes. A longer line is cut back to its last whole
field, so the fields that name the pane and its cause (`key=`, `by=`) come first and
are the last to go. At most 200 lines are written per minute; the rest are counted,
and one `suppressed count=<n>` line is written with the next line that is admitted.
A Goinkyo line held by the limit is counted once, however many ticks offer it again
unchanged before it is admitted. The file is rotated at 4 MiB to `panes.log.1`. Nothing is deleted first: the rename
replaces the previous generation, so at most two generations exist, and the `start`
line is written again at the top of the new file, and so is each brigade's current
Goinkyo line, on the next tick that offers it (the changed-only memory is reset by
a rotation), so a file can be read on its own. A rename the OS refuses (a reader
holds the file without delete sharing) changes nothing: the writer keeps appending
to the current file and tries the rotation again at the next line. Each line is one
unbuffered write on a handle kept open for the process. A write that fails is
ignored: the log never takes the UI down. Nothing per tick, chunk or key is written.
Nothing in banto reads the file.

**Layout.** A solo session fills the whole pane. A staged brigade (see
"Brigade" below) tiles Director left / Workers stacked down the right —
"master + stack". The proportions are the operator's to move with the left
mouse button, in three places: the sidebar's right edge (one width for the whole
emporium, 36 columns until moved), the edge between the Director and the stack
(per brigade, half the pane area until moved), and the bottom edge of a
stacked Worker's tile (per brigade, equal heights until moved). The stack's edge
is the two rows between neighbouring tiles — the upper tile's bottom border and
the lower tile's top border — except the cells the lower tile's title label is
drawn on, which stay the tile's own (a click there focuses it). Pressing on an
edge starts a drag; a yellow line previews where it would land, following the
pointer by the distance it has moved since the press (wherever on the edge it
was grabbed), and nothing is resized until the button is released, so a child
sees one new size per gesture, not one per mouse cell. `Esc`, another press, a window focus loss, a modal, or
a terminal resize cancels the drag and changes nothing. Each side keeps a
minimum (sidebar 16 columns, pane area 20, Director and stack 20 each, a
stacked tile 4 rows); a drag stops there, and a terminal too small for the
minimums gives up the preference rather than refusing to draw.
A Worker tile's title is its token, or `{name} ({token})` when its Director has
named it, e.g. ` impl (worker-2) ` (the token stays visible: the Director will
still sometimes say "worker-3" to the operator; a name may itself contain
parentheses — `impl (UI) (worker-2)` — the last parenthesised group is always the
token). The title goes through the one label function the
geometry uses, so the title's cells — the move handle, the resize exclusion, the
room the title-row buttons need — follow it by themselves: a long name takes the
buttons' room by the usual rule and a narrow tile clips the title as it always
did. The name reaches the emporium on the roster read at staging and on the
brigade facts the one-second tick carries (the brigade's list of named members,
every roster member whether or not it has a session id yet — a freshly spawned
Worker, the most common moment to name one, has no observation of its own, yet its
tile is on screen), so a name given from the Director's MCP process appears within
a tick with no restart; a brigade listed with no names clears them, one the tick
does not list keeps what the core holds. It is a label only: a
rename moves, resizes and collapses nothing and writes no layout — order, solo,
collapse and weights stay keyed by token. Statuses that name a member say its name
alone when it has one, in ASCII double quotes, else its bare token — quoted is a
name someone gave, unquoted a token (a name cannot contain `"`, so the quotes
cannot be broken): `relay: nudged "impl" of "banto開発" ← "docs": stage 2 is done`,
`worker "impl" exited — restarting`, `"impl", worker-3 of "A" and "docs" of "B"`.
One helper decides it, so no status can forget.
Worker tiles can be reordered within the stack (the master stays first): drag a
tile by its title label. From the press the tile's border and title are drawn
in yellow; once the pointer moves, its label follows the pointer (reversed
yellow, cut to the screen) and a yellow line shows the edge where the tile would
land, chosen by the pointer's row alone; the tile drops on release, and every
cue disappears on release and on every cancel. Or use
`Shift`+`Up`/`Down` after the prefix chord. The order of the tiles in a
brigade's stack changes only when the operator moves a tile: not by the brigade
leaving the stage and coming back, not by a restart, not by a pane exiting and
being relaunched, not by which pane happens to arrive first. It is decided in one
place, re-applied whenever the pane list is rebuilt or extended: first the
places the operator gave (a per-brigade preference by member token), then, for
members he never placed, the order they joined the brigade, then — for a member
the engine has not learned yet — the bottom. So a brigade nobody has rearranged
shows its Workers in the order they joined, the Goinkyo after whoever joined
before it, and a member that joins later (a Worker added, a consultation
started) goes to the bottom. The join order is the store's own: the roster is
read in the order the member rows were added (the table's `rowid`; no column of
its own), which is what survives a restart. Within a run the core records a
join where the membership is created — a Worker at `WorkerAdded`, the Goinkyo
when the tick that finds its row reports it (the row is written by the
Director's own MCP process, which no event reports; that tick carries the
roster, so a Worker added in the gap cannot take its place) — seeds it from the
roster at every staging and, at formation, from the Workers' numbering. A tile
whose pane appears for a member none of these told the core about is learned on
sight, last. Dismissing a Worker, or the Director ending a consultation, forgets
the member's place: a Worker's token is never reused, so for a Worker this only
bounds what is kept, but the Goinkyo's token is (each consultation is a new member
under it), and that member joins last. Nothing is stored for a brigade whose order
is only its join order.
Two boundaries on "the order they joined". It holds for a database only banto
has written to: the table's rowids are the hidden kind SQLite may renumber on
`VACUUM` or a table rebuild, which banto never does (an external one may). And a
brigade that predates schema v7 has the order that migration wrote — Director,
then Workers numbered by their old session ids — because the order the members
had joined was not kept when the table was rebuilt; the rows come out in that
numbering order in the bundled SQLite, which SQLite does not promise.
A stack tile can be collapsed to a four-row strip (its title row, two message rows,
border) that shows status-like facts instead of the session's screen, which keeps
running untouched: the `[_]` button in its title row (or `m`), `[+]` to open it
again. The strip's title row is the one every tile has ("The title row of a tile",
below): the label, then agent · activity · output age, then the buttons, from one
function (`tile_status`) and one painter (`paint_tile_title`); the strip itself is
only the two message rows under it. The word is one of three, in the sidebar dot's
colours, or none: **busy**, **waiting**, **idle**. For a pane banto hosts it is
decided by `EmporiumState::pane_word`: **waiting** when the pane waits on the
operator by the decision the marks, the count and the bell read (a Codex pane's own
`blocked` report, a Claude row or live file that waits, a dialog the operator opened
included), or when the member's live file said `waiting` in the latest relay tick (a
row is only as new as the last list reload, and a session that has turned `waiting`
since reads busy to the sample until then, so for a short window the tile can say
`waiting` before the mark appears; the marks, count and bell are unchanged, and a
live status does not outlive its tick, which replaces it every time); otherwise the
one idle decision the quit question reads (`quiescence_of`: a Codex pane's own
report, else the shell's sample of the live file or the rollout), where idle reads
**idle**, not idle reads **busy** (a Claude live file saying `busy` or a status
banto does not know; a Codex pane reporting `working`), a session whose only reason
for not being idle is a background command (a Claude live file saying `shell`) reads
**idle** with the mark `bg` after it, and unknown (nothing
read yet: just spawned or respawned, or no usable source) shows no word. A Codex
pane's own `idle`, `done` or `error` reads idle at once. The mark is not a fourth
word: `EmporiumState::pane_background` holds it only where `pane_word` says idle and
the sample says background, so the two cannot disagree. It never says "alive": that
is a word in contrast to "dead", and a tile answers whether the session is working
or idle, so a hosted session waiting at its prompt reads "idle" whichever agent it
is (the same word and colour as the list's live dot). Not read: the list row's own
activity, which for a pane of this banto's means "no live process proven" and
changes only on a reload, and the quit question's 3-second recent-input window, a
safety margin that would make the word flicker with every key. The marks, the
Waiting count, the bell and the relay still read the older decision,
`EmporiumState::displayed_activity` (a believed Codex report, else the session
list's row for the pane, else what a Claude member's live file said this tick as the
relay observed it, else none; "Decision (2026-11-14): a tile's word…" in the entry
below), and are kept apart from the word on purpose: they ask whether a pane is
*known* to be waiting, and its being unknown for a moment (a renamed pane whose row
has not arrived) is what keeps an announced wait from ringing again when the row
does, while a word has to answer for every pane. Apart from that one window, the
word and those decisions agree about a wait because both take it from
`displayed_activity`. History: the strip once read the raw rows and showed "alive"
for a Codex pane that was reporting `working`, found in the operator's trial; later
the strip and the title took the activity only from the row, so a row-less Codex
pane showed no word while the quit question already treated it as not idle, found in
review; a Claude Worker that had not run a turn showed none either, until its live
file became the third source of `displayed_activity`; and until 2026-11-15 the title
read `displayed_activity` itself. The product shown is the row's, else the one the
relay resolved for the member. Output age is "output now", then in ten-second steps
to a minute, then minutes, then hours; for Codex, whose session files record neither
busy nor waiting, the word comes from the pane's own report once it has made one
(`blocked` reads waiting, `working` busy, `idle`, `done` and `error` idle; "Codex
data sources") and until then from the shell's sample, and the output age stays the
other liveness signal, which is why it is shortened (`2m`) before it is dropped on a
narrow tile. The two body rows read `← <age> <first line of the last message sent to
this member>` and `→ <age> <first line of the last message this member sent>`, from
banto's own brigade message store, read without touching any read cursor (a glance
is not a delivery), counting only messages past the member's own join point
(`brigade_members.joined_after_id`, schema v18: so a new member shows no broadcast
sent before it joined, and a new Goinkyo shows none of the previous consultation's
lines, even before its first observation, since what the relay read about the
previous one is forgotten when a consultation ends and when the next is summoned; a
member from before v18 has no bound) and re-read only when a message has been
appended; each is reduced at the store to its first non-empty line with control
characters removed and cut to 200 characters, then clipped to the row by display
width, and blank when there is none. A collapsed tile that is waiting for the
operator still shows the waiting colour on its border. A tile's `[S]` button (or
`z`) *solos* it, as on an audio mixer: every other stack tile is shown collapsed
until the solo is turned off (the master tile has an `[S]` button too, and no
collapse button: soloing it collapses the whole stack, which then narrows as
described below). Turning the solo off brings back exactly what had been collapsed
one by one (the solo is an overlay on those choices, never a rewrite; it is
unrelated to the single-session "solo" stage). `M` clears both. Both buttons sit
right-aligned in every stack tile's title row, only where the row has room for the
label, a gap and both. When every stack tile is collapsed the stack stays the
right-hand column but narrows to a width of its own (the "collapsed stack width", 36
columns until moved, per brigade; a width in columns, not a share, since a strip
needs a fixed amount of room to be readable on any terminal) and the master takes
the rest; the strips stack from the top in order and the space below them is left
empty (at four rows each a 24-row terminal holds 5 collapsed Workers and a 40-row
one 9, the stack being the terminal's rows less the status bar's one; one more and
every strip gives a little rather than the last being dropped, down to the title row
alone at one row; below one row a tile is not drawn at all: a tile with no rows or
no columns, which a stack with more tiles than rows has, draws nothing and owns no
button, label or drop slot (`layout::is_empty`), and keeps its place in the order
and in the focus ring. *Why:* found in review: such a tile was placed on the next
tile's top row and its buttons were still built, so a press on a visible tile's
`[S]` or minimise button could resolve to the invisible one, and with the status on
the title row it would also have painted over its neighbour. Known and not fixed:
focus cycling (F3) can land on such an invisible tile). The master is never narrower
there than in the ordinary split at the same width (the strips can be narrower than
the ordinary stack, never wider), so whatever fit on it before — its title with its
`[S]` button — still fits. Opening any tile returns to the ordinary master share.
Dragging the master/stack edge sets whichever of the two preferences is in force
(the collapsed width while every tile is collapsed, the master's share otherwise);
the master's PTY is resized once on entering and once on leaving that state. A
collapsed tile is not focusable (the focus ring, F3 and the arrows skip it; the pane
number opens it first); its label still moves it. A collapsed tile's child is not
resized, since a repaint into a grid of a row or two would wreck the screen the
operator comes back to; it is sized when it opens. The row resize handle lies
between expanded neighbours and rewrites only their weights. **Putting the sizes
back** (prefix + `=`, from the sidebar or a pane, on a staged brigade). Forgets
every row weight the layout holds for the brigade — staged members' and off-stage
ones' alike, so a member reopened later brings no old height back — and returns the
master share and the collapsed stack width to their defaults. It leaves the
arrangement alone: stack order, collapsed flags, solo, display names and join order
are not sizes, and the sidebar width is a window-level preference of a different
layer, not part of any brigade. Like the other layout operations it ends a drag or
move in progress; the panes are resized once, only where a tile's size really
changes, and one layout save is written when something changed. A brigade already at
its defaults changes and saves nothing and says so (`sizes already at their
defaults`); with no brigade staged, or a single session, it is refused (`prefix: no
brigade layout to reset`). The case that prompted it: a Worker added later takes the
mean of the others' heights, so weights set when a brigade had two Workers leave one
tile much smaller than the rest once it has four. **What is kept across a restart.**
Everything arranged above, and the brigade bell's switch, is stored in banto's own
sqlite store and comes back the next time banto runs: the sidebar width (table
`ui_prefs`, key `sidebar_width`) and, in the same table, the brigade bell's switch
(key `bell_when_focused`, `1` or `0`; any other stored value reads as no preference,
which is off — see "The quiet brigade's bell") and the agent the Worker picker was
last confirmed with (key `last_worker_agent`, `claude` or `codex`; any other stored
value, a product a newer build wrote included, reads as none stored, and the picker
then opens on the Director's own product; no schema change: it is one more row of
`ui_prefs`); per brigade, the master share, the collapsed stack width and the soloed
member (columns on `brigades`); per member, its place in the stack order, its row
weight and its collapsed flag (columns on `brigade_members`). NULL means "no
preference, use the default". The per-member values live on the member's own row on
purpose: a dismissed member's row is deleted, so its arrangement goes with it, which
bounds what is kept and keeps a later member under the same token (only the
Goinkyo's is reused, by its next consultation) from inheriting it; disbanding the
brigade deletes the rest. A brigade's arrangement is read together with its roster
when it is staged, so the first PTY sizes are already the stored ones; it is written
(the whole snapshot, one transaction) at the moment it actually changes — a
committed resize or move, a collapse, a solo, restore-all, a size reset — and never
for a gesture that changed nothing, and never from a draw or a tick. What the run
already holds wins over a read of the store. Stored values are preferences like any
other: out-of-range ones read as "no preference", and what is used is clamped to the
terminal at use, never rewritten. A write that fails is silent, as a failed pin is.
A Worker added later gets the mean of the others' height.

**Keys** (one table: the list's keys first, then what the panes add):

| Key | Action |
|---|---|
| `j`/`k`, arrows, `PgUp`/`PgDn`, `Home`/`End` | Move selection |
| `Enter` / double-click | Open the session in a pane (switch to it if it is already open); on a Director, stage its whole cell |
| `n` | New session: a dialog to pick or type a working directory |
| `/` | Search (fuzzy, title + cwd) |
| `p` / `d` / `g` | Pin / archive (soft-hide) / join group |
| `Tab` | Toggle grouped view (Pinned / groups / Ungrouped) |
| `a` | Cycle what the list reveals: ordinary sessions → also hidden ones (agent-run) → also archived ones |
| `D` | Restore (un-archive) the selected session, while archived ones are revealed |
| `B` | Appoint Director + auto-spawn Workers (on a Director: disband) |
| `b` | Spawn one more Worker into the staged cell |
| `F2` | Move focus between the list and the panes |
| `F3` | Make the next pane of the staged brigade the current one (collapsed tiles are skipped); pressed from the list, the keyboard stays on the list |
| `F4` | Switch the brigade bell between silent-while-the-Director's-pane-is-focused (the default) and ringing there too (bell only, no mark); says which in the status line, and a standing `BELL ALWAYS` badge shows left of the counts while it is on. One keystroke from the list or from a pane, taken by banto like `F2`/`F3` and never forwarded to the agent; with a modal or the search box open the key goes to that, and with the prefix armed it is an unbound prefix key (no toggle), as for `F2`/`F3` |
| `q`/`Esc` (list focused) | Quit — at once when every hosted session is known to be idle (or none is hosted); otherwise the first press only shows the sessions it counts, each with its reason, for example `Esc again to quit — 2 not idle: a (unknown), b (busy)`, background commands as `(bg)` or `2 in background: x, y` (the forms and the naming are in "Is it idle? (the quit question)") and a second `q`/`Esc` within 3 seconds quits, since quitting ends every hosted process. N counts hosted sessions (staged or off-stage, collapsed tiles included) that are not known idle; "Is it idle? (the quit question)" below says what that is. Any other key, a mouse press, the focus leaving the list, every session becoming known idle or the window running out drops the prompt silently. Esc stays one of the two keys because an IME swallows `q`. Once quitting is decided the status line reads `Closing — ending N session(s)…` until the process exits. |
| prefix chord (`Ctrl+B` default, `[keys] prefix` in config.toml), from sidebar or pane, then: | |
| … `o`/`Tab` | cycle focus through sidebar and panes |
| … arrows | directional pane navigation |
| … `Shift`+`Up`/`Down` | move the focused stack tile one slot up/down (focus stays on it) |
| … `m` | collapse the focused stack tile to a strip, or open it again |
| … `z` | solo the focused tile (the master included; every stack tile but the soloed one is collapsed), or turn its solo off |
| … `M` | open every collapsed tile and turn the solo off |
| … `=` | put the staged brigade's tile sizes back to the defaults (see the layout paragraph) |
| … `1`-`9` | jump to pane N |
| … `s` | back to the sidebar |
| … `Esc` | cancel the prefix: nothing else happens — focus stays, nothing reaches the child |
| … `x` | dismiss the focused Worker or Goinkyo from its brigade (one confirmation: it leaves for good — membership, read cursor and the mail addressed to it — and its session is not deleted); on a Director or a single session, end its process (confirm) |
| … `b`/`Ctrl+B` | send a literal prefix chord through to the child |

While the prefix is armed, the status bar shows the full binding table.
Otherwise its idle hints follow the focus, because that is where keys go: with
the session list focused, the list keys (`Tab grouped/flat`, `F2 pane`, `F3 next
pane`, `F4 bell`, …, plus `D restore` while an archived row is on screen); with a pane
focused, only what banto itself takes there — the configured prefix chord as the
operator would name it (`Ctrl+B`, or a bare character), `F2 list`, `F3 next
pane` when the staged brigade has more than one tile that is not collapsed, and
`F4 bell` always; while
searching, the search keys. A transient status replaces the hints. The text is
built by one pure function (`banto_tui::hints`). While the brigade bell's switch
is on, the badge `BELL ALWAYS` stands in the right-hand segment of the bar, left
of the match counts, whichever focus and whichever left-hand text (hints, a
status, the quit prompt, the closing line) the bar has; a switch that is off
shows nothing. On a terminal too narrow for everything, the left-hand text gives
way first, then the counts, then the badge's tail.
Multiline paste and file drag&drop into panes are synthesized into bracketed
pastes host-side, since the Windows console never reports pastes as pastes.

**The title row of a tile.** Every staged tile, collapsed or open, has the same
title row, drawn by one function (`paint_tile_title`): the border, the label, the
status `Agent · word · output 2m ago`, then `[S]` and `[_]` (`[+]` on a collapsed
tile). The status is the list's words and colours, from the one function
`tile_status`, so a collapsed and an open tile cannot look different (a test renders
the same pane both ways and compares the rows cell for cell, but for the button that
now opens instead of collapsing). It is drawn in the stretch of the top border
between the label and the buttons: the label and the buttons did not move, no
hit-test changed, and one border cell stays free on each side of it. It is drawn
while the tile is being dragged too, in its own colours on the border and label the
gesture colours; the drop line and the ghost label are painted over the finished
frame, so where they cross it they win; hit-testing is the same. When the room is
short it gives way in this order, the same for both states: the agent label first
(`word · output 2m ago`); then the age is shortened to a bare duration (`word · 2m`;
"now", "20s", "3m", "2h", in the long form's steps); then the age goes (`word`); and
when not even the word fits, the whole status. A session whose turn has ended with a
background command still running has the mark `bg` after the word (`Claude · idle ·
bg · output 2m ago`), in the age text's colour, not busy's or waiting's. It sheds
with the word and is never drawn alone: the word and its mark are kept or dropped
together at every step, so `bg` never appears or disappears without the word, and a
pane whose activity is not known draws none. The word is never cut or shortened
and the label never sheds, so at the default collapsed width (36 columns) a plain
label such as `worker-1` still leaves room for a short word and the short age, but a
long display-named label leaves room for the word only. The widths at which the
steps fall are in the tests, not here. A pane whose activity is not known keeps a
form that still says what the age measures: `Agent · output 2m ago`, `output 2m
ago`, `output 2m` (a bare `2m` beside nothing would not say), then nothing; a pane
that has produced nothing shows "no output yet" in place of the age, which has no
short form. Only the label is reversed on a focused tile (a collapsed tile is never
focused); the status is not. A Director's open decision request puts the fixed tag
`▸ decision`, in the waiting colour, after the status: `label · status · ▸ decision
[buttons]`. The status sheds as before with the tag kept (the agent first, then the
age's steps), then `word · ▸ decision` (`word · bg · ▸ decision` for a background
pane), then the tag alone; the tag is never cut and
has no ellipsis, and when it does not fit the status is drawn as if there were no
request. Only a Director's tile ever shows it, and it stays while that pane is
focused. There is no collapsed-body rule: a collapsed Director cannot exist today.
On the Director's list row (whenever its brigade has an open request, staged or not)
the tag is on line two in place of the cwd, only when it fits beside the agent label
and a two-cell gap, and otherwise the row is as it was. *Why:* the operator asked
for the status on the open tile's title, then for the collapsed tile to use the same
row ("One title row for every tile" in the decision entry); the age is shortened
before it is dropped because on a collapsed tile the pane cannot be seen and, for
Codex, the output age is the only liveness signal.

**Size floor.** A hosted pane's screen and its PTY are never smaller than 2 rows × 2
columns (`MIN_ROWS`/`MIN_COLS` in `banto-core`'s `screen.rs`; a freshly spawned pane
starts at 24 × 80, `SPAWN_ROWS`/`SPAWN_COLS`, until the first layout pass sizes it).
A tile smaller than that keeps a floor-sized screen and a floor-sized child and is
drawn clipped: rows and columns beyond the tile are dropped, so its top-left shows.
The PTY is told the size the screen took, not the tile's; the one place either is
resized is `resize_staged_tiles`, so the two cannot differ. A collapsed tile keeps
the size it had and is floored when it opens again; an off-stage pane keeps the size
it has. *Why, measured:* the pinned `vt100` panics on wrapped text in a one-row
screen, in release builds too (an overflow in `Grid::col_wrap`, then `unwrap()`), a
one-column screen underflows `cols - width` for a wide glyph in a debug build, and a
screen with no rows or no columns panics outright; a sweep of 27 input classes over
sizes 0 to 4 on each axis found nothing at two rows and two columns or more. A
one-row tile is reachable by shrinking the terminal to 4 rows or by stacking 6
Workers on a 24-row terminal. *Where the pin lives:* `vt100` is `vt100-psmux`,
pinned in the root `Cargo.toml` (`[patch.crates-io]`) to one commit of `kt81/psmux`,
branch `banto/vt100-patches`, rev `3a8186e` (the two fixes it carries are explained
in the comment beside the pin). *A fact for the future:* the published crate 0.16.13
(2026-10-13) no longer has this one-row panic (read in its source and run in a
throwaway crate: no panic for one row or more and one column or more, debug and
release; zero rows or zero columns still panic in a debug build). Moving to it was
tried on 2026-11-16 and backed out: it counts one skin-toned emoji and one flag that
were tested as a two-cell cluster each, while a pane's PTY stream captured on
Windows that day moved the cursor past each as if it took four cells (assuming the
label after it was meant to stay aligned), and the skin-toned row was drawn with the
spaces after its emoji gone (the comment beside the pin has the code points and the
rest). The fork stays pinned.

**Is it idle? (the quit question).** Quitting ends every hosted process, but the
operator quits when everything is idle and, with the old rule (ask whenever anything
is hosted), was made to press twice every time. So the question is not "is anything
hosted" but "could ending a session lose work in flight", and it is asked of each
hosted pane (staged or off-stage, collapsed or not) as a fact of three answers and
one refinement, `banto_core::model::Quiescence`: **idle**, **not idle**,
**unknown**, and **background**, a kind of not idle that says why (below). Unknown
is its own answer, never folded into idle: both readers of it fail safe on it (a
quit asks; the brigade bell stays silent). Background is not a fourth answer to the
question "could ending a session lose work in flight": every reader that asks it
takes background as not idle (`Quiescence::is_working`: the quit count and the
brigade's quiet watch), and only the tile and the quit prompt tell it apart. The
quit prompt names each counted pane with its reason: `unknown` (nothing could be
read of it, or it is on a dialog), `input` (given work within the last 3 seconds),
`busy` (read as working or waiting, or as a status banto does not recognise), or
background. The order is unknown first, because no tile marks an unknown pane, then
input, busy and background, each group by name. A Director is named by its list
row's title, else its token; a Worker or Goinkyo by its display name, else its
token; any other pane as the list shows it (a placeholder not yet resolved to a
session reads `new session`). A name is cut to 11 columns, with a `#…` suffix when
two would read the same. The shell gathers the answers about once a second, just ahead
of the tick, from reads the tick already makes, and hands it to the core as
`Event::QuiescenceSampled`, which replaces the previous sample as a whole; the core
decides, so a replay of the recorded stream gets the same answers. Every tick
produces a complete sample: when the brigade roster cannot be read, the panes that
can still be observed without it (what the live files and the session list say) keep
their fresh answer, and only what needed the roster to be understood reads unknown.
Nothing older than the tick survives into the next one. One thing is decided in the
core from the pane rather than gathered by the shell: for a pane known to be Codex
that has reported its own state, the report overrides the shell's sample (below);
this is the one place `quiescence_of` is decided, for the quit question and the
brigade bell alike.

- *Claude Code:* the session's live file (`sessions/<pid>.json`, PID alive) says
  `status: idle` → idle. `busy`, `waiting` and any other or missing status → not
  idle, since each means something is running or waiting that ending the session
  would lose; `shell` (see "Data sources": the turn has ended, a background command
  still runs) → *background*, which is not idle for that one reason only, so a
  `shell` entry beside a busy one, a waiting one (the dialog's included), or one in
  a status nothing here knows makes the session not idle, and beside a live idle one
  leaves it background, whatever the order of the entries; with several live entries
  for the session any one that is not idle decides. One exception: `waiting` whose
  `waitingFor` is exactly `dialog open` (a picker the operator opened) is
  **unknown**, not not-idle, unless another entry is busy or waiting for something
  else, which wins: read as not idle, a dialog opened and closed again would count
  as work that ended; unknown is the honest value, since it is not known what status
  a dialog opened over a running turn reports. Unknown still asks at quit and holds
  a brigade's quiet stretch. No live entry with a live PID → unknown (a fresh
  session publishes its file a moment after it starts).
- *Codex:* first the pane's own report, when it is known to be Codex and has
  reported (OSC 7501; "Codex data sources", "The pane's own report"): `idle`, `done`
  and `error` → idle (nothing is running, and a pane may sit in `done` or `error`
  until the operator touches it); `working` and `blocked` → not idle. It overrides
  the rollout for the quit question and the brigade bell, because it is the only
  signal that sees work left running after a turn and a prompt waiting for the
  operator. With no report, or after `clear`, the rollout's last turn marker ("What
  the markers mean"): ended → idle; started → not idle; no marker, no such thread,
  no readable rollout or no Codex home → unknown. The relay does not read the
  report: it still reads the rollout marker, so a member whose turn ended is
  nudgeable even with background work, like a Claude member in `shell`. Limits,
  stated there: a TUI displaying another thread than the member's would mislead;
  sub-agent activity is not tracked; a Codex session that has not reported and left
  a command running still reads idle.
- *A pane the sample does not cover* (opened since the last sample, a brigade
  member with no session id yet) → unknown, so a new pane asks rather than being
  taken for idle. A pane whose process ended is gone and counts for nothing.
- *Recent input:* a pane also counts as not idle for 3 seconds
  (`QUIT_RECENT_INPUT_WINDOW`) after the operator gave it work — a key (the prefix
  sent through literally included; a key from pane focus that encodes to no bytes
  still counts) or a paste forwarded to it; no mouse event is work given (a press, a
  release, a drag, a wheel turn: all are forwarded to the child as before, but
  clicking into a pane or reading its history is not an instruction to the agent) —
  or something banto wrote itself to start or continue a turn: the relay's nudge
  line and its delayed submitting Enter, a Codex Worker's or a Goinkyo's kickoff
  line and its retries (an Enter-only retry included) — whatever its sample says.
  A pane in the background that took such input counts as not idle, not as
  background, since the input may have started a turn, and no pane is counted twice.
  The focus reports banto writes to a pane start nothing and do not count. The
  sample is up to a tick plus the product's own report lag behind a turn that input
  has only just started; without this, a prompt submitted a moment ago still reads
  idle and one `q` quits on it. The quit question reads all of this; the brigade
  bell's quiet stretch reads only banto's own writes (its arming also reads the
  operator's keys, pastes and button presses; see "The quiet brigade's bell"), and
  the relay decides from the sample alone.

One `q`/`Esc` quits when no hosted pane is counted; otherwise the first press arms
the prompt, which names each counted pane and why it counts: `Esc again to quit — 2
not idle: a (unknown), b (busy)`, `Esc again to quit — 2 in background: x, y` with
only background commands, and `(bg)` beside the not-idle panes when both are there.
A pane is counted once: as not idle when it took
input within the window or reads not idle or unknown, as background only when the
background command is its whole reason. The window, the second key and the ways it
is dropped are as in the key table. The prompt is rebuilt from the current counts at
every update, so it follows a background command to its end (Claude is re-invoked
when the command ends, "Data sources"), and goes when both counts reach zero. The
decision uses the last sample, so it can be about a tick (plus
the product's report lag) behind a turn that has only just begun. Cost: a Claude
pane adds no read (the live-file scan and process snapshot are the relay's); a
Codex brigade member adds none (the relay already tails its rollout every tick);
a hosted Codex pane outside any brigade adds one sqlite lookup and one rollout
tail per tick (`docs/DISCIPLINE.md`, Appendix A).

**The closing line.** The shutdown sweep that runs once quitting is decided
closes each hosted pane in turn and then waits for the children, up to 5 seconds
in all (`SHUTDOWN_GRACE`); no further frame is drawn while it runs, so before this
nothing on screen said that banto was closing, and the status line had already
gone back to its hints. Once `App::should_quit()` is set the status line therefore
shows `Closing — ending N session(s)…` — `Closing…` when nothing is hosted — with
priority over everything else the line can show (the quit prompt, a status, the
armed-prefix hint, a startup notice), until the process exits. It is the same
text for every way of quitting (one press or two), N counts every hosted session,
idle or not, since all of them are about to be ended, and the last frame is always
presented, never skipped as unchanged. Terminal close, a fatal error and a panic
are not quits in this sense and draw nothing of the kind.

**Attention.** Two things tell the operator that a pane wants him, and a host bell
comes with the second. A *mark*: a staged pane is in the mark set, and its border is
drawn in the waiting colour (magenta) unless it is the focused tile (below), while
the session list shows its session as Waiting — a Claude Code session whose live
file says `waiting` (for any reason; for a member with no list row yet, the live
file as the relay read it this tick), or a hosted Codex pane that reports `blocked`
(OSC 7501, "Codex data sources"; the row, the count and the mark all read the same
report, which holds exactly as long as the report, the pane or its being Codex does,
whether or not the list has a row for the pane yet; a Codex pane reporting `working`
shows busy in the list) — or after its child rang BEL while the pane was not
*effectively focused* — the focus on the panes, the pane selected, and banto's own
window focused (the same rule DECSET 1004 focus reports use; before the terminal has
said anything the window counts as focused). A mark from a BEL clears when the pane
becomes effectively focused. The mark *set*, the count and the bell do not depend on
focus (a Waiting pane is in the set while it is focused too, and its bell is the
same edge); only a BEL latch, and the brigade-quiet fire below, are suppressed for a
pane the operator is already in. What the border *draws* does depend on it: the tile
that has the pane focus (banto's own focus on that pane, whether or not its window
is focused) is drawn in the focus colour (cyan) and its title reversed, which
outranks the waiting colour, so a focused Waiting tile looks like any focused tile;
an unfocused one is magenta when Waiting and dark grey when not; a tile being moved
is yellow; a collapsed tile is never the focused one, so its border shows the
waiting colour. The list row's dot and the sidebar count show the Waiting session
whatever the focus. The sidebar's title counts the visible Waiting sessions of
either product (`banto · waiting N`). The bell rings for the *ring set*: the display
set minus a Claude session that is Waiting only because a dialog is open
(`waitingFor` exactly `dialog open`). Such a session is shown, counted and marked
like any Waiting one, but does not ring: it is the operator's own doing and he is
looking at it. If its reason later becomes anything else while it is still waiting,
it enters the ring set and rings once at that moment; a pane latched by a BEL rings
whatever else it is (a quiet brigade's fire has its own record and rings through
it); and the reason changes the bell and nothing else on screen. The *host bell* (a
BEL written to banto's own terminal) rings when the ring set (the visible Waiting
rows and marked panes, less dialog-open ones) gains a member, at most once per 3
seconds (`ATTENTION_BELL_COOLDOWN`, shared by every cause below). **On stage and
off.** A pane on stage (a staged brigade's panes, or one pane staged alone) speaks
only through its own tile. A hosted member whose pane is not on stage has no row of
its own (members are hidden from the list), so its Director's row speaks for it:
when the member is Claude `waiting`, a Codex pane reporting `blocked`, or its child
rang BEL while the pane was not effectively focused, the Director's row gets the
waiting-colour dot and counts in `waiting N`. A member that is waiting only on a
dialog the operator opened neither marks nor rings, as everywhere. A quiet brigade's
fire, or a Director's decision request with `request_attention` on, marks the row
the same way whether or not the brigade is on stage (and the Director's tile, if it
is, since the tile reads the same Waiting row); the mark counts in `waiting N`,
which the request's tag alone, once the mark is gone, does not. The row's mark is
display only: a set kept apart from the session's own activity, so the tile's word,
the quit question, the quiet watch and the relay never read it. It is memory only,
so a restart starts with none, and it ends when the operator is effectively focused
on that Director's pane, the focus a BEL latch is cleared by: staging the brigade
puts him there, so staging clears a quiet fire's or a request's mark in the update
that stages it (the request's tag stays, "Decision (2026-11-15)").

**One event, one ring.** The bell is rung for events, not for row marks, and does
not depend on what the list shows (a search that hides the Director's row does not
silence it). A member's wait rings once, when it is first announced: through its
Director's row when its pane was off stage, or on its own tile when it was on stage.
It does not ring again when its brigade moves on or off the stage with the wait
still standing: staging moves the mark from the row to the member's tile with no
second ring, and leaving a brigade for another with a wait that began on stage still
standing marks the row without one. A second member's wait rings even while the
Director's row is already marked; a second quiet fire under a quiet mark that is
still standing does not, being the same unlooked-at attention (the quiet fire rings
when its record is made). A wait ends when something is known about the pane that is
not waiting; or, for one that stood only on a BEL latch, when the latch is cleared,
whatever is known about its activity (a BEL has no activity word to say it is over);
or when its pane is gone (a pane later hosted under the same key is a new one). A
wait that begins again rings anew. A pane about which nothing is known for the
moment (discovery renamed it, or its process moved to another session id with `/clear`,
`/branch` or an in-pane `/resume`, and its row is not yet under the new id) says nothing either way, so a rename is not an event and the same wait does
not ring twice. Every ring is under the same 3-second cooldown as every attention
bell, which may swallow it.

Codex's session files carry no Waiting state, so beside the pane's own `blocked`
banto launches it with `tui.notifications` set to `approval-requested`,
`async-question` and `plan-mode-prompt` (with `bel` as the method and `always` as
the condition), so that a prompt waiting on the operator rings the child's BEL,
which the mark and bell above pick up. *Read in the Codex source at tag
`rust-v0.168.0`:* those three are TUI notification types (exec/edit/elicitation
approvals; a question the model asks; the plan-implementation prompt), and the
fourth, `agent-turn-complete`, is left out on purpose — the bell is meant to be
rarer than every turn. *Measured 2026-08-06 on `codex-cli` 0.146.1 (see
`crate::launch`), with the list as it was then, `approval-requested` alone:* a plain
turn rang nothing; a turn forced to request approval rang exactly one bare BEL with
the condition `always` and none with Codex's default `unfocused` (Codex counts a
banto-hosted pane as focused whatever the operator is looking at). The two names
added since were not part of that measurement, and Codex's own approval heuristics
decide how often an approval is asked at all.

**The quiet brigade's bell.** The operator's reason: a brigade going quiet is the
moment his input is wanted, and it is rare enough to deserve a sound. It rings the
same host bell under the same cooldown as the mechanism above, and marks through a
record of its own (see *Effect* below) rather than through a pane's latch.

- *Watched:* every brigade whose Director is hosted; its members are every hosted
  member (the Director, the Workers, a Goinkyo).
- *Fires* when every member's sample is idle and has been for 5 seconds
  (`BRIGADE_QUIET_SETTLE`), counted from the later of two moments: (a) the start of
  the unbroken all-idle stretch — the last member becoming idle, or the set of
  hosted members last changing (a member hosted, gone, or relaunched under the same
  key restarts it; a key that merely changes for the same process, as discovery or a
  `/clear` do, does not); and (b) the last time banto itself wrote into a
  hosted member of that brigade to start or continue a turn (the relay's nudge line
  and its submitting Enter, a kickoff line, a kickoff retry). It also needs the
  brigade to have been *armed* by real work since it last fired or since banto began
  watching it (next bullet), so a brigade that never did anything does not ring and
  one that fired does not ring again until someone works; a change of members
  neither sets nor clears that. The decision is made at the end of the tick, after
  the relay has decided what to type, so it never fires in a tick in which banto
  types into a member of that brigade; sampling is about once a second, so the wait
  is met between 5 and 6 seconds after its start. Why: a Worker that reports ends
  its own turn before the relay has typed the nudge into the Director, and the
  Director only reads busy in a later sample, so for a moment the whole brigade is
  idle with work still in flight; the bell must not ring just before the brigade
  starts working again. A nudge the relay holds back writes nothing and so moves
  this counting not at all; what keeps the bell from ringing while it is held back
  is the unread mail itself ("Mail on the way", below).
- *Armed by real work:* a member reading not idle while its process is still
  starting up delays the bell like any not-idle reading (it breaks the stretch) but
  does not arm it. A not-idle reading arms the brigade only from a process that has
  been sampled idle before (from the next sample on) and is past its start-up (the
  first 10 s after the process started, `PROCESS_START_GRACE`: "Decision
  (2026-11-14): start-up is not work"), or that was given work at or after its
  start, at any age — by the operator's keys or a paste forwarded to it (no mouse
  event), or by banto's own nudge or kickoff; input written under the same key
  before this process started does not count. Work still going when the grace ends
  arms at the first reading past it. Per process, so a relaunch under the same key
  starts over, with a new grace and with the previous process's stamps of work given
  dropped when the new one is spawned (the relay's record is kept), and a key rename
  keeps what the process had, including its start; the Director is covered like any
  member, and a pane with no recorded start has no grace to wait out. *Why,
  measured:* in the operator's trial on 2026-11-13 (event recording), opening a
  brigade of resumed sessions rang the bell about 5 seconds later with no work done:
  resumed Claude sessions read not idle for one to two seconds while starting (run
  A: all idle 6.1 s after a spawn at 2.1 s; run B: all idle 3.2 s after a spawn at
  0.9 s). *Known consequence:* a resumed member that starts working on its own,
  never touched and never seen idle, does not arm the bell that first time, and
  self-started work that ends within the first 10 s of a process does not arm it
  either.
- *Observed 2026-11-15 (a field observation, not a decision; nothing was changed for
  it):* a Claude session's context auto-compaction (28.6 s, unprompted, 54 minutes
  after its last turn) read as busy, armed the watch as self-started work, and the
  brigade rang when it ended. The bell log said `quiet_recorded` and a replay
  reproduced it. Of the 40 not-idle episodes in that 63-minute run it was the only
  one not preceded by a write into the pane.
- *A background command:* a Claude member whose live file says `shell` reads
  background, and the watch takes it as it takes not idle
  (`Quiescence::is_working`): it breaks the idle stretch and arms the brigade from a
  process that qualifies, as work does, so a brigade with a command left running is
  not quiet, and a request spends the same quiet either way. Only the tile and the
  quit prompt tell it from work. By construction, pinned by engine tests; no
  observation on a running brigade is recorded here.
- A member whose state is *unknown* blocks it and breaks the idle stretch: silence
  is not claimed on missing information. A *waiting* member is not idle.
- *A dialog the operator opened:* a Claude member that is `waiting` with
  `waitingFor` exactly `dialog open` reads unknown ("Is it idle?"), so it holds the
  quiet stretch and never arms the bell by itself. Why: before, opening and closing
  `/model` in an idle brigade made the member read waiting, then not idle, which
  armed the brigade, and the quiet bell rang five seconds after he closed it
  (confirmed by a test on the old engine). Known limit: a brigade with a dialog left
  open cannot go quiet until it is closed. Not measured: a permission prompt's
  `waitingFor`, a dialog the product opens by itself (it would read the same), a
  dialog opened over a running turn (busy wins).

- *Mail on the way:* a brigade is not quiet while one of its hosted members has
  unread brigade mail that the relay will still deliver on its own — the relay is
  automatic (`[brigade].relay = "auto"`) and has not used up its nudge attempts (3,
  `RELAY_MAX_ATTEMPTS`) on that batch. While such mail is pending, a member whose
  sample reads idle counts like an unknown one (blocks firing, breaks the stretch),
  however long the nudge is held back, whatever the hold reason (an unsent draft in
  the recipient, the operator's recent input in the pane, the idle streak, the
  cooldown, a member that is not idle). Why (reversed after the operator's trial on
  2026-11-13): he had scrolled and clicked in the Director's pane, so the relay held
  the nudge for a Worker's report for about 30 s (its input quiet period — wheel and
  mouse events forwarded to a Claude pane count as operator input), every member
  sampled idle meanwhile, and the bell rang 5 s in with the report still unread; to
  him the work was not finished. Mail the relay will not deliver — relay not
  automatic, attempts exhausted, or a Codex member held for `INPUT_LINE_UNREADABLE_AFTER`
  (60 s) because its pane is not at its prompt (see "Auto-relay") — does not hold
  the bell: the brigade is then waiting on the operator. Consequences: a nudge delivered but never read keeps the
  bell held through the relay's retries, and an operator's draft left in a Worker's
  pane holds that Worker's mail and so the bell. In detail, for a nudge typed but
  not answered: the bell stays held through the relay's retries (a cooldown of 60 s
  between nudges, up to 3 attempts); once the last attempt has been written the mail
  no longer counts, and the bell rings 5 s after that nudge's submitting Enter
  unless the mail is read first. Switching the relay to manual ends the hold, and
  the brigade rings after a normal settle from then. Mail counts as pending only if
  *this tick's* observation of that member says it has some (relay automatic,
  attempts not used up); nothing is carried over from an earlier tick, and a member
  the tick did not observe has no mail as far as the bell knows.
- *A kickoff still owed:* only a freshly added Codex Worker and a freshly summoned
  Goinkyo are given a first line typed by banto to start their first turn (see
  "Brigade"); a Claude Worker, the Director and a resumed member are not, and are
  never owed one. From the moment such a pane is hosted until banto's kickoff
  tracking drops it — the kickoff acknowledged (a session discovered for the pane,
  or its session row showing a first user message, which also drops a kickoff nothing was typed for), the pane gone, or the failure reported — a member whose
  sample reads idle counts like an unknown one: it blocks firing and breaks the
  stretch. That covers every state in between (waiting for the boot output to go
  quiet, waiting for the pane to reach its prompt, the line typed and awaiting its
  acknowledgement, a retry pending). Why: a newly summoned member can sit idle at its prompt for
  longer than the settle time before banto types its kickoff, and the bell must not
  ring while banto itself is about to start it. The stretch then runs from after the
  kickoff stops being owed, or from banto's last write into the member, whichever is
  later. Not owed, though the tracking entry stays — the brigade then really is
  waiting on the operator, which is what the bell is for: the pane was found not at
  its prompt and nothing has been typed (the operator has to answer what the pane
  shows); the operator has an unsent draft in that pane (the kickoff waits for
  them; a draft is judged as the relay judges one, see "Auto-relay"); a typed line
  got no answer and the pane is not at its prompt to be asked again, or a Codex
  Worker took its first turn but was not identified, or a Goinkyo took it and its
  session shows no first message (each reported once, below) —
  owed again the moment banto types the line again; the last allowed attempt
  (three) was typed, no turn was seen after it, and its 15-second acknowledgement
  window has run out.
- *Effect:* it records the brigade as wanting the operator (memory only) and, from
  that record, marks the Director's row in the list (the waiting-colour dot, counted
  in `waiting N`; a staged Director's tile reads the same Waiting row) and rings the
  host bell under the 3-second cooldown, whether or not the brigade is on stage and
  whatever the operator is doing in another brigade; the ring is made when the
  record is made, so a second fire under a mark still standing (not yet looked at)
  records nothing new and is silent. A Director's decision request makes the same
  record with its own cause and spends this watch's next quiet point ("Decision
  (2026-11-15)"), so one record per brigade covers both. Not while the Director's
  pane is effectively focused (the selected pane and banto's window focused) — there
  every turn the Director ends would otherwise ring; with the window in the
  background it marks and rings, and with `F4` on the bell rings while he is in the
  pane. A fired brigade is spent either way: being in the Director's pane does not
  save the ring for when the operator leaves it. The mark ends when the operator
  reaches the Director's pane, which staging the brigade does, without a second
  ring; the record of a brigade whose Director is no longer hosted, or that is
  disbanded, is dropped. Until 2026-11-14 the fire latched the Director's pane
  instead, and so reached the operator only for a staged brigade ("Decision
  (2026-11-14): an off-stage brigade speaks through its Director's row"). A brigade
  whose Director is not hosted is not watched. A brigade is judged only in a tick
  whose relay observations include its Director (the store was read this tick and
  still has the brigade, and the Director is hosted with a session), and the
  observation is about the pane banto itself holds for that member: the pane is
  hosted and banto's own membership for it is the same brigade and the same member
  token. Any other observation is ignored by the bell — for "Director observed" and
  for "unread mail" alike — and so only delays it; the reason, found in review, is
  that one taken at face value could mark a brigade as observed, or mail as unread,
  through a pane that is not that member (a tick gathered just before a session-id
  change the core has already applied). In a tick without that observation — a
  roster that could not be read, or the brigade gone from the store by any path —
  nothing fires and the quiet stretch
  starts over; the armed state is kept. A disband in flight does not fire and its
  watch is kept as it was; a successful disband ends the watch (what was kept for it
  is dropped with it); a failed one leaves it watched; and a former Director left
  open on its own never rings for the brigade that is gone (its pane keeps the
  membership maps it was staged with, which is why the test is observation, not
  those maps). A brigade formed again around the same Director is a new brigade and
  starts fresh. Consequences: during a roster outage of any length nothing rings,
  and the count starts over when observations return; a Director hosted before its
  session is discovered is not observed, so its brigade is not judged until
  discovery completes.
- *The principle:* the bell decides only from what the tick observed. Why: a
  remembered "unread" or "exists" could hold or ring forever once the thing it
  described could no longer be checked (found in review).
- *The switch:* the function key `F4` — one keystroke from the list or from a pane,
  taken by banto like `F2`/`F3` and not forwarded to the agent — toggles "ring even
  while the Director's pane is focused" — the bell only, under the usual cooldown,
  no mark (a focused pane never carries one); every other case is unchanged. The
  status line says which way it went (`brigade bell: on, it rings even while the
  Director's pane is focused` / `brigade bell: off, it stays silent while the
  Director's pane is focused`), and while it is on the badge `BELL ALWAYS` stands on
  the status bar. It is persisted (`ui_prefs`, key `bell_when_focused`; see "What is
  kept across a restart") and is off by default and when the stored value is
  unusable. It is deliberately a runtime switch and not a `config.toml` key: whether
  the operator wants the sound depends on what he is doing, and he wants to change
  it while running. It is a function key rather than a prefix chord because he wants
  to flip it casually, in one keystroke, while working.
- *Known limits:* the rule trusts each product's own report. A Codex member is read
  from its pane's report when it has made one (so work left running after its turn
  reads working, per the Codex source; not yet observed on a real session) and
  otherwise from its rollout, where a command left running reads idle ("Is it
  idle?"); a TUI displaying another thread than the member's would mislead; and
  sub-agent activity is not tracked.

**ConPTY caveats** (`docs/notes/embedded-pty-spike.md`, 2026-07-22 spike plus
2026-07-24 dogfooding, all on Windows): never answer the outer terminal's own
DSR/DA queries (that traffic belongs to ConPTY probing banto's real terminal,
not the hosted child — answering leaks garbage into the child's stdin and
corrupts its repaint on resume); a chunk boundary can carry meaning (the
relay's nudge Enter is sent ~300ms after the nudge text rather than
back-to-back, see "Auto-relay" below); a child's exit produces no EOF on some
paths, so an active waiter thread is needed rather than relying on read
returning empty. The spike's own "not yet verified" list, stated plainly
rather than glossed over: resize-under-stress and non-Windows behavior beyond
one dated Unix teardown follow-up (`docs/notes/embedded-pty-spike.md`,
2026-07-25 addendum — a measured comparison of graceful vs. force-kill
teardown timing on that platform). Mouse forwarding into children was also on
that list and has since landed: `Screen::wants_sgr_mouse`
(`crates/banto-core/src/screen.rs`) gates whether a wheel/click event
reaches the child's own SGR mouse mode instead of banto's own pane-scroll,
tested by `wheel_over_a_pane_that_wants_sgr_mouse_is_forwarded_not_consumed`
(`crates/banto-core/src/engine.rs`).

**Scrollback** was on that unverified list and turned out to fail outright: a
Codex pane kept no scrollback at all. Fixed by depending on
`portable-pty-psmux`, a fork of `portable-pty`, rather than the crates.io
original — measured 2026-08-02 with the same Codex binary, prompt and
geometry on both sides: no scrollback with vanilla, 191 lines with the fork.

**Why the fork fixes it is still open.** It differs from vanilla in three
flags (`PSEUDOCONSOLE_PASSTHROUGH_MODE`, `RESIZE_QUIRK`, `WIN32_INPUT_MODE`)
and that A/B swapped the crate whole. Passthrough was written up as the
mechanism and has since been ruled out: on 2026-08-07 the fork was held
constant and only `PSMUX_NO_PASSTHROUGH` varied, with the crate's own logs
confirming the flag really was on in one arm and off in the other, and the
scrollback count was identical across all four runs. The other two flags have
no toggle and remain untested. See `crates/banto-io/src/pty.rs`'s
`PortablePtyHost` doc for the details and for two traps a re-measurement
needs to know about.

## Brigade (Director/Worker cells)

A brigade is an internal operational cell of one Director session and its
Worker sessions (formed with at least one; dismissing Workers can later leave
it with none), hosted together as tiled panes in the emporium
(`crates/banto-io/src/store/migrations.rs` v4 migration comment). It is a
separate concept from groups (the user's own project/phase filing): a brigade
is a live operational unit, not a filing category. A session belongs to at
most one brigade, and a brigade has exactly one Director — both are "layered
in code, not a schema constraint" (same comment, verbatim).

**A third role, Goinkyo, exists in `BrigadeRole` and the messaging rules
below** — the retired elder called back in to arbitrate a Director/Worker
disagreement or to think a design or a course through, addressable by name but never a broadcast recipient (see
`send_to_peer` below). A Director calls one in with the `consult_goinkyo`
MCP tool (see "MCP mediation server" below), which files a written
consultation request and creates the member row; once a tick observes that
row with no session id yet, it is auto-spawned the same way a fresh Worker
is, for any brigade whose Director's pane is hosted, on stage or not (Claude
only — `goinkyo_model`/`goinkyo_effort`/`goinkyo_permission_mode`/
`goinkyo_disallowed_tools` below; the model and the effort only because this is a
creation, see "Model and effort: only when a session is created"), briefed from `goinkyo_prompt` with
`{request}` substituted for the consultation file's path. The spawn is
attempted at most once per consultation (`EmporiumState::goinkyo_pane`, one
guard per brigade, mapped
to the `SessionKey` that spawn used): a failed attempt is not retried
automatically. A tick looks at every brigade the store lists and every one banto
has a Goinkyo pane recorded for, and spawns only where the Director's pane is
hosted; a Goinkyo row of a brigade nobody is running, left by an earlier run, is
not started at startup. The working directory is the
Director's, read from its roster row as staging reads it (`.` when that row is
not resolved yet). The pane is the Goinkyo's from the moment it is spawned, under
its placeholder key: both ownership maps (its token and its brigade) are written
with the open, so the quiet-bell watch counts it and holds the brigade's bell
while its kickoff is owed, staging the brigade once the pane has landed finds it
instead of counting the member missing (staged in the instant between the spawn
and its landing, the status line still counts it missing, though the pane joins
the stage when it lands), and a wait of its own shows on its Director's row. A row with no pane
(no hosted Director) stays outside the quiet watch, as before. A spawn that fails
drops the kickoff queued with it. Not measured yet: a Goinkyo off stage that stops
at Claude's directory-trust dialog may read as unknown, which holds the quiet bell,
so nothing would ring for that brigade until someone answers the dialog.

**Worker tokens.** A Worker's token (`worker-N`) is its identifier — what every
address, preference and store row is keyed by — and is never reused within a
brigade, in the manner of `AUTOINCREMENT`: the brigade remembers the highest Worker
number ever issued (`brigades.worker_seq`, schema v16), and the next Worker is
`max(that, the highest `worker-<n>` among its current members) + 1`, chosen and
inserted by `Store::add_worker` in one `BEGIN IMMEDIATE` transaction (two processes
adding at once get different numbers). Formation goes through the same counter (N
Workers leave it at N), as does the emporium's add-worker, and numbers stop at nine
digits — the largest the parser reads back — after which adding is refused ("this
brigade has run out of Worker numbers"), a stored counter outside the range being
clamped to it (negative reads as 0, above reads as the bound); they are the only
paths that create a Worker row. So dismissing the highest-numbered Worker, or every
Worker, does not hand a number back, and a dismissed middle Worker leaves a gap that
stays. A brigade older than the counter has it backfilled with the highest
`worker-<n>` among its current members; the numbers of Workers dismissed before that
were never recorded, so such a brigade can hand one of those out once. A disbanded
brigade's counter goes with it; a new brigade has a new id and starts at 1. Between
them these keep apart the placeholder keys that creations are made under,
`new-worker::<brigade>::<token>`: brigade ids are never reused (`brigades.id` is
`AUTOINCREMENT`, and a store test pins that a deleted brigade's id is not issued
again) and a Worker's number is not handed out twice within a brigade, so two
Workers never share a placeholder key. The exceptions are the ones named here: a
brigade older than the counter can hand out, once, a number that a Worker dismissed
before the counter existed held (the Worker it belonged to is gone, so it is a reuse
of a number, not of a live key — inferred, not tested), and a hand-edited store. The
Goinkyo's `new-worker::<brigade>::goinkyo` is the same key for each consultation,
one after another, never at once: there is one spawn guard per brigade. The
Goinkyo's token is the fixed `goinkyo` and *is* reused, by each consultation
(`dismiss_goinkyo`, then consult again, is a new member under the same token), so
everything filed under a dismissed member's token — layout, order, names, mail — is
still forgotten on dismissal: that bounds what is kept and keeps the next `goinkyo`
from inheriting it.

**Display names.** A member's token is its identifier and never changes. On top of it a Worker can have a *display name*, given by the
Director whenever it likes and changeable at any time (`name_member`, below), so
that the Director, its Workers and the operator can keep track of what a Worker
is for. Only a Worker can be named. The name lives in
`brigade_members.display_name` (schema v16, nullable, on the member's own row, so
a dismissed member's name goes with it). The
write is one store operation (`Store::name_member`): a single `BEGIN IMMEDIATE`
transaction reads the roster, finds the member by token, checks it is a Worker,
applies the rule and writes — so while it holds the write lock no other process can
dismiss or add a member between the read and the write, and two namings to one name
cannot both win (the second is refused it as taken). A name cannot land on a
replacement Worker, since a Worker's token is never reused; addressing by token is
the Director's only handle, as in `send_to_peer`. One
rule decides what a name may be (`banto_core::member_name`): reduced like any
text someone else wrote (`display_text`: one line, control and format characters
dropped), trimmed, at most 24 display columns, not empty (empty input clears the
name), no ASCII double quote (a name is shown inside quotes), no square bracket
(`[`, `]` delimit the message header `[from worker-3 "impl" — to you]`, which is
provenance an agent reads, so a name can never close it), not equal — in any
case — to another member's name in the same brigade, and not a token or
anything that looks like one (`director`, `goinkyo`, `worker-<digits>`, or any
member's token). The tool refuses a name that breaks it, with the reason; every
reader applies the rule again — the store's roster reads and its single-member read
alike, and the core on everything it is told by an event (a brigade's names are
resolved as a roster before they are installed, join order deciding) — so a stored
or stated value that fails it, old data or a hand-edited row, reads as no name, and
of two members holding the same name only the one that joined first does. The
characters a name may not contain, and why each, are listed in the
`banto_core::member_name` module doc.

`goinkyo_permission_mode` defaults to `"auto"`, not Claude's own default of
`"manual"`: nobody is at an unattended Goinkyo's keyboard to answer a
permission prompt, and `manual` (like `"plan"`, whose own design still exits
through a human approval at the end) reliably stops there. Not a guarantee
of unattended operation either — Claude Code falls back to its ordinary
confirmation flow after repeated denials even under `"auto"`, and that flow
stops the same way `"manual"` does.

`goinkyo_disallowed_tools` defaults to `"Edit,Write,NotebookEdit"`
(`--disallowedTools`), passed on a resume too, not just a fresh spawn — same
reasoning as `goinkyo_permission_mode` above, since which tools a Goinkyo
may use is a property of the role, not of whether this particular launch
happens to be fresh or resumed. It exists because `"auto"` changed what
"unattended" means: under `"manual"` an edit stopped for approval nobody was
there to give, under `"auto"` it doesn't, so an unattended Goinkyo could now
write to the same working tree the Director is using without anyone having
agreed to that. Not containment — Bash stays available, so this is a
deterrent against that drift, not a defense against a hostile session — and
not free: it also blocks whatever a scratchpad or memory write would use.

The briefing alone is not a turn: Claude Code does nothing with a system
prompt until it receives one, so once the Goinkyo's own pane goes quiet
(`GOINKYO_KICKOFF_QUIET_PERIOD`) and is at its prompt, banto types a fixed
kickoff line into it — the same bootstrap problem, and the same shape of fix,
`CODEX_WORKER_KICKOFF_LINE` solves for a freshly-spawned Codex Worker. "At its
prompt" is asked of the pane, every tick until it holds: if Claude's live file
names the pane (the same identification the relay's nudge relies on) its word
is taken, idle types and busy holds; until a live file names it, the painted
input line decides, and only an empty prompt types (`Unrecognized` — a theme
picker, a trust dialog, anything not a prompt — holds). A retry asks for both
witnesses: the sentence is typed again only from a prompt the screen shows empty
that the live file, when it says anything, calls idle (an unlocatable screen may
still hold the first sentence, which an idle live file cannot rule out). A retry is
due once the acknowledgement window has passed since the line was typed *and* the
pane has been quiet for the quiet period, and it is never made once the live file
has read busy after the Enter: a turn ran, so the entry only waits for the
session's row. The row is the acknowledgement only once it has a preview, the
session's first user message: a slash command run in the pane (`/model`, `/help`)
already gives the session a row, with no preview, before any turn, and neither
stops the kickoff nor acknowledges a line banto typed. A row with a preview drops
the entry whether or not banto typed anything. With nothing typed it means the
operator gave the Goinkyo its own first line, and the kickoff is never typed
after it. If the acknowledgement window has passed since the Enter and the row has
no first message, it is reported once (`<name>: took its first turn but its
session shows no first message — banto will not type it again`) and is no longer
owed, whatever the attempt count (a turn that ran after the last allowed attempt is
reported this way, not as "no acknowledgement"); a row with a first message that appears later drops the entry as before. A
hold is said once in
the status line (`<name>: not at its prompt — answer what the pane shows, or
type its first line yourself`), then reminded on the recurring cycle
(`kickoff: waiting for <members> to reach its prompt`, naming only a kickoff
with nothing typed yet); nothing is dropped and
nothing becomes a failure — the line is typed on the tick the prompt appears. A
retry that finds the pane off its prompt after the window is reported once
(`<name>: its first line got no answer and it is not at its prompt — answer what
the pane shows, or type its first line yourself`), is no longer owed (the
brigade-quiet bell is not held for it), and is tried again, as long as attempts
are left, on the tick the pane is back at its prompt. An unsent draft in the pane
holds the kickoff the way it holds a nudge: it is judged by the relay's own
verdict (the keystroke mark, overruled by the input line reading empty once the
settle time has passed since the last forwarded key — a digit pressed at a trust
dialog leaves a mark and no text), banto's own sentence still sitting on the line
(a retry's lost Enter) is recognised first and is never taken for the operator's
draft, and the hold is announced on the relay's draft-hold reminder with its way
out (`kickoff: not typing the first line into <members> — unsent input in the
pane; Enter there sends it as the first turn, clearing the line lets banto type
its own`). A
Worker never needs this: the operator gives it its own first turn by typing
into its pane themselves, which nobody does for an unattended Goinkyo.

**Ending a consultation** removes the Goinkyo's member row, which releases
the guard above and — the next time a tick observes a brigade in use (its
Director hosted, or a Goinkyo pane recorded for it; on stage or not) with no
Goinkyo row at all — kills whatever pane was tracked for it (and unstages it
when it is staged), the same way dismissing a Worker already closes its pane. Two ways
to end one: the Director's own `dismiss_goinkyo` MCP tool (see "MCP
mediation server" below), or the operator picking Dismiss from the
prefix-`x` confirm dialog on the Goinkyo's own pane — the same one-step
dismissal a Worker's pane has. Both end in the same store operation
(`Store::dismiss_worker(brigade, "goinkyo")`); the MCP tool then relies on the
next tick finding the row gone, while the operator's path also unstages the
pane at once and forgets what the core kept for the token — the tick's later
`NoGoinkyo` still releases the one-shot spawn guard and asks to kill a pane that
is already gone (a no-op). Neither removes the consultation's request file at
once; the data directory sweep does (see "Working files of ended members"
below), and the next consultation overwrites it in the meantime.

That dialog decides in two separate stages, not one, both role-based now:
*whether the confirm dismisses* anything reads the focused pane's member token
in a staged brigade (a Worker's or the Goinkyo's, never the Director's) — not
the pane's position, and not whether the Director's own pane is currently known:
a Worker is plainly a Worker either way. (Not dismissed, the confirm only ends
the process: the Director's own pane, a single non-brigade session, and a pane of
a staged brigade the core holds no member token for.) `panes`' own order is display-only and best-effort (a convenience
`update_spawned` attempts on arrival, director-first when it can, never
load-bearing): `Store::brigade_members`' `ORDER BY` puts the Director
first and the common formation path preserves that into `panes`, but a
resume where the Director's own pane needs a fresh `Cmd::OpenEmbedded`
while another member's is already open can still append them out of that
order (see `engine.rs`'s `stage_brigade`/`update_spawned`) — this no longer
matters for correctness, only for which pane a fresh operator's eye lands
on first. *Whether a confirmed Dismiss actually deletes anything* is a
separate, later check on the member's real role from the store
(`update_membership_resolved`), which refuses for a Director regardless of
what the dialog showed — and, like an answer that names no member at all, now
says so in the status instead of doing nothing — still the check that actually
guards deletion; the two stages agreeing is what closes the old UX gap
(Dismiss missing, or offered, on the wrong pane) that a position-derived
first stage used to risk. A disband is noticed a tick later too: the
brigade's roster reads as empty, so its Goinkyo row reads as gone, the guard is
released and the kill names a pane the disband already closed (a no-op).
**Reopening a
brigade around an already-discovered Goinkyo (one that already has a
session id, resolvable to a real row) already works**, for free, through
the same resumed-member path a Director's or Worker's own closed pane
reopens through (`stage_brigade`) — `--permission-mode` and
`--disallowedTools` carry on a resume as on a fresh spawn (a Worker's
`--autocompact` does too, `worker_autocompact` above), because they answer
"who's here to click 'allow'" and "what may this role attempt", not "what should
this session think with"; `--model` and `--effort` do not (2026-11-14: a session
keeps what it last used, so a Goinkyo reopened after a restart is on its own
model and the effort from the operator's Claude settings, no longer `max`;
"Decision (2026-11-14)"). A Goinkyo that was never discovered
(its pane died before it had a session id) does *not* reopen through that
path — `stage_brigade`'s undiscovered-member branch is Worker-only — but
stays a fresh-spawn candidate for the ordinary tick mechanism above for as
long as the brigade's Director is hosted (a creation, so it gets its model and effort).

**A third case — a stranded Goinkyo — sits between those two.** A Goinkyo
can have a session id (its own discovery already ran once) that still
resolves to no row: Claude Code writes no `projects/*.jsonl` transcript
until a session's first turn, so a Goinkyo that never got as far as its own
kickoff (closed, or the operator's machine restarted, before it ran one)
has a real id but nothing `app.row_for_id` can find — permanently, not just
this tick. Left alone this reads as `missing` forever: the session id never
clears on its own, so the ordinary tick mechanism's `AwaitingSpawn` check
(which only fires on no session id at all) never sees this member as
spawnable again — a consultation the operator can't restart short of
dismissing it outright and filing a new one. `stage_brigade` now resets the
session id back to `None` for exactly this case, which lets the next tick's
`AwaitingSpawn` restart the very same consultation (its request file
outlives this, kept until dismissal) through the ordinary spawn path — the
Goinkyo analog of a Worker's own disposable, respawnable design. The one
trap this has to dodge: a pane can be alive under the id key already
(`Cmd::RekeyPty` renamed it) while `app`'s own row list simply hasn't caught
up yet with the not-yet-written transcript — that Goinkyo isn't stranded at
all, just momentarily unresolved, and resetting its session id in that
window would sever a *live* member from the row identifying it: its own
`_mcp` connection resolves who's calling through `brigade_of_session`,
keyed on that same session id, so clearing it would break `send_to_peer`/
`check_messages` for a Goinkyo still mid-consultation. `stage_brigade` tells
the two apart by checking whether a live pane already answers to that key;
if one does, it's reused, not reset.

**Formation.** `B` on a selected session appoints it Director and auto-spawns
`workers` (config, default 1, clamped 1..=8) fresh Workers beside it. `b` spawns
one more Worker into the currently-staged brigade. `B` on an existing Director
opens a disband confirmation instead (Workers cannot be promoted to Director
directly).

Forming (or adding to) a brigade can pass through up to three sequential
confirm gates before anything is actually formed (`crates/banto-core/src/
engine.rs`; at most one is ever open at once). Each is independently skipped
when its own condition doesn't apply, so most presses of `B` show none of
them:

1. **The Director-to-be's own pane is already open.** Brigade wiring (MCP
   config for Claude, `-c mcp_servers.banto.*`/hook overrides for Codex — see
   "MCP mediation server" below) only ever travels through that launch's own
   argv, so a pane opened before formation can't carry it.
   `Modal::ConfirmDirectorReopen` — Enter kills that pane and re-issues
   formation only once the process is confirmed gone by `Event::PtyExited`.
   Lost output alone does not confirm that the process exited; banto waits
   for the wait thread before reopening it. Esc cancels the whole
   attempt. Placed first, ahead of the other two, because it is both the
   most disruptive of the three (running work in that pane is lost) and the
   cheapest to abort (nothing has been picked or written yet) — asking the
   operator to pick a Worker product first, only to then say the pane is
   about to restart anyway, would be backwards.
2. **`[brigade] worker_agent = "select"`.** `Modal::WorkerAgentPicker` asks, before
   there is anything to form or add, which product the Workers about to be created
   run as, and which model and effort they are created with; both `B` (forming) and
   `b` (adding one) open it, and its title says which: ` New Brigade — Workers —
   Claude ` / ` Add Worker — Codex `, with `Enter form` or `Enter add` in its hint
   (`Enter form|add Tab field Shift-Tab agent Esc cancel`). It opens on the agent
   confirmed last time, stored in `ui_prefs` (`last_worker_agent`; one value for
   both pickers and every brigade, written once, when it changes), or on the
   Director's own product when none is stored, with that agent's defaults in two
   one-line fields, Model and Effort. Tab (or Up/Down) moves between them; Shift-Tab
   changes the agent and refills both fields from the new agent's defaults,
   discarding what was typed; a field that is empty or only spaces passes no flag,
   any other text is trimmed and passed as typed, unvalidated. A Codex model stays
   as typed (`terra`): it is resolved to a slug when the session is created.
   Confirming creates those Workers with what the fields hold, once: only the agent
   is stored. With any other `worker_agent` there is no picker and the Workers get
   the resolved product's configured defaults.
3. **The Director's product or the resolved Worker product is Codex, and
   banto's own hook doesn't look trusted.** The Director is briefed by the
   same hook as a Worker, so a Codex Director with Claude Workers (`worker_agent
   = "claude"`, or `"select"` and Claude picked) is checked as much as Codex
   Workers under a Claude Director. A Claude Director with Claude Workers is not
   checked.
   `Modal::ConfirmCodexTrust` — Enter opens a solo pane running
   Codex's own trust-review startup and abandons this formation attempt
   outright, rather than waiting to resume it: Codex records trust as a hash
   of its own hook command, which banto has no way to reproduce or compare
   against (`banto_io::codex_trust`'s module doc), so it cannot tell whether
   an approval in that pane actually took — treating an unverifiable
   "probably fine" as ground truth is a mistake this project has paid for
   before. The operator presses `B` again once done. (Skipped, formation
   proceeding anyway with a status-line notice instead, on the one Codex
   installation banto cannot brief regardless of trust: its own executable
   path contains a space, which Codex's own hook launcher can't run a
   command from.)

A residual race the operator's own choices don't cover — something else
reopens the Director's pane in the gap between gate 1 confirming and the
store round trip actually forming the brigade — is handled without asking:
`update_brigade_formed` kills and rewires that pane unconditionally rather
than leaving a Director with no wiring at all.

**Unidentified Codex panes.** A newly created Codex pane may have no session id
until its first turn. While any such pane is unidentified, banto holds Codex
resumes, and formation or staging that would resume a Codex member, until
discovery or process exit resolves the uncertainty. If formation has already
been saved when this uncertainty appears, the brigade stays saved and a status
message asks the operator to identify the pane before staging it. This guard
reduces the risk of an unknown pane and a resumed session sharing a history.

For a solo Codex creation, discovery snapshots the Codex log database before
spawn and associates later log rows with the spawned PID and its process start
time; the unique associated thread must also be a new session in the pane's
cwd. Already claimed identities are excluded. An unavailable, replaced, or
ambiguous source leaves the pane unidentified rather than guessing. No
discovery has a timeout: a tracker lives as long as its pane and ends only when
the pane is identified or goes away. A brigade member is identified by the
identity its first turn's hook records, so that turn identifies it whenever it
happens and whoever takes it — banto's own kickoff or one the operator types.
While the kickoff is held (the pane is not at its prompt) or failing, the pane
stays unidentified and the guard above stays on; the operator is told when
typing fails, by the kickoff's own status, not by a timer.

Solo attribution requires the PTY child PID to identify the Codex process; a
wrapper or absent PID can leave the pane unresolved. Process start times have
seconds resolution. Uniqueness covers associations through the caught-up log
high-water mark at that poll, not associations that might appear later. File
identity and a decreasing maximum log id detect some replacements or resets,
but not an in-place rewrite and refill. These behaviors have synthetic test
coverage; the original disappearance has not been reproduced in the field.
On 2026-11-16, after restarting the prepared banto, the operator observed that
brigade 23's title was correct and that the Director remained visible when they
created a new brigade named Testing. This observation does not establish the
cause of the earlier disappearance or reproduce its unspecified full sequence.

**Member identity.** Each member gets a banto-owned `member_token`
(`"director"`, `"worker-1"`, `"worker-2"`, ...) rather than being keyed by its
session id — a Worker is formed by banto *before* its agent product assigns it
a session id (it's auto-spawned), so the id has to be a nullable, filled-in-later
column (`brigade_members.session_id`) rather than the primary identity
(`crates/banto-io/src/store/migrations.rs` v7/v12 migration comments — v7
named the column `claude_session_id`; v12 renamed it back to `session_id` once
Codex became a second product it has to hold). The token is stable for the
member's lifetime in the brigade; its session id is not (unknown until
discovered, never reused across brigades).

**A freshly-spawned Codex Worker has no session id to discover until it runs
a turn.** Unlike Claude Code, which publishes `sessions/<pid>.json` the
moment it starts, an idle Codex process with nothing typed into it never
fires its `SessionStart` hook, never gets a `threads` row, never writes a
rollout file — there is nothing for discovery to find yet. So banto starts
the first turn itself: once a fresh Worker's pane output has gone quiet for
`CODEX_KICKOFF_QUIET_PERIOD` (700ms, a threshold measured against this
product's own boot sequence) *and* the pane says it is at its prompt (the latest
OSC 7501 report reads `idle`, and the painted input line reads as an empty
prompt — a menu or a directory-trust question can report `idle` too, which is why
both are asked; typing blind into one would answer it by accident), banto types a fixed, ASCII-only
status line naming itself, then a delayed `\r` in a second PTY write (the
same two-step "text, then submit" shape the auto-relay nudge uses, and
deliberately not that same nudge line, since no peer has actually sent mail
yet). The turn this forces is what fires the hook; the hook's own stdin
carries the real session id, which `banto _hook` records
(`store::record_briefing`) for `poll_discovery`'s `codex_briefed_session_id`
to pick up. Only a briefing recorded at or after the moment the pane's process was
spawned counts: the column is never cleared, so a member resumed or re-staged still
holds the previous process's session id, and taking that one would identify the
fresh pane as the old session on the first poll (the pane rekeyed to the old id,
the kickoff dropped as acknowledged with nothing typed, the relay reading the old
rollout). A briefing with no recorded time cannot show it is newer and is ignored. If the pane is not at its prompt, banto says so once in the
status line (`<name>: not at its prompt — answer what the pane shows, or type its
first line yourself`) and keeps re-checking every tick rather than typing blind or
giving up — answering what the pane shows lets the kickoff resume on its own next
tick, with a qualified status reminder repeated while it remains blocked. The
trust registries gate nothing here (see "Directory trust"). After each typed line
and its delayed Enter, banto waits 15 seconds for acknowledgement (a discovered
Codex session, or a Goinkyo row after its send) before retrying, at most three
times; a retry also waits for the pane's own output to have been quiet for the
quiet period, since the `idle` + empty-line pair is stable only on a pane that
has stopped painting. It asks the same question before every retry and never
writes while the operator has an unsent draft in that pane (judged as the relay
judges one, see "Auto-relay"; a held kickoff is announced with its way out, as
for a Goinkyo above). The Enter transition has not been measured, so the interval
is deliberately generous; a Codex input line can be read only as empty or not
(never for the text on it), so a dropped Enter that leaves the sentence typed
reads as not at its prompt and holds the retry, which is what keeps a retry from
duplicating the bootstrap sentence. (A Goinkyo's input line can be read for text,
so its retry presses Enter alone when the line still holds exactly its
sentence.) A retry that finds the pane off its prompt is reported once, as for a
Goinkyo (`<name>: its first line got no answer and it is not at its prompt —
answer what the pane shows, or type its first line yourself`), is no longer owed,
and is tried again when the pane is back at its prompt. Once the pane has
reported `working`, `done` or `error` (OSC 7501; `error` is sticky, so a first
turn that failed would otherwise read as `idle` again once the operator touched
the pane) after banto's Enter went out — it was `idle` when the line was typed —
a turn ran, and the line is never typed again,
identified or not: if the session is still not discovered when the window runs
out it is reported once (`<name>: took its first turn but was not identified:
banto's hook did not run, most likely because Codex does not trust it — banto
will not type it again`; a Codex Worker is found only through the hook's own
record) and is no longer owed. This holds at any attempt
count: a turn that ran after the third and last attempt is reported so, and is
never told "no acknowledgement after 3 attempts", which is for a line that was typed
three times with no turn seen. Exhaustion remains visible
as a periodic qualified status reminder rather than silently looping.

**Lifecycle.** A Worker is disposable. Prefix-`x` on a Worker's (or the Goinkyo's)
tile asks once and *dismisses* it: it leaves the brigade for good — membership,
read cursor, and any mail addressed specifically to it, all gone — and its pane
ends; its session itself is not deleted (it can still be resumed on its own).
There is no "close the pane but keep the member": one with no pane has no tile to
act on, and too many tiles is what collapse and solo are for. A Director leaves
only by disbanding the brigade (`B`), which removes the whole cell. A **restart**
keeps everyone: staging reopens every member there is (resuming its session, or a
fresh spawn for a Worker with nothing to resume).

**A Worker that ends on its own.** An exit banto did not ask for (a crash, an
agent updating itself, Ctrl+D in the pane) — one whose *process is known to have
exited*, which is the wait thread's report and nothing else — is relaunched in
place when it can be:
the Worker has a session to resume and did not already die within the last minute.
When it cannot — it never spoke, so there is no session row to resume, or it died
again inside that minute — the Worker leaves the brigade (`Store::dismiss_worker`,
no confirmation) and the status says so (`worker-5 exited — removed from the
brigade`, `"impl" exited again — removed from the brigade`); a store failure says
so and leaves the member. It is Workers only: a Director's or a Goinkyo's pane
ending un-stages it and keeps the member, as before. It never fires for an exit
banto asked for (a kill, a dismissal, a disband), while banto is quitting, for a
brigade being disbanded, twice for one member, or when the row is already gone.

**A pane whose output is lost.** The pane's output stream can end without the
process having: the reader thread stops on any read error, and ConPTY and Unix
disagree about whether an exit also produces an end of stream. So that end is not
taken for an exit. The shell gives the wait thread one second (a bounded,
non-blocking wait inside the per-pane poll, `OUTPUT_LOST_GRACE` in
`crates/banto/src/embedded/session.rs`) to report the process's end; if it does,
it is an exit as above. If it does not, banto reports `Event::PtyOutputLost`
instead, and core closes the unreadable tile, asks the process to end, and **keeps
the member** — nothing is removed from the store and nothing is relaunched in
place (a reader that fails at once would loop, and relaunching while the old
process may still run would resume one session twice). The status says so
(`lost the output of worker-5 — pane closed; it stays in the brigade`), and the
process remains tracked until the wait thread reports `PtyExited`; staging a
brigade containing that pane is held until then, so it cannot resume the same
session while the old process may still be alive. A requested kill remains an
expected exit and does not dismiss the member. Nobody ever leaves a brigade on
output loss alone.

A failed `child.wait()` does not establish that the process exited. The waiter
tries to report `Unknown` as a nonterminal notification and retries after 250 ms;
its one-slot queue bounds pending notifications. After a subsequent wait error,
a dropped receiver stops further retries. Only a returned exit code or signal
confirms a live exit. If wait keeps failing after output loss, the process and
resume guard remain tracked until a later confirmed exit. A historical
`PtyExited` event with `Unknown` still follows the existing exit fold on replay.

**Config** (`crates/banto-core/src/config.rs`, `[brigade]` in config.toml):

```rust
pub struct BrigadeConfig {
    pub workers: u32,               // auto-spawned per cell, clamped 1..=8, default 1
    pub worker_model: String,       // --model for a NEW Claude Worker only (never a resume); "" = no flag; default "sonnet"
    pub worker_effort: String,      // --effort for a NEW Claude Worker only; "" = no flag; default "high"
    pub worker_model_codex: String, // -m for a NEW Codex Worker only, a slug or a family word; "" = no flag; default "terra"
    pub worker_effort_codex: String, // -c model_reasoning_effort for a NEW Codex Worker only; "" = no flag; default "high"
    pub worker_agent: WorkerAgentSetting, // Inherit (default) | Select | Claude | Codex — see below
    pub worker_permission_mode: String, // --permission-mode for an auto-spawned Worker; "" = no flag; default "auto"; Claude only
    pub worker_autocompact: String, // a Worker's auto-compact window, every launch; "auto", 100000..=1000000 or ""; default 250000; Claude --autocompact, Codex -c model_auto_compact_token_limit
    pub relay: RelayMode,           // Auto | Manual, default Auto — see "Auto-relay" below
    pub request_attention: bool,    // a Director's decision request also marks and rings, like a quiet fire; default true, read at start; false = the tag only
    pub director_prompt: String,    // role briefing template for the Director
    pub goinkyo_consult: String,    // how readily a Director consults the Goinkyo: "off" | "disputes" | "design" | "decisions"; default "design"; "" = no paragraph — see "Role briefings" below
    pub worker_prompt: String,      // role briefing template for each Worker
    pub goinkyo_prompt: String,     // role briefing template for a Goinkyo; also substitutes {request}
    pub goinkyo_model: String,      // --model for a NEW Goinkyo only; "" = no flag; default "fable"; Claude only
    pub goinkyo_effort: String,     // --effort for a NEW Goinkyo only; "" = no flag; default "max"; Claude only
    pub goinkyo_permission_mode: String, // --permission-mode for an auto-spawned Goinkyo; "" = no flag; default "auto"; Claude only
    pub goinkyo_disallowed_tools: String, // --disallowedTools for an auto-spawned Goinkyo; "" = no flag; default "Edit,Write,NotebookEdit"; Claude only
    pub message_log: bool,          // append every brigade message to a plain-text file under banto's data dir (see "Message log" below); default false
}
```

`worker_agent` decides which product a brigade's Workers run as, independent of the
Director's own product: `Inherit` (default) matches the Director's; `Claude`/`Codex`
fix it regardless; `Select` defers the choice to formation gate 2 above. The model
and effort settings come in pairs per product, `worker_model`/`worker_effort` for
Claude and `worker_model_codex`/`worker_effort_codex` for Codex, independent of each
other, since the two products don't share a model namespace or an effort vocabulary:
one shared field could never default sensibly for both at once. A key absent from
the file is banto's built-in for that key alone (the built-ins name no version:
`sonnet`/`high`, `terra`/`high`, and the Goinkyo's `fable`/`max`); every model and
effort value is trimmed (`config::flag_value`, the one rule for the file, the
built-ins, the picker, either product's Worker and the Goinkyo alike): empty or only
spaces passes no flag, so a new member starts on the operator's own default for that
product, and anything else is passed as the trimmed text, unvalidated.

`worker_permission_mode` defaults to `"auto"`, same flag and same rationale
as `goinkyo_permission_mode` above — a Worker's own operator is no more
watching every pane than an unattended Goinkyo's — carried on a resume too,
not just a fresh spawn, for the same reason. Unlike the Goinkyo, a Worker has
no `worker_disallowed_tools` counterpart: editing the working tree is what a
Worker is for, so there is nothing here for a denylist to guard against.
Codex has no `--permission-mode` equivalent, so this only ever reaches a Claude
Worker's argv — a Codex Worker's `worker_permission_mode` is computed the same as
any other Worker's (`RoleLaunch::of` does not gate it by product) but is dropped
downstream, in `build_open_launch`, the same place that already drops
`disallowed_tools` for Codex. (A Codex Worker does get its effort, at creation, as
`-c model_reasoning_effort`; that is not a `--permission-mode`-style role flag and
is not computed there.)

`worker_autocompact` is passed on every launch, a resume included, as
`worker_permission_mode` is and unlike the model and effort below: it is a launch
flag, and banto does not count on a resumed session remembering it. Default
`250000`. Claude Workers get `--autocompact <value>`; Codex Workers get `-c
model_auto_compact_token_limit=<n>`. Codex uses that key only to lower its
threshold, and clamps it to 90% of the model's window, so a larger value changes
nothing (for example, the window is 500k for gpt-6.1-terra, whose default threshold
is 450k, and 272k for the gpt-6 and gpt-5.6 families, whose default is 244.8k; these
are examples, not a list banto keeps). These Codex facts come from reading codex-cli
0.169.0's help and its upstream source, not from running it. One resumed Codex
thread was watched, on 2026-11-16: a Worker resumed with 250000, whose previous turn
had ended at 288k of a 475k window, compacted before the first model call of its
next turn. The default threshold there is 450k by the source, and the same thread's
earlier compaction had come at 448k; no run without the override was made to
compare. Accepted: `"auto"`, which gives Claude `--autocompact auto` and Codex
nothing; an integer from 100000 to 1000000, which goes to both agents; or `""`,
which passes nothing to either. Any other value, a TOML float included, passes
nothing, and startup shows a one-line notice: `worker_autocompact: ignored "abc"
(not auto or 100000-1000000)`, with the value escaped and cut to 24 characters. A
Director and a Goinkyo never get it, whichever agent they run, so their compaction
stays whatever that agent's own settings say.

**Model and effort: only when a session is created.** `worker_model`,
`worker_effort`, `worker_model_codex`, `worker_effort_codex`, `goinkyo_model` and
`goinkyo_effort` apply when a member's session is created, and never when one is
reopened (decision 2026-11-14 above). In the code `launch::SessionStart` is either
`Resume(id)`, which has no field for either, or `Create { model, effort }`; the
emporium's `build_open_launch` drops them for a resume whatever its caller hands it.
The creation sites are forming a brigade, adding a Worker (`b`), a Worker whose
process died before its session was discovered (created again from the configured
defaults, not from the picker), and a Goinkyo's spawn. The resume sites pass
neither: staging a brigade again, relaunching a member, reopening a solo session or
a Director. For Claude the flags are `--model <model>` and `--effort <level>`,
neither checked here (an unknown model fails at the first API call, in the Worker's
own pane; Claude warns about an invalid effort and ignores it); for Codex `-m
<slug>` and `-c model_reasoning_effort="<level>"`, the level quoted as a TOML string
and not checked (Codex knows its levels), among the `-c` overrides, ahead of the
brigade's own. An empty or whitespace-only value passes nothing; any other is
trimmed and passed as it is.

**A Codex model as a family word.** `worker_model_codex` (and the picker's Model
field) is a slug or a family word, and a word has to meet the installed Codex's
catalog before it can be passed, because Codex has no alias for `-m`. The core holds
the Worker's `Cmd::OpenEmbedded`, asks the shell for the catalog
(`Cmd::ReadCodexCatalog { round }`: `codex debug models --bundled`, run through the
`[agent_binaries]` entry or `codex` on the path, standard input closed, standard
error dropped, at most 5 s and 8 MiB of output, on the event loop's thread, so the
screen can stand still for as long as that bound), and releases it when
`Event::CodexCatalogRead { round, result }` answers with the output or the reason
there is none. Nothing is kept between reads, because the catalog belongs to a
binary that replaces itself when Codex updates.

Each read is a round, counted from 1, and its answer echoes the round and releases
only the creations waiting for that round. A creation requested while a round is
waiting joins it, so a formation's Workers still share one read; a new round starts
only when nothing is held, so at most one round is waiting at a time. An answer for
a round with nothing waiting changes nothing: one given up, one a disband emptied,
one already answered, one never asked for, or one carrying no round at all (a
recording from before the field existed loads it as 0). *Why (found in review):*
without the round, a late answer to an abandoned read released the next read's
creations with the wrong result.

No creation is issued, held or released for a member that is already open or being
opened (`member_open_or_opening`): a hosted pane under any key mapped to it, decided
by brigade and member token (`member_has_pane`), not by the placeholder key the
creation was made for; or an open in flight under any such key, issued and not yet
answered by `Spawned` or `SpawnFailed`, or held (`opening_member`: a brigade
member's is named by the ownership maps, written before the open is issued, a
Director's by its pending entry). A resume in flight counts, not only a creation.
The one question is asked by `open_worker`, by the release of a held creation (not
counting that creation's own pending entry), by the Workers a Director's arrival
opens, and by staging. Staging does not issue again a resume whose key is already in
flight: the shell refuses to resume a session that is already running, and its
refusal would clear the first open's pending entry; that member arrives as an
ordinary spawn. A roster that knows the member's session still resumes it even when
a fresh creation is held, and the held creation is then dropped at its answer. *Why
(found in review):* first, a member whose pane had been re-keyed to its session id
looked pane-less to a check made under the placeholder key and would have been
created a second time; then, a held creation was released beside a resume whose pane
had not arrived yet.

A held creation is carried out only if it is still wanted when its round's answer
arrives (`creation_still_wanted`): the brigade exists and is not being disbanded,
the member is still on its roster, and it is not open or being opened under any
key. Otherwise it is dropped,
silently, together with what opening it would have registered. A successful disband
drops that brigade's held creations at once, without waiting for the answer. *Why
(found in review):* the answer used to release every held creation unconditionally,
so a brigade disbanded in between got a Worker opened for it. The Director's spawn,
which opens the Workers of a brigade just formed, asks the same question before it
opens them. A round that gets no answer within 15 s (`CODEX_CATALOG_GIVE_UP`,
counted from the first tick that sees a creation held, and started afresh with each
new round; three times the command's own bound) is treated as unreadable, case 5
below, and the status says `no answer after 15s`: an answer that late is one that is
not coming, and without the bound the held Workers would stay held for the rest of
the run, since a read is asked for only when none is held.

The command's output is read leniently: a leading byte-order mark (Windows tools
write one), blank lines and log lines before the JSON are tolerated, and of each
model only `slug`, `visibility` and `priority` are read, the other fields ignored.
`banto_core::codex_model::resolve` decides, by five cases, and the status bar says
what happened:

1. A model that is empty or only spaces: no `-m`, no read, nothing said.
2. The catalog lists exactly that slug: passed as it is, nothing said.
3. The word is one whole `-`-separated token of one or more visible slugs, ignoring
   case (`terra` is in `gpt-6.1-terra` and not in `gpt-6.1-terrain`): the newest of
   them is passed. Newest means the highest version (the first token made of digits
   and dots, compared as numbers, so `10.0 > 9.9 > 6.1 > 6 = 6.0`), then the lower
   catalog `priority`, then the slug itself; a model the catalog hides never wins a
   family match, though it can still be asked for by its exact slug. The status says
   `codex worker: terra → gpt-6.1-terra`.
4. Nothing matches: passed as given, since he may know a slug this binary's catalog
   lacks, and the API then decides. The status says `codex worker: "X" is not in
   Codex's model list — passed as given`.
5. The catalog could not be read (no Codex found, a non-zero exit, a timeout, output
   that is not a catalog, or no answer within 15 s). A request holding a digit is
   taken for a slug and passed as it is (`codex worker: could not read Codex's model
   list (<why>) — "X" passed as given`); a bare family word is not passed at all, so
   Codex uses its own default model (`… "X" not resolved, so no -m: Codex uses its
   own default model`). By Codex's own rule that default is the first picker-visible
   model of its catalog, which in the catalog read on 2026-11-14 is `gpt-6.1-sol`,
   not Terra.

Several Workers created together with different words are each resolved and each
said, joined with `; `. The picker's Codex model is shown as typed in all of this.
The effort has no resolution step: whatever the fields or the config hold goes into
the `-c` as it is.

**Role briefing delivery differs by product.** `director_prompt`,
`worker_prompt`, and `goinkyo_prompt` all render the same template,
substituting `{brigade}` (the brigade id), `{token}` (this member's own
token), and `{peers}` (a comma-joined list of its addressable peers); a
Goinkyo's template also substitutes `{request}` (the path to the
consultation request `consult_goinkyo` filed — see "Brigade" above), left
alone in the other two roles' templates the same way any unrecognized
`{...}` is. An empty template means no briefing at all, deliberately a
*setting* and not a constant: without one, a cell exists only in banto's
data model and the operator's own screen, and a Director handed three MCP
tool names with no notion that it leads a cell mostly never uses them. Only
how the rendered text reaches the member differs by product:
- **Claude**: on the launch argv, `--append-system-prompt <rendered>` — and, on a
  resume, `--system-prompt-snapshot off` when the session's own transcript says
  the text would otherwise never be read.

  **A resumed Claude member does not read the argv text on its own.** Claude Code
  began recording the system prompt in 2.1.265, and `--system-prompt-snapshot`
  exists from 2.1.267 (both from the changelog; the behaviour below was measured
  on 2.1.326, print mode, synthetic sessions). banto therefore needs a Claude
  Code of at least 2.1.267 for a resumed member: an older one would be expected
  to reject the unknown flag and fail the launch loudly, which has **not been
  verified** against an old binary. Claude records the system prompt on a
  conversation's first request, as an `attachment` record whose
  `attachment.type` is `prompt_snapshot` and whose `attachment.systemPrompt` is a
  list of text sections, banto's appended briefing being the last one and
  byte-equal to the argv text. Every later request reuses that recorded prompt,
  `--resume` included, and **a resumed launch's own `--append-system-prompt` text
  is ignored** — so a session that already had a turn before it was appointed
  Director never got its briefing, and a changed template never reached a
  resumed member. `--system-prompt-snapshot off` makes Claude render the prompt
  on every request instead: the new text then reaches the model, and the cache
  stays stable for as long as the text does not change. It does not re-record,
  though, so the transcript keeps the old copy. After a compaction (a `system`
  record with `subtype` `compact_boundary`) the next launch *without* the flag
  records its own text afresh. Snapshots are written when a conversation is
  created (before and after its first request) and on the first request after a
  compaction; resumed turns wrote none. (An earlier version of this section, and
  of the code, relied on a check against 2.1.219 that the text applies to a
  `--resume` as to a fresh launch; for these sessions it does not.)

  banto's rule, applied at each launch of a Claude brigade member (Director,
  Worker, Goinkyo) that is passed a briefing
  (`crates/banto-io/src/claude_briefing.rs`, called where
  `crates/banto/src/embedded/emporium.rs` builds the launch):
  - A creation never gets the flag: it records what it is given.
  - A resume reads the session's own transcript, read-only
    (`<claude home>/projects/<project>/<session id>.jsonl`; banto never writes
    under the agent's home), from its end, in a bounded window (1 MiB, growing to
    32 MiB; bytes after the last newline are ignored), and walks complete lines
    backwards to the *first* of two records. Records of a subagent
    (`isSidechain: true`) and of any other type are walked past. Four real
    interactive transcripts (up to 127 MB) had the newest such record never more
    than about 12 MB from the end; a session with none within the bound gets
    the flag, which is redundant at worst. Every project folder holding the
    session id is read, since which copy Claude resumes from is unmeasured, and
    one that needs the flag is enough; no transcript at all needs it too.
    - A `prompt_snapshot`: its last section against the briefing about to be
      passed. Equal: launch as before. Different: add `--system-prompt-snapshot
      off`. `{peers}` is a hole in that comparison that matches only what a
      roster can render to — "none yet", or member tokens joined by `, `, a token
      being one or more of `[A-Za-z0-9_-]`, the characters banto's own tokens
      (`director`, `goinkyo`, `worker-<n>`) are made of — and the text around it
      must match exactly. A changed roster alone must not cost the flag; but a
      hole that took any text would also step over a sentence the operator has
      since deleted from the template right after `{peers}`, leaving the member
      on the rule it was meant to lose. The grammar lives beside `render` in
      `crates/banto/src/briefing.rs`. A hole is looked at for at most 4096 bytes
      and a comparison gives up after a fixed number of steps, so that a
      transcript built to be slow to match cannot hold a launch; both read as "a
      different briefing", which costs a redundant flag.
    - A compact boundary (`type` `system` and `subtype` `compact_boundary`, both
      exact): launch as before; the next default launch records its own text.
    - Anything else gets the flag: the bound reached, a missing or unreadable
      file, a line that does not parse, a snapshot of a shape this reader does
      not know, or the start of the file with neither record. **Not finding a
      snapshot is never read as "there is nothing to disagree with"**; only a
      boundary newer than any snapshot proves that. An upstream rename of either
      record therefore costs a redundant flag, never a member silently left on a
      stale briefing.
  - A launch with no briefing (an empty template), a session outside a brigade,
    and Codex are not touched.

  Because `off` never re-records, a member stays on `off` until its first
  relaunch after its next compaction: each launch in between reads the same
  transcript, so the answer changes only when the launch text does (a template
  edited back to the recorded text needs the flag no longer). Two levers follow
  for the operator:
  compacting a session just before appointing it Director avoids the flag
  entirely (the Director's first launch then records the briefing itself), and
  `/compact` inside a member ends its `off` period. And a cost: while a member is
  on `off`, a launch text that differs from the previous launch's (a roster change
  at relaunch, an edited template) changes the rendered system prompt, which
  rewrites the cached context once; launches with unchanged text do not.
- **Codex has no equivalent flag.** Its briefing instead rides banto's own
  `SessionStart` hook: every Codex member's launch adds a `-c
  hooks.SessionStart=[...]` override naming `<banto executable> _hook` as
  the hook command — byte-identical across every member and every launch,
  deliberately, since Codex hashes that literal command string to decide
  whether to trust it, and a launch carrying member identity in the command
  itself would make every member's hook need its own separate approval.
  Identity instead travels through `BANTO_BRIGADE`/`BANTO_MEMBER`/
  `BANTO_ROLE` in the environment, which the hook process inherits.
  `banto _hook` (`crates/banto/src/hook.rs`) renders the same template
  (`crates/banto/src/briefing.rs`) and returns it as the turn's
  `additionalContext` — unless the session already holds it, see "Delivered
  once, not on every resume" below — appending a Codex-only addendum: banto's own
  MCP tools are deferred from Codex's own tool list until called by name,
  and Codex's own developer prompt otherwise casts the model as the primary
  agent of a different, unrelated multi-agent feature — measured end to end
  in `docs/notes/codex-briefing-spike.md`. So the addendum names every tool
  the member's role may call: the three any member has, and to a Director
  only, its own four (`request_decision`, `name_member`, `consult_goinkyo`,
  `dismiss_goinkyo`). Until 2026-11-16 it named only the three, so a Codex
  Director was never told the four; a test checks every advertised tool
  against the two lists.

  **Delivered once, not on every resume.** Codex stores every
  `additionalContext` a hook returns as a developer message and replays all of
  them on resume, so answering every firing piled identical copies into a
  member's history (measured on a synthetic Codex 0.169.0 session: one copy at
  startup, then two, then three across two `exec resume` runs; real Worker
  rollouts held up to four live copies of a 4956-character text). A launch-time
  `developer_instructions` override did not refresh on resume either, so it is
  not a way round. A compaction drops every earlier copy, and the
  `SessionStart` Codex then queues (source `compact`) runs only at the start of
  the next regular turn, so a process that ends first never runs it: measured,
  a compaction followed by an exit leaves zero copies until something delivers
  again. The briefing therefore has to be delivered whenever the live history
  may lack it, and withheld only on proof that it holds it. The rule
  (`banto_io::codex_briefing`; `crates/banto/src/hook.rs` applies it):
  - A firing whose `source` is anything but `resume` (`startup`, `compact`,
    `clear`, `fork`, absent, or a value banto does not know) delivers, and
    reads nothing.
  - On `resume` the hook reads the end of the session's rollout, at the
    `transcript_path` Codex gives it on stdin — read-only, at most the last 32
    MiB, in windows that grow from 1 MiB — ignoring whatever follows the last
    newline (that is the process still writing it), and walks the complete lines
    backwards. A `compacted` record or a `thread_rolled_back` event delivers: the
    history was rewritten after any copy behind it. A developer message holding
    the rendered briefing, character for character, as one `input_text` part
    suppresses it: the hook prints `{}`. A developer message that is a banto
    briefing with other text — one containing the addendum's final sentence,
    below — delivers: the briefing changed since,
    for instance because the operator edited a template or `goinkyo_consult`,
    and the older copy is not what the member should hold. A real change
    therefore costs one extra copy in the history, which the addendum's last
    sentence covers. Every other record, a developer message that is not a banto
    briefing (another hook's context, a briefing from before banto ended its
    text with that sentence) and a record of a type banto has never seen
    included, is walked past — the walk is the second line behind the `source`
    rule, and treating an unfamiliar record as doubt would switch it off the day
    Codex adds a kind. This is why the sentence is the very last thing in every
    Codex briefing: whatever banto appends goes before the addendum.
  - Everything else delivers: no or unreadable `transcript_path`, any I/O
    error, a complete line that is not JSON, a walk that ends without a
    decision (whether the window was capped or reached the start of the file).
    A copy further back than the window is delivered again — one extra copy, not
    an unbounded read on the session's startup path.
  - **A briefing over the spill limit is never suppressed.** Codex stores a hook
    context longer than about 10,000 *bytes* (upstream counts `ceil(bytes / 4)`
    approximate tokens against 2,500) as a truncated preview plus a pointer to a
    temp file. The stored preview never equals the rendered text, so such a
    briefing is delivered on every resume: copies accumulate again for that
    case, and the member sees only the preview. Measured on a synthetic session:
    9,900 ASCII characters stored whole, 10,500 stored as a preview of about
    10,100 characters with the pointer. Because the limit is in bytes, a Japanese
    template of about 3,500 characters is already over it; the shipped defaults
    are well under. The consultation paragraph a Director is given after its
    template (`goinkyo_consult`, above) and the addendum count toward it, so a
    custom template near the limit can be pushed over by text the operator did
    not write. A spilled preview keeps the tail of the text and puts a
    "Full hook output saved to:" line after it, so the addendum's sentence is near
    its end, not at it; the walk looks for the sentence anywhere in a developer
    message, so it recognises the preview as a banto briefing of some other text
    and delivers (an older exact copy behind it must not suppress). That the
    preview keeps the tail is from a synthetic reproduction, not re-measured on
    a real Codex.
  - `briefed_at` is recorded on every firing that carries a `session_id` on its
    stdin, for a member whose brigade and token the environment gives and while
    the store opens — a withheld firing included. It says the hook fired for the
    member (Codex's discovery reads it), not that text was printed.
  - **A Codex member is never silently unbriefed.** Codex runs a hook only if it
    trusts it, and a hook that does not run says nothing anywhere. Codex trusts one
    banto executable path at a time (measured 2026-11-17 with a synthetic
    `CODEX_HOME` on Codex 0.169.0), and banto can run from two, so trust can be lost
    without the operator doing anything; and the formation check (gate 3 above) is
    only as good as `hook_trust_state`'s reading of `config.toml` — it reads
    `Primed` when some `<session-flags>` `session_start` record holds a hash, not
    when that record matches this executable. "Never silent" means one of two
    notices, each said once:
    - *A member whose session banto knows.* For a hosted Codex member (any role)
      whose pane has shown a turn since its process was spawned, and whose row's
      `briefed_at` is older than that spawn or empty, the status line says
      `codex: no briefing reached <member> — banto's hook did not run, most likely
      because Codex does not trust it (Codex trusts one banto.exe path at a time)`,
      once per launch. It names no key: the only way to Codex's trust-review pane is
      gate 3, which is reached by `B`, and `B` on a session that is not a Director
      forms a brigade (and on a Director disbands one), so pointing at it would
      send the operator into a side effect. A direct entry to the review pane is
      undecided.
    - *A fresh Codex Worker.* Its only discovery source is the hook, so a hook that
      did not run leaves it without a session id and it never reaches the notice
      above. The kickoff's report for a turn that ran with the Worker still
      unidentified says why: `<name>: took its first turn but was not identified:
      banto's hook did not run, most likely because Codex does not trust it — banto
      will not type it again` (the Worker kickoff's own report, above).
    The pane showed a turn when its own OSC 7501 report said `done` or `error` —
    never `working`, which also blips at start-up and at `/new`, `/clear` and
    `/resume`. The core latches that per pane (`EmporiumState::codex_turn_ran`),
    cleared when the pane spawns, so a sticky report need not be caught on a
    particular tick and a relaunch starts again; an interrupted turn can end in
    neither, so the first notice can be late, not early. The shell compares
    `briefed_at` with the moment the pane's process was spawned
    (`PtyHandle::spawned_at`, the same `>=` comparison as the session-id discovery
    above) every tick and hands the core the answer with the observation
    (`RelayObservation::briefed_since_launch`); the core only decides from a latch
    an earlier tick set, since the tick that sees `done` may hold a roster read from
    before the hook wrote its row. It waits for a free status line rather than
    overwriting one. This reads, and does not change, how the briefing is
    delivered. Not covered by the first notice: a member whose session id banto has
    not yet learned has no observation (the second notice is the one for a fresh
    Worker; a Codex Director or Goinkyo in that state is not covered).
  - A complete line that is valid JSON but not an object, or whose `type` is not
    a string, counts as a line that does not parse: it delivers.
  - The addendum ends with one sentence for the cases the rule cannot prove:
    if the conversation holds more than one banto briefing, the most recent one is
    current and replaces the earlier ones.

The shipped defaults tell a Director to delegate independent, parallelizable work to
its Workers via `send_to_peer` and keep genuinely sequential work itself, that it
can name a Worker with `name_member` when it gives it a standing role, and (since
2026-11-15) three points about reaching the operator: banto itself tells him when
the brigade has gone quiet, so do not announce completion; when he alone can decide
and work stops on it, write the question in your own output and then call
`request_decision`, which shows him that you are waiting, not the question; use the
agent's own question picker only when no Worker is running. These apply to the
default prompt only: an operator's own `director_prompt` does not get them. That policy
is intentionally a setting the operator can change, not a fixed behavior. (No
placeholder was added for names: the template still substitutes only `{brigade}`,
`{token}`, `{peers}` and, for a Goinkyo, `{request}`.)

**How readily a Director consults the Goinkyo — `[brigade] goinkyo_consult`.**
Four levels of how readily the Director consults the Goinkyo *on its own
initiative*; at every level it also consults when the operator asks. A paragraph
banto owns, not the template, tells it so. It is appended after the rendered
`director_prompt` — the shipped one or the operator's own — so an operator who
writes his own template still gets it. The values:
  - `"off"`: do not call `consult_goinkyo` on your own initiative, only when the
    operator asks. A consultation already open is finished and dismissed as usual.
  - `"disputes"`: consult (kind `arbitration`) for a disagreement with a Worker
    that one more exchange has not settled, contradictory reports no check can
    decide between, or a choice the Director cannot make. Nothing else.
  - `"design"` (the default): `disputes`, and also consult (kind `design`) before
    the shape of a *lasting change* is settled — something later work will rest on
    or the operator will live with, or a change in what existing behaviour is
    meant to be. A shape the operator stated himself, one more of a kind that
    already exists, a mechanical edit and a plain repair need none. When the
    operator approved a shape without a consultation, it stands.
  - `"decisions"`: `design`, and also consult (giving the courses weighed as
    `alternatives`) whenever the Director itself chooses one course over another in
    the work: which of several fixes, what to leave out, what to advise the
    operator to do first. A choice about how it runs the brigade does not count.
    At this level an approval does not stand in for a consultation.
  - Every level also says to file an operator-requested consultation as
    `arbitration` for a disagreement or an impasse and `design` otherwise, and to
    tell the operator in one line when it goes against what the Goinkyo advised.

**banto enforces none of them.** Every level, `off` included, is an instruction in
the Director's briefing that moves how often it consults, not a guarantee: `off` is
an instruction, not a lock, and `consult_goinkyo` works at `off` exactly as at
any other level. A Director can consult at `off` and can fail to consult at
`decisions`. The tool is never refused for this setting and `tools/list` is
unchanged. The paragraph's wording is code, in one place
(`crates/banto/src/briefing.rs`, each sentence written once and the levels are
joins of them), which `crate::briefing::compose` calls for both products, so the
Claude and Codex Directors cannot be told different things.

How the value is read:
  - It is read as written whatever its TOML type, so `goinkyo_consult = false`
    does not fail the document (a discovered config that failed to parse falls
    back to every default). Case and surrounding spaces do not matter.
  - Anything that is not a level reads as `"design"`, and one line at startup
    says so: `goinkyo_consult: ignored "<value>" (not off, disputes, design or
    decisions) — using design`.
  - `""` (or only spaces) is no paragraph at all, the file's usual empty-string
    convention: the operator who wants none of banto's policy text writes it.
  - The paragraph needs a template to follow. **With an empty `director_prompt`
    the setting does nothing, `off` included**: a Claude Director then launches
    with no briefing flag at all and a Codex Director gets only the addendum, so
    `off` cannot be expressed without a briefing. Workers and the Goinkyo never get it.

*When a change takes effect.* The UI reads the config once when banto starts, and
a Claude Director gets the level in its launch argv, so after an edit **restart
banto** and the Director is relaunched with it. A Codex Director's `banto _hook`
re-reads the file at every firing (a fresh process each time), so its next
`SessionStart` picks an edit up without a restart; a changed briefing is then
delivered instead of withheld — see "Delivered once" above. A running Director
never sees a change; the operator's immediate lever is to tell it in its pane, in
one line. The invalid-value startup notice is shown only by the UI.

*Size.* The paragraph counts toward Codex's spill limit above. Measured: `off` 479
bytes, `disputes` 1,144, `design` 2,368, `decisions` 3,311. The shipped default
template, the `decisions` paragraph and the addendum, with a full roster (eight
Workers and a Goinkyo), come to 8,471 bytes, and a test keeps that under 9,900.
A custom template near the limit can be pushed over by text the operator did not
write.

## MCP mediation server

An embedded `claude` session is launched with `claude --mcp-config <file>`
pointing at `banto _mcp --brigade <id> --member <token> --role <role>
[--session <id>]`; Claude Code spawns that as a stdio MCP server and speaks
JSON-RPC 2.0 to it (newline-delimited, no Content-Length framing — requests
carry an `id` and get a response, notifications don't). Because banto controls
the launch argv, the config file lives under banto's own data directory
(`<data dir>/mcp/<brigade_id>-<token>.json`, the data directory being
`dirs::data_local_dir()/banto` unless the operator chose another — below) and is never
installed into Claude Code's own configuration (`crates/banto/src/mcp.rs`
module doc; `crates/banto/src/embedded/emporium.rs`'s `write_mcp_config`).
Every path under the data directory is named in one place,
`crates/banto-io/src/data_dir.rs` (`DataDir`); nothing else joins one.

**Child processes find the parent's files.** banto starts other banto processes
— `_mcp` (Claude: the MCP config file; Codex: the `-c mcp_servers.banto.args`
override) and `_hook` (Codex's `SessionStart` hook) — and each of them would otherwise resolve for itself, from
its own environment and from the working directory of the session it serves, which
is not the parent's: a relative `--config`, `BANTO_CONFIG`, `db_path` or home would
name another file (or none), `--config`/`--claude-home`/`--codex-home` would never
be heard of, and agreement through inherited variables is not something
Claude or Codex guarantee. So the parent resolves its config file, database and
homes **once** (`crates/banto/src/startup.rs`; a relative input means the
parent's working directory at startup, and is made absolute; an empty path, or
any path when the working directory cannot be read, is left as given — except
that an empty `db_path` is a startup error for every command that opens the store,
naming the key and the config file, rather than a silently non-persistent session;
`_hook` then runs as it does with no store: no briefing, nothing recorded) and
hands the result on. The data directory is resolved the same way, once, and is
what the default database (`<data dir>/banto.db`) hangs off, so a database named
elsewhere does not move it. Its sources, first hit wins: the hidden `--data-dir`
flag, `BANTO_DATA_DIR` (empty means unset), the top-level `data_dir` key of
`config.toml` (a key under a `[section]` belongs to that section and means
nothing), `dirs::data_local_dir()/banto`. A relative value is made absolute
against the working directory; an empty `data_dir` is a startup error naming the
key and the file; a relative one with no readable working directory is an error
too. A data directory, and equally a `db_path`, inside the resolved Claude home
or Codex home is refused, naming the key and the home (invariant 1): the UI and
`_mcp` fail to start, and the hook, which must never fail the operator's agent,
silently opens no store. "Inside" is judged on where the path leads: its longest
existing ancestor is canonicalised (symlinks and junctions followed, 8.3 short
names and a trailing dot or space resolved on Windows), the rest is read as
written, and names are compared folded on Windows and macOS. Whatever that
cannot see, the filesystem's identity does: a path with an existing ancestor that
is the very directory a home is (same volume and file index, however either was
spelled: `\\localhost\C$\…`, a computer name, a short name) is inside it
(`banto_io::file_identity`). Nothing is created,
and a link made after startup is not seen. Changing the data directory moves
nothing: the new place starts with an empty database. When both a data directory
and a `db_path` are named and the database is not `<data dir>/banto.db`, the
status line says the sweep does not run and that the MCP, request and log files
stay in the data directory (`startup::split_database_notice`). `_mcp` takes it as hidden global flags straight after the
subcommand name: `--parent-config <file>` (the file the parent loaded, loaded the
same way) or `--parent-no-config` (it loaded none), `--db-path`, `--data-dir`,
`--claude-home` and, when it has one, `--codex-home` — each path absolute and a single argument
(spaces and backslashes survive the MCP JSON and the Codex TOML array like the
executable's own path does). A child given them uses
exactly those and does not look at its own environment or working directory for
them — and a child that carries the hand-off but no `--codex-home` has none,
because the parent had none; the same goes for `--data-dir`: a parent with no data
directory passes none, and its child has none rather than finding one of its own
(`consult_goinkyo` then says so, and no message log is written). `--config` is
refused next to the hand-off flags.
`_hook` cannot: Codex trusts the hook by a hash of its literal command
string, and that string cannot be quoted, so it carries no argument, and the
values travel as environment variables on the member's launch, beside its
identity — `BANTO_PARENT_CONFIG` (or `BANTO_PARENT_NO_CONFIG`) and
`BANTO_PARENT_DB` (the hook looks in no agent home and in no file under the data
directory, so it is not told one; the one other file it reads, a Codex session's
rollout, is named by Codex on the hook's own stdin, and is only read). A hook without them (one an
older banto launched) resolves for itself, as does a child started by hand or from
an MCP config written by an older banto. Only banto's own variables are set, with
one exception: every Claude Code pane (not a Codex one) also gets
`CLAUDE_CODE_NATIVE_CURSOR=1`, which fixes how Claude draws its caret so that the
input line stays readable (the relay draft guard's account of reading the input
line has why). `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and the like are never touched —
where an agent keeps its home is not banto's to decide. A member that is already running keeps
the server it was launched with; the next time its pane is opened (re-staging the
brigade, a relaunch, a new member) the MCP config file is rewritten and the
launch argv rebuilt with the hand-off.

**One banto UI per database.** A banto UI that would open a database another
running banto UI already uses does not start: it exits non-zero with one line on
standard error naming the database, the holder's process id when known, and the way
out (quit that banto, or give this one another database through `BANTO_DATA_DIR`,
or the config keys `data_dir` or `db_path`). The claim
is `single_instance::claim_database`, called in `main`'s UI arm and nowhere else:
`_mcp` and `_hook` are the running UI's own children (the hook is the agent's) and
open the database it holds without migrating it (see "Upgrading banto"), and `list` and
`versions` open none. It comes after the
database path is resolved and checked (a path inside an agent's home is refused
first) and *before* `Store::open`, because opening migrates, and a migration under a
running older banto would leave that one unable to reopen its own database. It also
comes before the terminal is touched, so a refusal leaves the screen alone. The
database's directory is created first so that a first run takes the lock too; the
lock is held until the event loop returns, the five-second shutdown included.

The lock is the operating system's (`std::fs::File::try_lock`: `LockFileEx` on
Windows, `flock` on Unix) on `<database file>.lock` beside the database
(`banto_io::instance_lock`). The path is canonicalised where it can be, so two
spellings of one database (a relative path, a link, `..`, another case of the same
name on a case-insensitive filesystem) meet at one lock file. The lock file is
opened for reading and writing, never deleted, and holds nothing, because Windows
forbids other processes from reading a locked file. The holder's process id is in a
sidecar, `<database file>.lock.pid`, written after the lock is taken and read only
to name the holder in the refusal: a hint, never consulted for the decision, so a
stale or missing one just leaves the process unnamed. *Measured* (worker-8,
2026-11-18, Windows 11 NTFS and WSL2 ext4): the lock is released at once, within
about 24 ms, when its holder exits or is killed hard (20 of 20 cycles); a child
that outlives its holder does not keep it; an append-only handle cannot take a lock
on Windows; and the bytes of a locked file are unreadable to other processes there.

*When the check cannot be made* because the lock file cannot be opened or locking
itself fails (a filesystem with no locks, as some network shares) the UI starts
anyway and the status line says so with the kind of the I/O error
(`single-instance check failed (…)`): refusing there would keep banto off a share
that merely lacks locks and protect nothing. A directory that cannot be created is
also an unchecked claim, but it does not start the UI: startup stops at the store,
which meets the same directory, exactly as it did before there was a lock. It also
means two bantos on one database, which is what this exists to prevent, can run; the
notice is the only sign. A banto on another database may still run beside this one, in the same data
directory even; the sweep and the file names are discussed under "Working files of
ended members". Not measured: the lock on a network filesystem, and on macOS.

**Upgrading banto.** A newer banto replacing an older one on disk touches three things
that outlive the binary, and each has a rule.

*The database is copied before it is migrated.* When the UI (`Store::open`, in `main`'s
UI arm) opens a database whose `PRAGMA user_version` is above 0 and below the schema
this build carries, it first writes `<database file>.before-v<N>` beside the database
(`banto.db.before-v15`; the file name of a `db_path` elsewhere is kept). `N` is the
version the migration reaches *first*, so the copy always holds schema `N - 1`: a v14
database becomes `banto.db.before-v15` even when this build goes on to v19. That is the
reading of the operator's hand-made `before-v15`, `-v16` and `-v17` beside a v18
database (each is the database before it became that version), and it lets a build
that carries schema `K` name the copy it can go back to, `before-v<K + 1>`, without
knowing which build wrote it. The copy is `VACUUM INTO` (a consistent snapshot) to
`<name>.partial` in the same directory, synced, then given its name by a hard link
(which fails where the name is taken, on NTFS and Unix, where a rename would replace
the file on Windows; a filesystem with no hard links falls back to looking and then
renaming), so a crash leaves at most a `.partial`, which the next start replaces. A
copy that already exists is never overwritten and no other is written: the older one is
the better one, and the start goes on. A copy that appears while this one is being
written is kept too, except on a filesystem with no hard links (FAT, some SMB shares):
there the look-then-rename is not atomic, and a copy that appears between the look and
the rename can be replaced. A brand-new database (no file, an empty one, version 0) and
a current one are not copied. If the copy cannot be written the start fails with one
error naming the intended path and the cause, and the database is untouched, *not
migrated*. A `.partial` is removed by the next start or by the attempt that failed; a finished copy
is never deleted. A database *newer* than the build (`UnsupportedSchemaVersion`) is
refused with the fix in the error: it was upgraded by a newer banto; run that build, or
go back with this build and the newest `banto.db.before-v<N>` beside the database with
`N` at most this build's schema plus one.

*Only the UI migrates.* `Store::open_current` is the door of `_mcp` and `_hook`: it
opens an existing database (never creating one), never migrates, never copies, and uses
it only when `user_version` is exactly the schema this build carries; any other value
(older or newer) is the refusal `banto was replaced on disk; restart banto`, followed by
the two schema numbers. `_hook` stays lenient (a refusal is "no store": no briefing,
nothing recorded, `{}` on stdout, exit 0). `_mcp` completes the MCP handshake, lists its
tools as usual and answers every tool call with that sentence as a tool error, so the
agent can show it rather than only find a server that failed to start. The reason: on
Unix a running banto's executable can be replaced, and the next child the UI starts is
then the new build; if that one migrated, the single-instance lock, which exists so that
nothing migrates a database under a running banto, would be defeated by the UI's own
children.

*The executable is resolved once.* At UI start, before the single-instance claim and the
store (backing up and migrating can take a while, and a banto replaced meanwhile would
otherwise become the baseline on Windows, or resolve to `<path> (deleted)` on Linux),
`exe::BantoExe::resolve` reads
`std::env::current_exe()` and the file's identity (`banto_io::file_identity::file_identity`:
volume and file index through `GetFileInformationByHandle` on Windows, device and inode
on Unix: the probe used for directories, which opens nothing for reading and so works
on a running executable). That value is handed down in `EmporiumSettings::exe` and
`Deps::exe`, and everything that names the executable uses it instead of asking the
system again: the `--mcp-config` file's `command`, the Codex `-c` overrides (the `_mcp`
server and the `_hook` command), the trust pane's argv, and the "can the hook carry this
path" judgment. At each *member open* (a spawn or resume of a brigade member, which will
start `banto _mcp` or the Codex hook) and at the trust pane's open, `BantoExe::check`
reads the identity of that path again: a file that is gone or is another file refuses
the open with the status line `failed to open: banto was replaced on disk; restart
banto`, before any member config is written or anything is spawned. A pane that is not
a brigade member starts no banto child and is not checked. The reason is measured (Linux
under WSL, 2026-11-18): once a binary is replaced by a rename, `/proc/<pid>/exe` reads
`<path> (deleted)`, a path with a space in it, and a `current_exe()` asked at each use
would have handed the Codex hook command a path it cannot carry and dropped the hook
without a sign. A banto that cannot name its own executable at all still starts; each
member open then fails with the system's reason. One that names it but cannot read the
file's identity also starts, and shows one startup notice that a replacement cannot be
noticed this run (with the error kind); its member opens are not refused. Not
measured: what replacing a *running* executable does on Windows (the check covers a
rename-aside, which Windows allows); an in-place rewrite keeps the file's identity and
is not noticed (Unix refuses that while the executable runs).

*`banto --version`* prints `banto <package version> (commit <commit>, schema <n>)`, for
example `banto 0.1.0 (commit abc1234, schema 18)`. The commit is
`option_env!("BANTO_BUILD_COMMIT")`, set by the release workflow when it compiles, and
printed as given; absent or blank it is `commit unknown` (a `cargo build` or
`cargo install --git` build). There is no `build.rs`. The schema is
`banto_io::store::SCHEMA_VERSION`, the number the migrations end at. `-V` prints the same.

**Working files of ended members.** The data directory holds two kinds of
file per member that nothing used to delete: the MCP config
(`mcp/<brigade_id>-<token>.json`, rewritten by the emporium every time it opens
the member) and, for a Goinkyo, the consultation request
(`mcp/goinkyo/<brigade_id>.txt`, written by the `_mcp` process before the member's
row exists). The **emporium** (banto's own UI process) sweeps them, and only
it — the `_mcp` processes write files but never delete one.
Immediacy is not a goal: a file is removed some time after its member ended.

*What is deleted.* A file, and only a file, for which all of these hold:

1. its name is exactly one banto writes, for a token banto issues:
   `<id>-director.json`, `<id>-goinkyo.json` or `<id>-worker-<n>.json` in `mcp/`
   (`<id>` a decimal brigade id from 1 and `<n>` a Worker number from 1, neither
   with a leading zero), or `<id>.txt` in `mcp/goinkyo/`. The one function family
   that writes these names is the parser's only grammar, and a token outside that
   set gets no file at all (a Claude member with such a token fails to spawn;
   a Codex member has no config file and is unaffected). Everything else —
   `7-evil.json`, `07-worker-1.json`, `7-worker-01.json`, `7-Worker-1.json`,
   `7-worker-1.JSON`, the message logs, the database — is never a candidate;
2. `mcp/` and `mcp/goinkyo/` are real directories: if either is a symbolic link,
   junction or other reparse point it is skipped whole, and a candidate that is
   itself a link or a directory is skipped;
3. the store has no member that owns it: for a config, no member row with that
   brigade and token; for a request, no member row `goinkyo` in that brigade. Only
   the bare `(brigade, token)` of every row is compared, whatever the row's role or
   agent say, and a row that cannot be read at all stops the sweep;
4. the file was last written at least thirty minutes ago, read just before the
   deletion (an unreadable time, or one in the future, is "not old").

*What decides to sweep.* Only a data directory whose own database this process
has open: the resolved database path must equal `<data dir>/banto.db` (compared
as written after both were made absolute; an unknown or relative one, or any
other database — a test profile, a `db_path` in the config — means no sweep).
The store is the whole of the authority, so an instance with another store never
deletes anything, however it shares the directory. This is checked at the edge,
where the emporium runs the command (`Resolved::sweepable_data_dir`).

*When.* Sixty seconds after the emporium's first tick and every ten minutes
after that, and at no other time: ending a member or disbanding a brigade does not
ask for a sweep, since what it just ended is younger than the grace and could not
be deleted. The directories are listed **before** the store is read, then each
candidate's age is read, then it is deleted. A file that cannot be deleted (in
use, no permission, already gone) is counted and tried again next time, never an
error, and a store that cannot be read deletes nothing.

*Why that is enough.* A consultation request exists before its Goinkyo's row, and
a Goinkyo's file names are fixed (`7-goinkyo.json`, `7.txt`) and so reused by the
next consultation: the store alone cannot tell a request about to be adopted, or
a name about to be reused, from a leftover. The age test can, because banto
rewrites a member's config every time it opens the member and writes a request
when it is filed — a file about to be used is minutes old, not half an hour. A
config written while the store is being read is not in the listing at all.

*What this does not guard against.* A wall clock that jumps forward by more than
thirty minutes, or a file restored with an old modification time (either looks
old); a process suspended for more than thirty minutes between writing a request
and registering its Goinkyo; the moment between the last look at a file and its
deletion (there is no "delete only if unchanged"); a data directory whose root is
itself a link, which is taken as the operator's choice; and another banto with a
different database writing the same file names in the same directory (nothing
stops two stores from both using `1-director.json`; only the sweep is safe from
it). It is also assumed, not measured, that a running Claude has read its config
at launch and does not need the file again. The logs and the database are never
deleted.

**Message log (`[brigade] message_log = true`, off by default).** For someone
developing banto who wants to audit what the agents told each other. Every
message `send_to_peer` accepts, and every `consult_goinkyo` request (as the text
the Goinkyo reads), is appended in full to
`<data dir>/logs/brigade-<brigade_id>.log`, written by the
`banto _mcp` process that handled the call (`crates/banto-io/src/brigade_log.rs`).
The file's name is not configurable, so it is always inside banto's own data
directory (wherever the operator put that). An entry is a header line starting with `@@@` — UTC time, `message
#<queue id>` or `consultation-request`, `from=<token>` and `to=<token>` or
`to=role:<role>` (a display name in quotes after its token), and last
`bytes=<n>` — then the body verbatim, then two newlines:

```
@@@ 2026-11-13T09:15:02Z message #42 from=worker-1 "impl" to=director bytes=34
Parser check finished (synthetic).

```

`bytes=` is the length in bytes of the body as given, nothing the log adds
counted; because the body is verbatim, a line of it can look exactly like a
header, and a reader must skip `bytes=` bytes after the header line (and the two
newlines) rather than scan for `@@@`. Each entry is handed to the OS whole, in one
`write` on an append-only handle; it could interleave with another process's
only if the OS accepted the write short and a second one were needed. No lock is
taken. Measured on one machine (local NTFS, 16 processes at once, entries up to
1 MB each) that never happened — a measurement, not a guarantee. A log that cannot
be written never fails the tool call or changes its result (one note on stderr, never stdout,
the MCP channel; every later entry is still attempted). There is no reader,
rotation or cleanup: **the file holds real conversation content, banto never
rotates or deletes it, and it must not be committed anywhere.** Brigade ids are
never reissued within one database (`brigades.id` is `AUTOINCREMENT`); a replaced
database starts over at 1 and would append to the old files.

**Codex carries the same server a different way — no config file at all.**
Codex has no `--mcp-config` equivalent; the launch instead adds `-c
mcp_servers.banto.command=...` and `-c mcp_servers.banto.args=[...]`
overrides naming the identical `<banto executable> _mcp --brigade <id>
--member <token> --role <role> [--session <id>]` invocation (structured TOML
fields Codex hands its own process spawner directly, not re-split by a
shell), plus `-c mcp_servers.banto.default_tools_approval_mode="approve"` to
bypass Codex's own per-tool approval prompt for this server — repeated on
every single launch rather than written once to `~/.codex/config.toml`, since a
`-c`-registered server can't persist that trust at all, and (measured
2026-07-28) the bypass degrades silently, no error and no warning, the
moment a launch leaves it off (`crates/banto/src/launch.rs`'s
`CodexBrigade::overrides` and the override functions it calls). This is a
different Codex prompt from the hook-trust one gated in "Brigade" below —
tool approval and hook trust are Codex's own two separate mechanisms, solved
two separate ways here.

The server shares banto's own sqlite store with the TUI process and exposes
seven tools:
- `send_to_peer(text[, to])` — enqueues a message: a Director broadcasts to
  every Worker by default, or a Worker/Goinkyo sends to the Director;
  `to` names one specific member instead — the only way a Director reaches
  a Goinkyo, since a broadcast never does.
- `check_messages()` — pulls the messages addressed to this session's role
  that it hasn't seen yet, wrapped in framing that names them as relayed from
  another AI rather than a direct operator instruction.
- `brigade_status()` — this member's own identity plus a roster of its
  addressable peers, each with what it's doing right now and whether it's
  holding unread mail from this member. Replaces an earlier bare ping-style
  health check, added once dogfooding showed a Director launched with only
  the two message tools and no roster information mostly never used them. A Claude
  peer's state is its live file's own word (`busy`, `waiting`, `idle`), except that
  `shell` prints as `idle (background command running)`. A Codex peer's state is
  read from its rollout's last turn marker: `busy (turn in progress)` when a turn has
  begun and not ended, `idle (turn ended; background work not visible)` when the last
  one ended, `running` when it is alive with no turn record to read, and `not running`
  when it is gone. Its limits: a background command left after a turn is not visible,
  so such a peer reads idle; a pending approval or question is not visible either, so
  such a peer reads busy; Codex never reads `waiting`. The hosted tile can disagree,
  because it reads the pane's own OSC 7501 report first, and that report lives in the
  UI process, which this tool (answered by the separate `_mcp` process) cannot see.
  The relay's nudge reads the same rollout marker, so the nudge and this tool agree.
- `consult_goinkyo(question, my_case, settled, unsettled, blind_spot[,
  their_case][, about])` — Director-only: files a written consultation
  request and creates the Goinkyo's member row (see "Brigade" above for what
  happens to that row next). `about` names the Worker the disagreement is
  with, which makes `their_case` required too; omit both for an impasse with
  no specific Worker. Refuses if a Goinkyo already exists for the brigade —
  only one consults at a time; `dismiss_goinkyo` ends the current one. It is
  never refused for the operator's `goinkyo_consult` setting: that is a briefing
  instruction, not a gate (see "How readily a Director consults the Goinkyo").
- `dismiss_goinkyo()` — Director-only: ends the brigade's active
  consultation by removing the Goinkyo's member row (see "Brigade" above for
  what that triggers). Refuses if no Goinkyo is currently part of the
  brigade.
- `name_member(member, name)` — Director-only: sets, changes or clears (empty
  `name`) a Worker's display name (see "Display names" in "Brigade" above for
  the rule). `member` is the Worker's token; the Director and the Goinkyo
  cannot be named. The result says what it did or why it refused. The tool's
  description tells a Director when it is worth doing — when it gives a Worker
  a standing role — and that the token stays the address: `send_to_peer`'s `to`
  takes tokens only, and a display name given there is refused with the list of
  token-and-name pairs.
- `request_decision()` — Director-only (by the store's live membership, checked
  again in the write's transaction): records that the brigade is waiting on the
  operator's decision, and returns at once. It takes no parameters (a `text` a
  caller still sends is ignored and nothing of it is stored); the Director writes
  the question in its own pane first, and the description and the result say that
  banto shows THAT it is waiting, not the question. It does not block, is not named
  as an ask, and its result claims no bell or notification, true whether
  `request_attention` is on or off; a second call while one is open is a new
  request. See "Decision (2026-11-15)".

What the other tools show of a name, looked up live on every call and never
cached in the server process (so a rename is visible to the very next call):
`brigade_status` adds, per peer, the name in quotes after the token and the
peer's agent product when the store knows it (`worker-3 "impl" (Claude) — idle`),
and the caller's own name to its `You are …` line; `check_messages` headers and
`send_to_peer`'s `Delivered to …` add the name in quotes after the token
(`[from worker-3 "impl" — to you]`).

The text of a message is pulled, never stdin-injected: even though the embedded
banto is the sole writer to a child's stdin, injecting a peer's message there would
forge operator input mid-turn; a tool result respects turn boundaries and carries
the firewall framing for free (`crates/banto/src/mcp.rs` module doc). What banto
does type into an idle member's stdin is the fixed nudge line under "Auto-relay"
below, and that starts a turn, so a message can cost its recipient a turn.

**Verified end to end against real Claude Code** (`docs/notes/mcp-spike.md`,
2026-07-23, follow-up 2026-07-25): `claude --strict-mcp-config --mcp-config
<file> --allowedTools "mcp__banto__banto_ping" -p "..."` round-tripped a real
handshake, tool list, and tool call against `banto _mcp` launched exactly as
production code launches it. The spike's own stated gaps, not glossed over:
multiple concurrent `_mcp` servers under real sqlite contention, and
non-Windows, were both explicitly marked "not yet verified" there. The
2026-07-25 follow-up is what motivated the role-briefing mechanism above and
the ping-to-`brigade_status` rename: a full day of dogfooding produced zero
Director-initiated messages, traced to a member having no idea a brigade
existed in its own context at all — an information gap, not a reliability one.

## Auto-relay

A Director↔Worker exchange over the MCP tools above pulls each message's text
(see above), which means an idle member is not guaranteed to notice unread mail
without a nudge. With the relay in `auto`, a message to an idle recipient
can wake it for a turn over its whole context, even when the message asks for
nothing. The relay holds the nudge back when the relay is `manual`, the pane is
marked for possible unsent input or Claude's reader detects text, a Codex member's
pane is not at its prompt (below), the member has no
hosted pane or an unknown idle state, or its
attempts on the batch are used up. The auto-relay closes that loop: it
observes every live brigade pane's idle/busy status, including panes behind
the currently displayed brigade (the same
`sessions/<pid>.json` live-state read the Activity indicator uses) and unseen
message count, and once a member has been idle for `RELAY_IDLE_STREAK_REQUIRED`
consecutive observation ticks with mail waiting, types a fixed line into its
stdin — `"[banto relay] Your brigade peer sent you a message. Call the
check_messages tool now."` — followed by a submitting Enter roughly 300ms
later (`RELAY_SUBMIT_DELAY`; the delay exists because a chunk boundary can
carry meaning for the embedded PTY — see "The emporium: hosted panes" above). A nudge is
suppressed while the member's pane is focused and has just received forwarded
keystrokes (`RELAY_INPUT_QUIET_PERIOD`), is subject to
a cooldown between repeat nudges to the same member, and gives up after a
capped number of attempts on one unseen batch
(`crates/banto-core/src/engine.rs` relay constants). It also never lands behind a
line banto itself has in a pane: while a typed line (a kickoff sentence, or a
nudge) still waits for its delayed Enter, or an Enter went out into the pane on
this very tick, the pane counts as not idle for the relay, so its idle streak
starts over — otherwise one Enter would submit the two lines as a single message,
and the idle reading would be from before the line.

A time window cannot protect a draft, so the operator's unsent text is
tracked as a fact instead (`EmporiumState::has_unsent_operator_input`): a
pane is marked when banto forwards something that may add text to its input
line — a printable key, a paste, Tab, or Shift+Enter — and unmarked only when
an Enter that reaches the child as a `\r` (plain, Alt, Ctrl) is forwarded, the
pane goes away, or a settled
screen reading recognises an empty Claude input line. The Codex
reader additionally requires the relay to be eligible to nudge on that same
tick before an empty reading clears the mark (below). While a pane is
marked, a nudge to it is suppressed whatever the focus and however long ago
the operator last typed, a nudge's own delayed Enter is dropped if the mark
appeared before it was due, and the status line says the nudge is being held
(re-shown every 30s, never over another message). A kickoff held by the mark is
named on that same line and cycle. A member whose own kickoff is still owed with
nothing typed is not nudged either, for a nudge would be its first turn; the hold
ends when the kickoff is typed or dropped (for a Goinkyo, when its row shows the
operator's own first line).

When a known Codex pane has enabled Win32 input mode (`CSI ? 9001 h`), banto
forwards exactly Shift+Enter and Ctrl+J as Win32 key
press and release records. Both count as adding text, including for the draft
mark and cancellation of a pending relay Enter. The pane's own mode is tracked
until `CSI ? 9001 l`; this is not inferred from another pane. The spawn event
records the launched agent even before a new session is indexed. Its agent field
is optional for older replays; an unconflicted session row or member fact can
also establish identity. Conflicting known identities disable this encoding.
Without that
mode, for non-Codex panes, and for other modifier
combinations, the existing key encoding and input effects apply. In particular,
the legacy Shift+Enter encoding is a line feed, and legacy Ctrl+J does not gain
the special path's text-adding effect.

Every status that names a brigade member — this one, the nudge notice, the
unreadable-input notice and the kickoff notices — names the brigade the way the
operator knows it, by its Director's *current* session title in quotes:
`relay: nudged worker-2 of "Fix the relay draft guard"`. The title is reduced
first — control characters, escape sequences and invisible format characters
(bidi overrides, zero-width joiners) are dropped, an ASCII `"` inside it becomes
`'`, and a title or name that reduces to nothing counts as absent — then cut to one
line of at most 28 display columns (an ellipsis when cut, never between a letter
and its combining mark or inside a flag); when the Director's
session is not known or has no title, the title the brigade was formed under is
used, and only when neither exists the number (`worker-2 of brigade 7`). Members
of several brigades are grouped so each name is said once (`worker-1, worker-3 of
"A" and worker-2 of "B"`). The brigade number is an identifier for agents and for
banto's own plumbing; the operator is never shown it where a title is known.

The nudge notice also says what prompted it — the sender and first line of the
newest message addressed to the nudged member: `relay: nudged director of "Fix
the relay draft guard" ← worker-3: stage 2 is done`. The line is cut to at most
60 display columns (an ellipsis when cut); with no message to quote the notice is
the short form, never an arrow with nothing after it. Only the nudge notice
carries this; how often and how long a status shows is unchanged.

The mark is blunt on purpose — it does not model which keys an agent binds to
clearing its line — so for a Claude pane it is backed by reading the input line off
the pane's own screen (`banto_core::input_line`), at the moment relay decides and
never otherwise, under the fence in `docs/DISCIPLINE.md` §3. The reader recognises
Claude's framed input block (a top border and a full-width rule below, `❯` and a
no-break space on the first row) and answers `Empty`, `HasText` or `Unrecognized`.
The top border, the row above the prompt row, is either the plain full-width rule or
the rule with a label in it: a leading run of rule cells of at least half the pane's
width, one blank, a label region, one blank, and exactly one rule cell in the last
column. The label region starts and ends with a non-blank cell, holds no rule cell,
and may hold single blanks between words and wide characters (their second cells
identified from the terminal model, not inferred from an empty cell), but not two
blanks in a row; a label longer than half the width minus three cells (wide
characters count two), or a label the terminal model lays out wider than the
application reserved (a joined emoji sequence: it fails closed and brings back the
old notice, no worse), reads `Unrecognized`, as does a row of text, a blank or
mostly blank row, a dialog border with its title at the left or in the middle, two
label segments, and anything else. The bottom rule must still be full width (a
labelled bottom row is not an input box), and the label is never part of the text
the reader returns. Its content is not examined, so any short text in that position
reads the same as a name (a hint, a count, a mode name); this is an accepted
consequence, named by both reviewers, and the nudge is protected by the rest of the
reader's conditions (the prompt glyph in column 0 of the cursor row, the full-width
rule below, the cursor inside the block) and by the member being idle and not
waiting. What is measured and what is not: one plain word of 7 cells, a blank on
each side and the single trailing rule cell, at widths 81 and 84, is measured;
blanks between words and wide characters are extension by construction (the one
other session on the measuring machine with a label of this shape has a name with a
space in it; the label is the session name given with `/rename`, free text in any
script, and the second name the operator gave contains wide characters, so a
wide-character label exists on a real pane: on 2026-11-15 the session renamed to
that name showed the label, and its member then idled for two minutes without the
"cannot read the input line" notice, which is due after 60 s
(`INPUT_LINE_UNREADABLE_AFTER`) of an idle, unrecognised reading, so it is observed
to be read; the screen itself was not replayed against the reader, so it is still
not a cell-by-cell measurement); the half-width floor is margin (the label sat in
the last eighth, and half keeps a dialog title at the left or in the middle out).
A blank prompt row with the cursor hidden and not in the empty column is
`Unrecognized`, not `HasText`: when a server-side flag (`tengu_native_cursor`,
cached in the Claude home's `.claude.json`, so off in a home with no cache) is off,
Claude hides the terminal cursor and parks it in the footer or on the prompt row
depending on focus reports, so its position says nothing about the line (measured
2026-11-18, Claude Code 2.1.327, a fresh Claude home, a 40x130 pane). banto also
launches every Claude pane with `CLAUDE_CODE_NATIVE_CURSOR=1`, which the binary
checks before the flag; set on the claude process directly, it kept the cursor
visible at column 2 through focus changes, typing and erasing, and every reading
was right. The rule only guards a pane where that did not take. Not measured: a home
with the flag cached on plus the variable (read from the code to behave the same),
other pane sizes, a modal at the prompt.
`HasText` holds the nudge even with no mark (recalled history is text no keystroke
announced); `Empty` clears the mark, but only `INPUT_ECHO_SETTLE` (3s) after banto
last forwarded input to the pane, because a pane's screen lags its keys (36–108ms
measured on 2026-11-10) and an `Empty` read inside that window is not yet true;
`Unrecognized` leaves the mark in charge. The delayed Enter never reads the screen:
by then the line holds banto's own
nudge sentence. A Claude pane that stays idle yet `Unrecognized` for
`INPUT_LINE_UNREADABLE_AFTER` (60s) is reported once on the status line, since
Claude's layout can change without notice and a reader that stopped recognising it
would look like one that works; waiting (a permission dialog) is not counted. The
same reading lets a retried Goinkyo kickoff press Enter alone when the line still
holds exactly its sentence, and is the second caller that asks it: a Goinkyo
kickoff with no live file to consult, and a Codex Worker kickoff (with the Codex
reader), type only into an `Empty` prompt. A third asks the whole verdict above
before a kickoff is typed, with the reader of the pane's own agent: a kickoff is
held by the mark exactly as a nudge is, and an `Empty` line read after the settle
time releases it as it releases a nudge, so a key that went to a dialog does not
hold a first line forever; an unreadable line leaves the mark in charge.

*Incident (2026-11-15): a renamed session's label in its input-box border.* The
operator kept seeing `relay: cannot read the input line of director of "<brigade>" —
unsent-draft detection is back to keystroke tracking` for a brigade he had only
opened. Measured by replaying a 100-minute recording against the reader: one
Director's pane read `Unrecognized` on essentially every tick of its life, always
for "no full-width rule above the prompt row" and nothing else; stage, focus,
scrolling and size made no difference. That pane's top border was, at width 81, rule
× 71, blank, 7 non-rule non-blank cells, blank, rule × 1 (at width 84: rule × 74,
…): a label right-aligned inside the box's top border. The pane belonged to the only
session in the run whose transcript carries `agent-name` records (the link is by
structure, character count and class, and by record types only; nobody read the
label or the transcript's content). A session with only a `/rename` title
(`custom-title` records) kept the plain border: the other brigade's Director, also
renamed with `/rename`, was always readable. Observed on 2026-11-15, Claude Code
2.1.325: the operator ran `/rename` on a long-lived session whose transcript until
then held only `custom-title` records (it had been renamed under an earlier Claude
Code); the transcript gained `agent-name` records and the label appeared at the top
right of that session's input box at once, and he confirmed by eye that the
originally measured pane has the label there. So the current `/rename` writes
`agent-name` and draws the label, while an earlier version's `/rename` wrote only
`custom-title` and drew none, which is why two Directors, both renamed, differed
(his own explanation, offered before the check: the readable session was long-lived
and had been renamed before Claude's behaviour changed). The notice fires about 61 s
after the member first idles, once per member, whatever the stage; it looked like an
off-stage brigade's only because that is where he was not looking. The reader now
accepts the labelled border (above), so such a member reads `Empty` or `HasText` and
its draft protection works.

**Codex's bounded empty reader.** At the same relay decision, the reader
recognises one prompt shape. The visible
cursor is in column 2 of row R, with no fixed offset from the bottom. Column 0
of R holds a bold, non-dim `›` or `»`; column 1 is blank; columns 2 through 25
hold exactly `Ask Codex to do anything`, each character dim and single-width;
the rest of the row is blank. Rows R−1 and R+1 are entirely blank, both must
exist, and the screen must be at least 27 columns wide, not in synchronized
output, and at scrollback offset zero. No color or OSC status is required. A
mismatch reads `Unrecognized`; blank cells contain only spaces or no-break
spaces, or have no contents, and are neither wide nor wide continuations. The
Codex reader never returns `HasText`. A
recognised `Empty` clears a mark only on a tick otherwise eligible to nudge and
at least 3 seconds after the last forwarded input. Until then the mark holds.
This applies automatically, with no new setting or operator action. The dim
attribute is required and was observed through ConPTY. The prompt glyph and
placeholder are tied to Codex 0.169.0; a future change reads `Unrecognized`, and
the relay then types nothing into that pane (below).

**The Codex gate (approved 2026-11-19).** The relay types a nudge into a Codex
member's pane only when the pane is at its prompt, whether or not the operator's
mark is set: its output has been quiet for `CODEX_KICKOFF_QUIET_PERIOD`; its own
OSC 7501 report is present and not `blocked` (any other state will do, because a
pane that took a turn sits in `done`; the relay's idle reading from the rollout is
still asked separately); and the reader above says `Empty`, with the 3-second
settle after banto's last forwarded input. It is the kickoff's own test
(`codex_kickoff_at_prompt`) with the report condition widened from `idle`. The
screen is read only on a tick that would nudge otherwise. Why: on Codex 0.169.0 the
pane's report says `idle` while a picker (`/permissions`, `/model`, `/statusline`,
`/title`) is open, and the nudge's `k` moved the selection and its delayed Enter
applied it — Read Only permissions, another model, a rewritten `config.toml`.
A pane that is not at its prompt is never typed into, and there is no fallback to
typing blind. No attempt on the batch is spent while it is held, and the nudge goes
out on the tick the prompt reads `Empty` again. The hold's clock starts on the
first held tick and ends only when the pane reads at its prompt or the member has
no unseen mail left; a tick on which the member is merely not eligible to nudge
(busy that tick, the operator's keystroke quiet window after typing into the
picker) neither restarts it nor ends it. After `INPUT_LINE_UNREADABLE_AFTER`
(60 s) of being held the operator is told, once and then on the status reminder
cycle: `relay: <member>: not at its prompt — answer what the pane shows; its mail
waits`. That held mail also stops counting as on the way for the brigade-quiet bell
("Mail on the way"), as it now waits on the operator. A pane carrying the operator's
unsent mark that is not at its prompt keeps the existing "unsent input in the pane"
wording instead, and keeps holding the bell as any draft does. Claude members are unchanged.

The reading is evidence of a painted shape, not a guarantee that Codex has no
draft. Unmarked text recalled from history or restored reads `Unrecognized`, so the
relay does not type over it. An attachment-only composer can
look empty to this reader, so a nudge can be sent with images attached. A draft
held by Codex outside the composer is not observable here; whether a restored
startup draft can coexist with this placeholder is unverified. A mark over an
unrecognised shape still holds until a later recognised empty reading or an Enter
reaches the pane. Claude's reader can additionally detect `HasText` without a
mark; Codex's cannot.

**Known limit (an observation of 2026-11-14, found by reading and by a test that
tripped on it, not seen in the field):** a nudge's delayed submitting Enter that is
still queued when its pane is relaunched is written to the new process, and counts
as work given to it. It needs a nudge written less than 300 ms
(`RELAY_SUBMIT_DELAY`) before the pane exits.

`[brigade].relay` in config.toml (`RelayMode`, default `Auto`) toggles this
off (`Manual`) for an operator who would rather prompt `check_messages`
themselves.

## Activity indicator

1. `sessions/<pid>.json` exists, PID alive, and `status=busy` → **busy**
   (special color, highest priority)
2. `sessions/<pid>.json` exists, PID alive, and `status=waiting` → **waiting**
   (human input is required, whatever `waitingFor` says; only the bell tells a
   dialog apart). A hosted Codex pane that reports `blocked` (OSC 7501)
   is shown as waiting too, and one that reports `working` as busy — laid over the
   row for as long as the report holds ("Codex data sources")
3. `sessions/<pid>.json` exists, PID alive, and `status=shell` (the turn ended with
   a background command still running) → **busy** in the list, `banto list`, and
   through the list the counts and marks (a tile says `idle` with a mark: "The
   emporium: hosted panes"); a waiting entry outranks it, a person being needed
4. PID alive with any other status → **alive** (`idle` itself, and unknown upstream
   statuses, which deliberately degrade here): the list's word for a session whose
   process is up and doing nothing
5. Otherwise bucket by jsonl mtime: today / this week / older
   (thresholds and colors configurable)

This is the display of the session list (the list row and its dot, `banto list`'s
tags, and through the display decision `displayed_activity` the counts and marks;
`banto_core::model::Activity`). A tile's word is a separate decision, `pane_word`,
which says busy, waiting or idle and never "alive" ("The emporium: hosted panes",
the strip and title paragraph). One more step applies to the list for the sessions
banto itself hosts: a row of such a session whose own activity says there is no live
process (step 5, dormant) is shown as live (`Alive`) while the pane lasts
(`App::set_hosted_ids`), since the process is the one banto started; the Codex check
of the logs database runs only on a list reload, which a pane at its prompt never
causes. A busy or waiting activity stands, a reload does not lose it, and ending the
pane gives the row its own word back; the rows of sessions not hosted here, and
`banto list`, are unchanged. The stricter question "is this session known to be doing
nothing?" — asked by the quit confirmation and the brigade bell (see "The emporium:
hosted panes") — reads the same file the other way round: only `idle` is idle,
`shell` is not (it is told apart as *background*, not idle for that one reason
only), and an unrecognised status is not either, so the list's drawing and the
decision still agree about `shell`; a tile says the same fact in more words (`idle`
with the mark `bg`) and the quit prompt names the command ("Is it idle? (the quit
question)"). (Why `shell` shows busy in the list: the operator
restarted banto without knowing a command was still running in a pane that said
"alive".) The relay, which asks whether a member can be nudged, still treats `shell`
as idle enough: the turn has ended and the member can take a message; it never
nudges a `waiting` session, `dialog open` included, since a nudge's Enter would
choose something in the dialog. The same classification without its last step, the
transcript's age (`live_activity`), is what the marks, the count and the bell read
for a member with no list row, and what its tile reads through the shell's sample: a
Claude Worker that has not run a turn has a live file and no transcript, so it is
read from the file as the relay read it this tick, and goes with the tick that does
not see it. A Director's row is the one place a brigade's attention shows in the
list: its dot reads Waiting whatever its own state while its brigade is marked
("Attention"); the row's activity itself, which the tile's word and the decisions
read, is untouched. The raw status stays visible in one place: the `brigade_status`
MCP tool prints the live file's own word (`busy`, `waiting`, `idle`) for a Claude
peer, and for `shell` `idle (background command running)`, since the bare word told
a Director nothing and `idle` alone would say nothing is running.

Watch `projects/` and `sessions/` with `notify` for realtime updates — plus,
when a Codex home resolves, its rollout tree and filtered name-index events
(see "Codex data sources" above).

## Stack

Rust workspace (edition 2024). ratatui + ratatui-core + crossterm / nucleo /
rusqlite(bundled) / notify / vt100 (VT emulation for the emporium's embedded
panes) / portable-pty (the embedded PTY host) / unicode-width / serde,
serde_json, toml / clap / dirs / sysinfo (PID liveness) / thiserror, anyhow.
Unix builds additionally depend on `libc`.

## Phases

1. Indexer + search + TUI list (mouse support) — useful on its own — done
2. Opener (psmux / WT) + `_wrap` + double-resume prevention + focus — done;
   the opener and `_wrap` were retired 2026-11-13 (below), double-resume
   prevention stays
3. Activity dots + notify live updates — done
4. Groups / pins — done
5. In-place resume as the default action (Enter hands off banto's own
   terminal directly; `s` still splits into a psmux pane / WT tab) — done;
   retired 2026-11-13 (below)
6. The emporium (2026-07-22 decision above): embedded multiplexer, brigade
   Director/Worker cells, MCP mediation, auto-relay (see "The emporium: hosted
   panes" / "Brigade" / "MCP mediation server" / "Auto-relay" above) — done. Its
   architecture was formalized separately as `docs/DISCIPLINE.md`'s TEA /
   sans-IO discipline — a five-phase migration (Phase 0 through Phase 4;
   Phases 2 and 3 each landed as two commits), all completed 2026-07-24
   through 2026-07-25 (`docs/DISCIPLINE.md` §10) — done.
7. One UI (2026-11-13 decision above): the chōba as a mode, in-place resume,
   the external openers and `_wrap` retired; the list and the hosted panes are
   the only UI.

Delivered alongside groups: a new-session modal (`n`), session archiving
(`d`, soft-hide only — the real session file on disk is never touched), and
an always-visible summary panel below the list.

## Risks

- JSONL format changes → contained by lenient parsing + fixtures

(The Windows Terminal tab-focus and psmux-compatibility risks recorded here
until 2026-11-13 left with the external openers.)
