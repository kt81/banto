#!/usr/bin/env python3
"""A "what ran" ledger: which model and which reasoning effort each banto role actually ran, from the
local Claude Code transcripts alone, and what the same tokens would have weighed on another model.
Counts only; runs no model.

Reads the transcripts and banto's config.toml READ-ONLY (default root: $CLAUDE_CONFIG_DIR/projects,
else ~/.claude/projects). Writes ledger_roles.csv, ledger_sessions.csv and ledger_errands.csv to --out
(default: a new directory under the system temp directory; refused inside a git working tree unless
--force-out) and prints a report. The outputs hold real session metadata: keep them out of any
repository.

Needs Python 3.9+, standard library only.
"""
import argparse
import csv
import os
import re
import sys
from collections import defaultdict
from datetime import datetime, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import transcripts as tr  # noqa: E402

ROLES = ("director", "worker", "goinkyo", "subagent", "solo")
CSV_NAMES = ["ledger_roles.csv", "ledger_sessions.csv", "ledger_errands.csv"]
LIMITS = (("top", "--top", 0, 1_000_000),)
_MIN_TS = datetime.min.replace(tzinfo=timezone.utc)

# What banto launches a new member with when config.toml says nothing, copied from
# `BrigadeConfig::default` in crates/banto-core/src/config.rs. A Director has no entry: banto passes
# it no model or effort flag, so it starts on the operator's own Claude defaults and there is
# nothing configured to compare it with. The values are applied when the session is first created
# and never on a resume (the same file's docs), so a difference below is a fact about what ran, not
# proof that banto launched it wrong. A Goinkyo created before 2026-11-23 was launched on fable/max,
# the built-in until then, so with no config.toml those sessions read as a difference.
BUILT_IN = {"worker": ("sonnet", "high"), "goinkyo": ("opus", "xhigh")}
CONFIG_KEYS = {"worker": ("worker_model", "worker_effort"), "goinkyo": ("goinkyo_model", "goinkyo_effort")}


