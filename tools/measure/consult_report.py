#!/usr/bin/env python3
"""Count how often each banto Director consulted the Goinkyo, per day, from the
local transcripts alone. Counts only: no conversation text is read into the
output, and no model is run.

A consultation is a call of banto's MCP tool `consult_goinkyo` by a Director:
in a Claude Code transcript a `tool_use` named `mcp__banto__consult_goinkyo`, in a
Codex rollout an `event_msg` of type `mcp_tool_call_end` whose invocation is the
tool `consult_goinkyo` of the MCP server `banto`. The tool's `kind` argument
("arbitration" or "design") splits the count. A call whose result carries an error
(a refusal: a Goinkyo already present, a bad argument) is not a consultation and is
counted apart. The same is done for `dismiss_goinkyo`.

Reads the transcripts READ-ONLY (Claude Code: $CLAUDE_CONFIG_DIR/projects, else
~/.claude/projects; Codex: $CODEX_HOME/sessions, else ~/.codex/sessions) and
prints a report. It writes no file. Needs Python 3.9+, standard library only.
"""
import argparse
import glob
import hashlib
import json
import os
import re
import sys
from collections import defaultdict
from datetime import datetime, timedelta, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import transcripts as tr  # noqa: E402

CONSULT, DISMISS = "consult_goinkyo", "dismiss_goinkyo"
KINDS = ("arbitration", "design")
# The MCP server name banto registers itself under in Codex, the counterpart of the
# `mcp__banto__` prefix Claude Code gives its tools.
CODEX_SERVER = "banto"

# A cheap line filter: only a file with a line that could hold a Director-only
# banto tool call is read in full.
_NAMES = "|".join(re.escape(n) for n in sorted(tr.DIRECTOR_ONLY_TOOLS))
CLAUDE_SCREEN = re.compile(re.escape(tr.BANTO_TOOL_PREFIX) + "(?:%s)" % _NAMES)
CODEX_SCREEN = re.compile(r'"(?:%s)"' % _NAMES)

_OFFSET_RX = re.compile(r"^([+-])(\d{1,2})(?::?(\d{2}))?$")
_BAD_RECORD = (TypeError, ValueError, AttributeError, OverflowError)


class Attempt:
    """One call of consult_goinkyo or dismiss_goinkyo."""
    __slots__ = ("id", "tool", "ts", "kind", "status")

    def __init__(self, ident, tool, ts, kind):
        self.id, self.tool, self.ts, self.kind = ident, tool, ts, kind
        self.status = "unlinked"     # becomes "ok" or "refused" when a result is found


class Sess:
    """What is read from one transcript file."""

    def __init__(self, agent, path, ident):
        self.agent, self.path, self.id = agent, path, ident
        self.director = False
        self.attempts = []           # Attempt, in file order
        self.results = {}            # tool_use id -> True when the result is an error (Claude)
        self.stamps = []             # (record key, timestamp or None) of every record
        self.first_ts = self.last_ts = None
        self.lines = self.skipped = 0


# ---------------------------------------------------------------------------
# reading
# ---------------------------------------------------------------------------

def _screened(path, rx):
    """Whether any line of `path` matches `rx`. A plain scan, no JSON."""
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        return any(rx.search(line) for line in fh)


def _arguments(value):
    """Codex gives an invocation's arguments as an object; an argument string of JSON is read
    the same way, anything else reads as no arguments."""
    if isinstance(value, str):
        try:
            value = json.loads(value)
        except ValueError:
            return {}
    return tr.as_dict(value)


def _kind(value):
    return value if isinstance(value, str) else None


