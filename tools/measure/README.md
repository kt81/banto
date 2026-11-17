# Measurement tools

Python scripts that measure how a team of Claude Code agents run under banto
spends tokens, and how its changes fare in review. They are here so that anyone
can measure their own team; nothing in banto depends on them.

Requirements: Python 3.9 or later (the scripts state this; the tests pass on 3.9,
3.10 and 3.14), standard library only. No model is run. The scripts read transcripts
and, for one of them, the git log; they never write to either, and they import no
networking module.

## What each script answers

**`usage_report.py`: where the tokens went.** It attributes the usage of each
API call to its session and to an errand, and writes the per-call, per-session,
per-errand and per-context figures to CSV files. An errand is one unit of
delegated work as the Director and its Workers exchange it: for a Worker, it
opens at the delivery of an instruction and closes at the Worker's first
`send_to_peer`, the report it sends back; for the Director, a solo session and a
subagent, a turn opens at each operator message or relay nudge and runs to the
next.

**`usage_whatif.py`: counterfactuals.** It estimates what the same transcripts
would have cost under two alternatives, and says nothing about a setup that did
not happen. A: a fresh Worker for each errand instead of one long-lived Worker
(the baseline context is the session's first call, the median post-compaction
context, or the smaller of the two). B: a compaction whenever a call's context
would pass a threshold T. It also splits each Director session's turns by kind
(operator message or relay nudge).

**`ac_measure.py`: before and after a change.** It takes a moment T, the time a
change in compaction behaviour took effect (for example a setting such as
`autoCompactWindow`), and compares each session's calls since T with the calls
before it; a like-for-like comparison uses only the hours just before T
(`--like-for-like-hours`, default 3). It lists the compactions since T and what
they cost. For the Worker sessions selected for the per-errand sections (`--session`,
or by default every Worker session with a compaction at or after T), it adds their
errands and the files re-read after a compaction. It also reports what started each
compaction. It prints a report and writes no file. The summarising call that a
compaction makes is not in any transcript, so its cost is estimated from stated
assumptions, shown separately in its section 3. That estimate is priced with the
multiplier of an inferred model: the session's last call with tokens before the
compaction, else its first such call after. Zero-token placeholder calls are skipped.
Each row names the model with "(inferred)", or says "no model found (counts 1.0)".

**`quality_report.py`: per change.** For one banto brigade it groups the git
commits of a repository into changes (commits no more than `--merge-gap-min`
minutes apart form one change) and, for each change, gives the errands by kind
(implementation, review, docs, measurement/investigation, other), the review
verdicts, the rework rounds, the weighted cost per change by session label, and the
fix commits that touched the same files within `--fix-days`. It also writes two
candidate lists that a person must read and judge, each to its own file:
**(a)** `candidates_a.txt`: operator messages with a problem word within
`--horizon-h` hours after a review PASS or a release build, which may be problems
the review missed; **(b)** `candidates_b.txt`: Director messages, to the operator or
to a Worker, containing a retraction or correction phrase, not limited by
`--horizon-h`. Both lists are candidates, not findings.

**`consult_report.py`: how often Directors consult the Goinkyo.** For each Director
session and each day it counts the consultations the Director filed, split by `kind`
(`arbitration` or `design`), the `dismiss_goinkyo` calls, and how many consultations the
session filed in all. It exists to show whether a setting such as `[brigade]
goinkyo_consult`, which changes how readily a Director is told to consult, changes how often
it does. It prints a report and writes no file; the table holds counts and the first eight
characters of a session id, and no conversation text.

- **What is counted.** A consultation is a call of banto's MCP tool `consult_goinkyo`. In a
  Claude Code transcript that is a `tool_use` named `mcp__banto__consult_goinkyo`, its `kind`
  taken from `input.kind`. In a Codex rollout it is an `event_msg` record of type
  `mcp_tool_call_end` whose `invocation.server` is `banto` and `invocation.tool` is
  `consult_goinkyo`, its `kind` from `invocation.arguments`. A Director is a session that
  called any Director-only banto tool (`consult_goinkyo`, `dismiss_goinkyo`, `name_member`,
  `request_decision`, the same rule as `transcripts.py`), so a Director that never consulted
  still has a row of zeros on each day it was active. A subagent's file is left out.
