#!/usr/bin/env python3
"""What-if estimates on top of the usage numbers (counts from local transcripts;
nothing is run, nothing is measured about a setup that did not happen).

  1. baselines: fresh-start context and post-compaction context per session
  2. counterfactual A: a fresh Worker per errand instead of one long-lived Worker
  3. counterfactual B: compact whenever a call's context would pass a threshold T
  4. every Director session split by turn kind (operator message vs relay nudge)

Reads the transcripts READ-ONLY (default root: $CLAUDE_CONFIG_DIR/projects, else
~/.claude/projects). Prints tables and writes no file unless --out DIR is given,
in which case it also writes whatif_A.csv there (refused inside a git working
tree unless --force-out).

Needs Python 3.9+, standard library only.
"""
import argparse
import csv
import os
import statistics
import sys
from collections import defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import transcripts as tr  # noqa: E402

ROLES = ("director", "worker", "other")


def med(xs):
    return statistics.median(xs) if xs else None


def fmt(x):
    return "-" if x is None else f"{x:,.0f}"


def role_of(s):
    return "director" if s.role == "director" else ("worker" if s.role == "worker" else "other")


def attach_order(s):
    """Per session: .order (priced calls in event order, zero-usage records left
    out), .after_compact [(compaction event, context of the first call after it,
    its timestamp)], .post_tokens (the compaction records' own postTokens) and
    .first_ctx. Dropped (cross-file duplicate) calls stay in .order: their context
    was real in the file they sit in."""
    order, after_compact, post_tokens = [], [], []
    pending = None
    for ev in s.events:
        if ev[0] == "compact":
            pending = ev
            if ev[4] is not None:
                post_tokens.append(ev[4])
        elif ev[0] == "call":
            c = s.calls[ev[1]]
            if c.context == 0:
                continue  # a zero-usage record (e.g. <synthetic>) says nothing about the context
            order.append(c)
            if pending is not None:
                after_compact.append((pending, c.context, c.ts))
                pending = None
    s.order, s.after_compact, s.post_tokens = order, after_compact, post_tokens
    s.first_ctx = order[0].context if order else None


def baseline(s, mode):
    """B of counterfactual A for one session, and where it came from."""
    ac = [x[1] for x in s.after_compact]
    first, post = s.first_ctx, med(ac)
    if mode == "first" or post is None:
        return first, "first"
    if mode == "postcompact":
        return post, "post-compaction median"
    if first is None:
        return post, "post-compaction median"
    return (first, "first (smaller)") if first <= post else (post, "post-compaction median (smaller)")


def counterfactual_a(s, B, w):
    """A fresh Worker per errand. Only cache reads shrink: by (context of the
    errand's first call - B) on every call of the errand, plus one cache write
    of B on its first call. Returns a dict of totals for the session."""
    actual = cf = bt = btb = 0.0
    n_err = clamped = 0
    for e in s.errs:
        calls = [s.calls[m] for m in e.calls if s.calls[m].in_window and not s.calls[m].dropped]
        if not calls:
            continue
        n_err += 1
        delta = max(0, calls[0].context - B)
        for i, c in enumerate(calls):
            actual += c.weighted
            cr = c.cr - delta
            if cr < 0:
                clamped += 1
                cr = 0
            cc = c.cc + (B if i == 0 else 0)
            cf += w.cost(c.model, c.inp, cc, cr, c.out)
    for m in s.between:
        c = s.calls[m]
        if not c.in_window or c.dropped:
            continue
        bt += c.weighted
        btb += w.cost(c.model, c.inp, c.cc, max(0, B - c.inp - c.cc), c.out)
    return {"errands": n_err, "actual": actual + bt, "counterfactual": cf + bt,
            "between": bt, "between_at_b": btb, "clamped": clamped}


