#!/usr/bin/env python3
"""Measure what a change in compaction behaviour (for example setting Claude
Code's autoCompactWindow) did, from local transcripts alone: compare each
session's calls since a moment T with the calls before it, list the compactions
since T and what they cost, and, for chosen Worker sessions, look at their
errands, re-reads of files after a compaction, and what started each compaction.

Counts only; nothing is run. Reads the transcripts READ-ONLY (default root:
$CLAUDE_CONFIG_DIR/projects, else ~/.claude/projects) and prints a report to
stdout; it writes no file. The summarising call of a compaction is NOT in the
transcript, so its cost is estimated from stated assumptions (section 3).

Needs Python 3.9+, standard library only.
"""
import argparse
import json
import os
import re
import shlex
import statistics
import sys
from collections import Counter, defaultdict
from datetime import datetime, timedelta, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import transcripts as tr  # noqa: E402

# Commands that read a file's content into the conversation; a path they name
# counts as a read of that file.
READERS = {"cat", "head", "tail", "sed", "grep", "rg", "awk", "nl", "less", "type", "get-content", "select-string", "more"}
# A shell command that changes files without the Edit/Write tools; seen only to be
# reported as "not tracked as edits".
SHELL_WRITE_RX = re.compile(r"\bsed\s+-i|\bcargo\s+fmt|>>?\s*[\w./\\-]+\.\w+|Set-Content|Out-File")
# Tools that read or write a file by path.
FILE_WRITE_TOOLS = ("Edit", "Write", "NotebookEdit")
SHELL_TOOLS = ("Bash", "PowerShell")
# Characters of summary text per output token, used to estimate the summarising call.
CHARS_PER_TOKEN = 3.5
# api-request records: what the client asked for, one per request, with the querySource.
API_REQUEST_RX = tr.type_rx("api-request")


def k(x):
    return f"{x:,.0f}"


def hm(dt):
    return dt.strftime("%H:%M:%S") if dt else "-"


def norm(p, cwd=""):
    """Comparable form of a path: forward slashes, lower case, Git Bash /c/ drive
    form turned into c:/, and the session's own working directory stripped."""
    p = p.strip("'\"` ").replace("\\", "/")
    p = re.sub(r"^/([a-zA-Z])/", lambda m: m.group(1).lower() + ":/", p)
    p = p.lower()
    if cwd and p.startswith(cwd):
        p = p[len(cwd):]
    if p.startswith("./"):
        p = p[2:]
    return p


def cwd_prefix(session):
    """The session's working directory as a norm()-comparable prefix with a trailing slash."""
    if not session.cwd:
        return ""
    return session.cwd.replace("\\", "/").lower().rstrip("/") + "/"


def bash_read_paths(cmd, cwd=""):
    """File paths a shell command reads with one of READERS (pipelines and && ; split first)."""
    out = []
    for seg in re.split(r"&&|\|\||;|\||\n", cmd):
        seg = seg.strip()
        if not seg:
            continue
        try:
            toks = shlex.split(seg, posix=False)
        except ValueError:
            toks = seg.split()
        if not toks:
            continue
        head = toks[0].lower().split("/")[-1].split("\\")[-1]
        if head not in READERS:
            continue
        for t in toks[1:]:
            if t.startswith("-") or t.startswith("$"):
                continue
            t2 = t.strip("'\"")
            if (re.search(r"\.[A-Za-z0-9]{1,6}$", t2)
                    and ("/" in t2 or "\\" in t2 or re.match(r"^[\w.\-]+\.[A-Za-z0-9]{1,6}$", t2))
                    and " " not in t2 and not t2.startswith("^")):
                out.append(norm(t2, cwd))
    return out


def summary_cost_estimate(pre_tokens, summary_chars, w):
    """The summarising call of a compaction is not in the transcript. Assumes its
    input is preTokens once, its output is summary chars / CHARS_PER_TOKEN (the
    hidden analysis block is not counted, so this is a floor), and prices the input
    three ways: A as cache read, B as uncached input, C as cache write.
    Returns (output tokens, A, B, C)."""
    ot = summary_chars / CHARS_PER_TOKEN
    return (ot, pre_tokens * w.wcr + ot * w.wout, pre_tokens * 1.0 + ot * w.wout, pre_tokens * w.wcw + ot * w.wout)