def build_parser():
    ap = argparse.ArgumentParser(
        prog="what_ran.py",
        description="Which model and effort each banto role actually ran, per session, errand and role, "
                    "against what banto's config says it should; and what the same tokens would weigh on another model.",
        epilog="Prints the report to stdout and writes ledger_roles.csv, ledger_sessions.csv and ledger_errands.csv "
               "to --out (default: a new directory under the system temp directory, its path printed).\n\n" + tr.ASSUMPTION,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    tr.add_root_args(ap)
    tr.add_out_args(ap)
    ap.add_argument("--since", help="ISO-8601 UTC; calls before it are left out (default: all history)")
    ap.add_argument("--until", help="ISO-8601 UTC; calls at or after it are left out (default: all history)")
    tr.add_weight_args(ap)
    ap.add_argument("--config", default=None,
                    help="banto's config.toml (default: $BANTO_CONFIG, else $XDG_CONFIG_HOME/banto/config.toml, else "
                         "~/.config/banto/config.toml, else the platform's config directory; none found = the built-in defaults)")
    ap.add_argument("--expect", action="append", default=[], metavar="ROLE=MODEL[/EFFORT]",
                    help="a model (and effort) to expect of a role, replacing config.toml and the built-ins for it, "
                         "e.g. director=opus/xhigh; MODEL matches as a case-insensitive substring of the model name; "
                         "repeatable; ROLE is one of " + ", ".join(ROLES))
    ap.add_argument("--reprice", action="append", default=[], metavar="ROLE=MODEL",
                    help="re-price every call of ROLE as if it had run on MODEL, a key of the model multipliers "
                         "(opus, sonnet, fable, haiku, or one added with --model-weight); the tokens are kept; repeatable")
    ap.add_argument("--top", type=int, default=10, help="errands in the top table (0 to 1000000; default 10)")
    return ap


# ---------------------------------------------------------------------------
# what banto was configured to launch
# ---------------------------------------------------------------------------

def config_candidates(env=None, home=None, platform=None):
    """The discovered places for config.toml, in banto's order: $XDG_CONFIG_HOME/banto (when set),
    ~/.config/banto, then the platform's own config directory: %APPDATA% on Windows, Application
    Support on macOS, nothing further on Linux, where it is ~/.config again. An explicit path
    (--config, then $BANTO_CONFIG) is not a candidate: it is used as it is (`load_expected`).
    Mirrors `resolve_config_path` and `default_config_path` in crates/banto-io/src/config.rs."""
    env = os.environ if env is None else env
    home = home or os.path.expanduser("~")
    platform = sys.platform if platform is None else platform
    out = []
    if env.get("XDG_CONFIG_HOME"):
        out.append(os.path.join(env["XDG_CONFIG_HOME"], "banto", "config.toml"))
    out.append(os.path.join(home, ".config", "banto", "config.toml"))
    if platform == "win32" and env.get("APPDATA"):
        out.append(os.path.join(env["APPDATA"], "banto", "config.toml"))
    if platform == "darwin":
        out.append(os.path.join(home, "Library", "Application Support", "banto", "config.toml"))
    return out


_KEY_RX = re.compile(r"^\s*([A-Za-z0-9_.-]+)\s*=\s*(.*?)\s*$")


def _toml_value(raw):
    """The text of a simple TOML scalar: a quoted string without its quotes, or a bare word up to a
    comment. A string with escapes or a multi-line value is outside what is read here."""
    raw = raw.strip()
    if raw[:1] in ("'", '"'):
        end = raw.find(raw[0], 1)
        return raw[1:end] if end > 0 else raw[1:]
    return raw.split("#", 1)[0].strip()


def parse_brigade_table(text):
    """{key: text} of the `[brigade]` table of a config.toml. Only top-level scalars of that one
    table; python 3.9 has no TOML reader and this reads four keys."""
    found, inside = {}, False
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("["):
            # TOML allows blanks inside the brackets: "[ brigade ]" is the same table.
            inside = re.sub(r"\s+", "", s.split("#", 1)[0]) == "[brigade]"
            continue
        if not inside or not s or s.startswith("#"):
            continue
        m = _KEY_RX.match(line)
        if m:
            found[m.group(1)] = _toml_value(m.group(2))
    return found


def load_expected(config_path=None, env=None, home=None, platform=None):
    """(expected, note): {role: (model or None, effort or None)} for the roles that have a
    configured default, and one line saying where it came from. An empty string in config.toml is
    banto's escape hatch (no flag is passed): it reads as nothing expected.

    An explicit path (--config here, else $BANTO_CONFIG, even when empty) is used as it is, as banto
    uses it: a missing one is not replaced by a file found elsewhere. banto's strict startup refuses
    to run then; this reads the built-in defaults and says so."""
    env = os.environ if env is None else env
    if config_path is not None:
        explicit, label = config_path, "--config"
    elif env.get("BANTO_CONFIG") is not None:
        explicit, label = env["BANTO_CONFIG"], "BANTO_CONFIG"
    else:
        explicit, label = None, None
    path, note = None, ""
    if explicit is not None:
        if os.path.isfile(explicit):
            path = explicit
        else:
            note = f"{label} names a missing file ({explicit}); banto would not start; built-in defaults used"
    else:
        path = next((c for c in config_candidates(env, home, platform) if os.path.isfile(c)), None)
        if path is None:
            note = "no config.toml found; built-in defaults used"
    table = {}
    if path:
        try:
            with open(path, "r", encoding="utf-8", errors="replace") as fh:
                table = parse_brigade_table(fh.read())
            stamp = datetime.fromtimestamp(os.path.getmtime(path), tz=timezone.utc)
            note = (f"config {path} (modified {tr.iso(stamp)}); its value is what it says NOW, not what it said "
                    "when each session was created")
        except OSError as err:
            note = f"config {path} unreadable ({err.strerror or err}); built-in defaults used"
            path = None
    expected = {}
    for role, (mkey, ekey) in CONFIG_KEYS.items():
        model, effort = BUILT_IN[role]
        if path and mkey in table:
            model = table[mkey]
        if path and ekey in table:
            effort = table[ekey]
        expected[role] = (model.strip() or None, effort.strip() or None)
    return expected, note


def apply_expect_flags(expected, items):
    """--expect ROLE=MODEL[/EFFORT] over `expected`; ValueError for a malformed item."""
    out = dict(expected)
    for item in items:
        role, sep, spec = item.partition("=")
        role = role.strip().lower()
        if not sep or role not in ROLES or not spec.strip():
            raise ValueError(f"--expect wants ROLE=MODEL[/EFFORT] with ROLE one of {', '.join(ROLES)}, got {item!r}")
        model, _, effort = spec.partition("/")
        out[role] = (model.strip() or None, effort.strip() or None)
    return out


def differences(model, effort, expect):
    """What differs between what ran and what was expected: [] when nothing does or nothing is
    expected. A model matches when the expected word occurs in its name; an effort when the words are
    equal (case-insensitively). A call with no recorded effort cannot differ in effort: it is said
    to be unrecorded instead."""
    if not expect:
        return []
    want_model, want_effort = expect
    diffs = []
    if want_model and want_model.lower() not in (model or "").lower():
        diffs.append(f"model {want_model} expected, {model} ran")
    if want_effort and effort != "?":
        if want_effort.lower() != effort.lower():
            diffs.append(f"effort {want_effort} expected, {effort} ran")
    return diffs


def describe_expected(expect):
    if not expect:
        return "none configured"
    model, effort = expect
    return f"{model or 'any'}/{effort or 'any'}"


# ---------------------------------------------------------------------------
# tallies
# ---------------------------------------------------------------------------

class Tally:
    """Calls, the four token counts and the weighted cost of one group of calls."""
    __slots__ = ("calls", "inp", "out", "cc", "cr", "weighted", "first", "last")

    def __init__(self):
        self.calls = self.inp = self.out = self.cc = self.cr = 0
        self.weighted = 0.0
        self.first = self.last = None

    def add(self, c):
        self.calls += 1
        self.inp += c.inp
        self.out += c.out
        self.cc += c.cc
        self.cr += c.cr
        self.weighted += c.weighted
        if c.ts:
            self.first = c.ts if self.first is None or c.ts < self.first else self.first
            self.last = c.ts if self.last is None or c.ts > self.last else self.last


def ran_key(c):
    return (c.model, c.effort)


def day_of(c):
    return c.ts.strftime("%Y-%m-%d") if c.ts else "?"


def member_label(s):
    """Who a session is: its briefed name and brigades for a Worker or a Goinkyo, else its role."""
    if s.role in ("worker", "goinkyo") and s.members:
        names = sorted({m for m, _b in s.members})
        brigades = sorted({b for _m, b in s.members}, key=lambda b: (len(b), b))
        return "/".join(names) + " b" + ",".join(brigades)
    return s.role


def role_tallies(sessions):
    """{(role, model, effort): Tally} over the calls that ran."""
    out = defaultdict(Tally)
    for s in sessions:
        for c in tr.live_calls(s):
            if tr.ran_call(c):
                out[(s.role, c.model, c.effort)].add(c)
    return out


def skipped_calls(sessions):
    return sum(1 for s in sessions for c in tr.live_calls(s) if not tr.ran_call(c))


def session_tallies(sessions):
    """{(session, day, model, effort): Tally}"""
    out = defaultdict(Tally)
    for s in sessions:
        for c in tr.live_calls(s):
            if tr.ran_call(c):
                out[(s.id, day_of(c), c.model, c.effort)].add(c)
    return out


def errand_tallies(sessions, errands):
    """[(errand id, errand, session, {(model, effort): Tally})] for each errand with a call that ran."""
    by_id = {s.id: s for s in sessions}
    rows = []
    for e in errands:
        sess = by_id[e.session]
        groups = defaultdict(Tally)
        for mid in e.calls:
            c = sess.calls[mid]
            if not c.dropped and c.in_window and tr.ran_call(c):
                groups[ran_key(c)].add(c)
        if groups:
            rows.append((len(rows) + 1, e, sess, groups))
    return rows


def reprice_plan(weights, items):
    """{role: model} from the --reprice items; ValueError for a malformed or unknown one. A key must be
    one the weights know: a model matching none would be priced 1.0 and the table would look like a
    result."""
    plan = {}
    for item in items:
        role, sep, model = item.partition("=")
        role, model = role.strip().lower(), model.strip()
        if not sep or role not in ROLES or not model:
            raise ValueError(f"--reprice wants ROLE=MODEL with ROLE one of {', '.join(ROLES)}, got {item!r}")
        if weights.key_for(model) is None:
            raise ValueError(f"--reprice {item}: no model multiplier matches {model!r} "
                             f"(known: {', '.join(sorted(weights.model_mult))}; add one with --model-weight KEY=MULT)")
        plan[role] = model
    return plan


def reprice_rows(sessions, weights, plan):
    """{role: [calls, actual weighted, re-priced weighted]}: the same tokens, a role in `plan` priced
    as the model it names, every other role as it was."""
    rows = {role: [0, 0.0, 0.0] for role in ROLES}
    for s in sessions:
        for c in tr.live_calls(s):
            if not tr.ran_call(c):
                continue
            r = rows[s.role]
            r[0] += 1
            r[1] += c.weighted
            r[2] += weights.cost(plan[s.role], c.inp, c.cc, c.cr, c.out) if s.role in plan else c.weighted
    return rows


# ---------------------------------------------------------------------------
# outputs
# ---------------------------------------------------------------------------

def write_csvs(out, sessions, errand_rows, rtal, stal, expected, weights):
    basis = weights.basis()
    by_id = {s.id: s for s in sessions}
    tcols = ["calls", "input", "output", "cache_creation", "cache_read", "weighted"]

    def tvals(t):
        return [t.calls, t.inp, t.out, t.cc, t.cr, round(t.weighted, 1)]

    with tr.open_output(os.path.join(out, "ledger_roles.csv"), newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["role", "model", "effort"] + tcols + ["weights_assumed"])
        for (role, model, effort), t in sorted(rtal.items(), key=lambda kv: (ROLES.index(kv[0][0]), -kv[1].weighted)):
            w.writerow([role, model, effort] + tvals(t) + [basis])
    with tr.open_output(os.path.join(out, "ledger_sessions.csv"), newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["session", "project", "role", "member", "day", "model", "effort"] + tcols
                   + ["expected", "differs", "weights_assumed"])
        for (sid, day, model, effort), t in sorted(stal.items(), key=lambda kv: (by_id[kv[0][0]].first_ts or _MIN_TS, kv[0])):
            s = by_id[sid]
            exp = expected.get(s.role)
            w.writerow([sid, s.project, s.role, member_label(s), day, model, effort] + tvals(t)
                       + [describe_expected(exp), "; ".join(differences(model, effort, exp)), basis])
    with tr.open_output(os.path.join(out, "ledger_errands.csv"), newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["errand_id", "session", "role", "kind", "start", "end", "model", "effort"] + tcols + ["weights_assumed"])
        for eid, e, _s, groups in errand_rows:
            for (model, effort), t in sorted(groups.items(), key=lambda kv: -kv[1].weighted):
                w.writerow([eid, e.session, e.role, e.kind, tr.iso(e.start), tr.iso(e.end), model, effort] + tvals(t) + [basis])


def kfmt(x):
    return f"{x:,.0f}"


def print_report(args, weights, since, until, sessions, dups, rtal, stal, errand_rows, expected, config_note, plan_rows, out):
    P = print
    live_total = sum(t.weighted for t in rtal.values()) or 1.0
    by_id = {s.id: s for s in sessions}
    P("WHAT RAN (counts from local transcripts; no model was run)")
    P(tr.ASSUMPTION)
    P(weights.describe())
    P(tr.unmatched_line(tr.unmatched_models(sessions, weights)))
    P(f"window: since {tr.iso(since) or '-'} until {tr.iso(until) or '-'}; project filter {args.project or '-'}")
    P(f"files {len(sessions)}; calls that ran {sum(t.calls for t in rtal.values())}; "
      f"left out as placeholders (synthetic or no tokens) {skipped_calls(sessions)}; "
      f"duplicate message ids dropped across files {dups}")
    P("role: director = used a Director-only banto tool; worker / goinkyo = banto tools only, told apart by the briefing in the "
      "file's first system-prompt snapshot (Claude Code 2.1.265+; an older file reads as a worker); subagent = a file under "
      "'subagents'; solo = no banto tool. Effort is the `effort` field of each assistant record ('?' = none recorded).")
    P("")

    P("1. ROLE x MODEL x EFFORT  (calls, input, output, cache-creation, cache-read, weighted, share of all, share of role)")
    role_total = defaultdict(float)
    for (role, _m, _e), t in rtal.items():
        role_total[role] += t.weighted
    for role in ROLES:
        rows = sorted(((k, t) for k, t in rtal.items() if k[0] == role), key=lambda kv: -kv[1].weighted)
        if not rows:
            continue
        P(f"  {role}: {sum(t.calls for _k, t in rows):,} calls, weighted {kfmt(role_total[role])} = {100 * role_total[role] / live_total:.1f}% of all")
        for (_r, model, effort), t in rows:
            P(f"    {model:24} {effort:7} {t.calls:>7,} {kfmt(t.inp):>11} {kfmt(t.out):>11} {kfmt(t.cc):>13} {kfmt(t.cr):>15} "
              f"{kfmt(t.weighted):>14} {100 * t.weighted / live_total:5.1f}% {100 * t.weighted / (role_total[role] or 1):5.1f}%")
    P("")

    P("2. PER SESSION  (session, role, member; then each model/effort it ran: calls, weighted, first..last day)")
    by_session = defaultdict(lambda: defaultdict(Tally))
    for (sid, _day, model, effort), t in stal.items():
        g = by_session[sid][(model, effort)]
        g.calls += t.calls
        g.weighted += t.weighted
        g.first = t.first if g.first is None or (t.first and t.first < g.first) else g.first
        g.last = t.last if g.last is None or (t.last and t.last > g.last) else g.last
    for sid in sorted(by_session, key=lambda i: (by_id[i].first_ts or _MIN_TS, i)):
        s = by_id[sid]
        P(f"  {sid[:8]} {s.role:8} {member_label(s)}")
        for (model, effort), g in sorted(by_session[sid].items(), key=lambda kv: -kv[1].weighted):
            span = f"{g.first:%m-%d}..{g.last:%m-%d}" if g.first and g.last else "-"
            P(f"      {model:24} {effort:7} {g.calls:>6,} calls {kfmt(g.weighted):>13} weighted  {span}")
    P("")

    P("3. PER ERRAND / TURN  (an errand is one instruction to a Worker or Goinkyo, a turn one operator message or relay nudge to a Director)")
    mixed = [r for r in errand_rows if len(r[3]) > 1]
    P(f"  {len(errand_rows)} with calls that ran; {len(errand_rows) - len(mixed)} on one model/effort, {len(mixed)} on more than one "
      "(the model or effort changed inside it). The per-errand rows are in ledger_errands.csv.")
    P(f"  top {args.top} by weighted cost  (weighted, session, role, kind, start, model/effort: calls ...)")
    ranked = sorted(errand_rows, key=lambda r: -sum(t.weighted for t in r[3].values()))[:args.top]
    for _eid, e, _s, groups in ranked:
        mix = ", ".join(f"{m}/{f}:{t.calls}" for (m, f), t in sorted(groups.items(), key=lambda kv: -kv[1].weighted))
        P(f"  {kfmt(sum(t.weighted for t in groups.values())):>12} {e.session[:8]} {e.role[:8]:8} {e.kind:6} {tr.iso(e.start)} {mix}")
    P("")

    P("4. CONFIGURED VERSUS OBSERVED  (per session and UTC day: first-last call time, what it ran, and whether that differs from what banto's config launches that role with)")
    P("  expected: " + "; ".join(f"{r} {describe_expected(expected.get(r))}" for r in ROLES))
    P("  " + config_note)
    P("  A model and an effort are given to a member when its session is created, never on a resume, and an operator can change "
      "either inside the session: a difference says what ran, not why.")
    flagged = 0
    total_rows = 0
    unrecorded_rows = unrecorded_calls = 0
    for (sid, day, model, effort), t in sorted(stal.items(), key=lambda kv: (by_id[kv[0][0]].first_ts or _MIN_TS, kv[0][0], kv[0][1], -kv[1].weighted)):
        s = by_id[sid]
        exp = expected.get(s.role)
        diffs = differences(model, effort, exp)
        total_rows += 1
        flagged += bool(diffs)
        verdict = "DIFFERS: " + "; ".join(diffs) if diffs else ("ok" if exp else "-")
        if effort == "?":
            unrecorded_rows += 1
            unrecorded_calls += t.calls
            if exp:
                verdict += " (effort unrecorded)"
        span = f"{t.first:%H:%M}-{t.last:%H:%M}Z" if t.first and t.last else "-"
        P(f"  {sid[:8]} {s.role:8} {member_label(s)[:24]:24} {day} {span:12} {model:22} {effort:7} {t.calls:>6,} calls {kfmt(t.weighted):>12}  {verdict}")
    P(f"  {total_rows} session-day-model-effort rows; {flagged} differ from what was expected ('-' = nothing configured for the role); "
      f"{unrecorded_calls:,} calls in {unrecorded_rows} rows ran with no recorded effort, compared on model only")
    P("")

    P("5. PRICE WHAT-IF  (the same tokens, re-priced as if each named role had run on another model; actual token counts are kept)")
    plan, rows = plan_rows
    if not plan:
        P("  none asked for: pass --reprice ROLE=MODEL, e.g. --reprice worker=opus --reprice goinkyo=opus")
    else:
        P("  " + ", ".join(f"{r} -> {m}" for r, m in plan.items()))
        total_actual = sum(r[1] for r in rows.values()) or 1.0
        total_new = sum(r[2] for r in rows.values()) or 1.0
        P(f"  {'role':9} {'calls':>8} {'actual':>14} {'share':>7} {'re-priced':>14} {'share':>7} {'change':>8}")
        for role in ROLES:
            calls, actual, new = rows[role]
            if not calls:
                continue
            P(f"  {role:9} {calls:>8,} {kfmt(actual):>14} {100 * actual / total_actual:6.1f}% {kfmt(new):>14} "
              f"{100 * new / total_new:6.1f}% {('x%.2f' % (new / actual)) if actual else '-':>8}")
        P(f"  {'all':9} {sum(r[0] for r in rows.values()):>8,} {kfmt(total_actual):>14} {'100.0%':>7} {kfmt(total_new):>14} {'100.0%':>7} x{total_new / total_actual:.2f}")
        P("  A model that ran a role would not have used the same tokens: this changes the price of what happened, not what would have happened.")
    P("")
    P(f"CSV files written to {out}: " + " ".join(CSV_NAMES))


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
        expected, config_note = load_expected(args.config)
        expected = apply_expect_flags(expected, args.expect)
        plan = reprice_plan(weights, args.reprice)
        out = tr.prepare_out_dir(args.out, "what_ran", args.force_out, names=CSV_NAMES)
    except (ValueError, tr.OutDirError) as err:
        print(err, file=sys.stderr)
        return 2
    root = args.root or tr.default_root()
    sessions = tr.load_sessions(root, args.project)
    errands, dups = tr.prepare(sessions, weights, since, until)
    plan_rows = (plan, reprice_rows(sessions, weights, plan))
    rtal = role_tallies(sessions)
    stal = session_tallies(sessions)
    errand_rows = errand_tallies(sessions, errands)
    try:
        write_csvs(out, sessions, errand_rows, rtal, stal, expected, weights)
    except tr.OutDirError as err:
        print(err, file=sys.stderr)
        return 2
    print_report(args, weights, since, until, sessions, dups, rtal, stal, errand_rows, expected, config_note, plan_rows, out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
