"""Shared reading of Claude Code transcripts for the measurement scripts.

Everything here is READ-ONLY on the transcripts and uses the standard library
only (Python 3.9+). The scripts in this directory import this module; it has no
command line of its own.

What a transcript (one JSONL file per session) was found to contain, checked on
real files:

* assistant records carry message.id, message.model and message.usage
  {input_tokens, output_tokens, cache_creation_input_tokens,
  cache_read_input_tokens}. One API call is written as SEVERAL lines, one per
  content block (thinking, text, each tool_use), all with the same message.id.
* user records carry a string (operator text, or a banto relay nudge) or a
  list of blocks (tool_result, text, image).
* system records with subtype "compact_boundary" carry compactMetadata
  {trigger, preTokens, postTokens, durationMs}.
* a session resumed or forked can repeat earlier calls in a later file with the
  same message.id, so cost is only meaningful after dedupe_calls().

Parsing is lenient on purpose: a line that is not JSON, or not an object, is
counted as skipped and never fatal; so is a record whose fields have the wrong
type (message="x", a token count that is a word); unknown record types and
fields are ignored.
"""
import contextlib
import os
import re
import sys
import tempfile
import time
import uuid
import glob
import json
import math
from collections import Counter
from datetime import datetime, timedelta, timezone

ASSUMPTION = (
    "The weights below are an assumption about how the subscription counts "
    "usage, not a known fact. Of the default per-model multipliers, Fable's 4 "
    "(relative to Sonnet's 0.68) was fitted to usage-screen readings; the "
    "Opus:Sonnet ratio (1 : 0.68) is an unofficial outside report's and is "
    "unchecked here; Haiku's is unknown and its 0.68 is a placeholder copied "
    "from Sonnet so that it is never silently 1.0."
)

# ---------------------------------------------------------------------------
# banto conventions. Each constant names what it matches; a transcript without
# banto brigade traffic simply never matches any of them.
# ---------------------------------------------------------------------------

# banto's MCP tools reach the agent as mcp__banto__<tool>.
BANTO_TOOL_PREFIX = "mcp__banto__"
# Tools only a Director is given; a session that calls one is a Director.
DIRECTOR_ONLY_TOOLS = {"consult_goinkyo", "dismiss_goinkyo", "name_member", "request_decision"}
# Text banto types into an agent's pane to make it call check_messages.
NUDGE_PREFIX = "[banto relay]"
# Events kept only in detail mode for quality_report: a request_decision call and a tool result that
# carried no delivery. They mark when something happened, not what an errand or a turn is, so the
# errand builder skips them and its boundaries stay what they were before they existed.
MARK_EVENTS = ("decision", "result")
# The first line of a check_messages answer that delivered mail.
DELIVERED_RE = re.compile(r"^\s*\d+ message\(s\) relayed by banto")
# The header banto puts above each delivered message: [from worker-2 "name" — to you]
FROM_RE = re.compile(r"^\[from [^\]]*\]\s*$")
FROM_HDR_RE = re.compile(r"^\[from ([^\]]+)\]\s*$", re.M)
FROM_PARTS_RE = re.compile(r'^(worker-\d+|goinkyo|director|operator)(?: "([^"]*)")?(?: — (to you|broadcast))?')
# The first message a Worker / Goinkyo receives names its place in a brigade.
WORKER_BRIEFING_RE = re.compile(r"You are (worker-\d+), a Worker in banto brigade (\d+)")
GOINKYO_BRIEFING_RE = re.compile(r"You are the Goinkyo of banto brigade (\d+)")

CTRL_RE = re.compile("[\x00-\x1f\x7f-\x9f\u200b-\u200f\u202a-\u202e\u2066-\u2069\ufeff]")
_TS_RE = re.compile(r"^(\d{4})-(\d\d)-(\d\d)(?:[T ](\d\d):(\d\d)(?::(\d\d)(?:\.(\d+))?)?)?\s*(Z|[+-]\d\d:?\d\d)?$")


def _parse_ts(text):
    """ISO-8601 -> aware UTC datetime, or None. Date-only and 'Z' are accepted;
    no fractional-second width is assumed (older Pythons reject some)."""
    if not text:
        return None
    m = _TS_RE.match(str(text).strip())
    if not m:
        return None
    y, mo, d, h, mi, s, frac, zone = m.groups()
    try:
        dt = datetime(int(y), int(mo), int(d), int(h or 0), int(mi or 0), int(s or 0),
                      int((frac or "0")[:6].ljust(6, "0")), tzinfo=timezone.utc)
    except ValueError:
        return None
    if zone and zone != "Z":
        sign = 1 if zone[0] == "+" else -1
        digits = zone[1:].replace(":", "")
        hours, minutes = int(digits[:2]), int(digits[2:])
        if hours > 23 or minutes > 59:      # no clock has such an offset; shifting by it would move the moment silently
            return None
        dt -= sign * timedelta(hours=hours, minutes=minutes)   # may leave 1..9999
    return dt


def parse_ts(text):
    """_parse_ts, but a timestamp the offset pushes outside year 1..9999 is None
    like any other unreadable one: every caller already handles None, none can
    handle an OverflowError."""
    try:
        return _parse_ts(text)
    except OverflowError:
        return None


