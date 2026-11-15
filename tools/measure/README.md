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
`unmatched_models` and `changes` keys.

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

- **Codex sessions.** Their transcripts carry no per-call usage, so they are not
  counted.
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