def label(value):
    """A counter key from a field of unknown type: strings and None stay as they are."""
    return value if value is None or isinstance(value, str) else str(value)


def scan_session_ops(s):
    """One raw pass over the session file: ordered file operations and tool-result sizes.
    Returns (ops [(ts, kind, tool, path, tool_use_id)], results {tool_use_id: (chars, lines)})."""
    cwd = cwd_prefix(s)
    ops, results = [], {}
    for d in tr.iter_records(s.path, tr.type_rx("assistant", "user")):
        ts = tr.parse_ts(d.get("timestamp"))
        if d.get("type") == "assistant":
            content = tr.as_dict(d.get("message")).get("content")
            for b in content if isinstance(content, list) else []:
                if not (isinstance(b, dict) and b.get("type") == "tool_use"):
                    continue
                nm, inp, tid = b.get("name", ""), b.get("input") or {}, b.get("id")
                if not isinstance(nm, str) or not isinstance(inp, dict):
                    continue
                if nm == "Read" and isinstance(inp.get("file_path"), str):
                    ops.append((ts, "read", "Read", norm(inp["file_path"], cwd), tid))
                elif nm in FILE_WRITE_TOOLS and isinstance(inp.get("file_path"), str):
                    ops.append((ts, "write", nm, norm(inp["file_path"], cwd), tid))
                elif nm == "Grep" and isinstance(inp.get("path"), str) and re.search(r"\.[A-Za-z0-9]{1,6}$", inp["path"]):
                    ops.append((ts, "read", "Grep", norm(inp["path"], cwd), tid))
                elif nm in SHELL_TOOLS:
                    cmd = inp.get("command", "")
                    if not isinstance(cmd, str):
                        continue
                    for p in bash_read_paths(cmd, cwd):
                        ops.append((ts, "read", nm, p, tid))
                    if SHELL_WRITE_RX.search(cmd):
                        ops.append((ts, "shellwrite", nm, "?", tid))
        elif d.get("type") == "user":
            c = tr.as_dict(d.get("message")).get("content")
            for b in c if isinstance(c, list) else []:
                if isinstance(b, dict) and b.get("type") == "tool_result" and isinstance(b.get("tool_use_id"), str):
                    t = tr.block_text(b.get("content"))
                    results[b["tool_use_id"]] = (len(t), t.count("\n") + 1 if t else 0)
    ops.sort(key=lambda o: o[0] or datetime.min.replace(tzinfo=timezone.utc))
    return ops, results


def live_sorted(s):
    return sorted((c for c in s.calls.values() if not c.dropped and c.ts), key=lambda c: c.ts)


def first_briefing_name(path):
    """The Worker name in the first briefing a file contains: a member's own
    briefing is its first message, while later mentions only quote someone else's."""
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            if "You are worker-" in line:
                m = tr.WORKER_BRIEFING_RE.search(line)
                if m:
                    return m.group(1)
    return None


def section_identity(P, active, sessions, T0):
    P("=== 0. identity / matching ===")
    for s in active:
        who = first_briefing_name(s.path) if s.role == "worker" else None
        first_all = min(c.ts for c in s.calls.values() if c.ts)
        P(f"  {s.id[:8]} file first call {first_all:%m-%d %H:%M:%S}Z  role guess {s.role}  own-briefing name "
          f"{who or 'none (Director or solo)'}  size {os.path.getsize(s.path) // 1_000_000} MB")
    new_files = [s.id[:8] for s in sessions if s.first_ts and s.first_ts >= T0]
    P(f"  transcripts whose first record is at/after T (i.e. new session ids since the change): {new_files or 'none'}")


