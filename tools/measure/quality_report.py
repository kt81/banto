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
release build), candidates_b.txt (Director retractions; each candidate in both files
has a stable id), errands_dump.txt and changes.json. report.txt also carries the effect
metrics per change and a WINDOW SUMMARY at its end (caught by review, escaped review,
Director reversals by trigger, lead time, parallelism, operator effort; see README.md);
--judgments FILE turns candidate counts into judged counts. The candidate and dump files
hold excerpts of real conversation: keep --out out of any repository.

It reads banto brigade traffic (Director/Worker transcripts, relay headers); a
transcript set without it yields no changes with errands. Classification of
errands (implementation / review / docs / ...) and the verdict, finding and
retraction detection are keyword heuristics tuned on one team's conventions: see
the named constants below, and treat every number as a candidate count.

Needs Python 3.9+, standard library only. Needs `git` on PATH for the commit log.
"""
import argparse
import glob
import hashlib
import json
import os
import re
import statistics
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
REQUEST_DECISION_AVAILABLE = datetime(2026, 11, 15, 10, 43, 38, tzinfo=timezone.utc)

KIND_LETTER = {"implementation": "I", "review": "R", "docs": "D", "measurement/investigation": "M", "other": "O"}

# Errand-instruction keywords, tried in this order on the first 400 characters
# (see classify()). Each pattern lists phrases a Director used to open that kind.
# 'other': housekeeping that is not work on a change.
OTHER_RX = re.compile(r"(channel check|live-check|pause request|welcome|\bsleep 180\b)", re.I)
# 'measurement/investigation': survey, diagnosis, design-report and measurement errands.
INV_RX = re.compile(r"(^\W*go [A-Z]\b|^\W*phase [A-Z]\d?\b|survey errand|research errand|diagnos|investigat|design report|code-reading|report-only|measurement errand|measurement script|what-if|new measurement|follow-up measurement|measurement,|picker measurement|usage measurement)", re.I)
# "review of " is the broadest review phrase: an implementation errand can mention a review in passing
# ("...; review of the old notes is below"). It beats a Worker's name only when it opens the instruction.
REVIEW_RX = re.compile(r"(re-?review|review errand|review request|review round|independent review|delta review|final review|final gate run|confirmation (round|review)|short confirmation|short closing check|closing check)", re.I)
DOCS_RX = re.compile(r"(docs? errand|documentation errand|documents? —|docs follow-up|docs touch|docs tidy|docs correction|\.md\b|md only|one more sentence|one sentence to|one clause|a number to correct|one statement|brigade-bell docs)", re.I)
IMPL_RX = re.compile(r"(implementation errand|new errand|follow-on errand|\bfix\b|fixes|implement|go for|unfrozen|fix round|next item|one more item|work resumes|blocker|decisions on|addition to|change to part|two small codex parts)", re.I)
# A Director thanking a Worker for a finished review ("Your review of X was ...")
# opens a follow-up, not a new review: strip that opening before matching.
THANKS_REVIEW_RX = re.compile(r"^\W*(?:thanks[^.]*?[—-]\s*)?your review of", re.I)
REVIEW_OF_RX = re.compile(r"review of ", re.I)
OPENING_REVIEW_OF_RX = re.compile(r"^\W*review of ", re.I)

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

# Finding headings are narrower than ITEM_RX: ordinary numbered checklists are not findings.
FINDING_SEVERITY_RX = re.compile(r"^\s*(?:[-*>#•]+\s*)?(?:\*\*|`)?(?:(\d+)[.)]\s*)?(?:\*\*|`)?(BLOCKER|HIGH|MED(?:IUM)?|LOW|NIT|MAJOR|MINOR|SHOULD-FIX)(?:[ -]?(\d+))?(?=[\s:.)—\-*`]|$)", re.I)
FINDING_SENTENCE_RX = re.compile(r"^\s*(?:\w+\s+)?finding\s*,\s*(?:BLOCKER|HIGH|MED(?:IUM)?|LOW|NIT|MAJOR|MINOR)\b.*\b[\w.-]+\.[a-z0-9]+:\d+", re.I)
FINDING_LABEL_RX = re.compile(r"^\s*(?:[-*>#•]+\s*)?(?:\*\*|`)?([FNHWB]\d{1,2}[′']*)(?=[\s:.)—\-*`]|$)")
CLEAR_FINDING_RX = re.compile(r"\b(?:pass(?:es|ed)?|holds|no (?:issue|problem|finding|blocker)s?|confirmed clean|fixed|resolved)\b", re.I)
NEGATED_CLEAR_RX = re.compile(r"\b(?:not|never|isn't|wasn't|weren't|hasn't|haven't|hadn't)\b(?:\W+\w+){0,3}\W*$", re.I)
EMPTY_SEVERITY_RX = re.compile(r"(?:none(?:\s+found)?|no\s+(?:issues?|problems?|findings?|blockers?)|0)(?:[.!])?", re.I)

# Conventional-commit "fix" subject: a later fix touching the same files counts against the change.
FIX_SUBJECT_RX = re.compile(r"^fix(\(|:|!)")
# A Director states the commit a Worker should start from ("HEAD is abc1234").
BASE_HASH_RX = re.compile(r"HEAD (?:is |at |now |was )?(?:now )?([0-9a-f]{7})\b")

# Candidate (a): an operator message that reads like a problem report. Japanese
# words (broken, behaves oddly, bug, not fixed, not working, does not appear,
# vanished, wrong, glitch, error, failure, defect, problem, flicker, slow, heavy,
# freeze, crash, ...) and English ones; NEG_RX removes "no problem"-style phrases first.
# Two words were dropped after a person judged a first run's candidates: "vanished" (消え) and
# "sound plays" (鳴ら) each matched several candidates judged not a problem and none judged real
# or unsure. The other words stay: each of them matched a real or unsure candidate, matched at
# most one 'not', or matched nothing in that run. One window and one judge: it is a first pass.
PROB_WORDS = [
    "おかしい", "変だ", "変に", "バグ", "直って", "直らな", "動かな", "動いて", "出ない", "出る", "出て", "落ち", "違う", "違って", "ずれ", "崩れ", "まだ", "ダメ", "だめ", "エラー", "失敗",
    "壊れ", "不具合", "問題", "されない", "見えない", "ちらつ", "遅い", "重い", "固ま", "フリーズ", "クラッシュ", "鳴って", "鳴る",
    r"\bbug\b", r"\bbroken\b", r"\bwrong\b", r"\bstill\b", r"doesn'?t", r"does not", r"didn'?t", r"not working", r"\berror\b", r"\bfail", r"\bcrash", r"\bpanic",
]
PROB_RX = re.compile("|".join(PROB_WORDS), re.I)
NEG_RX = re.compile(r"問題(?:は)?(?:ない|なし|ありません|なさそう)|no problem|not a problem|直って(?:いる|います|た)|動いて(?:いる|います)", re.I)
# Candidate (b): a Director's retraction or correction phrase (English and Japanese:
# correction, retraction, cancel, mistake, error, misunderstanding, earlier statement, sorry, excuse me).
# Two phrases were dropped after a person judged a first run's candidates: "cancel" (取り消) and
# "ignore my/the previous|earlier|last|broadcast" each were the only match of several candidates
# (eleven and six) and every one was judged not a reversal. 誤り ("error") matched mostly 'not' but
# also eight real ones, so it stays; phrases with one or two matches either way are left alone.
RETRACT_PHRASES = [
    r"\bI was wrong\b", r"\bmy (?:mistake|slip|error|bad)\b", r"\bI (?:mis-?read|mis-?stated|misspoke|got (?:that|it|this) wrong)\b", r"\bretract", r"\bcorrection\b", r"\bscratch that\b",
    r"\bdisregard\b", r"\bthat (?:was|is) wrong\b", r"\bI (?:had|have) it backwards\b", r"\bI reverse", r"\breversing\b",
    r"\bwithdraw", r"\bnever ?mind\b", r"\bon second thought\b", r"\bapolog", r"\bmy earlier\b.{0,60}\b(?:wrong|false|incorrect)", r"\bI (?:said|told|wrote)\b.{0,80}\b(?:wrong|false|incorrect|not true)\b",
    r"\bit was false\b", r"\bnot true\b",
    "訂正", "撤回", "間違", "誤り", "勘違い", "前言", "申し訳", "失礼",
]
RETRACT_RX = re.compile("|".join(RETRACT_PHRASES), re.I)


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
    operator messages, assistant text, release builds, and the inputs that are not
    the operator's. Shapes:
    ('atext', ts, text) ('send', ts, to, text) ('release', ts, cmd)
    ('deliver', ts, parts) ('operator', ts, text) ('nudge', ts) ('system', ts)
    ('result', ts) ('decision', ts). An atext, send or operator event carries one more element,
    the name of its transcript record (see event_key), when the record had a uuid.

    An operator message is a user record the operator typed: not a tool result, not
    a relay nudge (those carry origin "human" too, so the prefix is what tells them
    apart), not a record the harness or banto inserted (a `<tag>` wrapper, a
    `[banto ...]` line typed into a pane, a task notification or channel message).
    Meta, compaction-summary and transcript-only records never reach here: the reader drops them.
    'system' is every other user record, 'result' a tool result that carried no
    delivery, 'decision' a request_decision call."""
    out = []
    keys = getattr(s, "keys", {})
    for index, ev in enumerate(s.events):
        kind = ev[0]
        ts = ev[1] if len(ev) > 1 else None
        if kind == "call" or kind == "compact" or ts is None or ts < lo or (hi is not None and ts >= hi):
            continue
        key = (keys[index],) if index in keys else ()
        if kind == "atext":
            out.append(("atext", ts, ev[2]) + key)
        elif kind == "send":
            out.append(("send", ts, ev[2], ev[3]) + key)
        elif kind == "release":
            out.append(("release", ts, ev[2]))
        elif kind == "deliver":
            if ev[3] is not None:
                out.append(("deliver", ts, ev[3]))
        elif kind in ("result", "decision"):
            out.append((kind, ts))
        elif kind == "user":
            _, _, text, is_nudge, is_human, _is_tag, origin = ev
            if is_nudge:
                out.append(("nudge", ts))
            elif is_human and origin in (None, "human") and not text.lstrip().startswith("[banto"):
                out.append(("operator", ts, text) + key)
            else:
                out.append(("system", ts))
    out.sort(key=lambda e: e[1])
    return out


