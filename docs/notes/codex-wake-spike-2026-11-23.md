# Waking, starting and stopping a Codex pane without typing into it (2026-11-23)

banto starts a Codex turn today by typing a line into the pane and pressing Enter
(the relay's nudge, and until now the first line of a new member). This note
records what was run and measured to see whether Codex has a way in that types
nothing: a startup prompt on the command line, `codex queue` for a session that
is already running, and a way to stop a turn. It states what happened and copies
no rollout text, no log lines, no session ids and no paths.

Versions: **codex-cli 0.171.0**, Windows 11. Model `gpt-6-luna`, effort `low`
(the lightest non-legacy model in `codex debug models --bundled`).

Each fact is marked **measured** (observed on a machine, in this work) or **read
from source** (taken from the code, `--help` or Codex's documentation, not
observed here). The source was a shallow clone of openai/codex `main` read the
same day; that it matches the 0.171.0 tag was not checked. The documentation is
the developer-commands, hooks, remote-connections and non-interactive-mode pages
of learn.chatgpt.com.

## How the panes were run

All **measured** runs drove `codex` in a ConPTY through the repository's `ptydrv`
example (`crates/banto/examples/`), with banto's own argument shape for a new
Codex session: `-m`, the three notification `-c` overrides and
`-c model_reasoning_effort=...`. No banto binary was used, so none of these panes
had banto's MCP server or its `_hook`. Time spans are seconds from the event
named.

Practice for the next run: close a pane with the driver's hang-up, never a kill;
stop an `app-server` the run started (the `--listen` ones below) when it is done,
and leave a shared daemon and any other `codex.exe` already running alone, since
other sessions may be using them.

Two launch shapes matter for everything below (**measured**, from `/daemon` inside
the TUI):

- A plain `codex -m <model>` connects to the shared local app-server daemon
  ("Running daemon: v0.171.0").
- Any non-boolean `-c` override makes the TUI run its own in-process app-server
  ("Not connected to the local background server."). banto always passes such
  overrides, so a banto-hosted pane is the second kind. **Read from source:**
  `daemon_startup.rs` excludes the daemon for command-line overrides other than
  boolean `features.*`.

## A startup prompt on the command line

`codex [flags] "<prompt>"` starts the first turn with nothing typed.

- **Measured, no dialog:** the TUI stays interactive and the prompt shows as an
  ordinary `>` line with its answer; about 4 s from launch to the answer on
  screen. Codex creates the rollout file about 1 s after launch.
- **Measured, folder-trust dialog** ("Trust this folder?", shown for a directory
  Codex has not seen): the prompt is not on screen while the dialog is up; it
  stays unprinted until the dialog is answered, then runs as the first turn, not
  lost. About 4 s from the answer to the response. Codex itself appended a
  `[projects.'<dir>']` section with `trust_level = "trusted"` to `config.toml`
  (two lines); nothing was written by hand.
- **Measured, hook-review dialog** (an untrusted hook given with `-c
  hooks.SessionStart=...`: "Hooks need review", with "Continue without trusting
  (hooks won't run)"): same behaviour. Choosing "Continue without trusting" runs
  the prompt as the first turn and runs no hook; `config.toml` did not change.
  "Trust all and continue" was not tried.
- **Measured, newlines and length:** a prompt of four LFs (one blank line), one of
  2,036 characters in 29 lines and one of 19,996 characters all reached the
  rollout's user message character for character, and were answered; the
  20 KB one was not shown as a collapsed paste and took about 4 s. These went
  through Windows `CreateProcess` via `portable-pty`. Not measured: the limit
  (Windows allows 32,767 characters on a command line, from memory, not checked
  here), non-ASCII text, and a Unix `argv`.
- **Measured, resume:** `codex resume <id> [flags] -C <cwd> "<prompt>"` on a
  thread that had run a turn appended a new turn to the same thread (the rollout
  gained a `task_started` and a user message), about 3 s to the answer.
- **Measured, a thread that never ran a turn cannot be resumed:** `codex resume
  <id>` on a session that had been started and closed at its idle screen printed
  "No saved session found with ID ..." and exited 1 after about 1.3 s. Codex
  writes the rollout at the first turn, not at launch (no rollout file 8 s after
  launch), so there is nothing to resume.

### `SessionStart` on that first turn

**Measured**, with a trusted hook: it fires on the first positional turn. It
was called about 1.4 s after that turn's `task_started` record, before the model's
reply, with `source: "startup"`. The turn was not delayed or altered and the
prompt arrived whole.

- **Measured:** "Continue without trusting" disables only the hooks that are not
  trusted; a hook trusted earlier still ran in the same launch. An untrusted hook
  does not stop the turn, which runs behind the review screen even if that screen
  is left open; the hook just does not run.
- **Measured:** `-c hooks.state."<key>".trusted_hash="sha256:..."` on the command
  line did **not** remove the review dialog (two hooks stayed "new or changed").
  Only trust saved in `config.toml` counted. So at 0.171.0 a hook's trust cannot
  be given on the command line; `default_tools_approval_mode` for MCP tool calls
  is a different setting and was not part of this.
- **Measured:** the `hooks.state` key is `<layer>:<event>:<group index>:<handler
  index>`, so trust follows the hook's position. A hook given by `-c` that sat at
  `session_start:0:0` read as `modified` against an already saved entry at the
  same key; "Trust all" there could have replaced that entry's hash (not tried),
  which would put the review dialog back on the next launch of whatever that entry
  belonged to. The spike put its hook in a second `SessionStart` group (`1:0`) and
  trusted only that one, with the review screen's single-hook "t trust"; the
  existing entry's hash was unchanged afterwards.

### The `.cmd` shim

Measured with a hand-made forwarding shim (`"...\codex.exe" %*`) started through
`cmd /c`, because Codex on the test machine is `codex.exe`, not a shim.

- A prompt with square brackets, and `& | ^ % ( ) < >` all **inside** the outer
  quotes, arrived whole, and so did the quotes in `-c` values.
- A prompt with an embedded `"` followed by `&` was cut: Codex received it up to
  the `"`. `cmd` toggles quoting on the embedded quote, so what follows is parsed
  as `cmd` syntax. The cut is measured; whether the rest then ran as a command was
  not (the pane was closed before any output could show it).
- Not measured: npm's real shim, a `.ps1` shim.

## `codex queue`

`codex queue --thread <id> --message "<text>"` (JSON-RPC `thread/queue/add`)
puts a text message on a session's queue from another process.

**Read from source** (`ext/queue/src/service.rs`, `tui/src/session_queue_commands.rs`):
the process holding the thread checks SQLite's `data_version` every 10 s and,
for a thread that is loaded and idle, starts a turn with
`turn_trigger = "queue"`; the same dispatch also runs the moment a turn ends. A
thread that is running, interrupted, shut down or not found is skipped, and one
interrupted by the user is not drained automatically. The queue holds at most
100 messages, text only. `thread/queue/add` is marked experimental in the
protocol; `codex queue` itself has no feature flag. With a daemon running,
`codex queue` refuses to go through an embedded app-server unless `--remote` is
given; this was not observed to happen.

### What it does (**measured**, an idle pane, no key typed)

- It starts a turn in a pane of either kind: the daemon-connected one and the
  in-process one that banto's overrides give. `codex queue` returns in 0.14-0.19 s
  with "Queued message ... for thread ...". A shared daemon was running on the
  test machine, and the in-process TUI picked the message up on the 10 s beat
  below, which fits the SQLite route read from source (the route itself was not
  traced).
- The screen shows no queue marker: the message appears as an ordinary
  `> <text>` line followed by its answer. (Judged from the lines that were
  extracted, not from every frame.)
- In the rollout the turn's `task_started` carries `turn_trigger: "queue"`, where
  a typed one carries `"user"`. That is the difference seen; no other was. The
  user message has a `client_id` either way, and for a queued one it is **not**
  the queue id the command prints (checked in full on three, and on the leading
  part on eight more): it is a different UUID whose timestamp part is 1-24 ms
  earlier. It cannot be used to match a message to its queue id; the text of the
  message can.
- **Delay.** Daemon-connected: 0.46 s (one run). In-process, five runs in one
  session: 3.9, 10.0, 4.9, 0.9 and 2.0 s, and two other in-process runs gave 3.4
  and 4.5 s. The five `task_started` stamps fall on a 10 s grid (the same
  fractional second each time); the 10.0 s run missed the previous beat by 0.04 s
  and waited a full one. The delay is therefore uniform over 0-10 s plus about
  0.1 s to start the turn; no run exceeded it, and five runs do not prove a bound.
  Not measured: a machine with no daemon, or banto's panes under load.
- **Whether it was received.** Queue's own output means queued, nothing more.
  Received is a `task_started` with `turn_trigger: "queue"` in the rollout whose
  user message has the queued text. Nothing on the screen says it. Codex's queue
  database also shows it: the row is there while the message waits and gone once
  the turn has started (read from a copy of the database; seen on every queue
  that flowed, below).
- It leaves no process behind (`codex queue` exits in about 0.2 s) and no file of
  its own; the existing Codex state, queue, thread-history and log databases and
  the history and session index files were updated.

### When it does not simply run (**measured**)

- **While a turn is running:** it waits and starts when that turn finishes. A
  message queued 6 s into a 31 s turn started under a second after the turn
  completed. The screen showed nothing about it while waiting (the "Working ..."
  line only).
- **With a draft in the input line:** the draft stayed. A half-typed line
  (no Enter) was still in the input after the queued turn ran; it was not sent
  and did not enter the answer.
- **Before the first turn:** refused. `Error: failed to queue session message:
  thread/queue/add failed: failed to read thread: ... no rollout found for thread
  id ...`. The same id queues once its first turn has written the rollout.
  **A refused queue queues nothing** (measured, second round): the command exits
  1 in 0.2-0.35 s, for an id whose pane has not had a turn and for an id that does
  not exist at all (a well-formed UUID and a name each: the name says "No active
  session found matching"). The queue database had no row for either, and after a
  human turn on the pane that had been refused, and 40 s more, no queue-triggered
  turn appeared. The refusal leaves nothing that could flow later.
- **After an Esc interrupt, and after `turn/interrupt`:** nothing runs. A queue
  made after Esc (and one made after an interrupt over the app-server) was still
  waiting 21-30 s later. Once a **human** turn completed, the queued turn(s) ran.
  Three messages queued after one Esc ran as three separate turns in the order
  queued, 2-4 s apart, after the human turn; they are never merged. Several
  nudges queued behind an interrupted pane therefore each become a turn.
- **After a hook stop** (below): runs at once. The stop ends the turn normally,
  not as an abort, and a queue made then started in under 0.5 s.
- **Not measured:** images (not accepted by the command), several-line or very
  long messages, the daemon-connected pane with a working turn, a draft, or an
  interrupt.

### When the turn starts: the second round (**measured**)

In-process panes with banto's argument shape, nothing typed to start a queued turn
(keys were typed only to start the human turns and to answer dialogs). Times are
from the stamps in the rollout and the driver's log. Three sets, each with its own
cause.

**After an Esc, then a human turn (3 runs).** A long no-tool answer was stopped
with Esc about 6 s in, one message was queued 2.5 s later, and the pane was left
alone for 13 s (longer than the 10 s beat): nothing ran, and the queue database
still held the row. A human turn then ran and the queued turn's `task_started`
followed its `task_complete` by 11-13 ms in all three runs. So the held message is
not waiting for a beat: it goes the moment the human turn ends. A human turn that
is the one thing the operator typed after an interrupt is therefore followed at
once by every nudge that piled up behind it.

**While a start-up dialog is up (1 run each).** A resumed thread was opened in a
git directory Codex had not seen (folder trust) and in another with an untrusted
hook given by `-c` (hook review). The queue was accepted (exit 0, a row in the
database) 13 s after launch in both. Nothing ran for the 29-43 s the dialog stayed
unanswered, which is several beats. The queued turn started 0.36 s (folder trust,
"Trust and continue") and 0.28 s (hook review, "Continue without trusting") after
the answer, which is not a beat time. Nothing vanished, and the row was gone
afterwards. Answering folder trust made Codex write the trust section to
`config.toml` as before; "Continue without trusting" wrote nothing. Both sections
were removed after, and `config.toml` is byte-identical to its state before the
run. Not measured: "Trust all" and "Review hooks".
**Inference:** the thread is not loaded until the dialog is answered, and loading
it triggers a dispatch.

**To a thread no pane has loaded (`codex resume <id>`, embedded).**
- Queue made within 0.15 s of the resume being spawned (2 runs): started 1.64 s and 1.55 s
  after the spawn.
- Queue made with **no pane holding the thread at all** (the command exits 0 and
  the row stays 22 s), then `codex resume` (1 run): started 1.54 s after the spawn
  and the row went.
- Queue made 7, 9 and 10 s after the spawn, in a resumed pane that had settled
  and had nothing typed (3 runs): started 10.20, 10.18 and 10.17 s after the spawn,
  whatever time within that window the queue was made (3.2, 1.2 and 0.2 s after
  it). That is the first 10 s beat, anchored at the process start; the earlier
  five-run spread is the same beat seen from different queue times.
- **Inference:** a message already queued when the thread loads is dispatched
  on load, about 1.5 s after launch; one queued after the load waits for the next
  beat. Either way nothing has to be typed. Not measured: whether the first beat
  is at the same 10 s on a loaded machine.

### With a `/model` or `/permissions` picker open (**measured**, 1 run each)

An in-process pane resumed on a thread that had run turns, left idle for 14 s, then
`/model` (and Enter) typed through the driver and left open, the cursor on the
current model. `codex queue` was sent with nothing else typed.

- **`/model`:** the queue was accepted, and the turn started 5.6 s later on the
  first beat (the queue was made 24 s after the spawn, the turn started at the 30.2
  s beat, which is the same 10.2 s grid as above). The picker stayed open and
  unchanged: same list, cursor still on the current model, "enter select, esc
  back". The queued message and its answer were printed into the transcript above
  the picker. Watched for 32 s; the picker never closed or moved.
- **`/permissions`:** the same. The turn started 1.0 s after the queue (it fell
  on a beat), the picker stayed with its cursor on the current setting, and the
  message and answer appeared above it.
- **Nothing changed.** The turn context of the queued turns shows the same model,
  effort and approval policy as the turns before (read from the rollout), the
  pickers showed the same "(current)" entry, and `config.toml` is byte-identical
  to its state before the run (hash compared). The queue's own turn is not a key
  and does not pick anything.
- **Esc afterwards:** closed each picker as it does without a queue and left the
  idle input line, with no further turn and no row left in the queue database.
  Nothing was selected, so what Enter does after a queued turn started was not
  tried (it would change the setting).
- Not measured: a picker opened while a turn is already running; a picker that is
  open when the queued turn *finishes* a long turn; pickers other than these two
  (for example a resume-session picker or a plugin list); a draft in the line
  behind the picker; the daemon-connected pane; whether the queued turn's
  approval prompts (a tool needing approval) would be answered by a key meant for
  the picker. The last is the one that matters for a nudge that also types: it was
  not tried, because the queued message ran no tool.

**A directory with no git repository showed no folder-trust dialog** on a new
session and on a resume (one run each, no trust section written), in line with
Codex writing no trust key there.

## Stopping a turn

`codex --help` lists no stop or interrupt command (**read from source**/`--help`:
`queue`, `app-server`, `remote-control`, `app-server proxy`). Two ways were run.

### `turn/interrupt` over an app-server (**measured**)

The turn must be running in an app-server that listens. Setup: `codex app-server
--listen unix://<sock>` started separately, the TUI attached with `codex --remote
unix://<sock>`, and a client talking JSON-RPC through `codex app-server proxy
--sock <sock>`, which is a raw byte bridge to a WebSocket: the client must speak the
HTTP upgrade and the WebSocket framing itself, then `initialize`, `initialized`,
`turn/interrupt` with `threadId` and `turnId`. No token was needed. The socket
directory must be private to the user, and on Windows that took a directory whose
inherited permissions were removed.

- During a tool call (a 120 s sleep): the reply came 0.29 s after the request and
  the rollout recorded `turn_aborted` 0.22 s after it. The TUI showed the same
  "Conversation interrupted" line as Esc and left the "1 background terminal
  running" note, as Esc does.
- Mid-generation (counting up to 800): stopped with the screen at 629 and nothing
  after it; the reply took 27 ms. A first attempt sent while the model was still
  thinking was answered in 24 ms with an abort. Whether the partial text is in the
  rollout was not checked.
- The queue behaves after it as after Esc (above).
- **Reach, not measured:** it reaches only the app-server that runs the thread.
  A banto-hosted pane runs it in-process (**read from source**: `in_process.rs`),
  with no socket to connect to. Sending `turn/interrupt` to such a pane from
  outside was not tried, to avoid touching the shared daemon, and is expected not
  to work. Hosting every pane against one banto-run app-server would reach them,
  but how `-c` overrides, `-C`, hooks and MCP settings behave in a `--remote`
  TUI was not tested, and `app-server` is marked experimental.

### A `PostToolUse` hook returning `continue:false` (**measured**)

An in-process pane with banto's argument shape and a trusted `PostToolUse` hook
that returns `{"continue":false,"stopReason":"..."}` when a flag file exists, and
`{}` otherwise. Asked to run three 12 s sleeps as three tool calls, with the flag
created during the second:

- The hook ran when the second call finished and the TUI printed "Hook stopped"
  with the reason; the third call did not run; the model answered in a few words
  and the turn closed as `task_complete`, not `turn_aborted`. About 12 s passed
  from creating the flag to the stop, about 10 s of it waiting for the running
  tool.
- **Read from source:** `PreToolUse` and `PermissionRequest` treat `continue:false`
  as unsupported (no stop); `PostToolUse`, `UserPromptSubmit` and `Stop` honour it.
  `UserPromptSubmit` refuses the prompt before the turn starts.
- **Limit, read not measured:** it can only stop at a tool boundary. A model
  writing text with no tool call, or one tool running a long time, is not stopped
  until the next hook point. Every tool call also starts the hook process.

### Other candidates (**read from source**, none was run)

A `Stop` hook that blocks until an outside line arrives (the line becomes the next
prompt; only works at the end of a turn, and what happens at its timeout was not
read); goals (`thread/goal/set`, a fixed continuation prompt, not a free line);
`codex exec resume` (a second process on the same session, with the double-resume
risk of invariant 4 of `CLAUDE.md`); `turn/steer` (exists in the protocol, its
handler was not read); MCP push notifications or sampling (none found in the
documentation or `core/src`; the `codex-mcp` crate was not read); `codex
remote-control` and mobile (tied to the ChatGPT apps).

## What a run like this leaves behind

All three are **measured**.

- **Trust is written into Codex's `config.toml`.** Answering a folder-trust
  dialog with "trust" makes Codex append a `[projects.'<dir>']` section with
  `trust_level = "trusted"`. Answering a hook-review screen with "t trust" makes it
  write a `trusted_hash` under `hooks.state`. Nothing has to be written by hand,
  and nothing is undone when the pane closes. The `hooks.state` key is the hook's
  position (event, group index, handler index), not its content, so a hook given by
  `-c` and placed where a saved hook already sits is judged `modified`, and trusting
  it can replace the saved hash. Put a hook under test in a group of its own, and
  delete the sections the run wrote when it is over.
- **A `--remote` TUI takes the working directory of the server process.** It does
  not take the `--cwd` given to `ptydrv`, so the session is listed as a session of
  the directory the `app-server` was started in.
- **The sessions are real.** Each run that took a turn is in the agent's own
  session list, under the directory it ran in.

## OSC 7501 around a startup prompt (**measured**)

The `codex` pane reports its state with OSC 7501. Times are from launch.

- No dialog: no report for 0.9-2.0 s. Then `idle` at 0.9 s and `working` 0.17 s
  later (one run), or `working` directly at 1.1 s and 2.0 s (two runs, with the
  long prompts), then `done` when the turn ended.
- During the folder-trust dialog: no report at all for 17.8 s; `working` only
  after the answer; no `idle`. During the hook-review dialog: none until
  `working` at 14.5 s, after the answer.
- Idle then queue: `idle` while waiting; `working` 0.17 s after the queued turn
  started.
- So the silence before the first `working` is the dialog or the 0.9-2 s start,
  and reads the same as a stopped pane. Not measured: the reports of panes with a
  trusted hook (their raw logs were still being written when read).

## Not measured, in one place

- `codex queue` on a machine with no daemon; with images, multi-line or very long
  text; `turn/interrupt` from outside to an in-process pane; any of the queue's
  wait cases (an interrupt, a dialog, an unloaded thread) in a daemon-connected
  pane.
- The queue with a dialog answered "Trust all" or "Review hooks"; the first beat
  on a loaded machine; the same cases with banto's own MCP server and hook in the
  pane.
- Each of the second-round timings is 1-3 runs; none is a bound. The picker runs
  (a `/model` and a `/permissions` picker) are one each; see that section for what
  they leave out.
- A positional prompt with "Trust all and continue", with MCP servers in `-c` (the
  MCP trust dialog did not appear in a hooks-only configuration), with non-ASCII
  text, on Unix, beyond 20 KB, or through npm's real `.cmd` shim.
- Whether a partial answer is kept in the rollout after an interrupt; stopping
  a turn that calls no tool; the delay under a loaded banto.
- The "1 warning" notice shown in every spike pane from launch.
