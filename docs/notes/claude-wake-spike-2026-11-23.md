# Waking, starting and stopping a Claude pane without typing into it (2026-11-23)

banto starts a Claude turn today by typing a line into the pane and pressing
Enter (the relay's nudge, and the first line of a new member), and it stops a
turn by sending Esc. This note records four rounds of spikes that looked for
ways in that type nothing: a hook that wakes an idle session, a prompt on the
command line, a hook that stops a turn, and a hook that makes a working model
read its mail. It states what happened and copies no transcript text, no log
lines, no session ids, no paths and no user names.

Versions: **Claude Code 2.1.330**, Windows 11 (native, not WSL). Models `haiku`
throughout, with `sonnet` and `opus` where a row says so. No effort was passed, so
each model ran at its own default (haiku and sonnet "high", opus "xhigh").

Each fact is marked **measured** (observed on a machine, in this work) or **read
from the docs** (taken from Claude Code's hooks and CLI documentation, fetched the
same day, not observed here). Where the two disagree or only one exists, the row
says so. Anything not marked was not checked.

The spikes were run by a worker, not the operator. The scratch scripts (a fake
MCP server, settings generators, transcript summarisers) are not in the
repository; what they did is described here.

## How the panes were run

All **measured** runs drove `claude` in a ConPTY through the repository's `ptydrv`
example (`crates/banto/examples/`), 120 columns by 40 rows, with
`CLAUDE_CODE_NATIVE_CURSOR=1`. No banto binary was used and every `BANTO_*`
variable was removed, so none of these panes had banto's real `_mcp` or `_hook`.
Hooks were given only through `--settings <file>`, never in a user or project
settings file. The account's default permission mode was `auto`, and no MCP
approval dialog appeared in any run that did not ask for another mode.

- **The fake banto MCP.** A small stdio JSON-RPC server named `banto`, so its tool
  was `mcp__banto__check_messages`, passed with `--mcp-config <file>` as banto
  does. Its `tools/list` carried `_meta: {"anthropic/alwaysLoad": true}`, the same
  as banto's tools (`crates/banto/src/mcp.rs`). It returned the files in an inbox
  directory, one message each, and deleted them; an empty inbox gave "No new
  messages". Later rounds added a slow-wait tool and a gate tool (below).
- **The instructions.** From `DEFAULT_DIRECTOR_PROMPT` and `DEFAULT_WORKER_PROMPT`
  in `crates/banto-core/src/config.rs` at HEAD (a210870), extracted mechanically
  (Rust escapes undone, nothing edited) with the placeholders filled, and given to
  `--append-system-prompt`. Their wording is not copied here.
- **The nudge sentence** a woken model was shown is the one-line text banto's relay
  types. It is not copied here either.
- **What decided each result.** Almost every verdict rests on the transcript
  JSONL (the `origin` of a user row, the `tool_use` names, the row order) and on
  timestamps written by the hook or the fake MCP server, not on the screen. Where a
  result rests on the screen alone, the row says so.
- **A fresh folder shows a trust dialog** whose default answer is "No, exit". The
  driver moved down one entry before Enter. Enter alone quits Claude.

Practice for the next run: close a pane with the driver's hang-up, never a kill,
and only after the pane is at least 10 s old (see the last section).

## Waking an idle session with an `asyncRewake` hook

A hook of type `command` with `asyncRewake: true` runs in the background; when it
exits with code 2, Claude Code starts a new turn whose input is the hook's stderr.
The hooks here waited for a flag file, put one message in the fake inbox, wrote
the nudge sentence to stderr and exited 2.

### That it works, and how fast (**measured**)

- **It wakes an idle pane with nothing typed.** Three runs on the first round
  (two armed on `Stop`, one on `SessionStart`): the new turn's API request
  went out 83-126 ms after the flag file was created. The hook noticed the flag in
  15-60 ms, and the transcript's user row followed within about 40-110 ms. The
  screen (sampled every 500 ms) showed the wake 0.3 s after the flag.
- **What the screen shows.** A "Stop hook feedback" line, then the model's reply. It
  does not appear as a `❯` line of the operator's own.
- **What the transcript records.** Two `queue-operation` rows, then a `user` row
  whose `origin` is `{kind: 'task-notification', producer: 'session-task'}`. A
  typed message has `origin: {kind: 'human'}`, so the two are told apart by
  `origin`. The body is a `<task-notification>` wrapper holding a system reminder
  that names the hook event (`Stop hook blocking error from command "<event>"`)
  followed by the hook's stderr verbatim, trailing newline included. The first
  bytes of what the model receives, read from the API-request record, mark it as
  an automated background-task event and not a user message; the rest of it was
  not read. This is the `origin` banto could use to see that a wake happened.
- **Stderr must be an instruction.** With stderr that was only a timestamp, Haiku
  answered that there was nothing to do, every time (six or more runs).
- **Exit code 0 wakes nothing.** The hook ended, and no turn, row or screen change
  followed.

### Whether the woken model reads its mail (**measured**)

This is the question the rest depends on. With the nudge sentence as stderr and the
real Worker and Director instructions: **15 of 15 woken models called
`mcp__banto__check_messages`.** The 15 are the wakes of the second round: haiku,
sonnet and opus, two runs each with the Worker text and one each with the Director
text (nine), and the wakes of the exec-form, Esc, timeout and draft rows below. None
ignored it, treated it as an error or began investigating
instead.

- The call came 1.3-4.5 s after the flag (haiku about 1.9 s, sonnet 1.3-4.5 s, opus
  about 3.6-3.7 s).
- After reading, the Workers answered that nothing was asked of them and did not
  send a reply, which is what the Worker text says to do for a message that asks
  for nothing. The Directors reported that the message was unclear and asked for
  nothing. One opus Director also listed the working directory once.
- A fake message signed with the Worker's own token was pointed out by sonnet and
  opus as "from myself". That is an artefact of the fake, not of the wake.
- When the fake inbox held a real request (300 words of text), one haiku Worker
  called `check_messages`, then searched four times for a `send_to_peer` the fake
  server did not have, and answered that it could not reach the Director. The
  reply path with a real `send_to_peer` was not measured.

### Which event arms it (**measured**)

- **Armed on `Stop`.** A hook armed at the end of every turn doubles up: if the
  previous waiter is still alive when the next turn ends, two are waiting. With
  one flag, both exited 2 and the woken turn processed two notification rows in a
  row, then each of those turns ended and armed two more.
- **A flag left in place loops.** The woken turn's `Stop` armed a waiter that saw the
  flag at once and woke the pane again; three wakes in nine seconds before the flag
  was removed by hand. Each lap is a model round trip.
- **Armed on `SessionStart` only.** One wake, then nothing re-arms: a second flag eight
  seconds later did nothing (no new hook start, no new transcript row).
- **Both together with a lock file** (a late waiter that finds a live one exits 0)
  was the shape used for the second round's wakes, and the survival after Esc seen
  below fits it. That it holds up over a long session and many wakes was not
  measured.
- **Reading, not measured:** a design that wants to wake repeatedly needs the hook to
  consume its own flag atomically and be re-armed by something; the single-armed and
  double-armed shapes above are the two failure modes seen.

### Shell, exec form and the process chain (**measured**)

- **A shell-form hook starts on native Windows** under Git Bash: the chain was the
  interpreter under two `bash.exe` under `claude.exe`, not `cmd`. A command string
  with forward slashes ran as written. `MSYSTEM` was set in that shell.
- **An exec-form hook** (`"command": "<absolute path to python.exe>", "args": [...]`)
  waited, exited 2 and woke exactly the same, with no shell in the chain
  (interpreter directly under `claude.exe`, `MSYSTEM` unset).
- **A bare executable name** (`"command": "python"`, `args` given) resolved through
  `PATH` and worked, and sat directly under `claude.exe`. On the test machine the
  name resolved to the Store alias.
- Not measured: a `.cmd` shim as the exec-form command. The documentation was read
  as describing real executables only.

### After a session ends, a timeout, and a long timeout

- **Measured:** closing the session (the driver's hang-up) took a waiting hook with
  it; no process was left.
- **Measured, timeout 30:** the waiter was gone between 12 and 47 s after it
  started, with no record in the transcript, on the screen or in its own log. It
  ends silently. The exact end time was not pinned.
- **Measured, timeout 86400:** accepted without a settings error. A waiter alive
  after 3 h was woken by a flag and the model replied. The longest-lived waiter
  recorded 223 one-minute heartbeats (about 3 h 43 min) before a banto restart
  killed its process tree.
- **Not measured:** a full day. An overnight run was cut by that restart; a
  second was started, but its log records only 13 min and no outcome. A longer
  measurement was dropped: reopening a member's session (including after a banto
  restart) arms a new waiter on `SessionStart`, and an unanswered wake is shown
  once on the status line, so the operator can recover a dead waiter by hand.
- **Measured, timeout 1200:** respected. The waiter armed on `SessionStart` was alive
  670 s (about 11 min) after start, 70 s past the default 600 s; a flag then woke
  it and the model read its mail.
- Not measured: the default 600 s on its own, as a control.

### What the wake does to the pane (**measured**)

- **Busy panes are not interrupted.** With a flag raised while the model was
  writing a long answer (the screen showed "esc to interrupt"), a
  `queue-operation` row was written at once, nothing reached the model during the
  turn, and the notification arrived as a user row about 22 s later, right after the
  turn ended and the next turn began. Two flags that arrived together became two
  user rows and one reply.
- **A draft survives.** With text in the input line and no Enter, the wake turn ran
  (about 17 s in one run) and the line still showed the text at every one of about
  70 samples taken 250 ms apart, before, during and after. No row with `origin`
  `human` appeared; nothing was sent. The text of the draft does not appear in the
  raw output bytes at all: Claude redraws only spinner differences, so the draft
  is visible only in the emulated screen. **This rests on the screen**, and a
  separate check on the first round found the screen capture was updating normally
  (no freeze in any of the sessions).
- **After Esc.** With a waiter armed on `SessionStart` alive, Esc was sent (a) before any
  output, when the model was thinking, and (b) in the middle of a long answer. In
  (a) the screen put the sent sentence back in the input line; in (b) it showed an
  interrupted note and the transcript a request-interrupted marker. A flag raised
  3 s after the Esc in (a) and 8 s after it in (b) woke the pane both times (user
  row within about 55 ms; the model called `check_messages`).
- **`Stop` does not run for a turn Esc ended.** A logging-only `Stop` hook, which
  fired for a turn that finished normally, fired zero times between the Esc and
  the wake, and fired again when the woken turn finished. The documentation says
  the same (**read from the docs**: `Stop` does not run on a user interrupt). So a
  waiter armed on `Stop` alone is not re-armed after an Esc; one armed on
  `SessionStart` survives it.
- **`--resume`.** `claude --resume <id> --settings <file>` ran the `SessionStart`
  hook again and a wake then worked; the new rows were appended to the same
  transcript file, and the event name in the stderr frame read `SessionStart:resume`
  rather than a startup name.
- **Hooks fire on a wake turn.** `UserPromptSubmit` ran for the wake's user row
  (the prompt it received was 265 characters, a typed one 62-90; the content was not
  recorded). `PostToolUse` with `matcher: "mcp__banto__check_messages"` fired only for
  that tool, 0.19-0.27 s after the call, including in wake turns. With no matcher
  it fired for every tool.
- **Two settings sources both run.** A project `.claude/settings.json` with
  `UserPromptSubmit` and `Stop` hooks and a `--settings` file with a hook on the
  same events both fired on a typed prompt (3 ms apart) and on a wake; `Stop` of the
  project file fired. The wait hook, which sat only in the `--settings` file, ran
  too. Only the scratch folder's own `.claude/` was touched.
- **Not measured:** the permission mode banto launches with and the MCP approval
  dialog it implies; several Workers waking at once; Linux or macOS; the nudge
  with a real `send_to_peer` in the fake.

## A prompt on the command line

`claude [flags] "<prompt>"` starts the first turn of a new interactive session
with nothing typed.

### Where it can sit (**measured**)

- **(A) Before the flags** (first in the argument list): the first turn ran with
  that prompt, and the fake MCP server started.
- **(B) After a trailing `--`:** the same. `--` must come after the last option,
  including a multi-value one such as `--disallowedTools`.
- **(C) After the current banto order**, whose last option is `--mcp-config <path>`:
  Claude refused to start. It said the prompt was read as a value of `--mcp-config`
  and not as a prompt, and told the caller to put the prompt before `--mcp-config`
  or after `--`. It then failed with "MCP config file not found" and exit code 1,
  and no session was created. Claude itself names (A) and (B) as the two fixes.
- **The first user row** of the transcript was the prompt character for character,
  with `origin` `human`, in both (A) and (B). It also appeared as an ordinary `❯`
  line on screen. The screen was the normal interactive one, not print mode.
- **A trust dialog comes first** in a folder Claude has not seen: the prompt ran after
  the dialog was answered. Behaviour in an already trusted folder was not looked at.
- **A launch-tooling pitfall:** passing the argument list to PowerShell's
  `Start-Process` as an array split the prompt at its first space, so only the first
  word became the prompt. `ProcessStartInfo.ArgumentList` keeps each argument whole.
  This is about how the test was started, not Claude; banto passes `argv` directly.

### On `--resume` (**measured**)

- `claude --resume <id> [flags] -- "<prompt>"` and `claude "<prompt>" --resume <id>
  [flags]` both appended a new turn to the existing session: the transcript file
  grew by 13 rows in each of two runs (67 to 80 and 52 to 65), no second file
  appeared, and the new user row was the prompt character for character, including a
  newline and quotation marks. The screen showed the prompt under the earlier
  conversation.
- Not checked: whether banto's title logic (the first user row) would pick the
  prompt or Claude's own automatic title row, which a run can add separately. That
  is in banto's code, not in this spike.

## Stopping a turn without Esc

Esc stops a Claude turn in about 120 ms. That figure is from an earlier measurement
(2026-11-22, Claude Code 2.1.330 with Haiku), recorded in `docs/REQUIREMENTS.md` in the
`*Measured*` block of the interrupt paragraphs under "Auto-relay"; it was not repeated
here. This part asked what else could.

### What the documentation says (**read from the docs**, hooks reference)

- `continue: false` in a hook's JSON output is a universal field: Claude stops
  processing, and `stopReason` is shown to the user. On `PreToolUse` it does not
  stop the call by itself; `permissionDecision: "deny"` goes with it. On
  `PreToolUse` and `PostToolUse` it is documented to take effect even if the tool
  call fails or the response was streaming.
- `PostToolBatch` can stop the loop before the next model call; `MessageDisplay`,
  the only hook that runs while text streams, discards `continue`; so does
  `FileChanged`. `claude stop` and `claude kill` are for background sessions, and
  `TaskStop` for background tasks. No other documented way to stop a foreground
  turn without a key was found. A Remote Control stop and `PostToolBatch` alone
  were not looked at further.

### What was measured

A hook looked for a flag file and, if present, consumed it and returned
`{"continue": false, "stopReason": "..."}` (with a `deny` for `PreToolUse`). The
test tool waited a set number of seconds.

- **`PostToolUse`.** Three 12 s tool calls were asked for, and the flag was raised
  about 6 s into the first. The running tool was not cut: it ran to its end. The hook
  ran 0.17 s after the tool finished and stopped the turn 26 ms later (the screen
  showed a "hook stopped Claude" line with the reason). The other two calls were
  never made and no further model request went out. From flag to stop was about 8 s,
  the tool's remaining time.
- **`PreToolUse`.** Four 6 s calls, flag during the first. The first ran to its end;
  about 0.9 s later the model issued the second, and the hook stopped the turn
  and denied that call ("permission denied / hook error" with the reason): it was not
  run.
- **A long answer with no tool** (about 1,500 words, about 34 s of thinking then
  about 12 s of text): `continue: false` from the first `MessageDisplay` batch did
  nothing. All 25 batches were shown, the answer was complete and `Stop` ran as
  usual. The only hooks that run in a tool-less turn are `UserPromptSubmit`, the
  `MessageDisplay` batches (continue ignored) and `Stop`; that there is no other
  event to stop on is an inference from the documentation.
- **The session takes input afterwards.** In all three cases a line typed after the
  stop was answered normally. The stopped conversation is left with the `tool_use` and
  (for `PostToolUse`) its result and nothing after; the next input continues it.
- **Not measured:** an `asyncRewake` wake after a hook stop.
- **So:** a hook stops a turn at the next tool boundary, within the length of the
  tool in flight, and cannot stop a running tool or a model that is writing text.
  Esc stops both (the `docs/REQUIREMENTS.md` measurement above, not repeated here).

## A gate that makes a working model read its mail

The design the Goinkyo proposed in place of Esc: while an interrupt is unread, a
`PreToolUse` hook refuses every tool call that is not banto's own, with a reason that
says to call `check_messages`. Once the model calls it, the interrupt is read and
the gate opens. These rows tested whether Claude Code supports that and whether
models follow it. The gate was a fake tool on the fake MCP server, called by the hook;
"unread" meant a file named `interrupt...` in the fake inbox; `check_messages`
consumed it.

### The `mcp_tool` hook type (**measured**, and **read from the docs**)

- The `--settings` hook is an object of type `mcp_tool` with `server`, `tool` and
  `input`, where `input` values may use `${tool_name}` and `${agent_id}`. It named the
  server passed through `--mcp-config` and called it. The substitution worked
  (**measured**); the field list matches the "MCP tool hook fields" section of the
  hooks reference (**read from the docs**). It also has an optional `timeout`
  (documented default 600 s) and `onFailure` of `continue` or `block`.
- The tool's text is parsed like a command hook's stdout. **Empty text passes the
  call**; a `PreToolUse` deny object (`hookSpecificOutput` with `hookEventName`,
  `permissionDecision: "deny"` and `permissionDecisionReason`) denies it. Both
  measured. `SessionStart` and `Setup` `mcp_tool` hooks are skipped at launch, from
  the docs; not tried.
- **How a deny is recorded.** The `tool_result` has `is_error: true` and a body that
  begins "PreToolUse:<Tool> hook error:" followed by the reason. The row carries
  `toolDenialKind: "permission-rule"` and a `permissionDecision` of `reject`, source
  `hook`, reason code `hook:PreToolUse`. The screen says "1 action not run" and
  "permission denied" with the reason. It does not say the user refused. A hook that
  passed leaves a success attachment with a duration.
- **The gate tool is visible to the model unless hidden.** With no `alwaysLoad` it is
  listed by name among the deferred tools. `--disallowedTools mcp__banto__interrupt_gate`
  removed it from that list, yet the hook still called it and the deny still worked.
  The later rounds left it visible. In six sessions with it visible (the last round's
  two subagent runs and four resumed runs, parent and subagent transcripts
  included), **no model ever called it**, and the gate log has no call for the gate
  tool itself. Each is one or two observations.
- The hook also fires for banto's own `check_messages`; the gate passes it.
- **Cost per call.** Haiku, 18 calls (6 Read, 6 Grep, 6 Bash), one run per
  arm, three runs at the same time: no hook median 125 ms; `mcp_tool` gate that
  passes 139 ms, the hook's own reported duration median 10 ms and maximum 29 ms; a
  do-nothing Python `command` hook median 301 ms, about 180 ms more. The difference
  from the `mcp_tool` gate is inside the spread of the tools themselves. One run
  per arm cannot show a small difference.

### Whether models follow a refusal (**measured**)

Haiku, sonnet and opus, two runs each, the real Worker text, `--permission-mode
auto`. The task read 25 small synthetic files, one call at a time; the interrupt was
raised after a few.

- All six runs went the same way: the next tool call was refused; within 0.6-1.7 s
  in the same turn the model called `check_messages`, read the interrupt, stopped
  work and answered with the one word the interrupt asked for. No further tool call
  followed. From refusal to that answer took haiku about 3.5-5 s, sonnet about
  4.5-5 s and opus about 7-12 s.
- Opus, once, searched for a `send_to_peer` to answer the Director after reading;
  the gate was open by then and the search went through. It did not resume the task.
- Limit: the task was cooperative, and the interrupt agreed with the reason. A model
  told to keep going could do otherwise.

### Subagents (**measured**; sonnet parent; Agent tool, one subagent reading the
same 25 files)