def classify(text, name, role_by_name):
    """(kind, how) of an errand from its instruction text and the Worker's display name.

    Order: housekeeping and investigation keywords, then a review instruction, then the display
    name, then the docs and implementation keywords. A review instruction outranks a name because
    a name is a standing label and an instruction is what the Worker was asked this time: a Worker
    named for implementation that is sent to review is doing a review. "review of " counts before
    the name only when it opens the instruction: mid-instruction it is often a passing mention, as
    in "Fix the parser; review of the old notes is below", which is implementation. The docs and
    implementation words stay below the name, since a name is the better evidence than a loose
    word such as "fix"."""
    head = THANKS_REVIEW_RX.sub("", text[:400])
    if OTHER_RX.search(head):
        return "other", "keyword"
    if INV_RX.search(head):
        return "measurement/investigation", "keyword"
    if REVIEW_RX.search(head) or OPENING_REVIEW_OF_RX.search(head):
        return "review", "keyword"
    if name in role_by_name:
        return role_by_name[name], "worker-name"
    if name and name.lower() in role_by_name:
        return role_by_name[name.lower()], "worker-name"
    for rx, kind in ((REVIEW_OF_RX, "review"), (DOCS_RX, "docs"), (IMPL_RX, "implementation")):
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


def selected_review_report(reports):
    """Prefer the first explicit verdict across messages, then the first derived verdict."""
    derived = None
    for t, text in reports:
        verdict = verdict_of(text) if text else None
        if verdict in ("PASS", "FAIL"):
            return t, text, verdict
        if verdict and derived is None:
            derived = (t, text, verdict)
    return derived or (reports[0][0], reports[0][1], None)


def has_clear_check(line):
    return any(not NEGATED_CLEAR_RX.search(line[:match.start()]) for match in CLEAR_FINDING_RX.finditer(line))