# Year 1 and year 9999 are where every datetime sum or difference overflows; the scripts
# subtract hours and days from a window edge, so an edge that close to the ends of the
# calendar is a typo or a sentinel, never a window worth measuring.
_FLAG_TS_MIN = datetime(2, 1, 1, tzinfo=timezone.utc)
_FLAG_TS_MAX = datetime(9998, 12, 31, tzinfo=timezone.utc)


def parse_flag_ts(text):
    """parse_ts for --since/--until: None also for a moment outside 0002-01-01 ..
    9998-12-31, too near either end of the calendar for the scripts' arithmetic
    around it to stay in range."""
    dt = parse_ts(text)
    return dt if dt is not None and _FLAG_TS_MIN <= dt <= _FLAG_TS_MAX else None


def iso(dt):
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ") if dt else ""


def clean(text, limit=80):
    """First non-empty line, control characters stripped, cut to `limit`."""
    for line in (text or "").splitlines():
        line = CTRL_RE.sub("", line).strip()
        if line:
            return line[:limit]
    return ""


def block_text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(b["text"] for b in content
                         if isinstance(b, dict) and b.get("type") == "text" and isinstance(b.get("text"), str))
    return ""


def as_dict(value):
    """`value` if it is a JSON object, else an empty dict: a wrongly typed field
    reads as absent instead of raising."""
    return value if isinstance(value, dict) else {}


def type_rx(*names):
    """A cheap line filter for `"type": "<name>"` that does not care how the
    writer spaced its JSON. Transcripts are written compact (`"type":"x"`), but
    a re-serialised file is equally valid and must not read as empty."""
    return re.compile(r'"type"\s*:\s*"(?:%s)"' % "|".join(re.escape(n) for n in names))


def iter_records(path, line_rx=None):
    """The JSON objects of a transcript, in order. Lines that fail `line_rx`
    (a cheap pre-filter), are not JSON or are not objects are passed over."""
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            if line_rx is not None and not line_rx.search(line):
                continue
            try:
                d = json.loads(line)
            except ValueError:
                continue
            if isinstance(d, dict):
                yield d


def reconfigure_stdout():
    """Windows consoles default to a legacy code page; transcripts are UTF-8."""
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass


# ---------------------------------------------------------------------------
# where things are
# ---------------------------------------------------------------------------

def default_root():
    """Transcript root: $CLAUDE_CONFIG_DIR/projects if that variable is set,
    else ~/.claude/projects. CLAUDE_CONFIG_DIR is documented by Claude Code as the
    place its settings, session history and plugins live; that the transcripts sit
    in a `projects` folder beneath it is inferred from the default layout, not
    read from documentation. `--root` always overrides."""
    cfg = os.environ.get("CLAUDE_CONFIG_DIR")
    base = os.path.expanduser(cfg) if cfg else os.path.join(os.path.expanduser("~"), ".claude")
    return os.path.join(base, "projects")


def project_dir_name(path):
    """Claude Code names a project directory after the working directory with
    every character outside [A-Za-z0-9] replaced by '-' (observed on real
    installs: C:\\Users\\me\\proj -> C--Users-me-proj)."""
    return re.sub(r"[^A-Za-z0-9]", "-", os.path.abspath(path))


def inside_git_worktree(path):
    """True if `path` (which need not exist yet) lies inside a git working tree:
    an ancestor holds a .git directory or file."""
    cur = os.path.realpath(os.path.abspath(path))
    while True:
        if os.path.exists(os.path.join(cur, ".git")):
            return True
        parent = os.path.dirname(cur)
        if parent == cur:
            return False
        cur = parent


class OutDirError(ValueError):
    pass


def agent_homes():
    """The agent homes that must never be written: ~/.claude and ~/.codex, and
    wherever $CLAUDE_CONFIG_DIR / $CODEX_HOME point instead."""
    base = os.path.expanduser("~")
    homes = [os.path.join(base, ".claude"), os.path.join(base, ".codex")]
    for var in ("CLAUDE_CONFIG_DIR", "CODEX_HOME"):
        if os.environ.get(var):
            homes.append(os.path.expanduser(os.environ[var]))
    return homes


def _same_or_inside(path, parent):
    a = os.path.normcase(os.path.realpath(path))
    b = os.path.normcase(os.path.realpath(parent))
    try:
        return os.path.commonpath([a, b]) == b
    except ValueError:      # different drives
        return False


def prepare_out_dir(out, script, force=False, now=None, names=()):
    """Create and return the output directory.

    Default (out is None): <system temp>/banto-measure/<script>-<timestamp>.
    Refused, whatever `force` says, inside an agent home: those are read-only by
    banto's own rule, and a transcript root is the usual thing to mistype here.
    Refused inside a git working tree unless `force`: the outputs hold real
    session metadata and excerpts that must never be committed. `names` are the
    files the caller is about to write; a symlink among them is refused up front,
    before anything is written (open_output refuses again at write time)."""
    fresh = out is None
    if fresh:
        # the directory itself is made below, exclusively: a name to the second is shared by two runs in that second
        out = os.path.join(tempfile.gettempdir(), "banto-measure")
    for home in agent_homes():
        if _same_or_inside(out, home):
            raise OutDirError(
                f"refusing --out {out}: it is inside an agent home ({home}), which is read-only; "
                "--force-out does not apply to it")
    if inside_git_worktree(out) and not force:
        raise OutDirError(
            f"refusing --out {out}: it is inside a git working tree and the outputs contain real "
            "session data; pick a directory outside any repository, or pass --force-out")
    try:
        os.makedirs(out, exist_ok=True)
        if fresh:
            stamp = time.strftime("%Y%m%d-%H%M%S", time.localtime(now))
            out = tempfile.mkdtemp(prefix=f"{script}-{stamp}-", dir=out)       # also private to this user (0700 where that exists)
    except OSError as err:
        raise OutDirError(f"cannot create the output directory {out}: {err.strerror or err}") from err
    for name in names:
        if os.path.islink(os.path.join(out, name)):
            raise OutDirError(_symlink_message(os.path.join(out, name)))
    return out