def section_since_vs_before(P, active, T0, w, hours):
    P(f"\n=== 1. per session: since {tr.iso(T0)} vs before ===")
    P(w.describe())
    hdr = "  {:9} {:>5} {:>9} {:>9} {:>14} {:>11} | {:>6} {:>9} {:>11}  models(since) / models(before)"
    P(hdr.format("session", "calls", "meanCtx", "maxCtx", "weighted", "w/call", "calls", "meanCtx", "w/call"))

    def row(xs):
        if not xs:
            return (0, 0, 0, 0, 0)
        ctx = [c.context for c in xs]
        tot = sum(c.weighted for c in xs)
        return (len(xs), statistics.mean(ctx), max(ctx), tot, tot / len(xs))

    for s in active:
        cs = [c for c in s.calls.values() if not c.dropped and c.ts]
        after = [c for c in cs if c.ts >= T0]
        before = [c for c in cs if c.ts < T0]
        a, b = row(after), row(before)
        ma = Counter(c.model.replace("claude-", "") for c in after)
        mb = Counter(c.model.replace("claude-", "") for c in before)
        P(f"  {s.id[:8]:9} {a[0]:>5} {k(a[1]):>9} {k(a[2]):>9} {k(a[3]):>14} {k(a[4]):>11} | {b[0]:>6} {k(b[1]):>9} {k(b[4]):>11}  {dict(ma)} / {dict(mb)}")
    P(f"  (same, but 'before' limited to the {hours:g} h preceding T)")
    lo = T0 - timedelta(hours=hours)
    for s in active:
        cs = [c for c in s.calls.values() if not c.dropped and c.ts and lo <= c.ts < T0]
        if cs:
            P(f"  {s.id[:8]:9} calls {len(cs):>5} meanCtx {k(statistics.mean(c.context for c in cs)):>9} maxCtx {k(max(c.context for c in cs)):>9} "
              f"w/call {k(sum(c.weighted for c in cs) / len(cs)):>9}")
    P("\n  compactions since T (all selected transcripts):")
    P("  session   time(Z)   trigger  preTokens  postTokens  durationMs  | ctx of last call before / first call after (cc,cr of that first call)")
    for s in active:
        cs = live_sorted(s)
        for (ts, trig, pre, post, dur) in s.compactions:
            if not ts or ts < T0:
                continue
            pb = [c for c in cs if c.ts < ts]
            na = [c for c in cs if c.ts > ts]
            nax = na[0] if na else None
            P(f"  {s.id[:8]:9} {hm(ts)}  {str(trig):7} {pre or 0:>9,} {post or 0:>10,} {dur or 0:>10,}   | "
              f"{pb[-1].context if pb else 0:>9,} / {nax.context if nax else 0:>9,} (cc {nax.cc if nax else 0:,}, cr {nax.cr if nax else 0:,})")
    P("  compactions before T in the same sessions: " + ", ".join(f"{s.id[:8]}={sum(1 for x in s.compactions if x[0] and x[0] < T0)}" for s in active))