def counterfactual_b(live, T, P_by_role, comp_out, w):
    """Compact when a call's effective context would pass T. Returns
    {role: [actual, counterfactual, modelled compactions, extra-call cost]}.

    Each modelled compaction costs one extra call at the pre-compaction context
    (all cache read, output = the role's median postTokens) plus one cache write
    of P at the next call. A real compaction in the transcript resets the carried
    drop: both histories are compacted from there."""
    agg = defaultdict(lambda: [0.0, 0.0, 0, 0.0])
    for s in live:
        r = role_of(s)
        Pv = P_by_role[r]
        if Pv is None:
            continue
        drop = 0
        owe_write = False
        for ev in s.events:
            if ev[0] == "compact":
                drop = 0
                owe_write = False
                continue
            if ev[0] != "call":
                continue
            c = s.calls[ev[1]]
            if c.dropped:
                continue
            eff = c.context - drop
            extra = 0.0
            if eff > T:
                pre = max(0, c.context - drop)
                extra = w.cost(c.model, 0, 0, pre, comp_out[r])
                drop = c.context - Pv
                eff = Pv
                owe_write = True
                if c.in_window:
                    agg[r][2] += 1
            if not c.in_window:
                # A call before the window is not priced, but the modelled history
                # went through it: a threshold crossed there is already behind us.
                owe_write = False
                continue
            red = c.context - eff
            cr = max(0, c.cr - red)
            cc = c.cc + (Pv if owe_write else 0)
            owe_write = False
            agg[r][0] += c.weighted
            agg[r][1] += w.cost(c.model, c.inp, cc, cr, c.out) + extra
            agg[r][3] += extra
    return agg


LIMITS = (("min_compactions", "--min-compactions", 0, 1_000_000),)
THRESHOLD_MAX = 1_000_000_000      # a context of a billion tokens: far past any model's window


def parse_thresholds(text):
    """The --thresholds list as ints, each within 1..THRESHOLD_MAX; blank items are skipped, an empty list is refused."""
    values = []
    for item in text.split(","):
        if item.strip():
            try:
                value = int(item)
            except ValueError:
                raise ValueError(f"--thresholds wants comma-separated whole numbers, got {item.strip()!r}") from None
            if not 1 <= value <= THRESHOLD_MAX:
                raise ValueError(f"--thresholds values must be from 1 to {THRESHOLD_MAX}, got {value}")
            values.append(value)
    if not values:
        raise ValueError("--thresholds needs at least one value")
    return values


def build_parser():
    ap = argparse.ArgumentParser(
        prog="usage_whatif.py",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        description="What-if estimates (fresh Worker per errand; compact at a threshold) from local Claude Code transcripts.",
        epilog="Prints the report to stdout. It writes only whatif_A.csv, and only when --out is given; "
               "with no --out nothing is written.\n\n" + tr.ASSUMPTION)
    tr.add_root_args(ap)
    ap.add_argument("--out", default=None, metavar="OUT",
                    help="directory for whatif_A.csv (default: none, no file is written). Refused inside a git "
                         "working tree unless --force-out, and inside an agent home always")
    ap.add_argument("--force-out", action="store_true", help="allow --out inside a git working tree")
    ap.add_argument("--since", help="ISO-8601 UTC; calls before it are left out (default: all history)")
    ap.add_argument("--until", help="ISO-8601 UTC; calls at or after it are left out (default: all history)")
    tr.add_weight_args(ap)
    ap.add_argument("--baseline", choices=["first", "postcompact", "min"], default="min",
                    help="B for counterfactual A: the session's first-call context, the median "
                         "post-compaction context, or the smaller of the two (default)")
    ap.add_argument("--thresholds", default="150000,250000,400000",
                    help="comma-separated compaction thresholds T for counterfactual B, each 1 to 1000000000 (default 150000,250000,400000)")
    ap.add_argument("--min-compactions", type=int, default=5,
                    help="fewest measured compactions per role to use a measured P (0 to 1000000; default 5)")
    return ap


