#!/usr/bin/env python3
"""Per-change quality/cost table for one banto brigade, from local artifacts only:
the Director's transcript, the Workers' transcripts and `git log`. Read-only on
the transcripts and on the repository; no model is run.

Unit of analysis ("change"): commits whose consecutive gap is <= --merge-gap-min
are one change. A change owns everything with a timestamp in (previous change's
last commit, this change's last commit]; the first change starts at --since.

Writes in --out (default: a new directory under the system temp directory;
refused inside a git working tree unless --force-out): report.txt (tables and
definitions), candidates_a.txt (operator problem reports after a review PASS or a
release build), candidates_b.txt (Director retractions), errands_dump.txt and
changes.json. The candidate and dump files hold excerpts of real conversation:
keep --out out of any repository.

It reads banto brigade traffic (Director/Worker transcripts, relay headers); a
transcript set without it yields no changes with errands. Classification of
errands (implementation / review / docs / ...) and the verdict, finding and
retraction detection are keyword heuristics tuned on one team's conventions: see
the named constants below, and treat every number as a candidate count.

Needs Python 3.9+, standard library only. Needs `git` on PATH for the commit log.
"""
import argparse
import glob
import json
import os
import re
import subprocess
import sys
from collections import Counter, defaultdict
from datetime import datetime, timedelta, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import transcripts as tr  # noqa: E402

# ---------------------------------------------------------------------------
# Conventions of the brigade this was tuned on. Each is a heuristic; change them
# to match another team's habits.
# ---------------------------------------------------------------------------

# Display names a Director gave Workers (name_member) mapped to the kind of
# errand that Worker does. Japanese: implementation / review / documentation.
# Extend with --role-name NAME=KIND. English aliases are matched case-insensitively.
ROLE_BY_NAME = {
    "実装": "implementation", "レビュー": "review", "文書": "docs",
    "implementation": "implementation", "review": "review", "reviewer": "review",
    "docs": "docs", "documentation": "docs",
}
KINDS = ["implementation", "review", "docs", "measurement/investigation", "other"]
OUT_NAMES = ["candidates_a.txt", "candidates_b.txt", "errands_dump.txt", "report.txt", "changes.json"]
KIND_LETTER = {"implementation": "I", "review": "R", "docs": "D", "measurement/investigation": "M", "other": "O"}

# Errand-instruction keywords, tried in this order on the first 400 characters
# (see classify()). Each pattern lists phrases a Director used to open that kind.
# 'other': housekeeping that is not work on a change.
OTHER_RX = re.compile(r"(channel check|live-check|pause request|welcome|\bsleep 180\b)", re.I)
# 'measurement/investigation': survey, diagnosis, design-report and measurement errands.
INV_RX = re.compile(r"(^\W*go [A-Z]\b|^\W*phase [A-Z]\d?\b|survey errand|research errand|diagnos|investigat|design report|code-reading|report-only|measurement errand|measurement script|what-if|new measurement|follow-up measurement|measurement,|picker measurement|usage measurement)", re.I)
REVIEW_RX = re.compile(r"(re-?review|review errand|review request|review of |review round|independent review|delta review|final review|final gate run|confirmation (round|review)|short confirmation|short closing check|closing check)", re.I)
DOCS_RX = re.compile(r"(docs? errand|documentation errand|documents? —|docs follow-up|docs touch|docs tidy|docs correction|\.md\b|md only|one more sentence|one sentence to|one clause|a number to correct|one statement|brigade-bell docs)", re.I)
IMPL_RX = re.compile(r"(implementation errand|new errand|follow-on errand|\bfix\b|fixes|implement|go for|unfrozen|fix round|next item|one more item|work resumes|blocker|decisions on|addition to|change to part|two small codex parts)", re.I)
# A Director thanking a Worker for a finished review ("Your review of X was ...")
# opens a follow-up, not a new review: strip that opening before matching.
THANKS_REVIEW_RX = re.compile(r"^\W*(?:thanks[^.]*?[—-]\s*)?your review of", re.I)

# A review report's verdict: an explicit PASS/FAIL, else a DERIVED mark (suffix *).
VERDICT_RX = re.compile(r"\b(PASS|FAIL)\b")
EXPLICIT_VERDICT_RX = re.compile(r"verdict[^A-Za-z]{0,20}(PASS|FAIL)", re.I)
# A refusal or a qualified non-verdict is not a PASS, whatever PASS appears later.
REFUSAL_RX = re.compile(r"\bI cannot\b|cannot (?:honestly|independently)", re.I)
CLEAN_RX = re.compile(r"\bReview complete\b|\bRe-review complete\b|\bclean\b")
NO_BLOCKER_RX = re.compile(r"\bno (?:confirmed )?blocker", re.I)
HOLDS_RX = re.compile(r"\bHOLDS\b")
BLOCK_RX = re.compile(r"\b(BLOCKERS?|BLOCK|DOES NOT (?:FULLY )?HOLD|CANNOT TELL)\b")
# Finding labels at the start of a line of a review report (BLOCKER-1, H2, F3, NIT ...).
ITEM_RX = re.compile(r"(?m)^[\s>*\-•]*(?:\*\*|`)?(?:\d+[.)]\s*)?(?:\*\*|`)?((?:BLOCKER|HIGH|MED(?:IUM)?|LOW|NIT|MAJOR|MINOR|SHOULD-FIX)-?\d{0,2}|(?:F|N|W|H|B|P|T|S|R|NEW-?)\d{1,2}[′']*)(?=[\s:.)—\-*`]|$)")