- **Refused calls are not consultations.** banto refuses a consultation (a Goinkyo is
  already in the cell, a missing argument) with an error result. In Claude Code that is a
  `tool_result` with `is_error: true`, linked to its call by `tool_use_id`, in the call's own
  file or in any other Claude Code file the run reads (under the matching projects when
  `--project` is given): a refusal a resumed session wrote into a later file is found even
  when that file holds no Director-only call; in Codex it is `result.Ok.isError` true, or a `result.Err`. Such a call goes in the
  `refused` column and in no other. A call whose result cannot be found, or in Codex has a
  result of a shape not seen, is **counted** (the benefit of the doubt) and also shown in the
  `nolink` column, so the count's softest part is visible. `dismiss_goinkyo` is treated the
  same way. A consultation that succeeded but whose `kind` is neither value is counted in
  `other`; banto's server does not accept one, so it should stay 0.
- **How a day is cut.** The report counts days in `--utc-offset`: `local` (the machine's own
  zone, the default) or an offset such as `+09:00`. The other scripts print UTC days; this one
  takes the zone the operator lives in, since a Director's day is the operator's, and says
  which it used. A negative offset is written with an equals sign
  (`--utc-offset=-05:00`), or the argument parser reads it as a flag. A call whose timestamp
  is missing, or cannot be moved into the zone (the machine's own zone refuses moments far
  from today on Windows, and a fixed offset overflows at the ends of the calendar), is
  counted in a row whose day is `?`, even with a window set, so the report never loses it.
  **`--since` and `--until` are UTC instants, not days in `--utc-offset`**: `--since` is
  inclusive, `--until` exclusive, and a bare date means 00:00 UTC. To window one JST day, put
  the zone in the bounds: `--utc-offset +09:00 --since 2026-11-07T15:00:00Z --until
  2026-11-08T15:00:00Z` is 8 November in Japan. They keep a record or call only when it
  falls in the window; the `session` column is the session's whole total, on any day.
- **A resumed session.** Claude Code can write earlier records again into the file of a
  resumed or forked session. A call belongs to the earliest file that holds its `tool_use` id
  (a Codex call, its `call_id`), a record to the earliest file with its `uuid`; the report
  says how many copies it dropped. Two files of one conversation are still two rows: a row is
  a session file, not a team. A record with no `uuid` (every Codex record) is matched by its
  whole line, so a copy that was rewritten rather than repeated is not recognised as one by
  record, though its calls still are by id. A file in which nothing of its own survives
  that dedupe is that file again: it gets no row and is not counted as a Director on any day.
  A resumed file that goes on keeps a row for each day of its own records, a day with no
  consultation included, and counts as a Director on those days.
- **What it cannot see.** A consultation that should have happened and did not: the count
  shows how often a Director asked, not how often it ought to have. It sees no Director that
  called no Director-only tool in its whole file, and it cannot say why a Director consulted
  or whether the consultation helped. "Active on a day" means the file has any record that
  day, a relay nudge included. The Codex side follows the `mcp_tool_call_end` shape: its
  structure was checked on real rollouts on 2026-11-17, for other banto tools
  (`check_messages`, `send_to_peer`, `brigade_status`). No real `consult_goinkyo` or
  `dismiss_goinkyo` call exists in any Codex rollout yet, so the reading of those two tools
  and of an `Err` result is built from that shape alone and has not met a real one. A Codex
  call that never wrote its end event (the process died mid-call) is not seen, and rollouts
  are read only from `sessions/` under the Codex root: an archived session kept elsewhere is
  not read. `--project` narrows the
  Claude Code side only, since Codex rollouts are not filed by project; `--no-codex` leaves
  them unread, and `--codex-root` overrides `$CODEX_HOME/sessions`.

### Effect metrics: what working as a brigade bought the operator

`quality_report.py` also puts seven effect figures beside each change in
`report.txt` and `changes.json` (under `effect`), a `DEFINITIONS` section and,
last in `report.txt`, a `WINDOW SUMMARY` (also `summary` in `changes.json`). They
are for comparing the same brigade across two windows (run the script twice with
different `--since`/`--until`) and for asking what a role contributes. They are
not a score and nothing adds them together. The ranking they serve is quality,
then speed, then operator effort, then cost.