def section_errands(P, s, T0):
    errs = s.errs
    ops, results = scan_session_ops(s)
    P(f"\n=== 2. session {s.id[:8]}: errands ending at/after T ===")

    def comps_in(lo, hi):
        return [x for x in s.compactions if x[0] and lo <= x[0] <= hi]

    for e in [e for e in errs if e.end and e.end >= T0]:
        calls = sorted((s.calls[m] for m in e.calls if not s.calls[m].dropped and s.calls[m].ts), key=lambda c: c.ts)
        if not calls:
            continue
        wall = max((e.end - e.start).total_seconds(), 1)
        cps = comps_in(e.start, e.end)
        durs = sum(x[4] or 0 for x in cps)
        P(f"\n  errand start {hm(e.start)}Z end {hm(e.end)}Z wall {wall:.0f}s  calls {len(calls)}  open_at_end {e.open}  weighted {k(sum(c.weighted for c in calls))}")
        ctx = [c.context for c in calls]
        P(f"    context mean {k(statistics.mean(ctx))} max {k(max(ctx))}  output tokens {k(sum(c.out for c in calls))}")
        P(f"    compactions in errand: {len(cps)}  sum durationMs {durs:,} = {100 * durs / 1000 / wall:.1f}% of wall ({durs / 1000:.0f}s of {wall:.0f}s)")
        bounds = [e.start] + [x[0] for x in cps] + [e.end]
        for i in range(len(bounds) - 1):
            seg = [c for c in calls if bounds[i] <= c.ts < bounds[i + 1]]
            P(f"    segment {i}: {hm(bounds[i])}-{hm(bounds[i + 1])}  calls {len(seg)}  ctx {k(seg[0].context) if seg else '-'}->{k(seg[-1].context) if seg else '-'}  weighted {k(sum(c.weighted for c in seg))}")
        P("    re-reads (reader = Read / Grep-on-file / Bash|PowerShell cat,head,tail,sed,grep,rg,awk,nl,less,type,Get-Content on a file path):")
        bytid = {}
        for o in ops:
            if o[0] and e.start <= o[0] <= e.end and o[1] == "read":
                d = bytid.setdefault(o[4], {"ts": o[0], "tool": o[2], "paths": set()})
                d["paths"].add(o[3])
        rcalls = sorted(bytid.items(), key=lambda kv: kv[1]["ts"])
        writes = [o for o in ops if o[0] and e.start <= o[0] <= e.end and o[1] == "write"]
        P(f"      reader calls in errand {len(rcalls)}, result chars {k(sum(results.get(t, (0, 0))[0] for t, _ in rcalls))}; Edit/Write calls {len(writes)}")
        for i in range(len(bounds) - 1):
            lo, hi = bounds[i], bounds[i + 1]
            earlier = {p for t, d in rcalls if d["ts"] < lo for p in d["paths"]}
            seg = [(t, d) for t, d in rcalls if lo <= d["ts"] < hi]
            seen_before = set(earlier)
            n_any = n_cross = n_cross_clean = n_cross_edit = n_cross_amb = 0
            ch_cross_clean = ln_cross_clean = ch_cross_edit = ch_cross_amb = 0
            lst = []
            for t, d in seg:
                ch, ln = results.get(t, (0, 0))
                hit_cross = d["paths"] & earlier
                if d["paths"] & seen_before:
                    n_any += 1
                if i > 0 and hit_cross:
                    n_cross += 1

                    def edited(p, lo=lo, d=d):
                        lr = max(x["ts"] for _tt, x in rcalls if p in x["paths"] and x["ts"] < lo)
                        return any(wr[3] == p and lr < wr[0] < d["ts"] for wr in writes)

                    if any("/" not in p for p in hit_cross):
                        n_cross_amb += 1; ch_cross_amb += ch; tag = "path-ambiguous"
                    elif all(edited(p) for p in hit_cross):
                        n_cross_edit += 1; ch_cross_edit += ch; tag = "edited-since"
                    else:
                        n_cross_clean += 1; ch_cross_clean += ch; ln_cross_clean += ln; tag = "NOT edited by Edit/Write since"
                    lst.append((hm(d["ts"]), d["tool"], ",".join(sorted(p[-45:] for p in hit_cross)), ch, tag))
                seen_before |= d["paths"]
            if i > 0:
                P(f"      after compaction {i}: reader calls {len(seg)}; calls touching a path read before the compaction: {n_cross}")
                P(f"        of which path not Edit/Write-n since the earlier read: {n_cross_clean} (result chars {k(ch_cross_clean)}, lines {k(ln_cross_clean)}); "
                  f"edited since: {n_cross_edit} (chars {k(ch_cross_edit)}); basename-only path (ambiguous): {n_cross_amb} (chars {k(ch_cross_amb)})")
                P(f"        distinct paths read before the compaction in this errand: {len(earlier)}; shell-side writes (sed -i, redirects) are NOT tracked as edits")
                for item in lst:
                    P(f"         {item[0]} {item[1]:10} {item[2]:50} {item[3]:>7,} chars  {item[4]}")
            P(f"      segment {i}: reader calls {len(seg)}; calls touching a path already read earlier in the errand (any segment, incl. own): {n_any} = {100 * n_any / max(1, len(seg)):.0f}%")