def neutral_findings(reports):
    """Count severity headings and labelled findings; suppress labelled clear checks.

    Numbered severity headings are distinct within a message, while explicit labels
    are deduplicated across follow-up messages. This is still a heuristic.
    """
    found = set()
    for message, (_ts, text) in enumerate(reports):
        for line in text.splitlines():
            severity = FINDING_SEVERITY_RX.match(line)
            if severity:
                detail = line[severity.end():].strip(" \t:—-.*`#")
                if not detail or EMPTY_SEVERITY_RX.fullmatch(detail):
                    continue
                number, word, suffix = severity.groups()
                key = ("number", message, number) if number else ("severity", word.upper(), suffix or "")
                found.add(key)
                continue
            if FINDING_SENTENCE_RX.match(line):
                found.add(("sentence", message, line))
                continue
            label = FINDING_LABEL_RX.match(line)
            if label and not has_clear_check(line):
                found.add(("label", label.group(1).upper()))
    return len(found)


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
        self.name_at = defaultdict(list)   # worker -> [(ts, display name the Director saw, None once cleared)]
        self.errands = []

    def learn_names(self, director_events):
        """The name each sender carried in each delivery, None where it carried none. banto prints a
        member's display name in the header of its messages and prints none once the Director has
        cleared it, so a header without a name is the evidence that the name is gone: skipping it
        kept a cleared name on every later errand (a Worker whose name had been cleared stayed a
        docs Worker in this report for days). A member that sends nothing after the clearing keeps
        its old name here, since nothing shows that it was cleared."""
        for e in director_events:
            if e[0] == "deliver":
                for sender, name, _scope, _body in e[2]:
                    self.name_at[sender].append((e[1], name or None))

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
        the first received report ends the errand; further messages before the next instruction stay with it."""
        known = {e["worker"] for e in self.errands}
        senders = {sender for e in director_events if e[0] == "deliver" for sender, _, _, _ in e[2] if sender.startswith("worker-")}
        missing = sorted(senders - known)
        for w in missing:
            pending = None
            for e in director_events:
                if e[0] == "send" and (e[2] == w or (e[2] in (None, "") and re.search(rf"\b{w}\b", e[3]))):
                    if pending is not None:
                        self.add_errand(w, pending["start"], pending["end"] or e[1], pending["instr"], pending["reports"], [], None, False, "director-side (no transcript)")
                    pending = {"start": e[1], "instr": e[3], "reports": [], "end": None}
                elif e[0] == "deliver" and pending is not None:
                    for sender, _n, _sc, body in e[2]:
                        if sender == w:
                            pending["reports"].append((e[1], body))
                            pending["end"] = pending["end"] or e[1]
            if pending is not None:
                self.add_errand(w, pending["start"], pending["end"] or self.until or pending["start"], pending["instr"], pending["reports"], [], None, pending["end"] is None, "director-side (no transcript)")
        return missing

    def finish_errands(self):
        self.errands = [e for e in self.errands if e["end"] and e["end"] > self.since]
        self.errands.sort(key=lambda e: e["start"])


def aggregate(changes, an, commits, sessions, label_of):
    """Fill each change with errands, usage, reviews, rework, base-hash check and later fixes.
    Returns the calls whose cost went into some change's usage."""
    since = an.since
    usage = defaultdict(lambda: defaultdict(lambda: [0, 0.0]))  # change id -> label -> [calls, weighted]
    ran = defaultdict(lambda: defaultdict(lambda: [0, 0.0]))    # change id -> (role, model, effort) -> [calls, weighted]
    counted = []
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
            if tr.ran_call(c):
                r = ran[ch["id"]][(s.role, c.model, c.effort)]
                r[0] += 1
                r[1] += c.weighted
            counted.append(c)
    for ch in changes:
        ch["errands"] = [e for e in an.errands if change_of(changes, since, e["start"]) is ch]
        ch["straddle_out"] = [e for e in ch["errands"] if e["end"] > ch["end"]]
        ch["carried_in"] = [e for e in an.errands if e["start"] <= ch["start"] < e["end"]
                            and change_of(changes, since, e["start"]) is not ch and e["start"] > since - timedelta(days=30)]
        ch["usage"] = usage[ch["id"]]
        ch["ran"] = ran[ch["id"]]
        ch["reviews"] = []
        for e in ch["errands"]:
            if e["kind"] != "review":
                continue
            if not e["reports"]:
                continue
            t, tx, v = selected_review_report(e["reports"])
            fi, bl = findings_of(tx) if tx else (0, 0)
            ch["reviews"].append({"reviewer": e["worker"], "t": t, "verdict": v, "findings": fi, "neutral_findings": neutral_findings(e["reports"]), "blockers": bl, "chars": len(tx)})
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
    return counted


def candidate_id(session_id, ts, kind, n=0, key=None):
    """A stable name for one candidate: the same inputs name it the same on every run, so a hand
    judgment written against one run still applies to the next. `kind` is "a", "b-text" or "b-send".
    With `key`, the name of the transcript record the candidate came from (its uuid, plus the
    block index or the tool_use id inside the record), the id depends on nothing else: not the
    timestamp, not any other record. That is stable because Claude Code appends transcript records
    and never rewrites them; a rewritten record could move an id keyed by block index. Without a
    key, `n` is the position among the same-kind records sharing the timestamp, counted over every
    such record whether or not a word list matched it; that id moves when a record is added or
    removed at that timestamp, so it is the fallback for a record that had no uuid, not a stable
    name. The id keeps 32 bits of the hash, so two candidates can collide by chance: see id_collisions."""
    if key is not None:
        raw = f"{session_id}\x00{kind}\x00key\x00{key}"
    else:
        raw = f"{session_id}\x00{ts.isoformat()}\x00{kind}\x00{n}"
    return f"{kind[0].upper()}-{hashlib.sha1(raw.encode('utf-8')).hexdigest()[:8]}"


def event_key(ev):
    """The transcript-record name window_events attached to an operator, atext or send event, or
    None when its record had no uuid (or the event was built without one)."""
    width = {"operator": 4, "atext": 4, "send": 5}.get(ev[0])
    return ev[width - 1] if width and len(ev) == width else None