- `PreToolUse` fires for a subagent's own calls, with its `agent_id` in the input
  (empty for the main agent). The subagent's transcript is a separate file under the
  session's `subagents/` directory. The Agent tool started the subagent in the
  background, so the parent sat idle until a task notification brought it back.
- **A gate that refuses everything but banto's tools** (round 3): the subagent
  retried the refused Read three times, and its own internal return tool, which the
  hook sees as a tool named `SubagentHandback`, was refused as well. It could not
  return for about 45 s and ended with no report. The parent was woken by the end
  notification, its next tool call was refused, it called `check_messages` and
  answered.
- **The same gate allowing `SubagentHandback`:** the subagent returned about 7 s after
  its first refusal, with a summary that mentioned being stopped by a hook. The
  parent, told of the return as a user row whose `origin` is a peer, called
  `check_messages` about 3 s later and answered. The reason text told the subagent not
  to call `check_messages` itself, and in that run it did not; in the first run it
  ignored three refusals, so that is not a guarantee. The hand-back arrives in the
  parent as a user row with `origin` `{kind: 'peer', from: <agent id>, ...}`.
- **Limit:** while the parent only waits for a background subagent, it makes no tool
  call, so nothing reaches it until the subagent returns.

### A cap on refusals, without naming any tool (**measured**; K = 3)