def main(argv=None):
    args = build_parser().parse_args(argv)
    tr.reconfigure_stdout()
    since, until = tr.parse_flag_ts(args.since), tr.parse_flag_ts(args.until)
    if args.since and since is None or args.until and until is None:
        print("--since/--until must be ISO-8601, e.g. 2026-01-31 or 2026-01-31T12:00:00Z", file=sys.stderr)
        return 2
    try:
        tr.check_flags(args, LIMITS)
        w = tr.weights_from_args(args)
        Ts = parse_thresholds(args.thresholds)
        out = tr.prepare_out_dir(args.out, "usage_whatif", args.force_out, names=["whatif_A.csv"]) if args.out else None
    except (ValueError, tr.OutDirError) as err:
        print(err, file=sys.stderr)
        return 2

    sessions = tr.load_sessions(args.root or tr.default_root(), args.project)
    tr.prepare(sessions, w, since, until)
    for s in sessions:
        attach_order(s)
    live = [s for s in sessions if tr.live_calls(s)]

    P = print
    P("WHAT-IF REPORT (counts from local transcripts; no model was run)")
    P(tr.ASSUMPTION)
    P(f"window since {tr.iso(since) or '-'} until {tr.iso(until) or '-'}; " + w.describe())
    P(tr.unmatched_line(tr.unmatched_models(sessions, w)))
    P("")

    # ---- 1. baselines ----
    P("1. BASELINES (contexts = input + cache-read + cache-creation of one call)")
    P("   first = context of the session's first call in its file (whole file, not only the window);")
    P("   after-compaction = context of the first call after each compact_boundary (whole file); postTokens = the record's own field")
    P(f"   {'session':8} {'role':8} {'first':>9} {'compactions':>11} {'after: median':>14} {'after: min':>11} {'postTokens med':>15} {'postTokens min':>15}")
    for s in live:
        ac = [x[1] for x in s.after_compact]
        P(f"   {s.id[:8]:8} {role_of(s):8} {fmt(s.first_ctx):>9} {len(s.compactions):>11} {fmt(med(ac)):>14} {fmt(min(ac) if ac else None):>11} "
          f"{fmt(med(s.post_tokens)):>15} {fmt(min(s.post_tokens) if s.post_tokens else None):>15}")
    P("")

    # ---- 2. counterfactual A ----
    P(f"2. COUNTERFACTUAL A: a fresh Worker per errand (Worker sessions only; B statistic = {args.baseline}: "
      f"{'the smaller of the first-call context and the median post-compaction context' if args.baseline == 'min' else args.baseline})")
    P(f"   {'session':8} {'B':>9} {'basis':28} {'errands':>7} {'actual':>14} {'counterfactual':>15} {'saved':>7}  {'between actual':>15} {'between at B':>13}")
    tot_actual = tot_cf = tot_between = tot_between_b = 0.0
    clamped = 0
    rows_a = []
    for s in live:
        if s.role != "worker":
            continue
        B, basis = baseline(s, args.baseline)
        if B is None:
            continue
        r = counterfactual_a(s, B, w)
        clamped += r["clamped"]
        rows_a.append((s.id, B, basis, r["errands"], r["actual"], r["counterfactual"], r["between"], r["between_at_b"]))
        tot_actual += r["actual"]
        tot_cf += r["counterfactual"]
        tot_between += r["between"]
        tot_between_b += r["between_at_b"]
        P(f"   {s.id[:8]:8} {B:>9,} {basis:28} {r['errands']:>7} {fmt(r['actual']):>14} {fmt(r['counterfactual']):>15} "
          f"{100 * (1 - r['counterfactual'] / (r['actual'] or 1)):6.1f}%  {fmt(r['between']):>15} {fmt(r['between_at_b']):>13}")
    P(f"   {'TOTAL':8} {'':>9} {'':28} {sum(r[3] for r in rows_a):>7} {fmt(tot_actual):>14} {fmt(tot_cf):>15} "
      f"{100 * (1 - tot_cf / (tot_actual or 1)):6.1f}%  {fmt(tot_between):>15} {fmt(tot_between_b):>13}")
    P("   (between-errand calls are left as they are in 'counterfactual'; the last two columns show them as they were and recomputed at B,")
    P("    i.e. context set to B with the difference taken out of cache reads. If every between call were also at B the total would be "
      f"{fmt(tot_cf - tot_between + tot_between_b)} = {100 * (1 - (tot_cf - tot_between + tot_between_b) / (tot_actual or 1)):.1f}% saved.)")
    P(f"   cache-read reductions that exceeded the call's cache reads and were clamped at 0: {clamped} calls")
    P("")

    # ---- 3. counterfactual B ----
    pooled, post_pool = defaultdict(list), defaultdict(list)
    for s in live:
        # every compaction in the files, not only those in the window: the baselines
        # of step 1 are measured the same way and a window has too few
        for _comp, ctx, _ts in s.after_compact:
            pooled[role_of(s)].append(ctx)
        for comp in s.compactions:
            if comp[3] is not None:
                post_pool[role_of(s)].append(comp[3])
    Bs = {}
    for r in ROLES:
        vals = [b for b in (baseline(s, args.baseline)[0] for s in live if role_of(s) == r) if b is not None]
        Bs[r] = med(vals)
    Pmeas = {}
    P("3. COUNTERFACTUAL B: compact when a call's context would pass T")
    P("   P (context after a modelled compaction) = median measured post-compaction first-call context of the role, over every compaction in the files;")
    P(f"   used only if at least {args.min_compactions} compactions were measured for the role, else the role's median B.")
    for r in ROLES:
        n = len(pooled[r])
        if n >= args.min_compactions:
            Pmeas[r] = (med(pooled[r]), f"measured median of {n} compactions")
        else:
            Pmeas[r] = (Bs[r], f"TOO FEW compactions measured ({n}); median B used")
        P(f"   {r:8} P = {fmt(Pmeas[r][0])} ({Pmeas[r][1]}); record postTokens median {fmt(med(post_pool[r]))} over {len(post_pool[r])}")
    comp_out = {r: (med(post_pool[r]) if post_pool[r] else (Pmeas[r][0] or 0)) for r in Pmeas}
    P("   Each modelled compaction costs one extra call at the pre-compaction context: all of it read from the cache,")
    P("   plus output = the role's median postTokens (the records carry no output-token field), plus one cache write of P")
    P("   at the first call after it (the new context is not in the cache yet). A real compaction in the transcript resets the carried drop.")
    P("")
    P(f"   {'T':>8} {'role':9} {'actual':>16} {'counterfactual':>16} {'saved':>7} {'modelled compactions':>21} {'of which extra calls':>21}")
    P_by_role = {r: Pmeas[r][0] for r in ROLES}
    for T in Ts:
        agg = counterfactual_b(live, T, P_by_role, comp_out, w)
        a_tot = c_tot = 0.0
        for r in ROLES:
            a, c, n, ex = agg[r]
            a_tot += a
            c_tot += c
            P(f"   {T:>8,} {r:9} {fmt(a):>16} {fmt(c):>16} {100 * (1 - c / (a or 1)):6.1f}% {n:>21} {fmt(ex):>21}")
        P(f"   {T:>8,} {'TOTAL':9} {fmt(a_tot):>16} {fmt(c_tot):>16} {100 * (1 - c_tot / (a_tot or 1)):6.1f}%")
    P("")

    # ---- 4. Director split ----
    dirs = [s for s in live if s.role == "director"]
    P(f"4. DIRECTOR SESSIONS ({len(dirs)}): cost by turn kind")
    for s in dirs:
        P(f"   session {s.id[:8]}")
        kinds = defaultdict(lambda: [0, 0.0, []])
        for e in s.errs:
            calls = [s.calls[m] for m in e.calls if s.calls[m].in_window and not s.calls[m].dropped]
            if not calls:
                continue
            a = kinds["relay nudge" if e.nudge else "operator message"]
            cost = sum(c.weighted for c in calls)
            a[0] += len(calls)
            a[1] += cost
            a[2].append(cost)
        bt = [s.calls[m] for m in s.between if s.calls[m].in_window and not s.calls[m].dropped]
        tot = sum(v[1] for v in kinds.values()) + sum(c.weighted for c in bt)
        P(f"   {'kind':18} {'turns':>6} {'calls':>7} {'weighted':>16} {'share':>7} {'median cost/turn':>17}")
        for k, v in sorted(kinds.items()):
            P(f"   {k:18} {len(v[2]):>6} {v[0]:>7} {fmt(v[1]):>16} {100 * v[1] / (tot or 1):6.1f}% {fmt(med(v[2])):>17}")
        P(f"   {'before first turn':18} {'':>6} {len(bt):>7} {fmt(sum(c.weighted for c in bt)):>16} {100 * sum(c.weighted for c in bt) / (tot or 1):6.1f}%")
        P(f"   {'TOTAL':18} {'':>6} {sum(v[0] for v in kinds.values()) + len(bt):>7} {fmt(tot):>16}")
    P("")
    P("ASSUMPTIONS / WHAT THESE IGNORE")
    P("  A: a fresh Worker re-reads what the old one already knew (files, state, the brief), which adds calls and tool results;")
    P("     none of that is added, so A is optimistic. Output, uncached input and cache writes of the actual calls are kept;")
    P("     only cache reads shrink, by (context of the errand's first call - B) on every call of the errand, plus one cache write of B.")
    P("     Compactions inside an errand and the errand's own growth are left as measured. Errands cut by the window count only in-window calls.")
    P("  B: the carried drop is taken out of cache reads; a modelled compaction is charged as described above; no information")
    P("     is lost by it (a compacted agent may need extra calls to recover), so B is optimistic too. Real compactions in the")
    P("     transcript reset the carried drop (both histories are compacted from there).")
    P("  Contexts are input + cache-read + cache-creation of each call; weights are an assumption, not a known fact.")

    if out:
        try:
            with tr.open_output(os.path.join(out, "whatif_A.csv"), newline="") as fh:
                wr = csv.writer(fh)
                wr.writerow(["session", "B", "basis", "errands", "actual", "counterfactual", "between_actual",
                             "between_at_B", "weights_assumed"])
                wr.writerows([(r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7], w.basis()) for r in rows_a])
        except tr.OutDirError as err:
            print(err, file=sys.stderr)
            return 2
        print(f"whatif_A.csv written to {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
