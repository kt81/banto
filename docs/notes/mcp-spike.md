# MCP spike — banto as the brigade Director↔Worker mediator (2026-07-23)

## Question

Slice 2c of the brigade work needs Director and Worker sessions to exchange
messages under a human gate. The plan is for **banto itself to be an MCP server**
that the embedded `claude` sessions connect to, with banto mediating a pull-based
message queue. The largest unknown was the transport:

- Can an embedded `claude` be pointed at banto's own MCP server **without writing
  under `~/.claude`** (read-only invariant 1)?
- Does Claude Code actually spawn banto's server, complete the handshake, and
  round-trip a tool call?
- Can a **per-session identity** be threaded in at launch (the "register the pair
  at launch" API), so banto knows which brigade/role the caller is?

Per the project's spike-first discipline (measure, don't theorise), this was
verified before wiring the real mediation.

## Result — confirmed end to end

The key enabler: **banto controls the embedded launch argv**, so it launches
`claude --mcp-config <file>` where `<file>` lives under banto's own data dir. No
write under `~/.claude`.

- **U1 (headless, in-repo):** a minimal stdio MCP server (`banto _mcp`, hidden
  subcommand, `crate::mcp`) implements the newline-delimited JSON-RPC 2.0 stdio
  transport: `initialize` (echoes the client's requested `protocolVersion`),
  `tools/list`, `tools/call`, MCP `ping`, and drops notifications (no `id`).
  Exercised by unit tests over the pure `handle_line`, and by piping a real
  handshake through the built binary (3 requests → 3 responses; the
  `notifications/initialized` notification correctly got none).

- **U2 (real Claude Code, on-device):** launched via
  ```
  claude --strict-mcp-config --mcp-config <file> \
         --allowedTools "mcp__banto__banto_ping" \
         -p "Call the banto_ping tool now and reply with only the exact text it returns."
  ```
  with the config file:
  ```json
  { "mcpServers": { "banto": {
      "command": "…/banto.exe", "args": ["_mcp", "--session", "u2-check"] } } }
  ```
  Output: `pong from banto (session=u2-check)`.

That single line proves all three unknowns at once: Claude Code spawns
`banto _mcp` from `--mcp-config` (outside `~/.claude`), the handshake +
`tools/list` + `tools/call` round-trip succeeds, and the **`--session` argv
threads through** — the tool echoed the exact id banto injected at launch. The
MCP tool name Claude Code exposes is `mcp__<server>__<tool>` (here
`mcp__banto__banto_ping`); `--allowedTools` with that name pre-approves it in
non-interactive `-p` mode.

## Consequences for Slice 2c

- Delivery mechanism = **MCP tool-result, not stdin injection.** Even though the
  embedded banto is the sole writer to a child's stdin, injecting a peer's
  message there would forge human input mid-turn. A pull tool
  (`check_messages`) returning the queue as a tool result respects turn
  boundaries and gives the "this is from another AI, don't take it as an order
  from your operator" firewall framing for free.
- The shared medium can be **banto's existing sqlite store** (a `brigade_messages`
  queue table): `banto _mcp` opens the same DB the TUI does — exactly the
  cross-process sharing the store's `busy_timeout` was already designed for
  (TUI + `_wrap`). The main TUI process need not be in the MCP transport path.
- "Register the pair at launch" = pass `--brigade <id> --role <director|worker>`
  (alongside `--session`) into the `_mcp` args when banto opens a brigade member,
  so the server knows who the caller is and which queue to read/write.

## Not yet verified

- Multiple concurrent embedded sessions each with their own `_mcp` server and
  the sqlite queue under real contention.
- Whether a Worker reliably *chooses* to pull `check_messages` at useful
  checkpoints without heavy prompting (a prompt/protocol-design question, not a
  transport one).
- Non-Windows.

Spike code (`crate::mcp` + the hidden `_mcp` subcommand) is kept as the seed of
2c; the throwaway config file lived in the scratchpad (never committed).

## Follow-up (2026-07-25): the open question, answered on the Director side

This note's open questions included *"whether a Worker reliably chooses to
pull `check_messages` at useful moments"*. Dogfooding turned up the same
question one level up, with a sharper answer: across a full day of work in a
live cell, the Director sent **zero** messages. Not a reliability problem —
an information one. A member is launched with `--mcp-config` and nothing
else, so the brigade exists in banto's store, in its argv, and on the
operator's screen, but nowhere in the member's own context: what reaches the
model is three tool names. And because the relay only wakes a member that
*has* mail, a cell whose Director never sends the first message never starts.

Two changes close it: members now launch with a role briefing
(`--append-system-prompt`, verified to apply to a `--resume` as well as a
fresh launch — that is what reaches an already-running Director), and
`banto_ping` became `brigade_status`, which answers who you are, who your
peers are, what each is doing, and who is holding unread mail from you. The
old `banto_ping` name in the transcript above is historical; the tool is
`mcp__banto__brigade_status` now.

The Worker-side question the note originally asked stays answered the way it
always was — by the relay forcing the turn, not by hoping. There is no
equivalent forcing function on the Director side, and there shouldn't be:
delegating is the operator's call to delegate. A standing instruction is the
difference between *possible* and *expected*, which is all it needs to be.
