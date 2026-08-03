# Codex briefing spike — can a brigade member be briefed without `--append-system-prompt`? (2026-07-28)

## Question

banto briefs an embedded Claude Code member with `--append-system-prompt`: who
you are in this brigade, who your peers are, that mail arrives through banto.
`docs/notes/mcp-spike.md`'s follow-up records why that briefing exists at all —
without it a member's whole knowledge of the brigade is three tool names, and a
cell whose Director never sends the first message never starts.

Codex CLI has no `--append-system-prompt` (measured while adding Codex support).
A suggestion from outside the project was to reuse a channel banto already owns:
banto **is** the MCP server the member connects to, so put the briefing in the
MCP `initialize` response's `instructions` field, or in the tool descriptions.

Does any of that actually reach the model?

Measured against **codex-cli 0.145.0**, model `gpt-5.6-terra`, Windows 11.

## Method — why the model was not asked

The obvious probe is to ask the model what it can see. That was tried first and
**it answered wrongly in both directions**: it reported no tool whose name
contained `spike` while Codex was, in the same configuration, happily
dispatching a call to `spike/spike_probe`; and asked to quote a tool
description it answered `MISSING` for a description that was demonstrably in
its reach. A model's self-report about its own context is not an instrument.

So the spike reads the wire instead. Two throwaway servers:

- a stdio MCP server carrying one distinct token per candidate channel —
  `initialize.instructions`, the tool `description`, and an `inputSchema`
  property description — and logging every inbound JSON-RPC line, so a negative
  can be shown to be *the client dropped it* rather than *the client never asked*;
- a stand-in for the model endpoint (`-c model_provider` + `model_providers.<id>.base_url`
  pointing at localhost) that logs the request body and then fails the call on
  purpose. Only the request is wanted.

That second one is the reusable part: it renders the exact payload Codex builds,
including the tool declarations, with no completion spent.

## Result

**`instructions` is not dropped.** Codex concatenates it onto the front of every
tool description from that server. What the model can reach is:

```
name: mcp__spike__spike_probe
description:
  <the server's initialize.instructions, verbatim>
  <the tool's own description>
  exec tool declaration:
  ```ts
  declare const tools: { mcp__spike__spike_probe(args: {
    // <the inputSchema property description>
    note?: string;
  }): Promise<CallToolResult>; };
  ```
```

All three candidate channels survive, merged into one string.

**But that string is not in the model's context.** MCP tools are *deferred*:
they are absent from the outbound request's tool declarations entirely. The
captured 47 KB request that Codex sent — with the MCP server confirmed
initialized and `tools/list` answered before it — declares four tools (`exec`,
`wait`, `request_user_input`, `collaboration`) and contains no occurrence of the
server name, let alone the briefing. Codex's own `exec` tool description says
so outright:

> Some deferred nested tools may be omitted from this description. They are
> still available on the global `tools` object and listed in `ALL_TOOLS`.

`ALL_TOOLS` lives inside the `exec` JavaScript sandbox. The block above was
recovered by making the model run `ALL_TOOLS.filter(...)` and report the result.
A briefing placed there is reachable, not present: the model has to go looking,
and nothing tells it there is anything to look for.

Which switch causes the deferral was not isolated — `--disable code_mode_host`
was tried, but by then the spike had run into the startup problem below, so
that run proves nothing either way.

**The channel that does work is `-c developer_instructions="<text>"`.** It
arrives as a genuine developer-role message — confirmed on the wire, not
inferred — and it is pure argv, so it needs no write under `~/.codex` and none
in the operator's repository. Two caveats, both measured:

- It is **prepended to the `<permissions instructions>` block with no
  separator.** A briefing ending without a newline runs straight into
  `<permissions instructions>`.
- It applies to a **fresh launch only.** On `codex exec resume <id>` the flag is
  accepted without complaint and then silently ignored: the resumed request
  replays the original prefix, and the new text appears nowhere in the body.
  This is exactly where Codex diverges from Claude Code, whose
  `--append-system-prompt` was verified to apply to `--resume` too — and that
  resume path is what reaches an already-running Director.

**A briefing delivered this way is obeyed, but does not displace Codex's own
identity.** Told to prefix every reply with a fixed line, the model did. Asked
in the same run what its role was, it answered that it is the primary `/root`
agent and reports to the user — Codex's built-in developer message (17.7 KB,
ahead of the injected text in the same request) asserts that identity, and the
briefing did not overwrite it. Whatever briefing banto writes has to work
alongside that, not against it.

## Channels ruled out