def _symlink_message(path):
    return (f"refusing to write {path}: it is a symlink, and writing through it could overwrite a file "
            "elsewhere; remove it or pick another --out")


@contextlib.contextmanager
def open_output(path, newline=None):
    """Write an output file as UTF-8 text without ever writing into an existing one.

    The text goes to a new file created exclusively beside `path` and is moved onto
    `path` with os.replace when the block ends cleanly; on an error the temporary
    file is removed and a previous `path` is left as it was. replace swaps the
    directory entry, so whatever `path` was linked to (a symlink or a hard link into
    an agent home or a repository, which no check on the name can tell from a plain
    file) keeps its content. A symlink at `path` is still refused rather than
    silently replaced: an operator who put one there meant something else. Any
    OSError on the way (no permission, a directory in the way, a full disk) leaves
    the old file alone and surfaces as OutDirError, which every script turns into
    exit code 2."""
    if os.path.islink(path):
        raise OutDirError(_symlink_message(path))
    folder, name = os.path.split(path)
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
    for _ in range(100):
        tmp = os.path.join(folder, f".{name}.{os.getpid()}.{uuid.uuid4().hex[:8]}.tmp")
        try:
            fd = os.open(tmp, flags, 0o666)
            break
        except FileExistsError:
            continue
        except OSError as err:
            raise OutDirError(f"cannot write {path}: {err.strerror or err}") from err
    else:
        raise OutDirError(f"cannot create a temporary file beside {path}")
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline=newline) as fh:
            yield fh
        os.replace(tmp, path)
    except BaseException as err:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        if isinstance(err, OSError):
            # Windows will not replace a read-only file or a directory, a disk fills up: one message, one exit code
            raise OutDirError(f"cannot write {path}: {err.strerror or err}") from err
        raise


# ---------------------------------------------------------------------------
# weights
# ---------------------------------------------------------------------------

# How much one weighted token of each model family counts against the subscription, relative to
# Opus. Not from anything Anthropic publishes. Fable's 4, relative to Sonnet's 0.68, was fitted to
# the operator's usage-screen readings; the Opus:Sonnet ratio (1 : 0.68) is an unofficial outside
# report's and was not checked here; Haiku's value is unknown and its 0.68 is a placeholder copied
# from Sonnet so that it is never silently 1.0. ASSUMPTION says the same wherever the multipliers
# are printed. Override any of them with --model-weight.
DEFAULT_MODEL_MULT = {"opus": 1.0, "sonnet": 0.68, "fable": 4.0, "haiku": 0.68}


def num(x):
    """A weight as the shortest text that reads back as the same float ("4", "0.68"), so a printed
    weight is the one the cost used: :g would round 0.123456789 to six digits and a shared output
    would then name a number the report did not compute with."""
    text = repr(float(x))
    return text[:-2] if text.endswith(".0") else text


class Weights:
    """Weighted cost of a call = (input + cc*wcw + cr*wcr + out*wout) * model multiplier.
    The defaults are an assumption (see ASSUMPTION), not a measured price.

    A model's multiplier is that of the longest key (case-insensitive) that occurs anywhere in
    its name; a model that contains none weighs 1.0 and is counted by unmatched_models rather
    than passed over."""

    def __init__(self, wcw=1.25, wcr=0.025, wout=5.0, model_mult=None):
        self.wcw, self.wcr, self.wout = wcw, wcr, wout
        # Keys are folded to lower case so an override replaces a default instead of sitting
        # beside it, and so the matching below needs no second fold.
        self.model_mult = dict(DEFAULT_MODEL_MULT)
        self.model_mult.update({k.lower(): v for k, v in (model_mult or {}).items()})
        self._keys = {}

    def key_for(self, model):
        """The multiplier key `model` matches, or None. Longest key wins; among keys of equal
        length the alphabetically first, so the answer does not depend on flag order."""
        name = (model or "").lower()
        if name not in self._keys:
            hits = [k for k in self.model_mult if k in name]
            self._keys[name] = min(hits, key=lambda k: (-len(k), k)) if hits else None
        return self._keys[name]

    def mult(self, model):
        key = self.key_for(model)
        return self.model_mult[key] if key is not None else 1.0

    def cost(self, model, inp, cc, cr, out):
        return (inp * 1.0 + cc * self.wcw + cr * self.wcr + out * self.wout) * self.mult(model)

    def of(self, call):
        return self.cost(call.model, call.inp, call.cc, call.cr, call.out)

    def _multipliers(self):
        return ", ".join(f"{k} {num(v)}" for k, v in sorted(self.model_mult.items()))

    def describe(self):
        return (f"weights: input 1, cache-creation {num(self.wcw)}, cache-read {num(self.wcr)}, output {num(self.wout)}; "
                f"model multipliers: {self._multipliers()} (a key matches as a case-insensitive substring of the "
                f"model name, longest key first; a model matching none counts 1.0)")

    def basis(self):
        """One short line naming the weights, for a column in a file that may be
        shared without the report that came with it."""
        mult = ",".join(f"{k}={num(v)}" for k, v in sorted(self.model_mult.items()))
        return (f"input=1 cache_write={num(self.wcw)} cache_read={num(self.wcr)} output={num(self.wout)} "
                f"model_multipliers={mult} other_models=1")

    def as_dict(self):
        return {"input": 1, "cache_write": self.wcw, "cache_read": self.wcr, "output": self.wout,
                "model_multipliers": dict(sorted(self.model_mult.items())), "other_models": 1.0}


