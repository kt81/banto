#!/usr/bin/env python3
"""Attribute Claude Code token usage to sessions and to banto brigade errands,
from the local transcripts alone. Counts only; runs no model.

Reads the transcripts READ-ONLY (default root: $CLAUDE_CONFIG_DIR/projects, else
~/.claude/projects). Writes calls.csv, sessions.csv, errands.csv and
context_series.csv to --out (default: a new directory under the system temp
directory; refused inside a git working tree unless --force-out) and prints a
report. The outputs hold real session metadata: keep them out of any repository.

Needs Python 3.9+, standard library only.
"""
import argparse
import csv
import os
import statistics
import sys
from collections import Counter, defaultdict
from datetime import datetime, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import transcripts as tr  # noqa: E402

_MIN_TS = datetime.min.replace(tzinfo=timezone.utc)


LIMITS = (("top", "--top", 0, 1_000_000),)


def build_parser():
    ap = argparse.ArgumentParser(
        prog="usage_report.py",
        description="Attribute token usage to sessions and brigade errands from local Claude Code transcripts.",
        epilog="Prints the report to stdout and writes calls.csv, sessions.csv, errands.csv and context_series.csv "
               "to --out (default: a new directory under the system temp directory, its path printed).\n\n" + tr.ASSUMPTION,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    tr.add_root_args(ap)
    tr.add_out_args(ap)
    ap.add_argument("--since", help="ISO-8601 UTC; calls before it are left out (default: all history)")
    ap.add_argument("--until", help="ISO-8601 UTC; calls at or after it are left out (default: all history)")
    tr.add_weight_args(ap)
    ap.add_argument("--top", type=int, default=30, help="errands in the top table (0 to 1000000; default 30)")
    return ap


def errand_rows(sessions, errands):
    """One row per errand that has at least one in-window call."""
    by_id = {s.id: s for s in sessions}
    rows, ids = [], {}
    for e in errands:
        sess = by_id[e.session]
        calls = [sess.calls[m] for m in e.calls if not sess.calls[m].dropped and sess.calls[m].in_window]
        if not calls:
            continue
        ids[id(e)] = len(rows) + 1
        rows.append({
            "errand_id": len(rows) + 1, "session": e.session, "role": e.role, "kind": e.kind,
            "start": tr.iso(e.start), "end": tr.iso(e.end),
            "wall_seconds": int((e.end - e.start).total_seconds()) if e.end and e.start else 0,
            "calls": len(calls), "input": sum(c.inp for c in calls), "output": sum(c.out for c in calls),
            "cache_creation": sum(c.cc for c in calls), "cache_read": sum(c.cr for c in calls),
            "max_context": max(c.context for c in calls), "weighted": round(sum(c.weighted for c in calls), 1),
            "sends": e.sends, "open_at_end": e.open, "label": e.label,
        })
    return rows, ids


def session_rows(sessions, erows, since, until):
    rows = []
    for s in sessions:
        calls = tr.live_calls(s)
        if not calls:
            continue
        ctxs = [c.context for c in calls]
        models = Counter(c.model for c in calls)
        sess_err = [r for r in erows if r["session"] == s.id]
        between = [c for c in calls if c.where == "between"]
        comps = [x for x in s.compactions if x[0] and tr.in_window(x[0], since, until)]
        stamps = [c.ts for c in calls if c.ts]
        rows.append({
            "session": s.id, "project": s.project, "role": s.role, "role_evidence": s.evidence,
            "models": "|".join(f"{m}:{k}" for m, k in models.most_common()), "calls": len(calls),
            "input": sum(c.inp for c in calls), "output": sum(c.out for c in calls),
            "cache_creation": sum(c.cc for c in calls), "cache_read": sum(c.cr for c in calls),
            "mean_context": int(statistics.mean(ctxs)), "max_context": max(ctxs),
            "first": tr.iso(min(stamps)) if stamps else "", "last": tr.iso(max(stamps)) if stamps else "",
            "compactions": len(comps),
            "compaction_list": "|".join(f"{tr.iso(ts)}:{pre}:{dur}ms:{trig}" for ts, trig, pre, post, dur in comps),
            "weighted": round(sum(c.weighted for c in calls), 1),
            "errands": len(sess_err), "between_calls": len(between),
            "between_weighted": round(sum(c.weighted for c in between), 1),
            "median_calls_per_errand": statistics.median([r["calls"] for r in sess_err]) if sess_err else "",
            "median_weighted_per_errand": round(statistics.median([r["weighted"] for r in sess_err]), 1) if sess_err else "",
        })
    rows.sort(key=lambda r: -r["weighted"])
    return rows


CSV_NAMES = ["calls.csv", "context_series.csv", "errands.csv", "sessions.csv"]


def write_csvs(out, sessions, erows, eids, srows, since, until, weights):
    """The three files that carry a weighted cost end with a weights_assumed column: a file shared
    on its own would otherwise lose both the weights and the fact that they are an assumption."""
    basis = weights.basis()
    with tr.open_output(os.path.join(out, "calls.csv"), newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["session", "project", "timestamp", "model", "message_id", "input", "output",
                    "cache_creation", "cache_read", "context", "sidechain", "stop_reason", "weighted",
                    "where", "errand_id", "weights_assumed"])
        for s in sessions:
            for c in sorted(s.calls.values(), key=lambda c: c.ts or _MIN_TS):
                if c.dropped or not c.in_window:
                    continue
                where = c.where
                eid = eids.get(id(where), "") if where not in (None, "between") else ""
                label = "between" if where == "between" else ("turn/errand" if where else "unattributed")
                w.writerow([s.id, s.project, tr.iso(c.ts), c.model, c.id, c.inp, c.out, c.cc, c.cr, c.context,
                            int(c.side), c.stop, round(c.weighted, 2), label, eid, basis])
    with tr.open_output(os.path.join(out, "context_series.csv"), newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["session", "timestamp", "kind", "context", "model", "pre_tokens", "post_tokens", "duration_ms", "trigger"])
        for s in sessions:
            rows = [(c.ts, "call", c.context, c.model, "", "", "", "") for c in tr.live_calls(s)]
            for (ts, trig, pre, post, dur) in s.compactions:
                if ts and tr.in_window(ts, since, until):
                    rows.append((ts, "compaction", "", "", pre, post, dur, trig))
            rows.sort(key=lambda r: r[0] or _MIN_TS)
            for ts, kind, ctx, model, pre, post, dur, trig in rows:
                w.writerow([s.id, tr.iso(ts), kind, ctx, model, pre, post, dur, trig])
    cols = ["errand_id", "session", "role", "kind", "start", "end", "wall_seconds", "calls", "input",
            "output", "cache_creation", "cache_read", "max_context", "weighted", "sends", "open_at_end", "label", "weights_assumed"]
    with tr.open_output(os.path.join(out, "errands.csv"), newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=cols)
        w.writeheader()
        w.writerows({**r, "weights_assumed": basis} for r in erows)
    with tr.open_output(os.path.join(out, "sessions.csv"), newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=(list(srows[0].keys()) + ["weights_assumed"]) if srows else ["session"])
        w.writeheader()
        w.writerows({**r, "weights_assumed": basis} for r in srows)


def print_report(args, weights, since, until, sessions, live, dups, erows, srows, sessions_used, out):
    P = print
    total_w = sum(c.weighted for c in live) or 1.0

    def kfmt(x):
        return f"{x:,.0f}"

    P("USAGE REPORT (counts from local transcripts; no model was run)")
    P(tr.ASSUMPTION)
    P(weights.describe())
    P(tr.unmatched_line(tr.unmatched_models(sessions, weights)))
    P(f"window: since {tr.iso(since) or '-'} until {tr.iso(until) or '-'}; project filter {args.project or '-'}")
    P(f"files {len(sessions)}; lines {sum(s.lines for s in sessions)}; parsed {sum(s.parsed for s in sessions)}; "
      f"ignored (other record types) {sum(s.ignored for s in sessions)}; SKIPPED (broken) {sum(s.skipped for s in sessions)}")
    P(f"calls in window {len(live)}; duplicate message ids dropped across files {dups}; "
      "within a file the copy with the most output tokens is kept (last on a tie), first copy's timestamp")
    P("")
    P("TOTALS BY MODEL  (calls, input, output, cache-creation, cache-read, weighted)")
    by_model = defaultdict(lambda: [0, 0, 0, 0, 0, 0.0])
    for c in live:
        a = by_model[c.model]
        a[0] += 1; a[1] += c.inp; a[2] += c.out; a[3] += c.cc; a[4] += c.cr; a[5] += c.weighted
    for m, a in sorted(by_model.items(), key=lambda kv: -kv[1][5]):
        P(f"  {m:28} {a[0]:>7} {kfmt(a[1]):>12} {kfmt(a[2]):>12} {kfmt(a[3]):>14} {kfmt(a[4]):>16} {kfmt(a[5]):>16}")
    P("")
    P("TOTALS BY ROLE  (calls, weighted, share of weighted)")
    by_role = defaultdict(lambda: [0, 0.0])
    for s in sessions:
        for c in tr.live_calls(s):
            by_role[s.role][0] += 1
            by_role[s.role][1] += c.weighted
    for role, a in sorted(by_role.items(), key=lambda kv: -kv[1][1]):
        P(f"  {role:10} {a[0]:>7} {kfmt(a[1]):>16} {100 * a[1] / total_w:5.1f}%")
    P("")
    P("TOTALS BY DAY (UTC)  (calls, weighted)")
    by_day = defaultdict(lambda: [0, 0.0])
    for c in live:
        d = c.ts.strftime("%Y-%m-%d") if c.ts else "?"
        by_day[d][0] += 1; by_day[d][1] += c.weighted
    for d, a in sorted(by_day.items()):
        P(f"  {d}  {a[0]:>7} {kfmt(a[1]):>16}")
    P("")
    P("SESSIONS by weighted cost  (id, role, models, calls, mean/max context, weighted, share, compactions, errands, between-calls share)")
    for r in srows:
        P(f"  {r['session'][:8]} {r['role']:8} {r['models'][:34]:34} {r['calls']:>6} {r['mean_context']:>8,}/{r['max_context']:>8,} "
          f"{kfmt(r['weighted']):>14} {100 * r['weighted'] / total_w:5.1f}% comp={r['compactions']} err={r['errands']} "
          f"between={100 * r['between_weighted'] / (r['weighted'] or 1):4.0f}%")
    P("")
    P(f"TOP {args.top} ERRANDS / TURNS by weighted cost  (cost, share, session, role, kind, start, wall s, calls, max context, sends, label)")
    for r in sorted(erows, key=lambda r: -r["weighted"])[:args.top]:
        P(f"  {kfmt(r['weighted']):>12} {100 * r['weighted'] / total_w:5.1f}% {r['session'][:8]} {r['role'][:8]:8} {r['kind']:6} "
          f"{r['start']} {r['wall_seconds']:>6}s {r['calls']:>4}c ctx{r['max_context']:>8,} s{r['sends']} | {r['label']}")
    P("")
    P("SHARE OF WEIGHTED COST BY CONTEXT SIZE PER CALL")
    buckets = [("<100k", 0, 100_000), ("100-200k", 100_000, 200_000), ("200-400k", 200_000, 400_000), (">400k", 400_000, 1 << 60)]
    for name, lo, hi in buckets:
        cs = [c for c in live if lo <= c.context < hi]
        P(f"  {name:9} {100 * sum(c.weighted for c in cs) / total_w:5.1f}% of cost, {len(cs):>6} calls ({100 * len(cs) / (len(live) or 1):4.1f}% of calls)")
    P("")
    P("SHARE OF WEIGHTED COST BY TOKEN KIND")
    kinds = Counter()
    for c in live:
        m = weights.mult(c.model)
        kinds["cache read"] += c.cr * weights.wcr * m
        kinds["cache write"] += c.cc * weights.wcw * m
        kinds["output"] += c.out * weights.wout * m
        kinds["input"] += c.inp * 1.0 * m
    for k, v in kinds.most_common():
        P(f"  {k:12} {100 * v / total_w:5.1f}%")
    P("")
    P("PER SESSION: median calls per errand / median weighted cost per errand")
    for r in srows:
        if r["errands"]:
            P(f"  {r['session'][:8]} {r['role']:8} errands={r['errands']:>4} median calls {r['median_calls_per_errand']:>6} "
              f"median cost {kfmt(r['median_weighted_per_errand']):>12}")
    P("")
    P("NOT ATTRIBUTED")
    btw = [c for c in live if c.where == "between"]
    una = [c for c in live if c.where is None]
    P(f"  calls between errands (outside any errand/turn): {len(btw)} = {100 * sum(c.weighted for c in btw) / total_w:.1f}% of cost; "
      f"calls with no errand logic applied: {len(una)}")
    roles = Counter(s.role for s in sessions_used)
    P(f"  sessions by role guess: {dict(roles)}  (role is inferred: Director = uses a director-only banto tool; "
      "Planner = uses a planner-only tool, reads its request file or has its briefing in the first snapshot; "
      "Worker = uses banto tools only; Goinkyo = uses banto tools only and its first system-prompt snapshot carries the Goinkyo briefing; "
      "solo = no banto tool; subagent = path under 'subagents')")
    P(f"CSV files written to {out}: calls.csv sessions.csv errands.csv context_series.csv")


def main(argv=None):
    args = build_parser().parse_args(argv)
    tr.reconfigure_stdout()
    since, until = tr.parse_flag_ts(args.since), tr.parse_flag_ts(args.until)
    if args.since and since is None or args.until and until is None:
        print("--since/--until must be ISO-8601, e.g. 2026-01-31 or 2026-01-31T12:00:00Z", file=sys.stderr)
        return 2
    try:
        tr.check_flags(args, LIMITS)
        weights = tr.weights_from_args(args)
        out = tr.prepare_out_dir(args.out, "usage_report", args.force_out, names=CSV_NAMES)
    except (ValueError, tr.OutDirError) as err:
        print(err, file=sys.stderr)
        return 2
    root = args.root or tr.default_root()
    sessions = tr.load_sessions(root, args.project)
    errands, dups = tr.prepare(sessions, weights, since, until)
    live = [c for s in sessions for c in tr.live_calls(s)]
    if args.project or since or until:
        sessions_used = [s for s in sessions if tr.live_calls(s)]
    else:
        sessions_used = sessions
    erows, eids = errand_rows(sessions, errands)
    srows = session_rows(sessions_used, erows, since, until)
    try:
        write_csvs(out, sessions, erows, eids, srows, since, until, weights)
    except tr.OutDirError as err:
        print(err, file=sys.stderr)
        return 2
    print_report(args, weights, since, until, sessions, live, dups, erows, srows, sessions_used, out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