def find_candidates(changes, since, director_events, release_triggers, horizon_h, session_id="?"):
    """(a) operator problem words within horizon_h after a review PASS or a release
    build; (b) Director retraction phrases. Both are candidates only. Each carries its
    stable id; a (b) candidate also carries what started its turn and what immediately preceded
    it (see turn_trigger, predecessor_trigger).
    cand_a rows: (ts, kind, t0, excerpt, id); cand_b rows: (ts, to, excerpt, id, trigger, predecessor)."""
    trig = list(release_triggers)
    for ch in changes:
        for r in ch["reviews"]:
            if r["verdict"] == "PASS" and r["t"]:
                trig.append((r["t"], f"review PASS ({r['reviewer']})"))
    trig.sort()
    horizon = timedelta(hours=horizon_h)
    cand_a, cand_b = defaultdict(list), defaultdict(list)
    seen = Counter()
    id_kind = {"operator": "a", "atext": "b-text", "send": "b-send"}

    for i, ev in enumerate(director_events):
        kind_id = id_kind.get(ev[0])
        cid = None
        if kind_id:
            cid = candidate_id(session_id, ev[1], kind_id, seen[(ev[1], kind_id)], key=event_key(ev))
            seen[(ev[1], kind_id)] += 1
        ch = change_of(changes, since, ev[1])
        if ch is None:
            continue
        if ev[0] == "operator" and PROB_RX.search(NEG_RX.sub("", ev[2])):
            before = [x for x in trig if x[0] < ev[1] and ev[1] - x[0] <= horizon]
            if before:
                t0, kind = before[-1]
                cand_a[ch["id"]].append((ev[1], kind, t0, excerpt(ev[2]), cid))
        if ev[0] == "atext" and RETRACT_RX.search(ev[2]):
            cand_b[ch["id"]].append((ev[1], "to operator", excerpt(ev[2], 320), cid,
                                     turn_trigger(director_events, i), predecessor_trigger(director_events, i)))
        if ev[0] == "send" and RETRACT_RX.search(ev[3]):
            m = RETRACT_RX.search(ev[3])
            cand_b[ch["id"]].append((ev[1], f"to {ev[2] or 'all'}", excerpt(ev[3][max(0, m.start() - 120):], 320), cid,
                                     turn_trigger(director_events, i), predecessor_trigger(director_events, i)))
    for ch in changes:
        ch["cand_a"] = len(cand_a[ch["id"]])
        ch["cand_b_op"] = sum(1 for x in cand_b[ch["id"]] if x[1] == "to operator")
        ch["cand_b_wk"] = sum(1 for x in cand_b[ch["id"]] if x[1] != "to operator")
    return cand_a, cand_b


# ---------------------------------------------------------------------------
# effect metrics: what working as a brigade bought the operator
# ---------------------------------------------------------------------------

TRIGGERS = ("operator", "worker", "self")
JUDGMENT_VERDICTS = ("real", "not", "unsure")
JUDGMENT_ID_RX = re.compile(r"^[AB]-[0-9a-f]{8}$", re.I)
WORKER_SENDERS = ("worker-", "goinkyo")


def parse_judgments(text):
    """The hand judgments of candidates: one `<id> <real|not|unsure> [free note]` per line; blank
    and `#` lines are ignored. Returns ({id: (verdict, note)}, warnings). A malformed line is a
    warning, never an error; a repeated id keeps its last line."""
    out, warnings = {}, []
    for n, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split(None, 2)
        if len(parts) < 2 or not JUDGMENT_ID_RX.match(parts[0]) or parts[1].lower() not in JUDGMENT_VERDICTS:
            warnings.append(f"judgments line {n}: not '<id> <real|not|unsure> [note]', ignored")
            continue
        cid = parts[0][0].upper() + parts[0][1:].lower()
        if cid in out:
            warnings.append(f"judgments line {n}: {cid} was already judged, the later line wins")
        out[cid] = (parts[1].lower(), parts[2].strip() if len(parts) > 2 else "")
    return out, warnings


def id_collisions(cand_a, cand_b):
    """The ids that name more than one candidate in this run, sorted. The id is 32 bits of a hash, so
    a chance collision is possible and nothing else would show it: a judgment for such an id would
    silently land on both candidates."""
    n = Counter(x[4] for rows in cand_a.values() for x in rows)
    n.update(x[3] for rows in cand_b.values() for x in rows)
    return sorted(i for i, c in n.items() if c > 1)


def refuse_ambiguous(judgments, collisions):
    """(the judgments minus those for a colliding id, a warning for each one dropped). The caller's
    dict is left as it was."""
    usable = {i: j for i, j in judgments.items() if i not in collisions}
    warnings = [f"judgments: {i} names more than one candidate of this run (an id collision), so it is ambiguous and applied to neither"
                for i in sorted(judgments) if i in collisions]
    return usable, warnings


def judged_counts(ids, judgments):
    """How many of `ids` were judged real / not / unsure, and how many were not judged."""
    c = {"real": 0, "not": 0, "unsure": 0, "unjudged": 0}
    for i in ids:
        j = judgments.get(i)
        c[j[0] if j else "unjudged"] += 1
    return c


def worker_message(ev):
    """True when a delivery event carries a message from a Worker or from Goinkyo."""
    return any(p[0].startswith(WORKER_SENDERS) for p in ev[2])


def turn_trigger(events, i):
    """What started the Director's turn in which events[i] was written: 'operator' if the last thing
    to start a turn was a message the operator typed, 'worker' if it was a relay nudge or a
    check_messages answer carrying a Worker's or Goinkyo's message, else 'self'. Tool results,
    system text and answers with no Worker in them do not start a turn, and the Director's own text,
    sends and calls are never inputs. A delivery carrying a message from the operator counts as the
    operator's. It cannot see self-correction: a turn an operator message or a Worker report started
    counts as that trigger even when the Director found the error itself."""
    for ev in reversed(events[:i]):
        kind = ev[0]
        if kind == "operator":
            return "operator"
        if kind == "nudge":
            return "worker"
        if kind == "deliver":
            if any(p[0] == "operator" for p in ev[2]):
                return "operator"
            if worker_message(ev):
                return "worker"
    return "self"


def predecessor_trigger(events, i):
    """The same three names for the most recent input of ANY kind before events[i]: a tool result,
    system text or an answer with no Worker message is 'self'. Mostly 'self', since a Director
    writes most of its text right after a tool call; kept as the secondary column."""
    for ev in reversed(events[:i]):
        kind = ev[0]
        if kind == "operator":
            return "operator"
        if kind == "nudge":
            return "worker"
        if kind == "deliver":
            if any(p[0] == "operator" for p in ev[2]):
                return "operator"
            return "worker" if worker_message(ev) else "self"
        if kind in ("result", "system"):
            return "self"
    return "self"