def add_weight_args(ap):
    ap.add_argument("--wcw", type=float, default=1.25, help="weight of cache-creation tokens (0 to 1000; default 1.25; an assumption)")
    ap.add_argument("--wcr", type=float, default=0.025, help="weight of cache-read tokens (0 to 1000; default 0.025; an assumption)")
    ap.add_argument("--wout", type=float, default=5.0, help="weight of output tokens (0 to 1000; default 5; an assumption)")
    ap.add_argument("--model-weight", action="append", default=[], metavar="KEY=MULT",
                    help="per-model multiplier, 0 to 1000, for every model whose name contains KEY (case-insensitive, "
                         "longest key wins); repeatable; replaces or adds to the defaults "
                         + ", ".join(f"{k}={num(v)}" for k, v in DEFAULT_MODEL_MULT.items())
                         + "; a model matching no key counts 1.0 and is listed in the report")


# A weight or multiplier is a small price ratio; the ceiling is far above any plausible
# one and far below where a sum of token counts times it stops being a float to read.
WEIGHT_MAX = 1000.0
WEIGHT_LIMITS = (("wcw", "--wcw", 0.0, WEIGHT_MAX), ("wcr", "--wcr", 0.0, WEIGHT_MAX), ("wout", "--wout", 0.0, WEIGHT_MAX))


def check_number(value, flag, lo, hi):
    """`value` as a float if it is finite and within lo..hi, else ValueError naming the flag.
    nan fails every comparison and inf every bound, so neither can slip through a bare range test."""
    try:
        number = float(value)
    except (TypeError, ValueError):
        raise ValueError(f"{flag} must be a number from {lo:g} to {hi:g}, got {value!r}") from None
    if not math.isfinite(number) or not lo <= number <= hi:
        raise ValueError(f"{flag} must be a finite number from {lo:g} to {hi:g}, got {value!r}")
    return number


def check_flags(args, limits):
    """Refuse (ValueError) any parsed flag outside its (attribute, flag, lo, hi) limit. The scripts
    turn these numbers into timedeltas, slice bounds and divisors, where a nan, an inf or an
    absurd magnitude raises from deep inside the report."""
    for attr, flag, lo, hi in limits:
        check_number(getattr(args, attr), flag, lo, hi)


def weights_from_args(args):
    check_flags(args, WEIGHT_LIMITS)
    mult = {}
    for item in args.model_weight:
        name, sep, val = item.partition("=")
        if not sep or not name.strip():
            raise ValueError(f"--model-weight wants KEY=MULT, got {item!r}")
        mult[name.strip()] = check_number(val, f"--model-weight {name.strip()}", 0.0, WEIGHT_MAX)
    return Weights(args.wcw, args.wcr, args.wout, mult)


def add_root_args(ap):
    ap.add_argument("--root", default=None,
                    help="transcript root (default: $CLAUDE_CONFIG_DIR/projects if set, else ~/.claude/projects)")
    ap.add_argument("--project", default=None, help="only project directories whose name contains this substring")


def add_out_args(ap):
    ap.add_argument("--out", default=None,
                    help="output directory (default: a new directory under the system temp directory, path printed). "
                         "Refused inside a git working tree unless --force-out, and inside an agent home always")
    ap.add_argument("--force-out", action="store_true", help="allow --out inside a git working tree")


# ---------------------------------------------------------------------------
# sessions
# ---------------------------------------------------------------------------

class Call:
    __slots__ = ("id", "session", "ts", "model", "effort", "inp", "out", "cc", "cr", "side",
                 "stop", "dropped", "where", "weighted", "in_window")

    @property
    def context(self):
        return self.inp + self.cc + self.cr