# Conventional-commit "fix" subject: a later fix touching the same files counts against the change.
FIX_SUBJECT_RX = re.compile(r"^fix(\(|:|!)")
# A Director states the commit a Worker should start from ("HEAD is abc1234").
BASE_HASH_RX = re.compile(r"HEAD (?:is |at |now |was )?(?:now )?([0-9a-f]{7})\b")

# Candidate (a): an operator message that reads like a problem report. Japanese
# words (broken, behaves oddly, bug, not fixed, not working, does not appear,
# vanished, wrong, glitch, error, failure, defect, problem, flicker, slow, heavy,
# freeze, crash, ...) and English ones; NEG_RX removes "no problem"-style phrases first.
PROB_RX = re.compile("|".join([
    "おかしい", "変だ", "変に", "バグ", "直って", "直らな", "動かな", "動いて", "出ない", "出る", "出て", "消え", "落ち", "違う", "違って", "ずれ", "崩れ", "まだ", "ダメ", "だめ", "エラー", "失敗",
    "壊れ", "不具合", "問題", "されない", "見えない", "ちらつ", "遅い", "重い", "固ま", "フリーズ", "クラッシュ", "鳴ら", "鳴って", "鳴る",
    r"\bbug\b", r"\bbroken\b", r"\bwrong\b", r"\bstill\b", r"doesn'?t", r"does not", r"didn'?t", r"not working", r"\berror\b", r"\bfail", r"\bcrash", r"\bpanic",
]), re.I)
NEG_RX = re.compile(r"問題(?:は)?(?:ない|なし|ありません|なさそう)|no problem|not a problem|直って(?:いる|います|た)|動いて(?:いる|います)", re.I)
# Candidate (b): a Director's retraction or correction phrase (English and Japanese:
# correction, retraction, cancel, mistake, error, misunderstanding, earlier statement, sorry, excuse me).
RETRACT_RX = re.compile("|".join([
    r"\bI was wrong\b", r"\bmy (?:mistake|slip|error|bad)\b", r"\bI (?:mis-?read|mis-?stated|misspoke|got (?:that|it|this) wrong)\b", r"\bretract", r"\bcorrection\b", r"\bscratch that\b",
    r"\bdisregard\b", r"\bignore (?:my|the) (?:previous|earlier|last|broadcast)", r"\bthat (?:was|is) wrong\b", r"\bI (?:had|have) it backwards\b", r"\bI reverse", r"\breversing\b",
    r"\bwithdraw", r"\bnever ?mind\b", r"\bon second thought\b", r"\bapolog", r"\bmy earlier\b.{0,60}\b(?:wrong|false|incorrect)", r"\bI (?:said|told|wrote)\b.{0,80}\b(?:wrong|false|incorrect|not true)\b",
    r"\bit was false\b", r"\bnot true\b",
    "訂正", "撤回", "取り消", "間違", "誤り", "勘違い", "前言", "申し訳", "失礼",
]), re.I)


# ---------------------------------------------------------------------------
# git
# ---------------------------------------------------------------------------

def git_log(repo, since_iso):
    """The only external process: `git log` of the repository, oldest first."""
    return subprocess.run(
        ["git", "-C", repo, "log", "--reverse", f"--since={since_iso}", "--no-merges", "--name-only",
         "--format=%x01%h%x02%cI%x02%s"],
        capture_output=True, text=True, encoding="utf-8", errors="replace").stdout


def parse_git_log(out):
    commits = []
    for blk in out.split("\x01")[1:]:
        head, _, files = blk.partition("\n")
        parts = head.split("\x02")
        if len(parts) != 3:
            continue
        h, ts, subj = parts
        t = tr.parse_ts(ts)
        if t is None:
            continue
        commits.append({"hash": h, "ts": t, "subject": subj.strip(),
                        "files": sorted({f.strip() for f in files.splitlines() if f.strip()})})
    commits.sort(key=lambda c: c["ts"])
    return commits


def group_changes(commits, since, merge_gap_min):
    """Consecutive commits <= merge_gap_min apart are one change; each change
    owns (previous change's last commit, its own last commit]."""
    changes = []
    for c in commits:
        if changes and (c["ts"] - changes[-1]["commits"][-1]["ts"]).total_seconds() <= merge_gap_min * 60:
            changes[-1]["commits"].append(c)
        else:
            changes.append({"commits": [c]})
    prev_end = since
    for i, ch in enumerate(changes):
        ch["id"] = i + 1
        ch["start"] = prev_end
        ch["end"] = ch["commits"][-1]["ts"]
        ch["files"] = sorted({f for c in ch["commits"] for f in c["files"]})
        ch["prev_hash"] = changes[i - 1]["commits"][-1]["hash"] if i else None
        prev_end = ch["end"]
    return changes