def open_time(errands, lo, hi):
    """(seconds with at least one errand open, seconds with two or more open at once), clipped to
    [lo, hi]. Errands that only touch end to start are not parallel."""
    points = []
    for e in errands:
        a, b = max(e["start"], lo), min(e["end"], hi)
        if b > a:
            points += [(a, 1), (b, -1)]
    points.sort(key=lambda p: (p[0], p[1]))   # at one instant the closes come first
    open_s = multi_s = 0.0
    depth, last = 0, None
    for t, d in points:
        if last is not None and depth >= 1:
            open_s += (t - last).total_seconds()
            if depth >= 2:
                multi_s += (t - last).total_seconds()
        depth += d
        last = t
    return open_s, multi_s


def never_reported(ch):
    """Errands that started in the change and got no report back. Their end is only the Worker's
    last record, not a finish, so lead time and parallelism leave them out."""
    return sum(1 for e in ch["errands"] if not e["reports"])


def lead_time(ch):
    """(from the first errand that reported to the change's last commit, None) or (None, why it is
    empty). Errands that never reported are left out, see never_reported."""
    if not ch["errands"]:
        return None, "no errand opened in this change's window"
    starts = [e["start"] for e in ch["errands"] if e["reports"]]
    if not starts:
        return None, f"never reported: all {len(ch['errands'])} errand(s) of this change got no report, so none is counted"
    return ch["end"] - min(starts), None


def review_catch(reviews):
    """What the review rounds of one change reported. A FAIL is counted, not judged: that it was
    right is a hand question. BLK* is a derived mark (no explicit verdict) and is kept apart."""
    fails = [r for r in reviews if r["verdict"] == "FAIL"]
    return {"rounds": len(reviews), "fail_rounds": len(fails), "fail_findings": sum(r["findings"] for r in fails),
            "fail_blockers": sum(r["blockers"] for r in fails),
            "derived_blk_rounds": sum(1 for r in reviews if r["verdict"] == "BLK*"),
            "stopped_rounds": sum(1 for r in reviews if r["verdict"] in ("FAIL", "BLK*")),
            "stopped_findings": sum(r.get("neutral_findings", 0) for r in reviews if r["verdict"] in ("FAIL", "BLK*"))}


def compute_effects(changes, errands, director_events, since, cand_a, cand_b, judgments):
    """Fill ch["effect"] for every change and return the window summary (both plain data).
    `judgments` is None when no --judgments file was given."""
    reported = [e for e in errands if e["reports"]]
    owned = {id(e) for ch in changes for e in ch["errands"]}
    left_out = [e for e in errands if not e["reports"]
                and (id(e) in owned or any(min(e["end"], ch["end"]) > max(e["start"], ch["start"]) for ch in changes))]
    ops, decisions = defaultdict(int), defaultdict(int)
    for ev in director_events:
        if ev[0] in ("operator", "decision"):
            ch = change_of(changes, since, ev[1])
            if ch is not None:
                (ops if ev[0] == "operator" else decisions)[ch["id"]] += 1
    for ch in changes:
        a_ids = [x[4] for x in cand_a[ch["id"]]]
        b = cand_b[ch["id"]]
        by = {t: sum(1 for x in b if x[4] == t) for t in TRIGGERS}
        by_pred = {t: sum(1 for x in b if x[5] == t) for t in TRIGGERS}
        lt, lt_note = lead_time(ch)
        open_s, multi_s = open_time(reported, ch["start"], ch["end"])
        ch["effect"] = {
            "review": review_catch(ch["reviews"]),
            "escapes": {"candidates_a": len(a_ids), "ids_a": a_ids, "fix_commits": len(ch["later_fixes"]),
                        "judged_a": judged_counts(a_ids, judgments) if judgments is not None else None},
            "reversals": {"by_trigger": by, "by_predecessor": by_pred,
                          "candidates": [{"id": x[3], "to": x[1], "trigger": x[4], "predecessor": x[5]} for x in b],
                          "judged": ({t: judged_counts([x[3] for x in b if x[4] == t], judgments) for t in TRIGGERS}
                                     if judgments is not None else None)},
            "lead_time_s": lt.total_seconds() if lt is not None else None,
            "lead_time_note": lt_note, "left_out_errands": never_reported(ch),
            "parallel": {"open_s": open_s, "multi_s": multi_s, "share": multi_s / open_s if open_s else None},
            "operator_messages": ops[ch["id"]], "decisions": (decisions[ch["id"]] if ch["start"] >= REQUEST_DECISION_AVAILABLE else None),
            "cost": sum(a[1] for a in ch["usage"].values()),
        }
    es = [ch["effect"] for ch in changes]
    n = len(changes)
    leads = [e["lead_time_s"] for e in es if e["lead_time_s"] is not None]
    open_s, multi_s = sum(e["parallel"]["open_s"] for e in es), sum(e["parallel"]["multi_s"] for e in es)
    all_a = [i for e in es for i in e["escapes"]["ids_a"]]
    all_b = [c for e in es for c in e["reversals"]["candidates"]]
    return {
        "changes": n,
        "review": {"changes_with_review": sum(1 for e in es if e["review"]["rounds"]),
                   "changes_with_fail": sum(1 for e in es if e["review"]["fail_rounds"]),
                   "changes_with_stopped": sum(1 for e in es if e["review"]["stopped_rounds"]),
                   "stopped_rounds": sum(e["review"]["stopped_rounds"] for e in es),
                   "stopped_findings": sum(e["review"]["stopped_findings"] for e in es),
                   "fail_rounds": sum(e["review"]["fail_rounds"] for e in es),
                   "fail_findings": sum(e["review"]["fail_findings"] for e in es),
                   "derived_blk_rounds": sum(e["review"]["derived_blk_rounds"] for e in es)},
        "escapes": {"candidates_a": len(all_a), "changes_with_candidate_a": sum(1 for e in es if e["escapes"]["candidates_a"]),
                    "fix_commits": sum(e["escapes"]["fix_commits"] for e in es),
                    "changes_with_fix_commits": sum(1 for e in es if e["escapes"]["fix_commits"]),
                    "judged_a": judged_counts(all_a, judgments) if judgments is not None else None},
        "reversals": {"candidates_b": len(all_b),
                      "by_trigger": {t: sum(1 for c in all_b if c["trigger"] == t) for t in TRIGGERS},
                      "by_predecessor": {t: sum(1 for c in all_b if c["predecessor"] == t) for t in TRIGGERS},
                      "judged": ({t: judged_counts([c["id"] for c in all_b if c["trigger"] == t], judgments) for t in TRIGGERS}
                                 if judgments is not None else None)},
        "lead_time": {"median_s": statistics.median(leads) if leads else None, "max_s": max(leads) if leads else None,
                      "changes_with_lead_time": len(leads), "changes_without_lead_time": n - len(leads)},
        "left_out_errands": {"errands": len(left_out), "changes": sum(1 for e in es if e["left_out_errands"]),
                             "unowned": sum(1 for e in left_out if id(e) not in owned)},
        "parallelism": {"open_s": open_s, "multi_s": multi_s, "share": multi_s / open_s if open_s else None},
        "operator": {"messages": sum(e["operator_messages"] for e in es), "decisions": (sum(e["decisions"] for e in es if e["decisions"] is not None) if any(e["decisions"] is not None for e in es) else None),
                     "decisions_eligible_changes": sum(e["decisions"] is not None for e in es),
                     "messages_per_change": sum(e["operator_messages"] for e in es) / n if n else None,
                     "decisions_per_change": (sum(e["decisions"] for e in es if e["decisions"] is not None) / sum(e["decisions"] is not None for e in es) if any(e["decisions"] is not None for e in es) else None)},
        "cost": {"weighted": sum(e["cost"] for e in es), "per_change": sum(e["cost"] for e in es) / n if n else None},
        "judgments": None,
    }


