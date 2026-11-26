#!/usr/bin/env python3
"""Count Claude Director turn triggers, visible assistant text, and decision calls.

Reads transcripts only and prints counts, never conversation text or files.
"""
import argparse
from collections import Counter
import sys

import transcripts as tr


TRIGGERS = ("operator-typed", "relay-woken", "other/unknown")


def turns_for(s, seen):
    """Segment the parser's ordered events before applying any time window."""
    turns = []
    current = None
    for index, ev in enumerate(s.events):
        kind = ev[0]
        if index in s.sidechain_events:
            continue
        key = s.keys.get(index)
        if key:
            if key in seen:
                continue
            seen.add(key)
        if kind == "user":
            _, ts, text, nudge, human, _tag, origin = ev
            if nudge:
                trigger = "relay-woken"
            elif human and origin == "human" and not text.lstrip().startswith("[banto"):
                trigger = "operator-typed"
            else:
                trigger = "other/unknown"
            current = {"start": ts, "trigger": trigger, "characters": 0, "lines": 0, "decisions": 0}
            turns.append(current)
        elif kind in ("atext", "decision"):
            if current is None:
                current = {"start": ev[1], "trigger": "other/unknown", "characters": 0, "lines": 0, "decisions": 0}
                turns.append(current)
            if kind == "atext":
                current["characters"] += len(ev[2])
                current["lines"] += len(ev[2].splitlines())
            else:
                current["decisions"] += 1
    return turns


def build_parser():
    ap = argparse.ArgumentParser(description="Count Claude Director turns and assistant text; prints no conversation text or files.")
    tr.add_root_args(ap)
    ap.add_argument("--since", help="ISO-8601 UTC; turns starting before it are left out (default: all history)")
    ap.add_argument("--until", help="ISO-8601 UTC; turns starting at or after it are left out (default: all history)")
    return ap


def main(argv=None):
    args = build_parser().parse_args(argv)
    tr.reconfigure_stdout()
    since, until = tr.parse_flag_ts(args.since), tr.parse_flag_ts(args.until)
    if (args.since and since is None) or (args.until and until is None):
        print("--since/--until must be ISO-8601, e.g. 2026-01-31 or 2026-01-31T12:00:00Z", file=sys.stderr)
        return 2
    sessions = tr.load_sessions(args.root or tr.default_root(), args.project, detail=True)
    seen = set()
    print("DIRECTOR TURNS (Claude transcripts; counts only, no conversation text)")
    print(f"window: since {tr.iso(since) or '-'} until {tr.iso(until) or '-'}; selected by turn start")
    for s in sessions:
        if s.role != "director":
            continue
        turns = [t for t in turns_for(s, seen) if tr.in_window(t["start"], since, until)]
        if not turns:
            continue
        print(f"\nsession {s.id} (project {s.project})")
        print("  start                        trigger          chars lines decision")
        totals = Counter()
        characters = Counter()
        lines = Counter()
        decisions = Counter()
        for t in turns:
            totals[t["trigger"]] += 1
            characters[t["trigger"]] += t["characters"]
            lines[t["trigger"]] += t["lines"]
            decisions[t["trigger"]] += t["decisions"]
            print(f"  {tr.iso(t['start']) or '?':28} {t['trigger']:16} {t['characters']:5} {t['lines']:5} "
                  f"{'yes' if t['decisions'] else 'no'}")
        for trigger in TRIGGERS:
            print(f"  {trigger}: {totals[trigger]} turns, {characters[trigger]} characters, "
                  f"{lines[trigger]} lines, {decisions[trigger]} request_decision calls")
        print(f"  operator-typed turns: {totals['operator-typed']}; request_decision calls: {sum(t['decisions'] for t in turns)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
