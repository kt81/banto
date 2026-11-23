# A Codex Goinkyo — what was run and measured (2026-11-23)

The operator decided that a Goinkyo may run on Codex as well as Claude Code
(`goinkyo_agent`, described in `docs/REQUIREMENTS.md`, "Goinkyo's product"). This
note records what the live runs and the separate measurements showed, so the next
change to a Codex Goinkyo can be compared with it. It states what happened and
copies no rollout text, no log lines and no consultation content.

Versions: **codex-cli 0.171.0**, **Claude Code 2.1.330**, Windows 11.

Each fact is marked **measured** (observed on a machine, in this work) or **read
from source** (taken from the code, a commit message or `--help`, not observed
here). The live runs were the operator's, on 2026-11-23, and were reported to the
worker that wrote this note: their raw logs are not in the repository.

## The live runs

| Run | Outcome |
|---|---|
| A | Passed. |
| B1 | The Goinkyo waited about 92 s at Codex's own directory-trust question; once that was answered in its pane, the kickoff was typed automatically and the run completed. |
| B2, 31a | Killed by the hang-up race fixed in `cd6e5ad`: the tick read the Goinkyo's store row before the same pass had written its session id, took the live pane for a finished consultation's leftover and hung it up a few milliseconds after it was identified. |
| three runs on the fixed build | All completed; one of them in the tick pass that used to kill. |

All of this is **measured**, by the operator. Two of four Codex Goinkyos died of the
race in the first live batch (**read from source**: the `cd6e5ad` message). A Codex
Goinkyo is exposed because its hook lands about one tick period after the kickoff's
Enter (**read from source**, same message).

## What the launch flags allow

A Codex Goinkyo always carries `-s read-only -a never` (`--sandbox read-only
--ask-for-approval never`), on a creation and on a resume alike. Both flags are
listed by `codex --help` and `codex resume --help` at 0.171.0 (**read from source**).

- **Measured, in four runs:** banto's three MCP tools, reads and `git log`/`git status`
  all worked under `-s read-only -a never`, with no prompt, given the approval override
  `default_tools_approval_mode="approve"` on the `banto` MCP server. Without it a tool
  that needs approval is denied under `approval_policy = "never"` rather than asked
  about (**read from source**).
- **Measured, separately from any Goinkyo:** the write block was tried with
  `codex sandbox` under `[windows] sandbox = "unelevated"`. A write by `cmd`, by
  Windows PowerShell 5.1 and by PowerShell 7 was refused each time ("Access ...
  denied"), and a control run under workspace-write succeeded.
- **Measured:** in both runs that asked a Goinkyo to write, it declined to attempt
  one. So a write *from a running Codex Goinkyo* was not observed to be blocked; the
  block was observed in the sandbox alone.
- Not measured: the same write test on another operating system or under another
  `[windows] sandbox` value.

## Tokens

**Measured**, per Codex Goinkyo consultation: about 160k input tokens, most of them
cached, and under 1k output.

## The re-stage window

A Codex Goinkyo is not stranded the way a Claude one can be (see the paragraph on
re-staging a Goinkyo in `docs/REQUIREMENTS.md`), and has a gap of its own. All
**measured**, over 8 runs:

- Codex wrote the session's `threads` row and its rollout at the first persist, which
  is the kickoff's turn. The rollout was born 8 ms after the kickoff's Enter.
- banto discovered the session 1.03-1.17 s after that, in all 8 runs.
- `threads.created_at` is a creation stamp 2.6-3.8 s before the rollout file exists,
  not the time the row was written.
- In 3 of the 8 runs, discovery came 25-39 ms before Codex had written the briefing
  and kickoff records of that same turn. That is the ~40 ms window: a banto restart or
  a pane death inside it ends the turn before the kickoff record exists, and the
  re-stage then resumes the Goinkyo without a kickoff. Nothing handles that; a test
  pins it as the current behaviour.

## The title thread

- **Measured (live, 0.171.0), and read from source in the `cc0e718` message:** after
  the first message, Codex's TUI starts an ephemeral thread to title the conversation.
  It never gets a `threads` row. banto's identification of an operator-started Codex
  pane counted every thread id its pid logged, so when the title thread logged before
  discovery polled the pane stayed ambiguous for good, stayed in `unresolved_codex`,
  and every brigade with a Codex member refused to stage until that pane was closed.
  Fixed in `cc0e718`: only ids among the caller's candidates (the pane's `threads`
  rows) count. The live check after the fix did not exercise the failing order, since
  that run's title thread logged 0.3 s after discovery; unit tests reproduce it.
- **Measured, not fixed:** in panes banto spawned, the title thread fails with
  "invalid transport in `mcp_servers.banto`". It shows at DEBUG level only. Its cause
  is not established, and nothing is changed for it.