Surveyed and rejected before the above, so nobody re-derives them:

- **`AGENTS.md`** — read from the session's cwd, i.e. the operator's own
  repository. Invariant 1's spirit: banto writes only to its own directories.
- **Hooks (`SessionStart` + `additionalContext`)** — ruled out here on the
  belief that every discovery location is `~/.codex` or `<repo>/.codex`. That
  belief was wrong; see the follow-up at the end of this note.
- **Profiles (`-p`)** — the profile file lives under `$CODEX_HOME`.
- **`model_instructions_file`** — writable to a compliant location, but it
  *replaces* the built-in instructions rather than adding to them, so it would
  silently suppress an operator's real `AGENTS.md`.
- **A generic `CODEX_<KEY>` environment override** — searched for, not found.

The positional prompt argument (`codex resume <id> "<text>"`) does reach a
running session, but as a **user turn**: it reads as though the operator typed
it, and the model answers it. That is a different instrument, not a substitute.

## MCP tool calls need an approval, and one flag settles it

Told to call the spike tool, Codex logged
`mcp: spike/spike_probe started` → `(failed)` → `user cancelled MCP tool call`,
and **`tools/call` never reached the server** (verified against the server's
complete inbound log). This was under `codex exec`, where there is no human to
approve, and despite the run reporting `approval: never`: MCP calls carry a
second approval axis of their own, `mcp_servers.<server>.default_tools_approval_mode`
/ `mcp_servers.<server>.tools.<tool>.approval_mode`, which defaults to needing
approval whenever the server declares no tool annotations — banto's state today.

Setting that axis to `"approve"` is, from a source reading of 0.145.0, a
one-shot `-c` flag that short-circuits the approval check ahead of the
`approval_policy` / sandbox path — an allowlist, not a global bypass, and the
closest thing to Claude Code's `--allowedTools`. Reaching a tool the deferred
way does not sidestep it: the `exec` sandbox's callback chain funnels into the
same router-registered MCP handler, which is the only call site of the approval
check in the repo, so the recipe is the same whether the model calls the tool
natively or finds it through `ALL_TOOLS`.

Confirmed on a live call, in an interactive session: with
`default_tools_approval_mode="approve"` passed on the command line, no prompt
appeared and `tools/call` arrived at the server, which had never happened
before. The flag is read fresh from the merged config on every call and never
consults the persistence path, so it does not matter that a `-c`-registered
server cannot persist an approval at all — and it cannot: the "remember my
choice" write is gated on the server being present in the *user* config layer,
and silently degrades to session-only when it is not. banto should pass the
flag every launch and never rely on the remembered choice.

## A `codex exec` trap that cost this spike an afternoon

Partway through, `codex exec` stopped spawning the MCP server at all — the
server's log, written on its very first inbound line, was never created. Not
the server (it still answers a hand-piped handshake), not PATH (an absolute
`node.exe` behaves the same), not the config (`codex mcp list` still reports it
`enabled`, `--strict-config` accepts every key), not `startup_timeout_sec`. No
error, and nothing to read: **`codex exec` writes nothing to `logs_2.sqlite`**,
so the only log entries visible during the hunt belonged to an unrelated
interactive session — which is how a wrong conclusion got drawn from them
before that was noticed.

The same configuration in an **interactive** session starts the server, lists
its tools, and completes a `tools/call`. banto launches Codex interactively in
a PTY, so this never touches banto; it only ever broke the measuring
instrument. Whatever it is stayed undiagnosed and is left that way
deliberately.

The lesson worth keeping is the shape, not the cause: three separate gates in
this spike — a deferred tool, an unapproved MCP call, an untrusted hook — all
fail by producing *nothing*. A harness that reads success from the absence of
an error will read all three as working.

## Consequence for brigades on Codex

The briefing question is answered, and not by MCP: `-c developer_instructions`
is the `--append-system-prompt` analogue, fresh launches only.

The MCP finding is the one that bites. banto's three brigade tools would be
deferred for a Codex member — a Worker would not see `check_messages` in its
toolset at all. A deferred tool *is* callable once named (the spike called one
by name), so the briefing would have to carry the tool names itself, and the
relay's forcing function would have to survive the approval gate above.

None of this reopens the decision to defer Codex brigades by itself, but it
removes the technical reason for it. What is left is a cost: the briefing has
to name banto's tools by hand, because a Codex member cannot discover them; and
every member launch has to carry a fixed hook plus an approval flag, exactly.