def _read_lines(s):
    """(record, line) for every line of the file that is a JSON object."""
    with open(s.path, "r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            s.lines += 1
            try:
                d = json.loads(line)
            except ValueError:
                s.skipped += 1
                continue
            if not isinstance(d, dict):
                s.skipped += 1
                continue
            yield d, line


def _record_key(d, line):
    """What names a record across files: its uuid when it has one (Claude Code writes one on
    every record), else a hash of its line. A copy of a record in another file is then found
    the same way for both agents; a copy that was rewritten, and so differs, is not."""
    uid = d.get("uuid")
    if isinstance(uid, str) and uid:
        return uid
    return hashlib.md5(line.rstrip("\r\n").encode("utf-8", "replace")).hexdigest()


def _note_time(s, ts):
    if ts is None:
        return
    if s.first_ts is None or ts < s.first_ts:
        s.first_ts = ts
    if s.last_ts is None or ts > s.last_ts:
        s.last_ts = ts


def read_claude(path, root):
    parts = os.path.relpath(path, root).replace("\\", "/").split("/")
    s = Sess("claude", path, os.path.splitext(parts[-1])[0])
    for d, line in _read_lines(s):
        try:
            ts = tr.parse_ts(d.get("timestamp"))
            attempts, director, results = _claude_blocks(d, ts)
        except _BAD_RECORD:
            s.skipped += 1
            continue
        s.stamps.append((_record_key(d, line), ts))
        _note_time(s, ts)
        s.attempts.extend(attempts)
        for ident, is_error in results:
            s.results.setdefault(ident, is_error)
        s.director = s.director or director
    return s


def _claude_blocks(d, ts):
    """(Attempts, director, results) of one record. Attempts are the consult_goinkyo and
    dismiss_goinkyo tool_use blocks of an assistant record; `director` says the record holds any
    Director-only banto tool_use at all; results are (tool_use id, is_error) for every tool_result
    block of a user record."""
    kind = d.get("type")
    content = tr.as_dict(d.get("message")).get("content")
    if not isinstance(content, list):
        return [], False, []
    attempts, director, results = [], False, []
    for b in content:
        if not isinstance(b, dict):
            continue
        if kind == "assistant" and b.get("type") == "tool_use":
            name = b.get("name")
            if not isinstance(name, str) or not name.startswith(tr.BANTO_TOOL_PREFIX):
                continue
            short = name[len(tr.BANTO_TOOL_PREFIX):]
            if short not in tr.DIRECTOR_ONLY_TOOLS:
                continue
            director = True
            if short in (CONSULT, DISMISS):
                ident = b.get("id") if isinstance(b.get("id"), str) and b.get("id") else None
                attempts.append(Attempt(ident, short, ts, _kind(tr.as_dict(b.get("input")).get("kind"))))
        elif kind == "user" and b.get("type") == "tool_result":
            ident = b.get("tool_use_id")
            if isinstance(ident, str) and ident:
                results.append((ident, b.get("is_error") is True))
    return attempts, director, results


def read_codex(path):
    stem = os.path.splitext(os.path.basename(path))[0]
    s = Sess("codex", path, stem[-36:] if len(stem) >= 36 else stem)
    for d, line in _read_lines(s):
        try:
            ts = tr.parse_ts(d.get("timestamp"))
            attempt = _codex_attempt(d, ts)
        except _BAD_RECORD:
            s.skipped += 1
            continue
        s.stamps.append((_record_key(d, line), ts))
        _note_time(s, ts)
        if attempt is not None:
            s.attempts.append(attempt)
            if attempt.tool in tr.DIRECTOR_ONLY_TOOLS:
                s.director = True
    return s


def _codex_attempt(d, ts):
    """The Attempt in a Codex rollout record, or None. One `mcp_tool_call_end` record holds the
    call and its result together, so the result is read here: `result: {"Ok": {"isError": ...}}`
    is an answer (an error when isError is true) and `{"Err": ...}` is a failure; any other
    shape leaves the call unlinked."""
    if d.get("type") != "event_msg":
        return None
    p = tr.as_dict(d.get("payload"))
    if p.get("type") != "mcp_tool_call_end":
        return None
    inv = tr.as_dict(p.get("invocation"))
    tool = inv.get("tool")
    if inv.get("server") != CODEX_SERVER or tool not in tr.DIRECTOR_ONLY_TOOLS:
        return None
    ident = p.get("call_id") if isinstance(p.get("call_id"), str) and p.get("call_id") else None
    a = Attempt(ident, tool, ts, _kind(_arguments(inv.get("arguments")).get("kind")))
    result = p.get("result")
    if isinstance(result, dict):
        if "Err" in result:
            a.status = "refused"
        elif isinstance(result.get("Ok"), dict) and isinstance(result["Ok"].get("isError"), bool):
            a.status = "refused" if result["Ok"]["isError"] else "ok"
    return a


def load(claude_root, codex_root, project=None):
    """Every Director session under the two roots, ordered by first timestamp."""
    found = []
    if claude_root and os.path.isdir(claude_root):
        for f in tr.find_session_files(claude_root, project):
            if "subagents" in os.path.relpath(f, claude_root).replace("\\", "/").split("/"):
                continue
            found.append(("claude", f))
    if codex_root and os.path.isdir(codex_root) and not project:
        for f in sorted(glob.glob(os.path.join(codex_root, "**", "rollout-*.jsonl"), recursive=True)):
            found.append(("codex", f))
    sessions, others = [], []
    for agent, f in found:
        try:
            if not _screened(f, CLAUDE_SCREEN if agent == "claude" else CODEX_SCREEN):
                if agent == "claude":
                    others.append(f)
                continue
            s = read_claude(f, claude_root) if agent == "claude" else read_codex(f)
        except OSError as err:
            print(f"cannot read {f}: {err}", file=sys.stderr)
            continue
        if s.director:
            sessions.append(s)
        elif agent == "claude":
            others.append(f)
    # A result can sit in a later file than its call (a session resumed into a new file), and
    # that file need not hold any Director-only call: the files that were not read as a Director's
    # are searched for the results of the calls still waiting for one.
    results = {}
    for s in sessions:
        for ident, is_error in s.results.items():
            results.setdefault(ident, is_error)
    waiting = {a.id for s in sessions if s.agent == "claude" for a in s.attempts if a.id and a.id not in results}
    if waiting:
        found_in_others(others, waiting, results)
    for s in sessions:
        for a in s.attempts:
            if s.agent == "claude" and a.id in results:
                a.status = "refused" if results[a.id] else "ok"
    # The file that holds a record first owns it; a resumed file starts by repeating the earlier
    # one, so it starts no later and ends later, and the last timestamp breaks that tie.
    late = datetime.max.replace(tzinfo=timezone.utc)
    sessions.sort(key=lambda s: (s.first_ts or late, s.last_ts or late, s.path))
    return sessions


def found_in_others(files, waiting, results):
    """Fill `results` with the tool_result of each id in `waiting` that a file in `files` holds.
    A plain substring screen first, so only lines that can hold one are parsed."""
    rx = re.compile("|".join(re.escape(i) for i in sorted(waiting)))
    for f in files:
        try:
            with open(f, "r", encoding="utf-8", errors="replace") as fh:
                for line in fh:
                    if '"tool_result"' not in line or not rx.search(line):
                        continue
                    try:
                        d = json.loads(line)
                        _attempts, _director, found = _claude_blocks(d, None)
                    except (ValueError, *_BAD_RECORD):
                        continue
                    for ident, is_error in found:
                        if ident in waiting:
                            results.setdefault(ident, is_error)
        except OSError as err:
            print(f"cannot read {f}: {err}", file=sys.stderr)


def drop_copies(sessions):
    """A resumed or forked Claude session can repeat earlier records in a later file. A tool_use
    id (or a Codex call id) belongs to the earliest file that has it, a record key likewise (its
    uuid, else a hash of its line); the later copies are dropped. Returns (attempts dropped, records dropped)."""
    seen_ids, seen_keys = set(), set()
    gone_a = gone_r = 0
    for s in sessions:
        # only an earlier file's keys count: a repeat inside one file is that file's own business
        mine_ids, mine_keys = set(), set()
        keep = []
        for a in s.attempts:
            if a.id is not None:
                if (s.agent, a.id) in seen_ids:
                    gone_a += 1
                    continue
                mine_ids.add((s.agent, a.id))
            keep.append(a)
        s.attempts = keep
        stamps = []
        for key, ts in s.stamps:
            if key in seen_keys:
                gone_r += 1
                continue
            mine_keys.add(key)
            stamps.append((key, ts))
        s.stamps = stamps
        seen_ids |= mine_ids
        seen_keys |= mine_keys
    return gone_a, gone_r


# ---------------------------------------------------------------------------
# counting
# ---------------------------------------------------------------------------

def parse_offset(text):
    """'local' (the machine's own zone, DST included) or +hh:mm / -hh:mm / +h; None if unreadable."""
    if text == "local":
        return "local"
    m = _OFFSET_RX.match(text or "")
    if not m:
        return None
    hours, minutes = int(m.group(2)), int(m.group(3) or 0)
    if hours > 14 or minutes > 59:
        return None
    delta = timedelta(hours=hours, minutes=minutes)
    return timezone(-delta if m.group(1) == "-" else delta)


def day_of(ts, zone):
    """The calendar day of `ts` in `zone`, or "?" when there is none to give: no timestamp, or one
    that cannot be moved into the zone (the machine's own zone refuses moments far from today on
    Windows, and a fixed offset overflows at the ends of the calendar). One odd record must not
    cost the report."""
    if ts is None:
        return "?"
    try:
        return (ts.astimezone() if zone == "local" else ts.astimezone(zone)).date().isoformat()
    except (OSError, OverflowError, ValueError):
        return "?"


class Row:
    """One Director session on one day."""
    __slots__ = ("day", "session", "arbitration", "design", "other_kind", "dismissed", "refused", "unlinked", "total")

    def __init__(self, day, session):
        self.day, self.session = day, session
        self.arbitration = self.design = self.other_kind = self.dismissed = self.refused = self.unlinked = 0
        self.total = 0

    @property
    def consults(self):
        return self.arbitration + self.design + self.other_kind


def counted_consults(s):
    """Consultations the whole session filed: everything that is not a refusal."""
    return sum(1 for a in s.attempts if a.tool == CONSULT and a.status != "refused")


def build_rows(sessions, since, until, zone):
    """One Row per (Director session, day) on which the session had any record or call inside
    the window. A day with records and no consultation is a row of zeros: it is the rate that
    matters, and an absent consultation is only visible as a zero."""
    rows = {}

    def inside(ts):
        return ts is not None and (since is None or ts >= since) and (until is None or ts < until)

    for s in sessions:
        total = counted_consults(s)
        # drop_copies has already taken every record an earlier file holds: a file that is only a
        # copy has no stamps left and so no row, while a resumed file that goes on keeps the days
        # of its own records, a day with no consultation included
        for _key, ts in s.stamps:
            if inside(ts):
                day = day_of(ts, zone)
                if day != "?":      # a record gives a day only to show the Director was there
                    rows.setdefault((day, s.agent, s.id), Row(day, s)).total = total
        for a in s.attempts:
            # a call with no timestamp cannot be placed in a window or a day: it is in the "?" row
            if a.tool not in (CONSULT, DISMISS) or (a.ts is not None and not inside(a.ts)):
                continue
            key = (day_of(a.ts, zone), s.agent, s.id)
            row = rows.setdefault(key, Row(key[0], s))
            row.total = total
            if a.status == "refused":
                row.refused += 1
                continue
            if a.status == "unlinked":
                row.unlinked += 1
            if a.tool == DISMISS:
                row.dismissed += 1
            elif a.kind == "arbitration":
                row.arbitration += 1
            elif a.kind == "design":
                row.design += 1
            else:
                row.other_kind += 1
    return [rows[k] for k in sorted(rows)]


# ---------------------------------------------------------------------------
# report
# ---------------------------------------------------------------------------

def render(args, zone, since, until, sessions, rows, dropped):
    out = []
    P = out.append
    P("GOINKYO CONSULTATION REPORT (counts from local transcripts; no model was run, no conversation text is printed)")
    if zone == "local":
        try:
            stamp = (until or datetime.now(timezone.utc)).astimezone().strftime("%z")
            P(f"days: the machine's own zone, {stamp[:3]}:{stamp[3:]} at the end of the window (--utc-offset sets another)")
        except (OSError, OverflowError, ValueError):
            P("days: the machine's own zone (its offset at the end of the window cannot be read; --utc-offset sets another)")
    else:
        P(f"days: UTC{args.utc_offset}")
    P(f"window: since {tr.iso(since) or '-'} until {tr.iso(until) or '-'}; project filter {args.project or '-'}")
    by_agent = defaultdict(int)
    for s in sessions:
        by_agent[s.agent] += 1
    P(f"Director sessions read: {len(sessions)} (claude {by_agent['claude']}, codex {by_agent['codex']}); "
      f"lines {sum(s.lines for s in sessions)}; SKIPPED (broken) {sum(s.skipped for s in sessions)}; "
      f"copies dropped: {dropped[0]} calls, {dropped[1]} records")
    P("")
    P("PER DIRECTOR SESSION PER DAY")
    P("  arb/design: consultations by kind. other: a consultation whose kind is neither. dismissed: dismiss_goinkyo calls.")
    P("  refused: calls whose result was an error (not counted). nolink: counted, but no result could be found.")
    P("  session: consultations the session filed in all, on any day (not limited to the window).")
    P(f"  {'day':10} {'agent':6} {'session':8} {'arb':>4} {'design':>6} {'other':>5} {'dismissed':>9} {'refused':>7} {'nolink':>6} {'session':>7}")
    for r in rows:
        P(f"  {r.day:10} {r.session.agent:6} {r.session.id[:8]:8} {r.arbitration:>4} {r.design:>6} {r.other_kind:>5} "
          f"{r.dismissed:>9} {r.refused:>7} {r.unlinked:>6} {r.total:>7}")
    if not rows:
        P("  (no Director session had a record in the window)")
    P("")
    P("PER DAY, ALL DIRECTORS")
    P(f"  {'day':10} {'directors':>9} {'arb':>4} {'design':>6} {'other':>5} {'dismissed':>9} {'refused':>7} {'nolink':>6}")
    per_day = defaultdict(lambda: [set(), 0, 0, 0, 0, 0, 0])
    for r in rows:
        a = per_day[r.day]
        a[0].add((r.session.agent, r.session.id))
        for i, v in enumerate((r.arbitration, r.design, r.other_kind, r.dismissed, r.refused, r.unlinked), 1):
            a[i] += v
    for day in sorted(per_day):
        a = per_day[day]
        P(f"  {day:10} {len(a[0]):>9} {a[1]:>4} {a[2]:>6} {a[3]:>5} {a[4]:>9} {a[5]:>7} {a[6]:>6}")
    P("")
    P("WHAT THIS CANNOT SEE")
    P("  A consultation that should have happened and did not. A Director is any session that called a Director-only")
    P("  banto tool (consult_goinkyo, dismiss_goinkyo, name_member, request_decision, open_planner, close_planner) at least once, so one that never")
    P("  did is absent, and a day with only records of a relay nudge counts as a day the Director was active. A session")
    P("  resumed into a new file is a new row: a row is a file, not a team.")
    return "\n".join(out)


def default_codex_root():
    base = os.environ.get("CODEX_HOME")
    base = os.path.expanduser(base) if base else os.path.join(os.path.expanduser("~"), ".codex")
    return os.path.join(base, "sessions")


def build_parser():
    ap = argparse.ArgumentParser(
        prog="consult_report.py",
        description="Count Goinkyo consultations per Director per day from local Claude Code transcripts and Codex rollouts.",
        epilog="Prints the report to stdout and writes no file.",
        formatter_class=argparse.RawDescriptionHelpFormatter)
    tr.add_root_args(ap)
    ap.add_argument("--codex-root", default=None,
                    help="Codex rollout root (default: $CODEX_HOME/sessions if set, else ~/.codex/sessions)")
    ap.add_argument("--no-codex", action="store_true", help="do not read Codex rollouts")
    ap.add_argument("--since", help="ISO-8601 UTC; records and calls before it are left out (default: all history)")
    ap.add_argument("--until", help="ISO-8601 UTC; records and calls at or after it are left out (default: all history)")
    ap.add_argument("--utc-offset", default="local",
                    help="the zone a day is counted in: 'local' (the machine's own zone, the default) or an offset "
                         "such as +09:00 for JST (hours 0 to 14); write a negative one with an equals sign, "
                         "--utc-offset=-05:00, or the parser reads it as a flag")
    return ap


def main(argv=None):
    args = build_parser().parse_args(argv)
    tr.reconfigure_stdout()
    since, until = tr.parse_flag_ts(args.since), tr.parse_flag_ts(args.until)
    if args.since and since is None or args.until and until is None:
        print("--since/--until must be ISO-8601, e.g. 2026-01-31 or 2026-01-31T12:00:00Z", file=sys.stderr)
        return 2
    zone = parse_offset(args.utc_offset)
    if zone is None:
        print("--utc-offset must be 'local' or an offset such as +09:00 or -05:30 (hours 0 to 14)", file=sys.stderr)
        return 2
    sessions = load(args.root or tr.default_root(), None if args.no_codex else (args.codex_root or default_codex_root()),
                    args.project)
    dropped = drop_copies(sessions)
    print(render(args, zone, since, until, sessions, build_rows(sessions, since, until, zone), dropped))
    return 0


if __name__ == "__main__":
    sys.exit(main())
