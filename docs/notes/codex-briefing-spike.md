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

## Open: MCP tool calls need an approval, and the fix is unverified

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

**None of this was confirmed on a live call**: every attempt to exercise it ran
into the startup problem below, so the tool was never reachable to approve.
Treat it as the likely fix, not a measured one.

## Open: the MCP server stopped being started at all

Partway through the spike, and from then on for every subsequent run, Codex
stopped spawning the MCP server — the server's log file, which is written on
the very first inbound line, was never created. It is not the server (it still
answers a hand-piped handshake), not PATH (an absolute `node.exe` behaves the
same), not the config (`codex mcp list` still reports the server `enabled`, and
`--strict-config` accepts every key used), and not `startup_timeout_sec`.

The runs that did work were all slow ones — a prompt that made the model go
looking for the tool list, or a turn that spent ~30s retrying against the
capture endpoint. The runs that failed were all trivial turns answered in a
second or two. That points at startup being asynchronous and a short turn
simply finishing first, but it was not pinned down, and the machine was running
three other agent sessions throughout, so load is a confound rather than a
conclusion.

If it is a race, it matters for brigades directly: a Codex member could begin
its first turn before banto's tools exist, which is precisely the turn the
briefing is trying to shape.

## Consequence for brigades on Codex

The briefing question is answered, and not by MCP: `-c developer_instructions`
is the `--append-system-prompt` analogue, fresh launches only.

The MCP finding is the one that bites. banto's three brigade tools would be
deferred for a Codex member — a Worker would not see `check_messages` in its
toolset at all. A deferred tool *is* callable once named (the spike called one
by name), so the briefing would have to carry the tool names itself, and the
relay's forcing function would have to survive the approval gate above.

None of this reopens the decision to defer Codex brigades. It replaces one
unknown with a cost: fresh-launch-only briefing, tool names carried by hand, an
approval gate whose fix is only source-deep, and a startup that was not reliably
reproducible on the machine that measured it.

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

Trust is keyed by a hash of the hook's normalized identity plus a *positional*
key (`<source>:<event>:<group>:<handler>`), which upstream's own comment flags
as something to replace with a durable id. It is written only from the
interactive TUI's startup review prompt ("Trust all and continue"), into the
user layer. Two consequences for banto: the operator would have to establish
that trust once, by hand, in an interactive Codex; and the hook command banto
injects has to stay byte-stable across launches, so anything embedding a
worktree-varying absolute path would drift out of trust.

`--dangerously-bypass-hook-trust` is not the way out. It is scoped to hook
trust and touches neither the sandbox nor MCP approval, but it applies to
*every* hook in the merged config for that invocation — including a hook whose
command changed since a human last reviewed it. Passing it on every launch would
mean banto silently running whatever else the operator's or the project's config
happens to contain.