class Session:
    def __init__(self, path, root):
        rel = os.path.relpath(path, root)
        parts = rel.replace("\\", "/").split("/")
        self.path = path
        self.project = parts[0]
        self.id = os.path.splitext(parts[-1])[0]
        self.subagent = "subagents" in parts
        self.calls = {}          # message id -> Call (this file)
        # ordered events:
        #   ('call', id)  ('send', ts, to, text)  ('deliver', ts, label, parts)
        #   ('user', ts, text, is_nudge, is_human, is_tag, origin)
        #   ('compact', ts, trigger, pre, post, duration_ms)
        #   detail only: ('atext', ts, text)  ('release', ts, command)
        self.events = []
        # event index -> the record-level name of that event ("<record uuid>" for a user text,
        # "<record uuid>#t<block>" for an assistant text block, "<record uuid>#u<tool_use id>" for a
        # send_to_peer, "<record uuid>#b<block>" for one without a tool_use id). Only text, send and
        # user events get one, and only when the record has a string uuid. A side table, not a tuple
        # field, so every consumer of the event shapes is untouched. The block-index forms are
        # positional inside the record: stable only because the transcript is append-only.
        self.keys = {}
        self.sidechain_events = set()
        self.tools = Counter()
        self.compactions = []
        self.first_ts = None
        self.last_ts = None
        self.cwd = None
        self.role = "solo"
        self.evidence = ""
        # [(member, brigade)] named by the briefings in this file's prompt snapshots, in file order,
        # for a session that uses banto tools without a Director-only one (see snapshot_members).
        self.members = []
        self.lines = self.parsed = self.skipped = self.ignored = 0


# A release build is how a Rust project reaches the operator; other stacks add theirs here.
RELEASE_BUILD_RX = re.compile(r"(cargo\s+build[^\n|;&]*--release|cargo\s+build\s+-r\b)")


def split_delivery(text):
    """check_messages answer -> [(sender, display name or None, scope or None, body)]"""
    hs = list(FROM_HDR_RE.finditer(text))
    parts = []
    for i, m in enumerate(hs):
        body = text[m.end(): hs[i + 1].start() if i + 1 < len(hs) else len(text)].strip()
        hm = FROM_PARTS_RE.match(m.group(1).strip())
        if hm:
            parts.append((hm.group(1), hm.group(2), hm.group(3), body))
        else:
            parts.append((m.group(1), None, None, body))
    return parts


def instruction_label(text):
    """First line of the first delivered instruction: the line after the first
    '[from ...]' header of a check_messages answer."""
    lines = text.splitlines()
    for i, ln in enumerate(lines):
        if FROM_RE.match(ln.strip()):
            return clean("\n".join(lines[i + 1:]))
    return clean(text)


_TYPE_RX = type_rx("assistant", "user")


class _BadRecord(ValueError):
    """A record whose fields have the wrong type; it is skipped like a malformed line."""


def read_session(path, root, detail=False):
    """Parse one transcript. detail=True also keeps assistant text, release builds
    and the full parts of every delivery (larger in memory; the usage scripts do
    not need them)."""
    s = Session(path, root)
    check_ids = set()
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            s.lines += 1
            line = line.strip()
            if not line:
                continue
            # cheap filter: the big blob / attachment records are never needed
            if not _TYPE_RX.search(line) and "compact_boundary" not in line:
                s.ignored += 1
                continue
            try:
                d = json.loads(line)
            except ValueError:
                s.skipped += 1
                continue
            if not isinstance(d, dict):
                s.skipped += 1
                continue
            n_events, n_comp = len(s.events), len(s.compactions)
            try:
                # inside the guard: an offset that overflows the calendar is a bad record, not a crash
                ts = _parse_ts(d.get("timestamp"))
                _read_record(s, d, ts, check_ids, detail)
            except (TypeError, ValueError, AttributeError, OverflowError):
                # a record is taken whole or not at all
                del s.events[n_events:]
                for stale in [k for k in s.keys if k >= n_events]:
                    del s.keys[stale]
                del s.compactions[n_comp:]
                s.skipped += 1
                continue
            s.parsed += 1
            if ts:
                s.first_ts = ts if s.first_ts is None or ts < s.first_ts else s.first_ts
                s.last_ts = ts if s.last_ts is None or ts > s.last_ts else s.last_ts
            if s.cwd is None and isinstance(d.get("cwd"), str):
                s.cwd = d["cwd"]
    _assign_role(s)
    return s


def _object(value):
    """An optional JSON object field: absent is {}, any other type is a bad record."""
    if value is None:
        return {}
    if not isinstance(value, dict):
        raise _BadRecord(f"expected an object, got {type(value).__name__}")
    return value


def _optional_str(value):
    if value is not None and not isinstance(value, str):
        raise _BadRecord(f"expected a string, got {type(value).__name__}")
    return value


def _optional_number(value):
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise _BadRecord(f"expected a number, got {type(value).__name__}")
    return value


def _read_record(s, d, ts, check_ids, detail):
    kind = d.get("type")
    if kind == "assistant":
        _read_assistant(s, d, ts, check_ids, detail)
    elif kind == "user":
        _read_user(s, d, ts, check_ids, detail)
    elif d.get("subtype") == "compact_boundary":
        meta = _object(d.get("compactMetadata"))
        comp = (ts, _optional_str(meta.get("trigger")), _optional_number(meta.get("preTokens")),
                _optional_number(meta.get("postTokens")), _optional_number(meta.get("durationMs")))
        s.compactions.append(comp)
        s.events.append(("compact",) + comp)


def _key(s, d, index, sub):
    """Name the event about to be stored at `index` after its transcript record: the record's own
    uuid, which no other record's presence, absence or text can change. A record without a string
    uuid gets no name and its events fall back to positional naming downstream."""
    uid = d.get("uuid")
    if isinstance(uid, str) and uid:
        s.keys[index] = uid + sub