def change_of(changes, since, ts):
    if ts is None or ts <= since:
        return None
    for ch in changes:
        if ts <= ch["end"]:
            return ch
    return None  # after the last commit: unfinished work, not a change yet


# ---------------------------------------------------------------------------
# events, classification, verdicts
# ---------------------------------------------------------------------------

def window_events(s, lo, hi):
    """Ordered events of one detailed session within [lo, hi): sends, deliveries,
    operator messages, assistant text, release builds. Shapes:
    ('atext', ts, text) ('send', ts, to, text) ('release', ts, cmd)
    ('deliver', ts, parts) ('operator', ts, text)."""
    out = []
    for ev in s.events:
        kind = ev[0]
        ts = ev[1] if len(ev) > 1 else None
        if kind == "call" or kind == "compact" or ts is None or ts < lo or (hi is not None and ts >= hi):
            continue
        if kind == "atext":
            out.append(("atext", ts, ev[2]))
        elif kind == "send":
            out.append(("send", ts, ev[2], ev[3]))
        elif kind == "release":
            out.append(("release", ts, ev[2]))
        elif kind == "deliver":
            if ev[3] is not None:
                out.append(("deliver", ts, ev[3]))
        elif kind == "user":
            _, _, text, _is_nudge, is_human, _is_tag, origin = ev
            if is_human and origin in (None, "human"):
                out.append(("operator", ts, text))
    out.sort(key=lambda e: e[1])
    return out


def classify(text, name, role_by_name):
    """(kind, how) of an errand from its instruction text and the Worker's display name."""
    head = THANKS_REVIEW_RX.sub("", text[:400])
    if OTHER_RX.search(head):
        return "other", "keyword"
    if INV_RX.search(head):
        return "measurement/investigation", "keyword"
    if name in role_by_name:
        return role_by_name[name], "worker-name"
    if name and name.lower() in role_by_name:
        return role_by_name[name.lower()], "worker-name"
    for rx, kind in ((REVIEW_RX, "review"), (DOCS_RX, "docs"), (IMPL_RX, "implementation")):
        if rx.search(head):
            return kind, "keyword"
    return "other", "default"


def verdict_of(text):
    """Explicit PASS/FAIL if the report says one; else a DERIVED mark (suffix *):
    BLK* when the opening names a blocker / 'does not hold', OK* when it says it
    holds / finds no blocker; else None."""
    head = text[:400]
    if REFUSAL_RX.search(head):
        return None
    m = EXPLICIT_VERDICT_RX.search(text)
    if m:
        return m.group(1).upper()
    m = VERDICT_RX.search(head)
    if m:
        return m.group(1)
    if CLEAN_RX.search(head) and not re.search(r"BLOCK", head):
        return "OK*"
    if NO_BLOCKER_RX.search(head) or HOLDS_RX.search(head):
        return "OK*"
    if BLOCK_RX.search(head):
        return "BLK*"
    return None


def finding_tokens(text):
    return sorted({m.group(1).upper() for m in ITEM_RX.finditer(text)})


def findings_of(text):
    return len(finding_tokens(text)), len(re.findall(r"\bblockers?\b", text, re.I))


def excerpt(t, n=260):
    return re.sub(r"\s+", " ", t).strip()[:n]


def kfmt(x):
    return f"{x / 1000:,.0f}k" if x < 1e6 else f"{x / 1e6:,.2f}M"