The Goinkyo then dropped the allow-list. The gate in this round: banto's own tools
always pass, except that `check_messages` from a subagent is always refused; for any
other call, refuse up to 3 times per unread interrupt (counted in the server's
memory, separately for the main agent and for each subagent), then let the gate open
for that interrupt. No tool name appeared in the gate. The gate tool stayed visible.

- **Two subagent runs, sonnet parent.** In both, the subagent's calls were refused
  three times and the fourth call, its hand-back, passed because the cap had
  opened, and it returned. From the first refusal to the return took about 11 s in
  both. In one run two of the three refusals fell on hand-back attempts (the
  subagent tried to return after the first refusal, and was refused twice); in the
  other all three were refused Reads, repeated after the reason said to return.
- **No run showed a subagent working on through an opened gate.** The first call
  after the cap opened was the hand-back, both times. That a subagent could use the
  open gate to keep working was not observed in two runs; it is not ruled out.
- **The parent read the interrupt in one run and not in the other.** In the first it
  called `check_messages` about 2 s after the hand-back and answered. In the second
  it received the hand-back, wrote a summary of what the subagent got through, and
  ended its turn; it made no tool call, so the gate never ran for it, and the
  interrupt was still unread more than a minute later. Reading it then depends on
  the relay's ordinary nudge (**not measured**).