1. **Caught by review.** Review rounds whose report says FAIL, and the findings
   listed in them. Summary: changes with at least one FAIL out of the changes that
   had any review, and the total FAIL rounds. It counts FAILs; it does not say any
   was right. Rounds with only a derived mark (`BLK*`) are counted apart. The
   adjacent stopped count includes both explicit FAIL and derived BLK* rounds.
   Its separate finding heuristic counts severity headings with content,
   F/N/H/W/B-number labels, and severity-qualified finding sentences containing
   a file:line reference. Bare severity headings and headings whose entire detail
   is `none`, `none found`, `0`, or `no` plus issue/problem/finding/blocker
   (singular or plural) do not count. For labels, clear-check words such as
   `fixed` or `resolved` suppress a count unless preceded within three words by
   a recognized negation such as `not` or `never`. Explicit labels deduplicate
   across messages; numbered severity headings are distinct per message.
   Follow-up messages before the next instruction are included, with the first
   report still ending the errand. The first explicit verdict across the messages
   takes precedence over derived marks; otherwise the first derived mark is used.
   These rules can miss unlabelled findings, count a labelled check as a finding,
   miss negations outside the three-word window, or double-count restated
   numbered findings. Reviewer neutrality has not been established. The previous
   fields remain beside them for comparison.
2. **Escaped review.** Two signals side by side: the operator-problem
   candidates (`candidates_a.txt`) and the later `fix` commits touching the
   change's files within `--fix-days` (they are different events and are not
   merged). Every candidate has an id (`A-` and
   `B-` plus eight hex digits) derived from the session id, the kind and the
   transcript record it was found in: that record's `uuid`, plus the text block index
   or the `send_to_peer` tool_use id inside it. Nothing else enters the id, so no
   other record at the same timestamp, matching or not, can rename it, and it is
   the same on every run over the same transcripts. **That relies on Claude Code
   appending transcript records and never rewriting them,** so a record's uuid,
   its block order and its tool_use ids do not change between runs; were a record
   ever rewritten, an id keyed by block index could move. Two fallbacks are **not
   stable**: a record with no `uuid` is keyed by its position among same-kind
   records at its timestamp (it moves when a record at that timestamp is added or
   removed), and a `send_to_peer` block with no tool_use id by the position of the
   block in its record. The id keeps 32 bits of a hash, so two candidates can share
   one by chance: a run counts such collisions in the summary (`id_collisions`,
   normally 0) and warns on stderr, and a judgment naming a colliding id is
   reported as ambiguous and applied to neither candidate. The id is printed on its
   line in both candidate files. **Candidate counts and judged counts are
   different things:** without `--judgments` every number is how many lines a
   keyword matched; with it, each candidate is also counted real, not, unsure or
   unjudged by the line a person wrote for it.
3. **Director reversals by trigger.** Each (b) candidate is classified by what
   started the Director's turn in which it was written: `operator` (the last thing
   to start a turn was a message the operator typed), `worker` (a relay nudge, or a
   `check_messages` answer carrying a Worker's or Goinkyo's message), `self`
   (neither). Tool results, system text and the Director's own text do not start a
   turn. **By turn cannot see self-correction:** a turn that an operator message or a
   Worker report started counts as that trigger even when the Director found the
   error itself, so `self` is nearly empty by construction. A human who reads the
   candidate records self in the judgments note. A secondary column, `by
   predecessor`, names the most recent input of any kind before the message (a tool
   result or system text counts as `self`); it is mostly `self`, because a Director
   writes most of its text right after a tool call, and it is kept only to show
   that. Counts per trigger, and judged counts per trigger when `--judgments`
   covers them. It says what the Director answered, not why it was wrong.
4. **Lead time.** The first errand that reported, opened in the change's window,
   to the change's last commit; empty (and said so) when the change had no errand or
   none that reported. Summary: median and maximum. Waiting for the operator is
   inside it.