def fmt_dur(td):
    m = int(td.total_seconds() // 60)
    return f"{m // 60}h{m % 60:02d}"


# ---------------------------------------------------------------------------
# analysis
# ---------------------------------------------------------------------------

class Analysis:
    def __init__(self, since, until, fix_days, role_by_name):
        self.since, self.until, self.fix_days = since, until, fix_days
        self.role_by_name = role_by_name
        self.name_at = defaultdict(list)   # worker -> [(ts, display name the Director saw)]
        self.errands = []

    def learn_names(self, director_events):
        for e in director_events:
            if e[0] == "deliver":
                for sender, name, _scope, _body in e[2]:
                    if name:
                        self.name_at[sender].append((e[1], name))

    def name_of(self, worker, ts):
        cands = [n for t, n in self.name_at.get(worker, []) if t <= ts]
        return cands[-1] if cands else None

    def add_errand(self, worker, start, end, instr, reports, calls, session_id, open_end, source):
        name = self.name_of(worker, start)
        kind, how = classify(instr, name, self.role_by_name)
        base = BASE_HASH_RX.findall(instr)
        e = {"worker": worker, "start": start, "end": end, "instr": instr, "kind": kind, "how": how,
             "reports": list(reports), "calls": calls, "session": session_id, "open": open_end,
             "source": source, "name": name, "base_hash": base[0] if base else None}
        self.errands.append(e)
        return e

    def worker_session_errands(self, s, lab, events):
        """Errands of a Worker with a transcript: a delivery opens one, the first
        send_to_peer after it ends it; only deliveries carrying a Director message count."""
        cur, errs = None, []
        for e in events:
            if e[0] == "deliver":
                instr = "\n\n".join(body for sender, _n, _sc, body in e[2] if sender == "director")
                if cur:
                    cur["end"] = cur["end"] or e[1]
                cur = {"start": e[1], "instr": instr, "reports": [], "end": None}
                errs.append(cur)
            elif e[0] == "send" and cur is not None:
                cur["reports"].append((e[1], e[3]))
                if cur["end"] is None:
                    cur["end"] = e[1]
        for cur in errs:
            if not cur["instr"].strip():
                continue
            end = cur["end"] or s.last_ts
            calls = [c for c in s.calls.values() if not c.dropped and c.ts and cur["start"] <= c.ts <= end]
            self.add_errand(lab, cur["start"], end, cur["instr"], cur["reports"], calls, s.id, cur["end"] is None, "worker transcript")

    def director_side_errands(self, director_events):
        """Workers without a transcript (Codex has none in this format): the
        instruction is a Director send to that Worker (or a broadcast naming it),
        the report is the next message from that Worker the Director received."""
        known = {e["worker"] for e in self.errands}
        senders = {sender for e in director_events if e[0] == "deliver" for sender, _, _, _ in e[2] if sender.startswith("worker-")}
        missing = sorted(senders - known)
        for w in missing:
            pending = None
            for e in director_events:
                if e[0] == "send" and (e[2] == w or (e[2] in (None, "") and re.search(rf"\b{w}\b", e[3]))):
                    if pending is not None and not pending["reports"]:
                        self.add_errand(w, pending["start"], e[1], pending["instr"], [], [], None, False, "director-side (no transcript)")
                    pending = {"start": e[1], "instr": e[3], "reports": [], "end": None}
                elif e[0] == "deliver" and pending is not None:
                    for sender, _n, _sc, body in e[2]:
                        if sender == w:
                            pending["reports"].append((e[1], body))
                            pending["end"] = pending["end"] or e[1]
                    if pending["reports"]:
                        self.add_errand(w, pending["start"], pending["end"], pending["instr"], pending["reports"], [], None, False, "director-side (no transcript)")
                        pending = None
            if pending is not None:
                self.add_errand(w, pending["start"], self.until or pending["start"], pending["instr"], [], [], None, True, "director-side (no transcript)")
        return missing

    def finish_errands(self):
        self.errands = [e for e in self.errands if e["end"] and e["end"] > self.since]
        self.errands.sort(key=lambda e: e["start"])


def aggregate(changes, an, commits, sessions, label_of):
    """Fill each change with errands, usage, reviews, rework, base-hash check and later fixes."""
    since = an.since
    usage = defaultdict(lambda: defaultdict(lambda: [0, 0.0]))  # change id -> label -> [calls, weighted]
    for s in sessions:
        lab = label_of.get(s.id, "unattributed")
        for c in s.calls.values():
            if c.dropped or not c.ts:
                continue
            ch = change_of(changes, since, c.ts)
            if ch is None:
                continue
            a = usage[ch["id"]][lab]
            a[0] += 1
            a[1] += c.weighted
    for ch in changes:
        ch["errands"] = [e for e in an.errands if change_of(changes, since, e["start"]) is ch]
        ch["straddle_out"] = [e for e in ch["errands"] if e["end"] > ch["end"]]
        ch["carried_in"] = [e for e in an.errands if e["start"] <= ch["start"] < e["end"]
                            and change_of(changes, since, e["start"]) is not ch and e["start"] > since - timedelta(days=30)]
        ch["usage"] = usage[ch["id"]]
        ch["reviews"] = []
        for e in ch["errands"]:
            if e["kind"] != "review":
                continue
            if not e["reports"]:
                continue
            for t, tx in e["reports"]:
                v = verdict_of(tx) if tx else None
                fi, bl = findings_of(tx) if tx else (0, 0)
                if v is None and len(e["reports"]) > 1:
                    continue
                ch["reviews"].append({"reviewer": e["worker"], "t": t, "verdict": v, "findings": fi, "blockers": bl, "chars": len(tx)})
                break
        ch["reviews"].sort(key=lambda r: r["t"] or ch["start"])
        impl = [e for e in ch["errands"] if e["kind"] == "implementation"]
        ch["rework"] = max(0, len(impl) - 1)
        with_base = [e for e in ch["errands"] if e["base_hash"]]
        ok = [e for e in with_base if ch["prev_hash"] and ch["prev_hash"].startswith(e["base_hash"][:7])]
        ch["base_check"] = (len(ok), len(with_base))
        horizon = ch["end"] + timedelta(days=an.fix_days)
        later = []
        for c in commits:
            if ch["end"] < c["ts"] <= horizon and FIX_SUBJECT_RX.match(c["subject"]):
                ov = sorted(set(c["files"]) & set(ch["files"]))
                if ov:
                    later.append((c["hash"], c["subject"], len(ov), ov))
        ch["later_fixes"] = later


def find_candidates(changes, since, director_events, release_triggers, horizon_h):
    """(a) operator problem words within horizon_h after a review PASS or a release
    build; (b) Director retraction phrases. Both are candidates only."""
    trig = list(release_triggers)
    for ch in changes:
        for r in ch["reviews"]:
            if r["verdict"] == "PASS" and r["t"]:
                trig.append((r["t"], f"review PASS ({r['reviewer']})"))
    trig.sort()
    horizon = timedelta(hours=horizon_h)
    cand_a, cand_b = defaultdict(list), defaultdict(list)
    for ev in director_events:
        ch = change_of(changes, since, ev[1])
        if ch is None:
            continue
        if ev[0] == "operator" and PROB_RX.search(NEG_RX.sub("", ev[2])):
            before = [x for x in trig if x[0] < ev[1] and ev[1] - x[0] <= horizon]
            if before:
                t0, kind = before[-1]
                cand_a[ch["id"]].append((ev[1], kind, t0, excerpt(ev[2])))
        if ev[0] == "atext" and RETRACT_RX.search(ev[2]):
            cand_b[ch["id"]].append((ev[1], "to operator", excerpt(ev[2], 320)))
        if ev[0] == "send" and RETRACT_RX.search(ev[3]):
            m = RETRACT_RX.search(ev[3])
            cand_b[ch["id"]].append((ev[1], f"to {ev[2] or 'all'}", excerpt(ev[3][max(0, m.start() - 120):], 320)))
    for ch in changes:
        ch["cand_a"] = len(cand_a[ch["id"]])
        ch["cand_b_op"] = sum(1 for x in cand_b[ch["id"]] if x[1] == "to operator")
        ch["cand_b_wk"] = sum(1 for x in cand_b[ch["id"]] if x[1] != "to operator")
    return cand_a, cand_b


# ---------------------------------------------------------------------------
# output
# ---------------------------------------------------------------------------

def write_candidates(out, changes, cand_a, cand_b, horizon_h):
    with tr.open_output(os.path.join(out, "candidates_a.txt")) as fh:
        fh.write(f"(a) operator message with a problem word, within {horizon_h:g} h after a review PASS or a release build. Candidates only.\n\n")
        for ch in changes:
            fh.write(f"## change {ch['id']} {ch['commits'][-1]['hash']} ({len(cand_a[ch['id']])})\n")
            for t, kind, t0, ex in cand_a[ch["id"]]:
                fh.write(f"  {t:%m-%d %H:%M:%SZ}  after {kind} at {t0:%H:%M:%SZ}: {ex}\n")
    with tr.open_output(os.path.join(out, "candidates_b.txt")) as fh:
        fh.write("(b) Director text containing a retraction/correction phrase. Candidates only (high false-positive rate).\n\n")
        for ch in changes:
            fh.write(f"## change {ch['id']} {ch['commits'][-1]['hash']} ({len(cand_b[ch['id']])})\n")
            for t, to, ex in cand_b[ch["id"]]:
                fh.write(f"  {t:%m-%d %H:%M:%SZ}  {to}: {ex}\n")


def write_errands_dump(out, errands, changes, since):
    with tr.open_output(os.path.join(out, "errands_dump.txt")) as fh:
        for e in errands:
            ch = change_of(changes, since, e["start"])
            rep = e["reports"][0][1] if e["reports"] else ""
            is_rev = e["kind"] == "review" and rep
            v = verdict_of(rep) if is_rev else "-"
            fi = findings_of(rep)[0] if is_rev else "-"
            cid = ch["id"] if ch else "-"
            toks = ",".join(finding_tokens(rep)) if e["kind"] == "review" else ""
            fh.write(f"c{cid:>2} {e['start']:%m-%d %H:%MZ} {e['worker']:9} {e['kind'][:6]:6} {e['how'][:4]} v={v} f={fi} "
                     f"nrep={len(e['reports'])} tok={toks} | {excerpt(e['instr'], 90)} || {excerpt(rep, 110)}\n")


def render_report(args, w, director_id, brigade, since, until, commits, changes, errands, label_of, missing, out_dir):
    lines = []
    P = lines.append
    end = until or datetime.now(timezone.utc)
    P(f"QUALITY REPORT  director={director_id[:8]}  brigade={brigade or '?'}  repo={os.path.basename(os.path.abspath(args.repo))}  "
      f"window {since:%Y-%m-%d %H:%MZ} .. {end:%Y-%m-%d %H:%MZ}")
    P(f"commits in window {len(commits)}, changes {len(changes)} (consecutive commits <= {args.merge_gap_min:g} min apart merged); errands {len(errands)}; "
      "workers with transcripts: " + ", ".join(sorted({e['worker'] for e in errands if e['source'] == 'worker transcript'})) + "; without: " + (", ".join(missing) or "-"))
    no_usage = sorted({e["worker"] for e in errands} - set(label_of.values()))
    P(f"workers with errands but no transcript found (usage unmeasured): {', '.join(no_usage) or '-'}; transcript labels found: {', '.join(sorted(set(label_of.values())))}")
    P(w.describe() + ". Workers without a Claude transcript (for example Codex) have no usage here.")
    P(tr.ASSUMPTION)
    P("")
    P("PER-CHANGE TABLE")
    for ch in changes:
        P(f"--- change {ch['id']}: " + " + ".join(f"{c['hash']} {c['subject'][:70]}" for c in ch["commits"]))
        first_act = min([e["start"] for e in ch["errands"]] or [ch["end"]])
        P(f"  wall {fmt_dur(ch['end'] - ch['start'])} (prev change/window start {ch['start']:%m-%d %H:%MZ} -> {ch['end']:%m-%d %H:%MZ}); "
          f"active span from first errand start {fmt_dur(ch['end'] - first_act)}; files in commits {len(ch['files'])}")
        kc = Counter(e["kind"] for e in ch["errands"])
        byw = defaultdict(Counter)
        for e in ch["errands"]:
            byw[e["worker"]][KIND_LETTER[e["kind"]]] += 1
        P("  errands " + str(len(ch["errands"])) + ": " + " ".join(f"{KIND_LETTER[k]}{kc[k]}" for k in KINDS)
          + "   by worker: " + "; ".join(f"{wk} " + "".join(f"{k}{n}" for k, n in sorted(c.items())) for wk, c in sorted(byw.items())))
        seq = [r["verdict"] or "none" for r in ch["reviews"]]
        P("  reviews " + str(len(ch["reviews"])) + ": "
          + (", ".join(f"{r['reviewer'].replace('worker-', 'w')}:{r['verdict'] or 'none'}/{r['findings']}f/{r['blockers']}b" for r in ch["reviews"]) or "-")
          + "   verdict sequence: " + (" ".join(seq) or "-"))
        P(f"  rework rounds (implementation errands after the first) {ch['rework']}")
        u = ch["usage"]
        tot = sum(a[1] for a in u.values()) or 1
        P("  weighted cost " + kfmt(tot) + ": " + ", ".join(f"{lab} {kfmt(a[1])}/{a[0]}c" for lab, a in sorted(u.items(), key=lambda kv: -kv[1][1])))
        lf = ch["later_fixes"]
        P(f"  later fix commits (<= {args.fix_days:g} d, 'fix' subject, same files): {len(lf)}"
          + "".join(f"\n     {h} {sj[:70]} ({n} shared files)" for h, sj, n, _ in lf))
        P(f"  candidates: (a) operator problem after PASS/release {ch['cand_a']}; (b) Director retraction phrases {ch['cand_b_op']} to operator + {ch['cand_b_wk']} to Workers")
        P(f"  attribution: errands straddling the commit {len(ch['straddle_out'])}; errands in flight at window start {len(ch['carried_in'])}; "
          f"errands naming a HEAD hash equal to the previous change's last commit {ch['base_check'][0]}/{ch['base_check'][1]}")
    P("")
    P(f"classification source over all errands: {dict(Counter(e['how'] for e in errands))}; kinds: {dict(Counter(e['kind'] for e in errands))}")
    vs = Counter(r["verdict"] or "none" for ch in changes for r in ch["reviews"])
    P(f"review reports over all changes: {dict(vs)}")
    P(f"candidates files: {os.path.join(out_dir, 'candidates_a.txt')} ; {os.path.join(out_dir, 'candidates_b.txt')}")
    return lines


def write_changes_json(out, changes, weights):
    def plain(ch):
        d = {
            "id": ch["id"], "commits": [(c["hash"], c["subject"], c["ts"].isoformat()) for c in ch["commits"]],
            "start": ch["start"], "end": ch["end"],
            "errands": [(e["worker"], e["kind"], e["how"], e["start"].isoformat(), e["end"].isoformat(), len(e["reports"])) for e in ch["errands"]],
            "reviews": ch["reviews"], "rework": ch["rework"], "usage": dict(ch["usage"]),
            "later_fixes": [(h, sj, n) for h, sj, n, _ in ch["later_fixes"]],
        }
        return {k: (v.isoformat() if isinstance(v, datetime) else v) for k, v in d.items()}
    doc = {"assumption": tr.ASSUMPTION, "weights": weights.as_dict(), "changes": [plain(ch) for ch in changes]}
    with tr.open_output(os.path.join(out, "changes.json")) as fh:
        json.dump(doc, fh, ensure_ascii=False, default=str, indent=1)


# ---------------------------------------------------------------------------
# which transcripts, which Director, which brigade
# ---------------------------------------------------------------------------

def project_files(root, project, repo):
    """Transcripts of the repository's project directory (its path with every
    non-alphanumeric turned into '-'), or every directory whose name contains
    --project. Returns (root, files)."""
    if project:
        return root, tr.find_session_files(root, project)
    pdir = os.path.join(root, tr.project_dir_name(repo))
    return root, sorted(glob.glob(os.path.join(pdir, "**", "*.jsonl"), recursive=True))


def resolve_director_file(arg, files):
    if os.path.isfile(arg):
        return os.path.abspath(arg)
    hits = [f for f in files if "subagents" not in f.replace("\\", "/").split("/")
            and os.path.splitext(os.path.basename(f))[0].startswith(arg)]
    return hits[0] if len(hits) == 1 else None


def choose_brigade(explicit, director, sessions):
    """--brigade, else the brigade in the Director's own briefings, else the most
    common brigade among briefings in files overlapping the Director's lifetime."""
    if explicit:
        return str(explicit), "given"
    own = Counter()
    for (_w, b), n in tr.briefings(director.path).items():
        own[b] += n
    if own:
        return own.most_common(1)[0][0], "the Director's own briefings"
    near = Counter()
    for s in sessions:
        if s is director or s.subagent or not (s.first_ts and s.last_ts and director.first_ts and director.last_ts):
            continue
        if s.last_ts < director.first_ts or s.first_ts > director.last_ts:
            continue
        for (_w, b), n in tr.briefings(s.path).items():
            near[b] += n
    if near:
        return near.most_common(1)[0][0], "the most common briefing in transcripts overlapping the Director's lifetime"
    return None, "none found"


# Each ceiling is one year, which keeps `until + fix_days` and `since - 30 days` inside the calendar
# that tr.parse_flag_ts leaves them (see there).
LIMITS = (("merge_gap_min", "--merge-gap-min", 0.0, 525600.0),
          ("fix_days", "--fix-days", 0.0, 365.0),
          ("horizon_h", "--horizon-h", 0.0, 8760.0))


def build_parser():
    ap = argparse.ArgumentParser(
        prog="quality_report.py",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        description="Per-change quality/cost table for one banto brigade, from transcripts and git log.",
        epilog="Prints report.txt to stdout and writes report.txt, candidates_a.txt, candidates_b.txt,\n"
               "errands_dump.txt and changes.json to --out (default: a new directory under the system temp\n"
               "directory, its path printed). The candidate and dump files contain excerpts of real conversation.\n\n"
               "Candidates (heuristics: every one is to be read by hand, none is a measured defect):\n"
               "  (a) candidates_a.txt: an operator message containing a problem word (bug, broken, still, ...)\n"
               "      that comes within --horizon-h hours after a review PASS or a release build\n"
               "  (b) candidates_b.txt: a Director message, to the operator or to a Worker, containing a\n"
               "      retraction or correction phrase (I was wrong, scratch that, ...); --horizon-h does not\n"
               "      apply to it\n\n"
               "Heuristics, not measurements: every count is a candidate. " + tr.ASSUMPTION)
    ap.add_argument("--repo", default=".", help="git repository the brigade worked on (default: the current directory)")
    ap.add_argument("--director", default=None, metavar="ID_OR_PATH",
                    help="the Director's session: a transcript path or a session id prefix "
                         "(default: the Director-role session with the most calls in the window; the others are listed)")
    ap.add_argument("--brigade", default=None, metavar="N",
                    help="brigade number (default: the one in the Director's own briefings, else the most common "
                         "one in transcripts overlapping its lifetime)")
    tr.add_root_args(ap)
    tr.add_out_args(ap)
    ap.add_argument("--since", default=None,
                    help="ISO-8601 UTC window start (default: the Director's first record). Commits before it are ignored")
    ap.add_argument("--until", default=None, help="ISO-8601 UTC window end (default: now)")
    ap.add_argument("--merge-gap-min", type=float, default=15.0, help="commits this many minutes apart or fewer form one change (0 to 525600; default 15)")
    ap.add_argument("--fix-days", type=float, default=3.0, help="a later 'fix' commit touching the change's files within this many days counts against it (0 to 365; default 3)")
    ap.add_argument("--horizon-h", type=float, default=12.0,
                    help="hours: a candidate (a) (operator message with a problem word) counts only if a review PASS "
                         "or a release build came at most this long before it; 0 to 8760; (a) and (b) are defined below (default 12)")
    ap.add_argument("--role-name", action="append", default=[], metavar="NAME=KIND",
                    help="a Worker display name and the errand kind it implies (implementation, review, docs, "
                         "measurement/investigation, other); repeatable; adds to the built-in names")
    tr.add_weight_args(ap)
    return ap


def main(argv=None):
    args = build_parser().parse_args(argv)
    tr.reconfigure_stdout()
    since = tr.parse_flag_ts(args.since)
    until = tr.parse_flag_ts(args.until)
    if args.since and since is None or args.until and until is None:
        print("--since/--until must be ISO-8601, e.g. 2026-01-31 or 2026-01-31T12:00:00Z", file=sys.stderr)
        return 2
    role_by_name = dict(ROLE_BY_NAME)
    try:
        tr.check_flags(args, LIMITS)
        w = tr.weights_from_args(args)
        for item in args.role_name:
            name, sep, kind = item.partition("=")
            if not sep or kind not in KINDS:
                raise ValueError(f"--role-name wants NAME=KIND with KIND one of {', '.join(KINDS)}; got {item!r}")
            role_by_name[name] = kind
        out = tr.prepare_out_dir(args.out, "quality_report", args.force_out, names=OUT_NAMES)
    except (ValueError, tr.OutDirError) as err:
        print(err, file=sys.stderr)
        return 2
    if not os.path.isdir(args.repo):
        print(f"--repo {args.repo}: not a directory", file=sys.stderr)
        return 2

    if args.director and os.path.isfile(args.director):
        proj = os.path.dirname(os.path.abspath(args.director))
        root, files = os.path.dirname(proj), sorted(glob.glob(os.path.join(proj, "**", "*.jsonl"), recursive=True))
    else:
        root, files = project_files(args.root or tr.default_root(), args.project, args.repo)
    if not files:
        print(f"no transcripts found for repo {os.path.abspath(args.repo)} under {root} (looked for the project directory "
              f"{tr.project_dir_name(args.repo)}); pass --root, --project or --director", file=sys.stderr)
        return 1
    dir_file = resolve_director_file(args.director, files) if args.director else None
    if args.director and dir_file is None:
        print(f"--director {args.director}: no unique transcript matches", file=sys.stderr)
        return 1
    # A file not modified since the window opened cannot hold records inside it.
    keep = [f for f in files if f == dir_file or since is None or os.path.getmtime(f) >= since.timestamp() - 1]
    sessions = tr.load_sessions(root, files=keep, detail=True)
    tr.dedupe_calls(sessions, w)

    if dir_file:
        director = next(s for s in sessions if os.path.abspath(s.path) == os.path.abspath(dir_file))
    else:
        cands = [s for s in sessions if s.role == "director" and not s.subagent]
        lo = since
        def n_in(s):
            return sum(1 for c in s.calls.values() if not c.dropped and (lo is None or (c.ts and c.ts >= lo)))
        cands.sort(key=lambda s: -n_in(s))
        if not cands:
            print("no Director-role session (one that calls request_decision, name_member, consult_goinkyo or dismiss_goinkyo) found; "
                  "pass --director", file=sys.stderr)
            return 1
        director = cands[0]
        print("Director candidates (most calls in the window first): "
              + ", ".join(f"{s.id[:8]}({n_in(s)} calls)" for s in cands[:5]) + f" -> using {director.id[:8]}")
    since = since or director.first_ts
    if since is None:
        print("the Director transcript has no timestamps; pass --since", file=sys.stderr)
        return 1
    brigade, how = choose_brigade(args.brigade, director, sessions)
    print(f"brigade {brigade or '?'} (from {how}); window since {tr.iso(since)}")

    label_of = tr.label_sessions(sessions, brigade, director.id)
    lo_ev = since
    hi_ev = until + timedelta(days=args.fix_days) if until else None
    director_events = window_events(director, lo_ev, hi_ev)
    an = Analysis(since, until, args.fix_days, role_by_name)
    an.learn_names(director_events)

    for s in sessions:
        lab = label_of.get(s.id)
        if not lab or lab in ("director", "goinkyo") or lab.startswith("sub:"):
            continue
        an.worker_session_errands(s, lab, window_events(s, lo_ev, hi_ev))
    missing = an.director_side_errands(director_events)
    an.finish_errands()

    commits = parse_git_log(git_log(args.repo, tr.iso(since)))
    if until:
        commits = [c for c in commits if c["ts"] < until]
    changes = group_changes(commits, since, args.merge_gap_min)
    aggregate(changes, an, commits, sessions, label_of)

    releases = [(e[1], "release build (Director)") for e in director_events if e[0] == "release"]
    for s in sessions:
        lab = label_of.get(s.id)
        if lab and lab != "director" and not lab.startswith("sub:"):
            releases += [(e[1], f"release build ({lab})") for e in window_events(s, lo_ev, hi_ev) if e[0] == "release"]
    cand_a, cand_b = find_candidates(changes, since, director_events, releases, args.horizon_h)

    try:
        write_candidates(out, changes, cand_a, cand_b, args.horizon_h)
        write_errands_dump(out, an.errands, changes, since)
        lines = render_report(args, w, director.id, brigade, since, until, commits, changes, an.errands, label_of, missing, out)
        with tr.open_output(os.path.join(out, "report.txt")) as fh:
            fh.write("\n".join(lines))
        write_changes_json(out, changes, w)
    except tr.OutDirError as err:
        print(err, file=sys.stderr)
        return 2
    print("\n".join(lines))
    print(f"outputs written to {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