def section_summary_visibility(P, active, T0, w, selected):
    P("\n=== 3. is the summarising call visible? ===")
    vis = defaultdict(list)
    for s in active:
        for (ts, trig, pre, post, dur) in s.compactions:
            if not ts or ts < T0:
                continue
            dur = dur or 0
            lo = ts.timestamp() - dur / 1000 - 5
            day_rx = re.compile(r'"timestamp"\s*:\s*"' + ts.strftime("%Y-%m-%dT"))
            kinds, sumlen = Counter(), 0
            for d in tr.iter_records(s.path, day_rx):
                t = tr.parse_ts(d.get("timestamp"))
                if not t or not (lo <= t.timestamp() <= ts.timestamp() + 0.5):
                    continue
                msg = tr.as_dict(d.get("message"))
                kd = str(d.get("type")) + ("/" + str(d["subtype"]) if d.get("subtype") else "")
                if d.get("type") == "assistant":
                    kd += "(usage)" if msg.get("usage") else "(no usage)"
                if d.get("isCompactSummary"):
                    c = msg.get("content")
                    sumlen = len(c) if isinstance(c, str) else len(tr.block_text(c))
                    kd += "[isCompactSummary]"
                if d.get("type") == "api-request":
                    kd += "(" + str(d.get("querySource")) + ")"
                kinds[kd] += 1
            P(f"  {s.id[:8]} {hm(ts)}Z window [-{dur / 1000 + 5:.0f}s, +0.5s]: {dict(kinds)}  summary text chars {sumlen:,}")
            vis[s.id].append((ts, trig, pre or 0, post, dur, sumlen))
    qs = Counter()
    for s in active:
        for d in tr.iter_records(s.path, API_REQUEST_RX):
            t = tr.parse_ts(d.get("timestamp"))
            if t and t >= T0:
                qs[label(d.get("querySource"))] += 1
    P(f"  api-request querySource values since T, all selected sessions: {dict(qs)}")
    P("\n  estimate of the summarising call (NOT in the transcript); weights as above")
    P(f"  assumptions: input = preTokens once; output = summary chars / {CHARS_PER_TOKEN:g} (hidden analysis block not counted, so a floor); price class per run noted")
    P("  session   time   preTokens  outTok(est)   A: pre as cache-read   B: pre uncached input   C: pre as cache-write")
    tot = defaultdict(lambda: [0.0, 0.0, 0.0])
    for sid, lst in vis.items():
        for (ts, trig, pre, post, dur, sl) in lst:
            ot, a, b, c = summary_cost_estimate(pre, sl, w)
            P(f"  {sid[:8]:9} {hm(ts)} {pre:>9,} {ot:>10,.0f} {a:>20,.0f} {b:>22,.0f} {c:>22,.0f}")
            for x, v in enumerate((a, b, c)):
                tot[sid][x] += v
    for s in selected:
        t = tot.get(s.id, [0.0, 0.0, 0.0])
        P(f"  session {s.id[:8]} compactions since T, estimated summarising cost: A {t[0]:,.0f}  B {t[1]:,.0f}  C {t[2]:,.0f}")


def section_visible_cost(P, s, T0, w):
    P(f"\n=== 3b. session {s.id[:8]} since T: visible cost of the compactions vs the cache-read saved (assumption-based) ===")
    cs = [c for c in live_sorted(s) if c.ts >= T0]
    comps = [x for x in s.compactions if x[0] and x[0] >= T0]
    dropped_acc, saved, ci = 0, 0.0, 0
    firsts = []
    for c in cs:
        while ci < len(comps) and comps[ci][0] < c.ts:
            dropped_acc += (comps[ci][2] or 0) - (comps[ci][3] or 0)
            firsts.append(c)
            ci += 1
        saved += dropped_acc * w.wcr
    P(f"  calls {len(cs)}; sum weighted {k(sum(c.weighted for c in cs))}")
    P(f"  tokens dropped per compaction (pre-post): {[k((x[2] or 0) - (x[3] or 0)) for x in comps]}")
    P(f"  cache-read cost avoided over later calls, if the dropped tokens had stayed (and been read each call at {w.wcr:g}): {k(saved)}")
    P("  (it ignores: cache-write cost of the tokens the dropped context would never have re-written, the extra tool calls that exist only to recover lost context, and that the pre-compaction history would have kept growing)")
    for c in firsts:
        P(f"  first call after a compaction at {hm(c.ts)}: cc {c.cc:,} cr {c.cr:,} out {c.out:,}  weighted {k(c.weighted)}  of which cache-write {k(c.cc * w.wcw)}")
    P(f"  summed visible first-call cache-write cost: {k(sum(c.cc * w.wcw for c in firsts))}")