# ---------------------------------------------------------------------------
# output
# ---------------------------------------------------------------------------

def write_candidates(out, changes, cand_a, cand_b, horizon_h):
    with tr.open_output(os.path.join(out, "candidates_a.txt")) as fh:
        fh.write(f"(a) operator message with a problem word, within {horizon_h:g} h after a review PASS or a release build. Candidates only.\n"
                 "Each line starts with the candidate's id; judge it in a --judgments file as '<id> real|not|unsure [note]'.\n\n")
        for ch in changes:
            fh.write(f"## change {ch['id']} {ch['commits'][-1]['hash']} ({len(cand_a[ch['id']])})\n")
            for t, kind, t0, ex, cid in cand_a[ch["id"]]:
                fh.write(f"  {cid}  {t:%m-%d %H:%M:%SZ}  after {kind} at {t0:%H:%M:%SZ}: {ex}\n")
    with tr.open_output(os.path.join(out, "candidates_b.txt")) as fh:
        fh.write("(b) Director text containing a retraction/correction phrase. Candidates only (high false-positive rate).\n"
                 "Each line starts with the candidate's id and, in brackets, what started the Director's turn (operator / worker / self).\n\n")
        for ch in changes:
            fh.write(f"## change {ch['id']} {ch['commits'][-1]['hash']} ({len(cand_b[ch['id']])})\n")
            for t, to, ex, cid, trig, _pred in cand_b[ch["id"]]:
                fh.write(f"  {cid}  {t:%m-%d %H:%M:%SZ}  [{trig}]  {to}: {ex}\n")


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


def fmt_secs(x):
    return "-" if x is None else fmt_dur(timedelta(seconds=x))


def fmt_share(x):
    return "-" if x is None else f"{x:.0%}"


def fmt_judged(c):
    return f"{c['real']} real / {c['not']} not / {c['unsure']} unsure / {c['unjudged']} unjudged"


def fmt_triggers(by):
    return " ".join(f"{t} {by[t]}" for t in TRIGGERS)


DEFINITIONS = [
    "DEFINITIONS (effect metrics; every count is a count of what a heuristic matched, not a judged fact)",
    "  caught by review: review rounds whose report says FAIL, and the findings (BLOCKER-1, H2, F3 ... labels) listed in those rounds. A FAIL is counted, not judged real. Stopped includes explicit FAIL and derived BLK*. Its finding count recognises severity headings with detail, F/N/H/W/B labels except labelled clear checks, and severity-qualified finding sentences with file:line; see README for negation and limits. Follow-ups before the next instruction count; explicit labels deduplicate, numbered headings are distinct per message. It can miss unlabelled or misread ambiguous findings.",
    "    BLK* rounds (no explicit verdict, the opening names a blocker) are counted apart and are not FAIL.",
    "  escaped review: (a) candidates are operator messages with a problem word within --horizon-h after a review PASS or a release build; the second signal is later 'fix' commits",
    "    touching the change's files within --fix-days. Without a --judgments file both are candidate counts. With one, a candidate is real / not / unsure / unjudged by the line written for its id.",
    "    A candidate's id comes from the transcript record it was found in: that record's uuid, plus the text block index or the send_to_peer tool_use id inside it, so no other record, matching or not,",
    "    can rename it. This relies on Claude Code appending transcript records and never rewriting them, so a record's uuid, its block order and its tool_use ids do not change between runs;",
    "    were a record ever rewritten, an id keyed by block index could move. Two fallbacks are not stable: a record without a uuid is keyed by its position among same-kind records at its timestamp",
    "    (it moves when a record at that timestamp is added or removed), and a send_to_peer block without a tool_use id by the position of the block in its record.",
    "    The id keeps 32 bits of a hash, so two candidates can share one by chance: a run counts such collisions in the summary and warns on stderr, and a judgment naming a colliding id is ambiguous and applied to neither candidate.",
    "    The problem-word and retraction-phrase lists were tuned once, on one window, against the first-pass judgments of one judge, with no holdout: a first pass, not a validation.",
    "  Director reversal: a (b) candidate, a Director message (text to the operator, or a send_to_peer) containing a retraction phrase. Trigger = what started the Director's turn in which it was written:",
    "    operator = the last thing to start a turn was a message the operator typed; worker = it was a relay nudge, or a check_messages answer with at least one worker-N or goinkyo message in it;",
    "    self = neither (tool results, system text, an answer with no Worker message and the Director's own text never start a turn). 'by turn' CANNOT SEE self-correction: a turn an operator message",
    "    or a Worker report started counts as that trigger even when the Director found the error itself; a human records self in the judgments note. The secondary column 'by predecessor' names the",
    "    most recent input of any kind before the message (a tool result or system text is self); it is mostly self, since a Director writes most of its text right after a tool call.",
    "  operator-typed message: a user record in the Director's transcript that is not a tool result, not meta/compaction-summary/transcript-only, has origin human or none, and whose text does not",
    "    start with '[banto' (a relay nudge or a line banto typed into a pane) or '<' (a tag the harness wrote). An operator message that itself starts with '<' is missed.",
    "  lead time: from the first errand that reported (not the first errand), opened in the change's window, to the change's last commit; empty when the change had no errand, or none that reported.",
    "  parallelism: among the time at least one errand was open inside the change's window (a Worker's delivery opens one, its first send_to_peer closes it), the share with two or more open at once.",
    "  left out: an errand that never reported back has no real end (it stays open to the Worker's last record), so lead time and parallelism leave it out. Every other use of errands (kinds, reviews, cost, rework) still includes them.",
    "    The per-change count assigns an errand by the change it started in, once; an errand that starts at or before --since is owned by no change, so no per-change count has it.",
    "    The summary counts every left-out errand whose time was excluded from parallelism or that a change owns, owned by a change or not; 'unowned' says how many of them no change owns.",
    "  operator effort: operator-typed messages to the Director, and request_decision calls. The tool arrived at commit 1b44854 (2026-11-15 10:43:38 UTC); changes starting before it, including straddlers, are N/A. The decision summary uses only fully eligible changes.",
    "  cost: the weighted cost of the change as above, the same number, no new cost logic. Effort and cost are not added into a score.",
    "  A change owns (previous change's last commit, its own last commit]. Time or messages after the last commit belong to no change.",
]