def _read_assistant(s, d, ts, check_ids, detail):
    """Everything is validated before anything is stored, so a bad record leaves
    no half-read state behind."""
    msg = _object(d.get("message"))
    mid = msg.get("id")
    if mid is not None and not isinstance(mid, str):
        raise _BadRecord("message.id is not a string")
    model = _optional_str(msg.get("model"))
    usage = msg.get("usage")
    if usage is not None and not isinstance(usage, dict):
        raise _BadRecord("message.usage is not an object")

    new_call = None
    if mid and usage is not None:
        c = Call()
        c.id, c.session, c.ts = mid, s.id, ts
        c.model = model or "?"
        # Claude Code writes the reasoning effort a call ran with at the top level of the assistant
        # record, beside message.model, back to 2.1.224 (the oldest version checked). A record
        # without one, or with one that is not a string, reads "?": the field is the one thing
        # here that a malformed value must not cost the whole record.
        eff = d.get("effort")
        c.effort = eff.strip() if isinstance(eff, str) and eff.strip() else "?"
        c.inp = int(usage.get("input_tokens") or 0)
        c.out = int(usage.get("output_tokens") or 0)
        c.cc = int(usage.get("cache_creation_input_tokens") or 0)
        c.cr = int(usage.get("cache_read_input_tokens") or 0)
        c.side = bool(d.get("isSidechain"))
        c.stop = msg.get("stop_reason")
        c.dropped = False
        c.where = None
        c.weighted = 0.0
        c.in_window = True
        new_call = c

    events, subkeys, tools, checks = [], [], [], []
    content = msg.get("content")
    for block_no, b in enumerate(content if isinstance(content, list) else []):
        if not isinstance(b, dict):
            continue
        btype = b.get("type")
        if btype == "text":
            if detail and b.get("text"):
                events.append(("atext", ts, _optional_str(b["text"])))
                subkeys.append(f"#t{block_no}")
        elif btype == "tool_use":
            name = _optional_str(b.get("name")) or ""
            tool_id = _optional_str(b.get("id"))
            inp = as_dict(b.get("input"))
            short = name[len(BANTO_TOOL_PREFIX):] if name.startswith(BANTO_TOOL_PREFIX) else None
            if short:
                tools.append(short)
                if short == "check_messages":
                    if tool_id:
                        checks.append(tool_id)
                elif short == "request_decision" and detail:
                    events.append(("decision", ts))
                    subkeys.append(f"#u{tool_id}" if tool_id else f"#b{block_no}")
                elif short == "send_to_peer":
                    to = inp.get("to")
                    text = inp.get("text", "")
                    events.append(("send", ts, to if isinstance(to, str) else None,
                                   text if detail and isinstance(text, str) else ""))
                    subkeys.append(f"#u{tool_id}" if tool_id else f"#b{block_no}")
            elif detail and name in ("Bash", "PowerShell"):
                cmd = inp.get("command", "")
                if isinstance(cmd, str) and RELEASE_BUILD_RX.search(cmd):
                    events.append(("release", ts, cmd[:120]))
                    subkeys.append(None)

    # Commit. The call goes first: a record that carries a send_to_peer is often
    # the whole of a short errand's only call, and a send before its call would
    # close the errand before the call could join it.
    if new_call is not None:
        old = s.calls.get(mid)
        if old is None:
            s.calls[mid] = new_call
            s.events.append(("call", mid))
        elif new_call.out >= old.out:
            # keep the copy with the most output tokens, the later one on a tie
            # (a streamed message's last line is its final state); the call keeps
            # the FIRST copy's timestamp
            new_call.ts = old.ts
            s.calls[mid] = new_call
    for offset, sub in enumerate(subkeys):
        if sub is not None:
            _key(s, d, len(s.events) + offset, sub)
    if d.get("isSidechain"):
        s.sidechain_events.update(range(len(s.events), len(s.events) + len(events)))
    s.events.extend(events)
    s.tools.update(tools)
    check_ids.update(checks)


def _read_user(s, d, ts, check_ids, detail):
    msg = _object(d.get("message"))
    content = msg.get("content")
    if isinstance(content, list) and any(isinstance(b, dict) and b.get("type") == "tool_result" for b in content):
        events = []
        for b in content:
            if isinstance(b, dict) and b.get("type") == "tool_result":
                tool_use_id = _optional_str(b.get("tool_use_id"))
                if tool_use_id and tool_use_id in check_ids:
                    text = block_text(b.get("content"))
                    if DELIVERED_RE.match(text):
                        events.append(("deliver", ts, instruction_label(text), split_delivery(text) if detail else None))
                        continue
                if detail:
                    events.append(("result", ts))
        s.events.extend(events)
        return
    if d.get("isMeta") or d.get("isCompactSummary") or d.get("isVisibleInTranscriptOnly"):
        return
    origin = _object(d.get("origin")).get("kind")
    text = block_text(content)
    if not text.strip():
        return
    stripped = text.lstrip()
    is_nudge = stripped.startswith(NUDGE_PREFIX) or (origin == "task-notification" and "[banto relay]" in text)
    is_tag = stripped.startswith("<") and not is_nudge  # task-notification, command wrappers, bash tags
    _key(s, d, len(s.events), "")
    s.events.append(("user", ts, text, is_nudge, not is_nudge and not is_tag, is_tag, origin))