def section_costliest(P, s, T0, w):
    P(f"\n=== 4. session {s.id[:8]} errands before T: costliest ===")
    rows = []
    for e in s.errs:
        calls = [s.calls[m] for m in e.calls if not s.calls[m].dropped]
        if not calls:
            continue
        ctx = [c.context for c in calls]
        cps = [x for x in s.compactions if x[0] and e.start <= x[0] <= e.end]
        rows.append((sum(c.weighted for c in calls), e, len(calls), statistics.mean(ctx), max(ctx),
                     len(cps), sum(x[4] or 0 for x in cps), sum(c.out for c in calls)))
    P("  start(Z)               wall s  calls  meanCtx   maxCtx   weighted     w/call  compactions durMs  output")
    before = [r for r in rows if r[1].end < T0]
    since = [r for r in rows if r[1].end >= T0]

    def show(r):
        wt, e, n, mc, xc, nc, dur, out = r
        P(f"  {e.start:%m-%d %H:%M:%S}  {int((e.end - e.start).total_seconds()):>8}  {n:>5} {k(mc):>8} {k(xc):>8} {k(wt):>10} {k(wt / n):>9}  {nc:>5} {dur:>9,} {k(out):>7}")

    for r in sorted(before, key=lambda r: -r[0])[:8]:
        show(r)
    P("  errands since T (same columns):")
    for r in since:
        show(r)
    if before:
        P(f"  all errands before T: {len(before)}; median calls {statistics.median(r[2] for r in before):.0f}, "
          f"median w/call {k(statistics.median(r[0] / r[2] for r in before))}, median meanCtx {k(statistics.median(r[3] for r in before))}, "
          f"median maxCtx {k(statistics.median(r[4] for r in before))}")
        P(f"  of which with >= 1 compaction inside: {sum(1 for r in before if r[5])}")
    fe = Counter()
    for d in tr.iter_records(s.path, API_REQUEST_RX):
        t = tr.parse_ts(d.get("timestamp"))
        p = d.get("params") if isinstance(d.get("params"), dict) else {}
        oc = p.get("output_config") if isinstance(p.get("output_config"), dict) else {}
        fe[("since" if t and t >= T0 else "before", label(p.get("model")), label(oc.get("effort")))] += 1
    P(f"  api-request (model, effort) counts: {dict(fe)}")

    P(f"\n=== 4b. session {s.id[:8]}: weighted cost per call by token kind (share) ===")

    def decomp(cs):
        r = sum(c.cr * w.wcr for c in cs); wr = sum(c.cc * w.wcw for c in cs)
        o = sum(c.out * w.wout for c in cs); i = sum(c.inp for c in cs)
        t = (r + wr + o + i) or 1.0
        return (f"calls {len(cs):>4} w/call {k(t / max(1, len(cs))):>8}  cache-read {100 * r / t:4.1f}%  "
                f"cache-write {100 * wr / t:4.1f}%  output {100 * o / t:4.1f}%  input {100 * i / t:4.2f}%")

    for r in sorted(before, key=lambda r: -r[0])[:3]:
        P(f"  before, errand {r[1].start:%m-%d %H:%M}: " + decomp([s.calls[m] for m in r[1].calls if not s.calls[m].dropped]))
    for r in since:
        P(f"  since,  errand {r[1].start:%m-%d %H:%M}: " + decomp([s.calls[m] for m in r[1].calls if not s.calls[m].dropped]))


def section_starts(P, active, T0):
    P("\n=== 5. what started each compaction (resume vs organic), per session ===")
    for s in active:
        cs = live_sorted(s)
        lastb = [c for c in cs if c.ts < T0]
        P(f"  {s.id[:8]}: last call before T at {lastb[-1].ts:%m-%d %H:%M:%S} ctx {lastb[-1].context:,}" if lastb else f"  {s.id[:8]}: no call before")
        for (ts, trig, pre, post, dur) in s.compactions:
            if not ts or ts < T0:
                continue
            dur = dur or 0
            pb = [c for c in cs if c.ts < ts]
            if not pb:
                continue
            try:
                start = datetime.fromtimestamp(ts.timestamp() - dur / 1000, timezone.utc)
            except (OverflowError, OSError, ValueError):
                start = None    # a duration or a date no calendar holds: the line is still worth printing
            P(f"     compaction {hm(ts)}Z (started ~{hm(start)}Z): last call before at {hm(pb[-1].ts)}Z ctx {pb[-1].context:,}; "
              f"gap from it to compaction start {ts.timestamp() - dur / 1000 - pb[-1].ts.timestamp():.0f}s; "
              f"calls since T before this compaction: {sum(1 for c in pb if c.ts >= T0)}")
    for s in active:
        for (ts, trig, pre, post, dur) in s.compactions:
            if not ts or ts < T0:
                continue
            lo = ts.timestamp() - (dur or 0) / 1000
            inside = [c for c in s.calls.values() if not c.dropped and c.ts and lo <= c.ts.timestamp() <= ts.timestamp()]
            P(f"  {s.id[:8]} {hm(ts)}Z: assistant calls whose first record lies inside the compaction's own [start,end] window: {len(inside)}")