- **Not exercised:** the always-refuse rule for `check_messages` from a subagent: no
  subagent called it.

### `check_messages` left deferred, and resumed (**measured**; haiku and sonnet,
one run each)

Some real Workers were created before banto set `alwaysLoad`, and are resumed with
`check_messages` deferred. To reproduce it, the tool's `_meta` was removed from
`tools/list`, a session took one turn and was closed, and was reopened with
`--resume` in the same folder with the same settings, MCP file, Worker text and
permission mode. The tool's name then appears in the deferred-tools list.

- **A first attempt tested nothing.** Its first turn used the Worker text, so the
  model called `check_messages` in that turn; on resume the call was already in the
  conversation and the model used it directly. A test of a deferred tool needs a
  first turn that never touches it. The runs reported below used a first turn with no
  Worker text, and the transcript confirmed the first `check_messages` call was
  after the resume.
- **Haiku:** the refused Read was followed by a statement that it would load the tool,
  then a `ToolSearch` for it, which was **refused too** (the second refusal); the
  next step was a direct call to `mcp__banto__check_messages`, which went through and
  returned the interrupt. Two refusals, about 4 s from the first refusal to the read.
  The call succeeded without any `ToolSearch` having loaded the tool.
- **Sonnet:** the refused Read was followed directly by a call to
  `check_messages`, which went through. One refusal, about 4.5 s. It never searched.