def render_report(args, w, unmatched, director_id, brigade, since, until, commits, changes, errands, label_of, missing, out_dir, summary=None):
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
    P(tr.unmatched_line(unmatched))
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
        P("  what ran (role model/effort: calls, weighted): "
          + (", ".join(f"{role} {model}/{effort}: {a[0]}c {kfmt(a[1])}" for (role, model, effort), a in sorted(ch["ran"].items(), key=lambda kv: -kv[1][1])) or "-"))
        lf = ch["later_fixes"]
        P(f"  later fix commits (<= {args.fix_days:g} d, 'fix' subject, same files): {len(lf)}"
          + "".join(f"\n     {h} {sj[:70]} ({n} shared files)" for h, sj, n, _ in lf))
        P(f"  candidates: (a) operator problem after PASS/release {ch['cand_a']}; (b) Director retraction phrases {ch['cand_b_op']} to operator + {ch['cand_b_wk']} to Workers")
        P(f"  attribution: errands straddling the commit {len(ch['straddle_out'])}; errands in flight at window start {len(ch['carried_in'])}; "
          f"errands naming a HEAD hash equal to the previous change's last commit {ch['base_check'][0]}/{ch['base_check'][1]}")
        ef = ch.get("effect")
        if ef:
            rv, es, rt = ef["review"], ef["escapes"], ef["reversals"]
            P(f"  effect: caught by review: {rv['fail_rounds']} FAIL round(s) of {rv['rounds']}, {rv['fail_findings']} finding(s) in them")
            P(f"          stopped (FAIL or derived BLK*): {rv['stopped_rounds']} round(s), {rv['stopped_findings']} findings (heading heuristic)")
            P(f"          escaped (candidates): (a) {es['candidates_a']}"
              + (f" [{fmt_judged(es['judged_a'])}]" if es["judged_a"] else "") + f"; later fix commits {es['fix_commits']}")
            P(f"          Director reversals by trigger (turn): {fmt_triggers(rt['by_trigger'])}; by predecessor: {fmt_triggers(rt['by_predecessor'])}"
              + ("".join(f"; {t}: {fmt_judged(c)}" for t, c in rt["judged"].items()) if rt["judged"] else ""))
            lt = f"lead time {fmt_secs(ef['lead_time_s'])}" if ef["lead_time_s"] is not None else f"lead time - ({ef['lead_time_note']})"
            pl = ef["parallel"]
            P(f"          {lt}; parallelism {fmt_share(pl['share'])} of {fmt_secs(pl['open_s'])} with an errand open"
              f"; errands left out (never reported) {ef['left_out_errands']}"
              f"; operator messages {ef['operator_messages']}, request_decision {ef['decisions'] if ef['decisions'] is not None else 'N/A'}; weighted cost {kfmt(ef['cost'])}")
    P("")
    P(f"classification source over all errands: {dict(Counter(e['how'] for e in errands))}; kinds: {dict(Counter(e['kind'] for e in errands))}")
    vs = Counter(r["verdict"] or "none" for ch in changes for r in ch["reviews"])
    P(f"review reports over all changes: {dict(vs)}")
    P(f"candidates files: {os.path.join(out_dir, 'candidates_a.txt')} ; {os.path.join(out_dir, 'candidates_b.txt')}")
    if summary is not None:
        P("")
        lines.extend(DEFINITIONS)
        P("")
        lines.extend(window_summary_lines(summary))
    return lines