# like-for-like window: a minute's worth up to a year before T (T itself is held a year from the calendar's start)
LIMITS = (("like_for_like_hours", "--like-for-like-hours", 0.01, 8760.0),)


def build_parser():
    ap = argparse.ArgumentParser(
        prog="ac_measure.py",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        description="Measure what a change in compaction behaviour did, by comparing calls since a moment T with calls before it.",
        epilog="Prints a report to stdout and writes no file at all (there is no --out).\n\n"
               "Report sections:\n"
               "  0   identity: the sessions active since T and the role guessed for each\n"
               "  1   per session: calls since T against calls before T, and a like-for-like window\n"
               "  2   errands ending at or after T, cut at each compaction (selected sessions)\n"
               "  3   whether the summarising call of a compaction is visible in the transcript, and an\n"
               "      estimate of its cost\n"
               "  3b  the visible cost of the compactions against the cache reads they saved (selected sessions)\n"
               "  4   the costliest errands before T, and API-request model/effort counts (selected sessions)\n"
               "  4b  cost per call by token kind (selected sessions)\n"
               "  5   what started each compaction, resume or organic\n\n" + tr.ASSUMPTION)
    tr.add_root_args(ap)
    ap.add_argument("--since", required=True, metavar="T",
                    help="ISO-8601 UTC: the moment the change took effect (required)")
    ap.add_argument("--session", action="append", default=[], metavar="ID",
                    help="session id (prefix) whose per-errand sections (2, 3b, 4, 4b; listed below) are "
                         "printed; repeatable (default: every Worker session that has a compaction at or after T)")
    ap.add_argument("--like-for-like-hours", type=float, default=3.0, metavar="H",
                    help="hours: the window just before T used for the like-for-like comparison (0.01 to 8760; default 3)")
    tr.add_weight_args(ap)
    return ap


def main(argv=None):
    args = build_parser().parse_args(argv)
    tr.reconfigure_stdout()
    T0 = tr.parse_flag_ts(args.since)
    if T0 is None:
        print("--since must be ISO-8601, e.g. 2026-01-31T12:00:00Z", file=sys.stderr)
        return 2
    try:
        tr.check_flags(args, LIMITS)
        w = tr.weights_from_args(args)
    except ValueError as err:
        print(err, file=sys.stderr)
        return 2
    sessions = tr.load_sessions(args.root or tr.default_root(), args.project)
    tr.prepare(sessions, w)
    active = [s for s in sessions if any(not c.dropped and c.ts and c.ts >= T0 for c in s.calls.values())]
    if args.session:
        selected = [s for s in sessions if any(s.id.startswith(p) for p in args.session)]
    else:
        selected = [s for s in active if s.role == "worker" and any(x[0] and x[0] >= T0 for x in s.compactions)]
    P = print
    P("AUTO-COMPACT MEASUREMENT (counts from local transcripts; no model was run)")
    P(tr.ASSUMPTION)
    P(f"T = {tr.iso(T0)}; sessions active since T: {len(active)}; sessions for the per-errand sections: "
      f"{[s.id[:8] for s in selected] or 'none'}")
    P("")
    section_identity(P, active, sessions, T0)
    section_since_vs_before(P, active, T0, w, args.like_for_like_hours)
    for s in selected:
        section_errands(P, s, T0)
    section_summary_visibility(P, active, T0, w, selected)
    for s in selected:
        section_visible_cost(P, s, T0, w)
    for s in selected:
        section_costliest(P, s, T0, w)
    section_starts(P, active, T0)
    return 0


if __name__ == "__main__":
    sys.exit(main())