- So both read before the cap of 3. `ToolSearch` is not a banto tool and counts as a
  refusal, as the one in the haiku run shows. That a deferred tool can be called by
  name before it is loaded rests on one haiku run, and on sonnet never needing to.
  A model that always searches first would spend a refusal on the search; whether 3
  is then enough was not measured.

### When the gate fails (**measured**; haiku, an interrupt left unread, a 2 s
timeout, no `onFailure`)

Every failure let the call through. The documentation says the same of `PreToolUse`.

- The gate returned an error result: the call ran, with no delay.
- The gate slept past the timeout: the call ran after 2.1 s (the timeout); the late
  answer was ignored.
- The gate's server exited during the call: the call ran; the next call also ran, and
  the server was not restarted in the minute watched.
- The hook named a server that failed to start, or one that was not in the
  configuration: the call ran; the screen showed "PreToolUse:Bash hook failed" and
  that the MCP server was not connected.
- Not measured: a hook that fires while the MCP server is still connecting (the
  documentation says it waits within `MCP_TIMEOUT`), and whether a dead server
  reconnects later.

### Permission modes (**measured** for `auto` and `default`; the launch values from
HEAD)

- **From the repository at HEAD:** a Worker launches with `--permission-mode` taken
  from `worker_permission_mode`, whose default is "auto" (`config.rs`); the role's
  launch values are built in `engine.rs`'s `RoleLaunch::of` and written to the argument
  list in `launch.rs`. The gate rows above ran with `auto`.