5. **Parallelism.** Of the time at least one errand was open in the change's window,
   the share with two or more open at once. Errands that only touch are not
   parallel. Empty when no errand was open. **Errands that never reported are left
   out of 4 and 5:** an errand whose Worker never sent a report has no real end (it
   stays open to that Worker's last record, which once made three such errands
   dominate a whole window's share). Each is counted once, in the change it started
   in, per change and in the summary (`left out`). Every other use of errands (kinds,
   reviews, rework, cost) still includes them. The per-change count assigns an
   errand by the change it started in, once; an errand that starts at or before
   `--since` is owned by no change, so no per-change count has it. The summary
   counts every left-out errand whose time was excluded from parallelism or that a
   change owns, and says how many of them no change owns (`unowned`).
6. **Operator effort.** Operator-typed messages to the Director in the window, and
   `request_decision` calls the Director made. Commit `1b44854` introduced the
   tool at 2026-11-15 10:43:38 UTC. Changes starting before that time, including
   changes that straddle it, show N/A; the summary uses only changes starting at
   or after that time for its decision count and per-change denominator. An operator-typed message is a user
   record in the Director's transcript that is not a tool result, not meta, not a
   compaction summary, has origin human or none, and does not start with `[banto`
   (a relay nudge or a line banto typed into a pane) or `<` (a tag the harness
   wrote); an operator message that itself begins with `<` is missed.
7. **Cost.** The existing weighted cost of the change, shown beside the rest.

The problem-word list (`PROB_WORDS`, candidates (a)) and the retraction-phrase list
(`RETRACT_PHRASES`, candidates (b)) were tuned once against a person's first-pass
judgments of one window's candidates: a word or phrase that matched no real or
unsure candidate and several judged-not ones was dropped, and every real or unsure
candidate is still matched. That is one window and one judge, a first pass rather
than a validation; the comments above the lists in `quality_report.py` say which
entries went and why. Ids do not depend on which word matched, so a judgments file
written against the uuid-based ids still applies after such a change (an id whose
candidate no longer matches is reported as a warning).

`--judgments FILE` is plain text, one line per candidate:
`<id> <real|not|unsure> [free note]`. Blank lines and lines starting with `#` are
ignored; an id no candidate of the run has, a malformed line, or a repeated id is a
warning (on stderr and in the summary), never an error. Judging is a manual step:
read the candidate on its line, decide, write the line. The judgments file holds no
conversation text unless you write some in a note, so keep notes free of it.

## How to run

Each script is run as `python tools/measure/<script>.py`. Full flag lists are in
`--help`; the flags they share are these.

- `--root DIR`: the transcript root. Default: `$CLAUDE_CONFIG_DIR/projects` if that
  variable is set, else `~/.claude/projects`. The `CLAUDE_CONFIG_DIR` part is
  inferred from Claude Code's documentation and has not been exercised here.
- `--project TEXT`: only project directories whose name contains TEXT.
- `--since` and `--until`: ISO-8601 UTC bounds. In `usage_report` and
  `usage_whatif`, `--since` (default: all history) leaves out calls before it, and
  `--until` leaves out calls at or after it. `ac_measure` requires `--since`. In
  `quality_report` the bounds apply to commits, and `--since` defaults to the
  Director's first record, not to all history: commits before it are ignored.
  Accepted forms: a date alone, or a date and time separated by `T` or a space, with
  an optional fraction and a `Z` or `±hh:mm` offset, whose hours are 00 to 23 and
  minutes 00 to 59; converted to UTC. The moment must fall from `0002-01-01T00:00:00Z`
  through `9998-12-31T00:00:00Z` inclusive, otherwise (or if unreadable or impossible)
  it is refused with exit 2. A transcript record whose timestamp has an impossible
  offset reads as having no timestamp; one whose timestamp overflows the calendar once
  its offset is applied is skipped and counted like any malformed line.
- `--out DIR` and `--force-out`: where the outputs go. `usage_report` and
  `quality_report` default to a new directory created for each run, named
  `<script>-<YYYYmmdd-HHMMSS>-<random suffix>` inside `banto-measure/` under the system
  temp directory, and print its path; the suffix keeps two runs in the same second
  apart. The directory is private to the user where the OS supports it (Python makes
  it mode 0700 on POSIX; Windows does not apply that mode). `usage_whatif` writes only
  `whatif_A.csv`, and only when `--out` is given. `ac_measure` has no `--out`.
  An `--out` inside an agent home (`~/.claude`, `~/.codex`, `$CLAUDE_CONFIG_DIR`,
  `$CODEX_HOME`) is refused, whatever `--force-out` says, because those are
  read-only. An output directory inside a git working tree is refused unless
  `--force-out` is given. Each output file is written to a new temporary file beside
  it and moved onto its name when it is complete, so an existing file of that name,
  a hard link included, is replaced and never written through. A file that fails
  part-way leaves the previous file of its name as it was and no temporary file
  behind; files finished earlier in the same run stay replaced. A symlink at an
  output name is refused with exit 2. A failure to create the output directory, or to
  write or replace an output, prints `cannot create the output directory <dir>: <reason>`
  or `cannot write <path>: <reason>` to stderr and exits 2, with no traceback; the file
  at that name is left as it was.
- `--wcw`, `--wcr`, `--wout`: the weights of cache-creation, cache-read and output
  tokens (defaults 1.25, 0.025 and 5; each 0 to 1000). See the next section.
- `--model-weight KEY=MULT`: a multiplier, 0 to 1000, for every model whose name
  contains KEY (case-insensitive; the longest matching key wins). Repeatable; it
  replaces a default with the same KEY or adds a new one. KEY must be non-empty and
  `=` is required.

Each script's own flags are in its `--help`. Two to know: `ac_measure` requires
`--since T`, and `--session` (repeatable) selects the Worker sessions for its
per-errand sections; `quality_report` takes `--director` and `--brigade` to pick
the team, and `--role-name NAME=KIND` to map a display name to an errand kind.

Numeric flags are bounded, and a value outside its bound is refused with exit 2;
each bound is in that flag's `--help`. The script-specific ones: `--like-for-like-hours`
0.01 to 8760; `--merge-gap-min` 0 to 525600; `--fix-days` 0 to 365; `--horizon-h`
0 to 8760; `--top` and `--min-compactions` whole numbers 0 to 1000000; `--thresholds`
a non-empty comma-separated list of whole numbers 1 to 1000000000.

## The weights are an assumption

The weighted cost of a call is its input tokens, plus its cache-creation tokens
times `--wcw`, plus its cache-read tokens times `--wcr`, plus its output tokens
times `--wout`, all times its model's multiplier. Those weights are an assumption
about how a subscription counts usage, not a known fact: the only check against a
plan's real accounting is the small fit described below. Set them to whatever you
believe your plan charges for, and read every weighted figure as relative to the
others, not as a usage number. Each script's `--help` and report say the same.

The default multipliers, relative to Opus, are opus 1, sonnet 0.68, fable 4 and
haiku 0.68. A key matches a model name when it occurs in the name, ignoring case; a
model matching no key weighs 1.0. Such models are named in the report's "models
matching no multiplier" line and in `unmatched_models` in `changes.json`. Calls with
no tokens are not listed.

Where the defaults come from: an unofficial outside report of measurements of
subscription usage headers (posted on Reddit), not anything Anthropic publishes.
That source is not verified. Fable's 4, relative to Sonnet's 0.68, was fitted to
usage-screen readings: on one machine the defaults fitted five of six readings within
the display's rounding (two screenshots of three bars, 2026-11-15 UTC). The Opus:Sonnet
ratio (1 : 0.68) is the outside report's and is unchecked here. Haiku is unknown: its
0.68 is a placeholder copied from Sonnet's, so that it is never silently 1.0.

Every CSV that carries a weighted cost ends with a `weights_assumed` column naming
the weights and multipliers, so a file shared on its own still shows them.
`usage_report` writes it in `calls.csv`, `errands.csv` and `sessions.csv`;
`usage_whatif` in `whatif_A.csv`. `context_series.csv` has no weighted column.
`quality_report`'s `changes.json` is an object with `assumption`, `weights`,
`unmatched_models`, `changes` (each with an `effect` object) and `summary` keys.

## Privacy

Transcripts are read only. The scripts never write to them or copy them, and they
import no networking module.

Their outputs contain real session metadata, and some contain excerpts of real
conversation: `quality_report`'s `candidates_a.txt`, `candidates_b.txt` and
`errands_dump.txt` hold message text. Keep every output directory out of any
repository. The default location is outside the repository. A directory inside a git
working tree is refused unless you pass `--force-out`, and that flag exists for the
case where you have decided otherwise. A directory inside an agent home is refused
even with `--force-out`. While a run writes, its temporary file, holding the same
text, sits beside the output until it is moved into place; a failed run removes it.
Never commit these files.

## What the scripts cannot see

- **Codex sessions.** Their transcripts carry no per-call usage, so the token and cost
  scripts do not count them. Only `consult_report.py` reads Codex rollouts, and only for
  Goinkyo consultations.
- **The summarising call of a compaction.** It is not in any transcript; `ac_measure`
  estimates it, with the model inferred as described above, and says so.
- **Real billing or limits.** Nothing here reads a plan's usage, quota or invoice.
- **Meaning.** Errand kinds, review verdicts, retractions and the two candidate
  lists are keyword heuristics tuned on one team's phrasing, under the named
  constants at the top of `quality_report.py` and `transcripts.py`. Every count they
  give is a candidate count for a person to read, not a measurement.

## Running the tests

    python -m unittest discover -s tools/measure/tests

The tests use synthetic fixtures only (`tests/synth.py`), never real transcripts.
CI runs them on Linux only, in the step named `Measurement tools tests` in
`.github/workflows/ci.yml`.
