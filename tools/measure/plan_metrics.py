#!/usr/bin/env python3
"""Measure Claude Director inputs, reports, Progress and Plan notices."""
import argparse
from collections import defaultdict
import json
import re
import sys

import transcripts as tr


APPROVAL = re.compile(r"^The operator approved Plan (\d+), version (\d+),")
RELEASE = re.compile(r"^The Planner released Plan (\d+), version (\d+)\b")
CLOSE = re.compile(r"^The operator closed Plan (\d+)\b")
BRIGADE = re.compile(r"You are the (?:Director|Planner) of banto brigade (\d+)\.")


def brigade_of(path):
    match = BRIGADE.search(tr.first_snapshot_prompt(path))
    return match.group(1) if match else "?"


def operator(ev):
    _, _, text, nudge, human, _tag, origin = ev
    return human and not nudge and origin in (None, "human") and not text.lstrip().startswith("[banto")


def scan(s, root, seen_records, seen_results, known_turns):
    """Yield complete turns and committed tool/notice events from one file."""
    parsed = tr.Session(s.path, root)
    checks = set()
    pending = {}
    request_open = False
    current = None
    turns, timeline = [], []
    days = set()
    with open(s.path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            try:
                d = json.loads(line)
                if not isinstance(d, dict) or d.get("type") not in ("user", "assistant"):
                    continue
                uid = d.get("uuid")
                duplicate = isinstance(uid, str) and bool(uid) and uid in seen_records
                if isinstance(uid, str) and uid:
                    seen_records.add(uid)
                raw_content = tr.as_dict(d.get("message")).get("content")
                raw_blocks = raw_content if isinstance(raw_content, list) else []
                fresh_results = set()
                if d["type"] == "user":
                    message = tr.as_dict(d.get("message"))
                    content = message.get("content")
                    if isinstance(content, list):
                        filtered = []
                        for block in content:
                            if isinstance(block, dict) and block.get("type") == "tool_result":
                                ident = block.get("tool_use_id")
                                if isinstance(ident, str) and ident:
                                    if ident in seen_results:
                                        continue
                                    seen_results.add(ident)
                                    fresh_results.add(ident)
                            filtered.append(block)
                        d = dict(d, message=dict(message, content=filtered))
                ts = tr.parse_ts(d.get("timestamp"))
                if ts:
                    days.add(ts)
                start = len(parsed.events)
                tr._read_record(parsed, d, ts, checks, True)
            except (ValueError, TypeError, AttributeError, OverflowError):
                continue
            side = bool(d.get("isSidechain"))
            content = tr.as_dict(d.get("message")).get("content")
            blocks = content if isinstance(content, list) else []
            if d["type"] == "assistant" and not side:
                for b in blocks:
                    if not isinstance(b, dict) or b.get("type") != "tool_use":
                        continue
                    name, ident = b.get("name"), b.get("id")
                    if not isinstance(ident, str) or not isinstance(name, str):
                        continue
                    short = name.removeprefix(tr.BANTO_TOOL_PREFIX) if name.startswith(tr.BANTO_TOOL_PREFIX) else ""
                    if short in ("request_decision", "update_progress", "close_planner"):
                        pending[ident] = (short, tr.as_dict(b.get("input")), ts)
            if d["type"] == "user":
                for b in raw_blocks:
                    if not isinstance(b, dict) or b.get("type") != "tool_result":
                        continue
                    item = pending.pop(b.get("tool_use_id"), None)
                    if item is None or b.get("is_error") is True:
                        continue
                    name, args, at = item
                    if name == "request_decision":
                        request_open = True
                    elif name == "update_progress" and b.get("tool_use_id") in fresh_results:
                        items = args.get("off_plan", [])
                        if isinstance(items, list):
                            timeline.append((at, "progress", len(items)))
                    elif name == "close_planner" and b.get("tool_use_id") in fresh_results:
                            timeline.append((at, "director_close", None))
            for idx in range(start, len(parsed.events)):
                ev = parsed.events[idx]
                kind = ev[0]
                if idx in parsed.sidechain_events:
                    continue
                if kind == "user":
                    is_op = operator(ev)
                    trigger = "operator" if is_op else "relay" if ev[3] else "unknown"
                    current = known_turns.get(uid) if duplicate else None
                    if current is None:
                        current = {"start": ev[1], "trigger": trigger, "answer": bool(is_op and request_open),
                                   "suggestion": is_op and d.get("promptSource") == "suggestion_accepted",
                                   "reports": 0, "tokens": 0, "characters": 0}
                        if isinstance(uid, str) and uid:
                            known_turns[uid] = current
                        turns.append(current)
                    if is_op and request_open:
                        request_open = False
                elif kind == "call" and current is not None:
                    call = s.calls.get(ev[1])
                    if call is not None and not call.side and not call.dropped:
                        current["tokens"] += call.out
                elif kind == "atext" and current is not None and not duplicate:
                    current["characters"] += len(ev[2])
                elif kind == "deliver":
                    if current is None:
                        current = {"start": ev[1], "trigger": "unknown", "answer": False,
                                   "reports": 0, "tokens": 0, "characters": 0}
                        turns.append(current)
                    for sender, _name, _scope, body in ev[3]:
                        if re.fullmatch(r"worker-\d+", sender):
                            current["reports"] += 1
                        if sender == "banto":
                            match = APPROVAL.match(body) or RELEASE.match(body)
                            if match:
                                kind = "approval" if APPROVAL.match(body) else "release"
                                timeline.append((ev[1], kind, (int(match.group(1)), int(match.group(2)))))
                            else:
                                match = CLOSE.match(body)
                                if match:
                                    timeline.append((ev[1], "operator_close", int(match.group(1))))
    return turns, timeline, days


def build_parser():
    ap = argparse.ArgumentParser(description="Report Claude Director Plan metrics without printing conversation text.")
    tr.add_root_args(ap)
    ap.add_argument("--since", help="ISO-8601 UTC start, inclusive")
    ap.add_argument("--until", help="ISO-8601 UTC end, exclusive")
    return ap


def main(argv=None):
    args = build_parser().parse_args(argv)
    tr.reconfigure_stdout()
    since, until = tr.parse_flag_ts(args.since), tr.parse_flag_ts(args.until)
    if (args.since and since is None) or (args.until and until is None):
        print("--since/--until must be ISO-8601, e.g. 2026-01-31 or 2026-01-31T12:00:00Z", file=sys.stderr)
        return 2
    root = args.root or tr.default_root()
    sessions = tr.load_sessions(root, args.project, detail=True)
    tr.dedupe_calls(sessions)
    seen_records, seen_results, known_turns = set(), set(), {}
    active_days, all_turns, notices = set(), [], defaultdict(list)
    planner_inputs = 0
    print("PLAN METRICS (Claude transcripts; counts only, no conversation text)")
    print(f"window: since {tr.iso(since) or '-'} until {tr.iso(until) or '-'}; turns selected by start")
    for s in sessions:
        if s.role not in ("director", "planner"):
            continue
        turns, timeline, days = scan(s, root, seen_records, seen_results, known_turns)
        selected = [t for t in turns if tr.in_window(t["start"], since, until)]
        print(f"session {s.id} project {s.project} role {s.role} brigade {brigade_of(s.path)}")
        if s.role == "planner":
            planner_inputs += sum(t["trigger"] == "operator" for t in selected)
            continue
        all_turns.extend(selected)
        active_days.update(ts.date() for ts in days if tr.in_window(ts, since, until))
        key = brigade_of(s.path)
        if key == "?":
            key = "? session " + s.id
        notices[key].extend(timeline)
    answers = sum(t["answer"] for t in all_turns)
    own = sum(t["trigger"] == "operator" and not t["answer"] for t in all_turns)
    relay = [t for t in all_turns if t["trigger"] == "relay" and t["reports"]]
    reports = sum(t["reports"] for t in relay)
    total_reports = sum(t["reports"] for t in all_turns)
    operator_reports = sum(t["reports"] for t in all_turns if t["trigger"] == "operator")
    tokens = sum(t["tokens"] for t in relay)
    chars = sum(t["characters"] for t in relay)
    print(f"operator: {answers} answers, {own} own step-ins; {total_reports} Worker reports; {len(active_days)} active UTC days; Planner inputs {planner_inputs}")
    suggestions = [t for t in all_turns if t.get("suggestion")]
    suggested_answers = sum(t["answer"] for t in suggestions)
    print(f"suggestion-accepted operator turns: {len(suggestions)}; answers {suggested_answers}, own step-ins {len(suggestions) - suggested_answers}")
    ratio = f"{tokens / reports:.2f}" if reports else "N/A"
    print(f"relay turns with Worker reports: {len(relay)} turns, {reports} reports, {tokens} output tokens, {chars} pane characters; tokens/report {ratio}; reports in operator turns {operator_reports}")
    print("Progress and Plan notices (time, brigade, event; unknown Plan span shown by interleaving):")
    for brigade, events in sorted(notices.items()):
        seen_notices = set()
        approvals = defaultdict(set)
        releases = defaultdict(set)
        windowed = []
        for ts, kind, value in sorted(events, key=lambda event: (event[0] is None, event[0])):
            if kind in ("approval", "release"):
                notice_key = (kind, brigade, *value)
                if notice_key in seen_notices:
                    continue
                seen_notices.add(notice_key)
                (approvals if kind == "approval" else releases)[value[0]].add(value[1])
            if tr.in_window(ts, since, until):
                windowed.append((ts, kind, value))
                if kind == "progress":
                    print(f"  {tr.iso(ts) or '?'} brigade {brigade}: off-plan items {value}")
                elif kind == "director_close":
                    print(f"  {tr.iso(ts) or '?'} brigade {brigade}: Director close_planner call")
                elif kind == "operator_close":
                    print(f"  {tr.iso(ts) or '?'} brigade {brigade}: operator closed Plan {value}")
                else:
                    print(f"  {tr.iso(ts) or '?'} brigade {brigade}: Plan {value[0]} version {value[1]} {kind}")
        plans = {value[0] for _, kind, value in windowed if kind in ("approval", "release")}
        for number in sorted(plans):
            approved = {value[1] for _, kind, value in windowed if kind == "approval" and value[0] == number}
            released = {value[1] for _, kind, value in windowed if kind == "release" and value[0] == number}
            first = min(approvals[number]) if approvals[number] else None
            print(f"  brigade {brigade} Plan {number}: operator-approved revisions {len(approved) - int(first in approved)}, Planner releases {len(released)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