- **Manual (`--permission-mode default`):** a `Bash` call during an unread interrupt
  was refused by the hook before any approval prompt (36 ms after the `tool_use`, no
  dialog). But the model's next call, to `mcp__banto__check_messages`, did raise an
  approval dialog in manual mode, independently of the gate. So in a manual Worker
  the interrupt read waits for the operator. A call the gate passes (a write by
  `Bash`) still raised the normal approval prompt: the gate does not bypass approval.
- Not measured: `plan`, `dontAsk` and `bypassPermissions`.

### What the gate cannot do (**measured**, with the stop results above)

It acts at the next tool call. It does not cut a running tool and does not reach a
model that is writing text without a tool call. A parent waiting on a background
subagent, or one that ends its turn without a call (the second subagent run), is not
reached by it.

## What a run like this leaves behind

All of these are **measured** unless marked.

- **The sessions are real.** Every pane that took a turn is in the agent's session
  list, under the scratch folder it ran in, and shows up in the operator's list. A
  later `--resume` adds rows to an existing session. Close them with the hang-up and
  list what was made.
- **A pane can be left alive.** A waiting hook, the driver and Claude each outlive a
  careless exit. Closing by the driver's hang-up ended the hook as well (exit code
  129 each time); after every round no driver, Claude or hook process was left.
  Check for them by name at the end.