def snapshot_members(path):
    """[(member, brigade)] for the briefings in the system prompt of the file's FIRST `prompt_snapshot`
    record, in that order: member is "worker-N" or "goinkyo". A later snapshot is not read, and a
    first snapshot with no briefing gives [], whatever a later one carries.

    Only the system prompt of a snapshot counts. briefings() above counts every line that quotes a
    briefing, and a Director's file, or a Worker that read the briefing text while working on it,
    quotes it dozens of times: of 21 banto sessions from one project, whole-file counts gave a
    Director a Worker's briefing and a Worker a Goinkyo's. Claude Code writes the snapshot from
    2.1.265; an older file has none, and a Goinkyo in one stays filed as a Worker."""
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            if "prompt_snapshot" not in line:
                continue
            try:
                d = json.loads(line)
            except ValueError:
                continue
            att = d.get("attachment") if isinstance(d, dict) else None
            if not isinstance(att, dict) or att.get("type") != "prompt_snapshot":
                continue
            prompt = att.get("systemPrompt")
            text = prompt if isinstance(prompt, str) else json.dumps(prompt, ensure_ascii=False)
            found = [m.groups() for m in WORKER_BRIEFING_RE.finditer(text)]
            found += [("goinkyo", m.group(1)) for m in GOINKYO_BRIEFING_RE.finditer(text)]
            return found
    return []


def _assign_role(s):
    if s.subagent:
        s.role, s.evidence = "subagent", "path contains 'subagents'"
    elif set(s.tools) & DIRECTOR_ONLY_TOOLS:
        s.role = "director"
        s.evidence = "uses " + ",".join(sorted(set(s.tools) & DIRECTOR_ONLY_TOOLS))
    elif s.tools:
        s.role = "worker"
        s.evidence = "banto tools: " + ",".join(sorted(s.tools)) + " (no director-only tool)"
        # The Goinkyo uses the same banto tools as a Worker and no Director-only one: only its
        # briefing tells them apart. A file that was launched as a Goinkyo says so in its first
        # snapshot; a later one (a resume) can only repeat it.
        s.members = snapshot_members(s.path)
        if s.members and s.members[0][0] == "goinkyo":
            s.role = "goinkyo"
            s.evidence = "banto tools: " + ",".join(sorted(s.tools)) + "; its first system-prompt snapshot carries the Goinkyo briefing"
    else:
        s.role, s.evidence = "solo", "no banto tool call"


def find_session_files(root, project=None):
    files = sorted(glob.glob(os.path.join(root, "**", "*.jsonl"), recursive=True))
    if project:
        files = [f for f in files if project in os.path.relpath(f, root).replace("\\", "/").split("/")[0]]
    return files


def load_sessions(root, project=None, detail=False, files=None):
    """Read every transcript under root (or the given files), ordered by first
    timestamp. An unreadable file is reported on stderr and left out."""
    sessions = []
    for f in (files if files is not None else find_session_files(root, project)):
        try:
            sessions.append(read_session(f, root, detail))
        except OSError as err:
            print(f"cannot read {f}: {err}", file=sys.stderr)
    sessions.sort(key=lambda s: (s.first_ts or datetime.max.replace(tzinfo=timezone.utc), s.path))
    return sessions


def dedupe_calls(sessions, weights=None):
    """One call per message id across files: the earliest file's copy stays, later
    copies are marked dropped. Sets each kept call's weighted cost when `weights`
    is given. Returns the number of duplicates dropped."""
    seen, dups = {}, 0
    for s in sessions:
        for mid, c in s.calls.items():
            if mid in seen:
                c.dropped = True
                dups += 1
            else:
                seen[mid] = c
    if weights is not None:
        for s in sessions:
            for c in s.calls.values():
                c.weighted = weights.of(c)
    return dups


def in_window(ts, since, until):
    """since <= ts < until; a call without a timestamp is outside any bounded window."""
    if since is None and until is None:
        return True
    if ts is None:
        return False
    return not ((since and ts < since) or (until and ts >= until))


def prepare(sessions, weights, since=None, until=None):
    """Dedupe across files, price every call, mark the window and attach errands.
    Returns (all errands, duplicates dropped)."""
    dups = dedupe_calls(sessions, weights)
    for s in sessions:
        for c in s.calls.values():
            c.in_window = in_window(c.ts, since, until)
    return attach_errands(sessions), dups


def live_calls(s):
    """The session's calls that count: not a cross-file duplicate, inside the window."""
    return [c for c in s.calls.values() if not c.dropped and c.in_window]


def ran_call(c):
    """A call that ran something: not a synthetic placeholder and with tokens. A record with no tokens
    is a placeholder (ac_measure skips them too) and would count as a call that ran nothing."""
    return c.model != "<synthetic>" and (c.inp + c.out + c.cc + c.cr) > 0


def unmatched_in(calls, weights):
    """{model: {"calls": n, "tokens": n}} for those of `calls` whose model matches no multiplier
    key and so weighs 1.0. A call with no tokens is left out: it weighs nothing at any multiplier,
    and Claude Code's own placeholder model name would otherwise head every list."""
    found = {}
    for c in calls:
        tokens = c.inp + c.out + c.cc + c.cr
        if tokens and weights.key_for(c.model) is None:
            d = found.setdefault(c.model, {"calls": 0, "tokens": 0})
            d["calls"] += 1
            d["tokens"] += tokens
    return found