**Addendum, after Codex brigades shipped:** the decision was in fact
revisited — Codex brigades exist in production
(`crates/banto/src/hook.rs`, `crates/banto-core/src/config.rs`'s
`WorkerAgentSetting::Codex`). The first half of the predicted cost landed as
guessed: the briefing does name banto's tools by hand
(`crate::briefing::CODEX_ADDENDUM`). The second half didn't — no launch
carries an approval flag. `--dangerously-bypass-hook-trust` turned out to be
scoped to hook trust alone but to apply to *every* hook in a launch's merged
config, not just banto's, so passing it on every launch would silently trust
whatever else an operator's own config happened to add. What shipped instead
is a one-time, interactive trust-review pane asked before a brigade's first
Codex member forms, never repeated afterward — see `docs/REQUIREMENTS.md`'s
"Brigade" section for the shipped mechanism.

## Follow-up: a `SessionStart` hook does reach a resumed session

The list above rules hooks out for needing a file under `~/.codex` or the
operator's repository. That is wrong, and the correction is the most useful
thing in this note.

`-c` overrides are loaded as an ordinary config layer (`SessionFlags`), and hook
discovery walks every layer asking each one for its `hooks` table without caring
which layer it is. So a hook can be supplied entirely on the command line. The
syntax is an inline array of inline tables:

```
-c 'hooks.SessionStart=[{hooks=[{type="command",command="<cmd>"}]}]'
```

Measured, with the same request capture used above:

- **It fires on a fresh launch** — the hook is handed
  `{"hook_event_name":"SessionStart","source":"startup",…}` on stdin — and its
  `additionalContext` arrives as a **developer-role message with no wrapping
  markers**, placed *after* Codex's built-in developer blocks and immediately
  before the user turn. That is a better position than `developer_instructions`,
  which is prepended to the `<permissions instructions>` block far earlier.
- **It fires on `codex exec resume` too**, with `source: "resume"` and the
  resumed session's own id, and the context lands in the same place —
  immediately before the new user turn. This is the gap `developer_instructions`
  leaves open, closed: the hook runs live on the first turn after the session is
  constructed, whereas the resumed prefix is replayed from the rollout, so a
  fresh `developer_instructions` at resume time has nothing to attach to.

**The gate is trust, and it fails silently.** Both runs above passed
`--dangerously-bypass-hook-trust`, to separate "does the mechanism work" from
"is this hook allowed". Re-run without it and the hook does not execute, with no
warning, no error, and nothing in the output naming trust — the same command
simply produces no hook. An untrusted hook is dropped from the run list, not
reported.

**One approval holds — measured end to end.** The operator granted trust once
from the interactive TUI's startup review ("Trust all and continue"). After
that, with the bypass flag gone, the same hook fired from `codex exec`, and
from `codex exec resume` with `source: "resume"` — a different subcommand, a
different process, a different session. Not per pane, not per resume. The
record it wrote:

```toml
[hooks.state.'C:\<session-flags>\config.toml:session_start:0:0']
trusted_hash = "sha256:…"
enabled = true
```

That key is the whole story. Its first component is a **synthetic literal** for
the `-c` layer, with no session id, pid or timestamp in it, and distinct from
the real path a user-config hook would key under — so an operator adding their
own hooks cannot shift banto's indices, which are scoped per layer per event.
The hash is the *value*, not part of the key, and covers only the hook's
declared config text plus a compile-time platform flag; `cwd` is not a field
that can reach it at all. Same config text, same binary, same hash — every
launch, every member, every worktree.

Two things follow for banto, and they are constraints, not observations:

- **The injected `-c hooks.…` TOML must not vary by one byte between
  launches**, and there must be exactly one of them. Set `timeout_sec` and
  `matcher` explicitly rather than letting them default — a future Codex
  changing the default would silently move the hash, and a hash that no longer
  matches produces no error, just a hook that stops running.
- **Per-member identity cannot ride in the command string**, since that is
  hashed. It rides in the environment instead: a hook inherits the environment
  of the session banto launched (measured — `BANTO_*` variables set on the
  Codex child arrived intact in the hook, and varied per launch while the hook
  config, and so the trust record, stayed fixed). `session_id` and `cwd` also
  arrive on the hook's stdin.

`--dangerously-bypass-hook-trust` is not the way out, and is not needed. It is
scoped to hook trust and touches neither the sandbox nor MCP approval, but it
applies to *every* hook in the merged config for that invocation — including a
hook whose command changed since a human last reviewed it. Passing it on every
launch would mean banto silently running whatever else the operator's or the
project's config happens to contain.