- **Do not force-kill a young Claude.** `docs/REQUIREMENTS.md` records, in the
  "*Why (2026-11-21)*" paragraph of the pane-closing rules under "Brigade
  (Director/Worker cells)", that Claude Code 2.1.330 writes a "fullscreen boot
  pending" record at each launch and removes it only in its own shutdown, and that two
  Claudes killed within about ten seconds of starting make it turn its fullscreen
  renderer off machine-wide. That is **read from `docs/REQUIREMENTS.md`**, not
  re-measured here. No pane in this work was closed before 10 s, and the hang-up was
  only used once the pane was well past that.
- **The trust dialog defaults to "No, exit".** A fresh scratch folder asks; move down
  before Enter. Whether answering writes a trust record into Claude's own state was
  not checked, so delete the scratch folders and look.
- **Argument lists.** Start the driver with `ProcessStartInfo.ArgumentList`; an array
  given to `Start-Process` splits a prompt at spaces. A prompt must come before the
  last option or after `--`.
- **Models decline to sleep.** A request for a shell `sleep` or a Python sleep in a
  test turn was refused by the model on the first round, so the turn ended in 3 s and
  tested nothing. A busy turn needs real work: a long answer, or a tool the fake
  server provides that waits.
- **A "deferred tool" test must start without touching the tool.** A Worker text that
  says to check messages makes the first turn call it, which undoes the test.
- **Scratch-only settings.** Hooks went in only through `--settings` files, so
  nothing was written to a settings file Claude reads on its own, and nothing was
  written to Claude's home, banto's data or the repository (only the build output of
  the driver example, which `target/` already ignores).
- **A stale tool in the way.** `python -I` ignores `PYTHONIOENCODING`; a script that
  prints a dash needs `-X utf8` or it fails on a Japanese Windows console. PowerShell's
  `Set-Content -NoNewline` was not available, and a mode file set that way was never
  written, so one failure-mode run tested the wrong step; write such files with
  `[IO.File]::WriteAllText`.
- **A test inbox is shared by what reads it.** The fake `check_messages` and the gate
  read the same files; arm each step again after a read, because the read consumes the
  interrupt.

## Not measured, in one place

- A wake with a real `send_to_peer` in the fake server; the reply path after it.
- A wake under banto's own permission mode and MCP approval; several Workers waking
  at once; Linux and macOS; Codex (a separate note).
- The default 600 s hook timeout as a control; a 86,400 s wait; a `.cmd` exec-form
  hook; a waiter that is re-armed over a long session.
- An `asyncRewake` wake after a hook stop; Remote Control stop; `PostToolBatch` alone.
- The positional prompt in a folder already trusted; which title banto would pick for
  a session started with one.
- The gate in `plan`, `dontAsk` and `bypassPermissions`; a subagent calling
  `check_messages` under the always-refuse rule; a subagent using the opened gate to
  carry on; a hook firing while the MCP server connects; a dead gate server
  reconnecting; a model told to ignore the refusal.
- Whether a deferred `check_messages` can be called by name for models other than
  haiku and sonnet, and how many refusals a model that always searches first would
  need.
- The wake and the gate with several rounds of interrupts in one session (the cap's
  counter was reset between runs by starting new sessions).