def window_summary_lines(sm):
    lines = []
    P = lines.append
    rv, es, rs, lt, pl, op, jd = sm["review"], sm["escapes"], sm["reversals"], sm["lead_time"], sm["parallelism"], sm["operator"], sm["judgments"]
    P(f"WINDOW SUMMARY over {sm['changes']} change(s)")
    P(f"  1 caught by review: {rv['changes_with_fail']} of {rv['changes_with_review']} change(s) with a review had at least one FAIL round; "
      f"{rv['fail_rounds']} FAIL round(s), {rv['fail_findings']} finding(s) in them; {rv['derived_blk_rounds']} derived BLK* round(s) not counted as FAIL")
    P(f"    stopped (FAIL or derived BLK*): {rv['changes_with_stopped']} change(s), {rv['stopped_rounds']} round(s), {rv['stopped_findings']} findings (heading heuristic)")
    if es["judged_a"] is not None:
        P(f"  2 escaped review: (a) candidates {es['candidates_a']} in {es['changes_with_candidate_a']} change(s), judged: {fmt_judged(es['judged_a'])}")
    else:
        P(f"  2 escaped review: (a) CANDIDATES {es['candidates_a']} in {es['changes_with_candidate_a']} change(s), none judged (no --judgments file)")
    P(f"    second signal, later fix commits: {es['fix_commits']} in {es['changes_with_fix_commits']} change(s) (shown beside the candidates; they are not the same events)")
    P(f"  3 Director reversals (candidates) {rs['candidates_b']}: by trigger (what started the turn; cannot see self-correction) {fmt_triggers(rs['by_trigger'])}; by predecessor {fmt_triggers(rs['by_predecessor'])}")
    if rs["judged"]:
        for t in TRIGGERS:
            P(f"    {t}: {fmt_judged(rs['judged'][t])}")
    else:
        P("    none judged (no --judgments file)")
    P(f"  4 lead time: median {fmt_secs(lt['median_s'])}, max {fmt_secs(lt['max_s'])} over {lt['changes_with_lead_time']} change(s); "
      f"{lt['changes_without_lead_time']} change(s) have none (no errand, or none that reported)")
    P(f"  5 parallelism: {fmt_share(pl['share'])} of the {fmt_secs(pl['open_s'])} with an errand open had two or more open at once ({fmt_secs(pl['multi_s'])})")
    lo = sm["left_out_errands"]
    P(f"    left out of 4 and 5 because they never reported: {lo['errands']} errand(s), {lo['errands'] - lo['unowned']} in {lo['changes']} change(s) by start and {lo['unowned']} owned by no change")
    P(f"  6 operator effort: {op['messages']} operator-typed message(s) ({op['messages_per_change'] if op['messages_per_change'] is None else round(op['messages_per_change'], 2)} per change), "
      f"{op['decisions'] if op['decisions'] is not None else 'N/A'} request_decision call(s) ({round(op['decisions_per_change'], 2) if op['decisions_per_change'] is not None else 'N/A'} per eligible change, {op['decisions_eligible_changes']} eligible)")
    P(f"  7 cost: weighted {kfmt(sm['cost']['weighted'])} in total, {kfmt(sm['cost']['per_change'] or 0)} per change")
    if sm.get("id_collisions") is not None:
        P(f"  candidate ids: {sm['id_collisions']} id collision(s) (two distinct candidates with one id; a judgment for such an id is refused as ambiguous)")
    if jd:
        P(f"  judgments: {jd['entries']} judged id(s) read from the --judgments file; {len(jd['warnings'])} warning(s)")
        for wmsg in jd["warnings"]:
            P(f"    warning: {wmsg}")
    else:
        P("  judgments: no --judgments file given, so every escape and reversal figure above is a candidate count")
    return lines


def write_changes_json(out, changes, weights, unmatched, summary=None):
    def plain(ch):
        d = {
            "id": ch["id"], "commits": [(c["hash"], c["subject"], c["ts"].isoformat()) for c in ch["commits"]],
            "start": ch["start"], "end": ch["end"],
            "errands": [(e["worker"], e["kind"], e["how"], e["start"].isoformat(), e["end"].isoformat(), len(e["reports"])) for e in ch["errands"]],
            "reviews": ch["reviews"], "rework": ch["rework"], "usage": dict(ch["usage"]),
            "ran": [{"role": role, "model": model, "effort": effort, "calls": a[0], "weighted": round(a[1], 1)}
                    for (role, model, effort), a in sorted(ch["ran"].items(), key=lambda kv: -kv[1][1])],
            "later_fixes": [(h, sj, n) for h, sj, n, _ in ch["later_fixes"]],
        }
        if "effect" in ch:
            d["effect"] = ch["effect"]
        return {k: (v.isoformat() if isinstance(v, datetime) else v) for k, v in d.items()}
    doc = {"assumption": tr.ASSUMPTION, "weights": weights.as_dict(),
           "unmatched_models": unmatched, "changes": [plain(ch) for ch in changes]}
    if summary is not None:
        doc["summary"] = summary
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
    ap.add_argument("--judgments", default=None, metavar="FILE",
                    help="hand judgments of candidates: a text file with one '<id> <real|not|unsure> [note]' per line "
                         "(ids are the ones printed in candidates_a.txt and candidates_b.txt; blank lines and lines "
                         "starting with # are ignored; an unknown id or a malformed line is a warning). With it the "
                         "escape and reversal counts are also given as judged counts")
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
        judgments, judgment_warnings = None, []
        if args.judgments:
            try:
                with open(args.judgments, encoding="utf-8", errors="replace") as fh:
                    judgments, judgment_warnings = parse_judgments(fh.read())
            except OSError as err:
                raise ValueError(f"--judgments {args.judgments}: {err}")
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
    unmatched = tr.unmatched_in(aggregate(changes, an, commits, sessions, label_of), w)

    releases = [(e[1], "release build (Director)") for e in director_events if e[0] == "release"]
    for s in sessions:
        lab = label_of.get(s.id)
        if lab and lab != "director" and not lab.startswith("sub:"):
            releases += [(e[1], f"release build ({lab})") for e in window_events(s, lo_ev, hi_ev) if e[0] == "release"]
    cand_a, cand_b = find_candidates(changes, since, director_events, releases, args.horizon_h, director.id)
    collisions = id_collisions(cand_a, cand_b)
    for i in collisions:
        print(f"warning: candidate id {i} names more than one candidate (an id collision); a judgment for it would be ambiguous", file=sys.stderr)
    usable = judgments
    if judgments is not None:
        usable, ambiguous = refuse_ambiguous(judgments, collisions)
        judgment_warnings += ambiguous
    summary = compute_effects(changes, an.errands, director_events, since, cand_a, cand_b, usable)
    summary["id_collisions"] = len(collisions)
    if judgments is not None:
        known = {x[4] for rows in cand_a.values() for x in rows} | {x[3] for rows in cand_b.values() for x in rows}
        judgment_warnings += [f"judgments: {i} matches no candidate of this run, ignored" for i in sorted(set(judgments) - known)]
        summary["judgments"] = {"entries": len(judgments), "warnings": judgment_warnings}
        for msg in judgment_warnings:
            print(f"warning: {msg}", file=sys.stderr)

    try:
        write_candidates(out, changes, cand_a, cand_b, args.horizon_h)
        write_errands_dump(out, an.errands, changes, since)
        lines = render_report(args, w, unmatched, director.id, brigade, since, until, commits, changes, an.errands, label_of, missing, out, summary)
        with tr.open_output(os.path.join(out, "report.txt")) as fh:
            fh.write("\n".join(lines))
        write_changes_json(out, changes, w, unmatched, summary)
    except tr.OutDirError as err:
        print(err, file=sys.stderr)
        return 2
    print("\n".join(lines))
    print(f"outputs written to {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