def unmatched_models(sessions, weights):
    """unmatched_in over the sessions' counted calls (see live_calls)."""
    return unmatched_in((c for s in sessions for c in live_calls(s)), weights)


def unmatched_line(found):
    """The one report line for unmatched_models: always printed, so its absence never has to be read."""
    head = "models matching no multiplier (weighed at 1.0): "
    if not found:
        return head + "none"
    return head + ", ".join(f"{m} ({d['calls']:,} call{'' if d['calls'] == 1 else 's'}, "
                            f"{d['tokens']:,} tokens)"
                            for m, d in sorted(found.items(), key=lambda kv: (-kv[1]["tokens"], kv[0])))


# ---------------------------------------------------------------------------
# errands
# ---------------------------------------------------------------------------

class Errand:
    def __init__(self, session, role, kind, start, label):
        self.session, self.role, self.kind = session, role, kind
        self.start, self.end, self.label = start, start, label
        self.calls = []
        self.sends = 0
        self.open = True
        self.nudge = False


def build_errands(s):
    """Split a session's calls into errands. Returns (errands, between_call_ids).

    Worker: an errand opens at a delivery and closes at its first send_to_peer;
    calls outside are 'between'. Director / solo / subagent: a turn opens at each
    operator message or relay nudge and runs to the next."""
    errands, between = [], []
    cur = None
    last_ts = s.first_ts

    def close(end):
        nonlocal cur
        if cur is not None:
            cur.end = end or cur.end
            cur.open = False
            errands.append(cur)
            cur = None

    if s.role in ("worker", "goinkyo"):
        for ev in s.events:
            kind = ev[0]
            if kind in MARK_EVENTS:
                continue
            ts = ev[1] if kind != "call" else None
            if ts:
                last_ts = ts
            if kind == "deliver" and cur is None:
                cur = Errand(s.id, s.role, "errand", ev[1], ev[2])
            elif kind == "call":
                (cur.calls if cur is not None else between).append(ev[1])
            elif kind == "send" and cur is not None:
                cur.sends += 1
                close(ev[1])
        if cur is not None:
            close(last_ts)
            errands[-1].open = True
    else:
        for ev in s.events:
            kind = ev[0]
            if kind in MARK_EVENTS:
                continue
            if kind != "call" and len(ev) > 1 and ev[1]:
                last_ts = ev[1]
            if kind == "user":
                ts, text, is_nudge, is_human, is_tag = ev[1], ev[2], ev[3], ev[4], ev[5]
                starts = is_nudge or is_human or (s.role == "solo" and is_tag and not text.lstrip().startswith("<task-notification"))
                if not starts:
                    continue
                close(ts)
                label = clean(text) if not is_nudge else "relay nudge"
                cur = Errand(s.id, s.role, "turn", ts, label)
                cur.nudge = is_nudge
            elif kind == "deliver" and cur is not None and cur.nudge and cur.label == "relay nudge":
                cur.label = "relay nudge: " + ev[2]
            elif kind == "call":
                (cur.calls if cur is not None else between).append(ev[1])
            elif kind == "send" and cur is not None:
                cur.sends += 1
        if cur is not None:
            close(last_ts)
            errands[-1].open = True
    return errands, between


def attach_errands(sessions):
    """Run build_errands on every session and point each call's `where` at its
    errand (or 'between'). Returns all errands; each session gets .errs/.between."""
    all_errands = []
    for s in sessions:
        errs, between = build_errands(s)
        s.errs, s.between = errs, between
        for e in errs:
            for mid in e.calls:
                s.calls[mid].where = e
        for mid in between:
            s.calls[mid].where = "between"
        all_errands.extend(errs)
    return all_errands


# ---------------------------------------------------------------------------
# brigades
# ---------------------------------------------------------------------------

def briefings(path):
    """Counter of (worker, brigade) and ('goinkyo', brigade) briefings a file
    contains (whole file; a resumed member's file can span several brigades)."""
    c = Counter()
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            if "You are worker-" in line:
                for m in WORKER_BRIEFING_RE.finditer(line):
                    c[m.groups()] += 1
            if "You are the Goinkyo of banto brigade" in line:
                for m in GOINKYO_BRIEFING_RE.finditer(line):
                    c[("goinkyo", m.group(1))] += 1
    return c


def label_sessions(sessions, brigade, director_id):
    """session id -> 'director' | 'worker-N' | 'goinkyo' | 'sub:<parent label>'.
    A member's label comes from the briefing it received in THIS brigade."""
    label_of = {}
    for s in sessions:
        if s.subagent:
            continue
        if s.id == director_id:
            label_of[s.id] = "director"
            continue
        if s.role not in ("worker", "solo", "goinkyo"):
            continue
        mine = Counter({w: n for (w, b), n in briefings(s.path).items() if b == brigade})
        if mine:
            label_of[s.id] = mine.most_common(1)[0][0]
    for s in sessions:
        if s.subagent:
            parts = s.path.replace("\\", "/").split("/")
            pid = parts[parts.index("subagents") - 1] if "subagents" in parts else None
            label_of[s.id] = ("sub:" + label_of.get(pid, "?")) if pid else "sub:?"
    return label_of
