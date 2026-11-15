"""Hand-made synthetic transcripts for the tests. Nothing here comes from a real
session: ids, text, paths and dates are invented, and the shapes follow what
transcripts.py documents about the format.

One API call is written as several lines (one per content block), like the real
files, so event order in the tests matches event order in real data.
"""
import atexit
import contextlib
import io
import json
import os
import shutil
import stat
import sys
import tempfile
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

TOOL = "mcp__banto__"


def ts(minute, second=0, day=10):
    """A timestamp on an invented day: 2025-03-<day> 10:<minute>:<second>Z."""
    return f"2025-03-{day:02d}T10:{minute:02d}:{second:02d}.000Z"


def usage(inp=0, out=0, cc=0, cr=0):
    return {"input_tokens": inp, "output_tokens": out,
            "cache_creation_input_tokens": cc, "cache_read_input_tokens": cr}


def assistant(mid, when, blocks, model="model-a", u=None, side=False):
    return {"type": "assistant", "timestamp": when, "isSidechain": side,
            "message": {"id": mid, "model": model, "content": blocks, "usage": u or usage(), "stop_reason": None}}


def call_lines(mid, when, u, model="model-a", tools=(), text="working"):
    """One call as real transcripts write it: a text block, then one line per tool_use."""
    lines = [assistant(mid, when, [{"type": "text", "text": text}], model, u)]
    for name, tid, inp in tools:
        lines.append(assistant(mid, when, [{"type": "tool_use", "id": tid, "name": name, "input": inp}], model, u))
    return lines


def user_text(when, text, **extra):
    d = {"type": "user", "timestamp": when, "message": {"role": "user", "content": text}}
    d.update(extra)
    return d


def tool_result(when, tool_use_id, text):
    return {"type": "user", "timestamp": when,
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": text}]}}


def compact(when, pre, post, dur, trigger="auto"):
    return {"type": "system", "subtype": "compact_boundary", "timestamp": when,
            "compactMetadata": {"trigger": trigger, "preTokens": pre, "postTokens": post, "durationMs": dur}}


def delivery(*messages):
    """A check_messages answer. messages: (sender header, body)."""
    parts = [f"{len(messages)} message(s) relayed by banto"]
    for header, body in messages:
        parts.append(f"[from {header}]\n{body}")
    return "\n\n".join(parts)


def write_jsonl(path, records, raw_lines=(), spaced=False):
    """One record per line, compact JSON like the transcripts themselves, or with
    json.dumps's default `": "` spacing when `spaced` (equally valid JSONL that
    the readers must not mistake for another record type). raw_lines are appended
    verbatim, for malformed-line tests."""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    seps = None if spaced else (",", ":")
    with open(path, "w", encoding="utf-8") as fh:
        for r in records:
            fh.write(json.dumps(r, separators=seps, ensure_ascii=False) + "\n")
        for raw in raw_lines:
            fh.write(raw + "\n")
    return path


def worker_records():
    """A Worker with one errand and one call outside it.

    m0: a call outside any errand (it asks for mail), context 100000 (all cache read)
    deliver: one instruction from the director
    m1, m2: the errand's calls; m2 sends the report
    """
    recs = []
    recs += call_lines("m0", ts(0), usage(inp=0, out=0, cc=0, cr=100000),
                       tools=[(TOOL + "check_messages", "c1", {})])
    recs.append(tool_result(ts(1), "c1", delivery(('director — to you', "implement the thing"))))
    recs += call_lines("m1", ts(2), usage(inp=10, out=10, cc=0, cr=100000))
    recs += call_lines("m2", ts(3), usage(inp=10, out=10, cc=1000, cr=100000),
                       tools=[(TOOL + "send_to_peer", "s1", {"to": "director", "text": "Verdict: PASS"})])
    return recs


def rmtree(path):
    """shutil.rmtree for test cleanup. Git marks its object files read-only and Windows refuses to
    delete those, so a plain ignore_errors=True leaves a synthetic repository behind in the temp
    directory: clear the bit and retry, and ignore only what still fails."""
    def clear_and_retry(func, target, _exc):
        try:
            os.chmod(target, stat.S_IWRITE)
            func(target)
        except OSError:
            pass
    if sys.version_info >= (3, 12):
        shutil.rmtree(path, onexc=clear_and_retry)
    else:
        shutil.rmtree(path, onerror=clear_and_retry)


# A run without --out writes to a new directory under the system temp directory; point that
# at a scratch directory of our own so a test run leaves nothing behind there.
_SCRATCH_TMP = tempfile.mkdtemp(prefix="measure-tmp-")
atexit.register(rmtree, _SCRATCH_TMP)


def run_main(module, argv):
    """Run module.main(argv); returns (exit code, stdout, stderr)."""
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err),             mock.patch.object(tempfile, "tempdir", _SCRATCH_TMP):
        rc = module.main(argv)
    return rc, out.getvalue(), err.getvalue()


def make_root(prefix="measure-test-"):
    """A fresh transcript root (a temp directory) and its project directory."""
    root = tempfile.mkdtemp(prefix=prefix)
    proj = os.path.join(root, "proj")
    os.makedirs(proj)
    return root, proj
