"""Regressions found by an independent review of tools/measure: output guards,
JSON spacing, wrongly typed fields, send/call ordering, self-describing outputs
and the what-if carry across a window start. Each test failed before its fix."""
import contextlib
import csv
import io
import os
import tempfile
import unittest
from datetime import datetime, timezone
from unittest import mock

import synth
from synth import TOOL, call_lines, compact, delivery, run_main, ts, usage, user_text, write_jsonl

import ac_measure
import quality_report as qr
import transcripts as tr
import usage_report
import usage_whatif as wi

DEFAULT_BASIS = ("input=1 cache_write=1.25 cache_read=0.025 output=5 "
                 "model_multipliers=fable=4,haiku=0.68,opus=1,sonnet=0.68 other_models=1")


class Base(unittest.TestCase):
    def setUp(self):
        self.root, self.proj = synth.make_root()
        self.base = tempfile.mkdtemp(prefix="measure-review-")
        self.addCleanup(synth.rmtree, self.root)
        self.addCleanup(synth.rmtree, self.base)


class OutputGuards(Base):
    def setUp(self):
        super().setUp()
        write_jsonl(os.path.join(self.proj, "w.jsonl"), synth.worker_records())

    def test_a_symlinked_output_file_is_never_written_through(self):
        for module, name in ((usage_report, "calls.csv"), (wi, "whatif_A.csv")):
            with self.subTest(module.__name__):
                out = os.path.join(self.base, module.__name__)
                os.makedirs(out)
                victim = os.path.join(self.base, "victim-" + module.__name__)
                with open(victim, "w") as fh:
                    fh.write("SENTINEL")
                try:
                    os.symlink(victim, os.path.join(out, name))
                except (OSError, NotImplementedError, AttributeError):
                    self.skipTest("this platform cannot create symlinks here")
                rc, _, err = run_main(module, ["--root", self.root, "--out", out])
                self.assertEqual(rc, 2, err)
                self.assertIn("symlink", err)
                with open(victim) as fh:
                    self.assertEqual(fh.read(), "SENTINEL")

    def test_a_hard_linked_output_file_is_replaced_not_written_through(self):
        for module, name in ((usage_report, "calls.csv"), (wi, "whatif_A.csv")):
            with self.subTest(module.__name__):
                out = os.path.join(self.base, "hl-" + module.__name__)
                os.makedirs(out)
                victim = os.path.join(self.base, "victim-hl-" + module.__name__)
                with open(victim, "w") as fh:
                    fh.write("SENTINEL")
                try:
                    os.link(victim, os.path.join(out, name))
                except (OSError, NotImplementedError, AttributeError):
                    self.skipTest("this platform cannot create hard links here")
                rc, _, err = run_main(module, ["--root", self.root, "--out", out])
                self.assertEqual(rc, 0, err)
                with open(victim) as fh:
                    self.assertEqual(fh.read(), "SENTINEL")
                with open(os.path.join(out, name), newline="") as fh:
                    self.assertNotEqual(fh.read(), "SENTINEL")      # the name now holds the real output
                self.assertFalse([n for n in os.listdir(out) if n.endswith(".tmp")])

    def test_open_output_leaves_no_temporary_file_and_keeps_the_old_one_on_failure(self):
        path = os.path.join(self.base, "keep.txt")
        with open(path, "w") as fh:
            fh.write("OLD")
        with self.assertRaises(RuntimeError):
            with tr.open_output(path) as fh:
                fh.write("HALF")
                raise RuntimeError("the run died mid-write")
        with open(path) as fh:
            self.assertEqual(fh.read(), "OLD")
        self.assertEqual(os.listdir(self.base), ["keep.txt"])
        with tr.open_output(path) as fh:
            fh.write("NEW")
        with open(path) as fh:
            self.assertEqual(fh.read(), "NEW")
        self.assertEqual(os.listdir(self.base), ["keep.txt"])

    def test_a_name_that_cannot_be_replaced_is_a_message_and_an_exit_code_not_a_traceback(self):
        # a directory sits where calls.csv must go: no platform lets a file replace it (Windows also
        # refuses a read-only file, POSIX does not, so this is the portable form of that failure)
        out = os.path.join(self.base, "blocked")
        os.makedirs(os.path.join(out, "calls.csv"))
        rc, _, err = run_main(usage_report, ["--root", self.root, "--out", out])
        self.assertEqual(rc, 2, err)
        self.assertIn("calls.csv", err)
        self.assertNotIn("Traceback", err)
        self.assertFalse([n for n in os.listdir(out) if n.endswith(".tmp")])

    def test_open_output_turns_a_failed_replace_into_outdirerror_and_removes_the_temp_file(self):
        path = os.path.join(self.base, "keep.txt")
        with open(path, "w") as fh:
            fh.write("OLD")
        for error in (PermissionError(13, "Access is denied"), OSError(28, "No space left on device")):
            with self.subTest(error=type(error).__name__), mock.patch("transcripts.os.replace", side_effect=error):
                with self.assertRaises(tr.OutDirError) as caught:
                    with tr.open_output(path) as fh:
                        fh.write("NEW")
                self.assertIn(path, str(caught.exception))
                self.assertEqual(os.listdir(self.base), ["keep.txt"])
                with open(path) as fh:
                    self.assertEqual(fh.read(), "OLD")

    def test_open_output_turns_a_failed_write_into_outdirerror(self):
        path = os.path.join(self.base, "x.txt")
        with self.assertRaises(tr.OutDirError):
            with tr.open_output(path) as fh:
                raise OSError(28, "No space left on device")
        self.assertEqual(os.listdir(self.base), [])

    def test_an_output_directory_that_cannot_be_made_is_refused_not_raised(self):
        blocker = os.path.join(self.base, "afile")
        with open(blocker, "w") as fh:
            fh.write("x")
        rc, _, err = run_main(usage_report, ["--root", self.root, "--out", os.path.join(blocker, "sub")])
        self.assertEqual(rc, 2, err)
        self.assertNotIn("Traceback", err)

    def test_two_default_runs_in_the_same_second_get_their_own_directories(self):
        with mock.patch.object(tempfile, "tempdir", self.base):         # removed with self.base, parent included
            made = [tr.prepare_out_dir(None, "unit-test-unique", now=0) for _ in range(3)]
        self.assertEqual(len(set(made)), 3)
        for path in made:
            self.assertTrue(os.path.isdir(path))
            self.assertIn("unit-test-unique", os.path.basename(path))

    def test_agent_homes_are_refused_even_with_force_out(self):
        home, cfg, codex = (os.path.join(self.base, n) for n in ("home", "cfg", "codexhome"))
        env = {"HOME": home, "USERPROFILE": home, "CLAUDE_CONFIG_DIR": cfg, "CODEX_HOME": codex}
        targets = [os.path.join(home, ".claude", "out"), os.path.join(home, ".claude"),
                   os.path.join(home, ".claude", "projects", "p"), os.path.join(home, ".codex", "out"),
                   os.path.join(cfg, "out"), os.path.join(codex, "out")]
        with mock.patch.dict(os.environ, env):
            for target in targets:
                for force in (False, True):
                    with self.subTest(target=os.path.relpath(target, self.base), force=force):
                        argv = ["--root", self.root, "--out", target] + (["--force-out"] if force else [])
                        for module in (usage_report, wi):
                            rc, _, err = run_main(module, argv)
                            self.assertEqual(rc, 2, module.__name__)
                            self.assertIn("agent", err)
                            self.assertFalse(os.path.exists(target))

    def test_a_directory_next_to_an_agent_home_is_not_refused(self):
        home = os.path.join(self.base, "home")
        out = os.path.join(home, ".claude-notes", "out")
        with mock.patch.dict(os.environ, {"HOME": home, "USERPROFILE": home}):
            rc, _, err = run_main(usage_report, ["--root", self.root, "--out", out])
        self.assertEqual(rc, 0, err)


class SpacedJson(Base):
    def test_json_with_normal_spacing_is_read_like_compact_json(self):
        recs = synth.worker_records() + [compact(ts(4), 101010, 8000, 20000)]
        write_jsonl(os.path.join(self.proj, "w.jsonl"), recs, spaced=True)
        s = tr.load_sessions(self.root)[0]
        self.assertEqual(sorted(s.calls), ["m0", "m1", "m2"])
        self.assertEqual(len(s.compactions), 1)
        self.assertEqual([e[0] for e in s.events if e[0] in ("deliver", "send")], ["deliver", "send"])

    def test_ac_measure_raw_passes_read_spaced_json_too(self):
        recs = call_lines("m0", ts(0), usage(cr=1000), tools=[("Read", "r1", {"file_path": "src/a.rs"})])
        recs += synth.worker_records()
        recs.append({"type": "api-request", "timestamp": ts(3, 59), "querySource": "compact"})
        recs.append({"type": "api-request", "timestamp": ts(5), "querySource": "main"})
        recs.append(compact(ts(4), 101010, 8000, 20000))
        write_jsonl(os.path.join(self.proj, "w.jsonl"), recs, spaced=True)
        s = tr.load_sessions(self.root)[0]
        ops, _ = ac_measure.scan_session_ops(s)
        self.assertEqual([(o[1], o[2], o[3]) for o in ops], [("read", "Read", "src/a.rs")])
        rc, stdout, err = run_main(ac_measure, ["--root", self.root, "--since", ts(2)])
        self.assertEqual(rc, 0, err)
        self.assertIn("{'compact': 1, 'main': 1}", stdout)       # querySource counts since T
        self.assertIn("api-request(compact)", stdout)            # the record next to the compaction


class WronglyTypedFields(Base):
    def bad_records(self):
        t = ts(1)
        chk = TOOL + "check_messages"
        return {
            "assistant message is a string": {"type": "assistant", "timestamp": t, "message": "bad"},
            "user message is a string": {"type": "user", "timestamp": t, "message": "bad"},
            "input_tokens is a word": synth.assistant("x1", t, [], u={"input_tokens": "bad"}),
            "cache_read is a list": synth.assistant("x2", t, [], u={"cache_read_input_tokens": [1]}),
            "compactMetadata is a string": {"type": "system", "subtype": "compact_boundary", "timestamp": t, "compactMetadata": "bad"},
            "preTokens is a word": {"type": "system", "subtype": "compact_boundary", "timestamp": t, "compactMetadata": {"preTokens": "bad"}},
            "origin is a string": user_text(t, "hello", origin="bad"),
            "tool name is a number": synth.assistant("x3", t, [{"type": "tool_use", "id": "t1", "name": 5, "input": {}}], u=usage(inp=1)),
            "check_messages id is a list": synth.assistant("x4", t, [{"type": "tool_use", "id": [1], "name": chk, "input": {}}], u=usage(inp=1)),
            "message id is a list": synth.assistant([1], t, [], u=usage(inp=1)),
            "model is a number": synth.assistant("x5", t, [], model=7, u=usage(inp=1)),
            "timestamp offset overflows below year 1": synth.assistant("x6", "0001-01-01T00:00:00+23:59", [], u=usage(inp=1)),
            "timestamp offset overflows above year 9999": synth.assistant("x7", "9999-12-31T23:59:59-23:59", [], u=usage(inp=1)),
            "tool_use_id is a list": {"type": "user", "timestamp": t, "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": [1], "content": "x"}]}},
        }

    def test_wrongly_typed_fields_are_skipped_and_counted_like_a_malformed_line(self):
        for name, bad in self.bad_records().items():
            with self.subTest(name):
                recs = call_lines("a", ts(0), usage(inp=1)) + [bad] + call_lines("b", ts(2), usage(inp=1))
                path = write_jsonl(os.path.join(self.proj, "w.jsonl"), recs)
                s = tr.read_session(path, self.root)
                self.assertEqual(sorted(s.calls), ["a", "b"])
                self.assertEqual(s.skipped, 1)

    def test_a_whole_run_survives_them(self):
        recs = call_lines("a", ts(0), usage(inp=1)) + list(self.bad_records().values()) + call_lines("b", ts(2), usage(inp=1))
        write_jsonl(os.path.join(self.proj, "w.jsonl"), recs)
        rc, stdout, err = run_main(usage_report, ["--root", self.root, "--out", os.path.join(self.base, "o")])
        self.assertEqual(rc, 0, err)

    def test_an_overflowing_timestamp_is_no_timestamp(self):
        for text in ("0001-01-01T00:00:00+23:59", "9999-12-31T23:59:59-23:59"):
            self.assertIsNone(tr.parse_ts(text), text)
        self.assertEqual(tr.parse_ts("0001-01-01T23:59:00+23:59"), datetime(1, 1, 1, 0, 0, tzinfo=timezone.utc))

    def test_a_window_flag_at_the_calendar_edge_is_refused_like_any_unreadable_one(self):
        write_jsonl(os.path.join(self.proj, "w.jsonl"), call_lines("a", ts(0), usage(inp=1)))
        edges = ("0001-01-01", "0001-01-01T00:00:00Z", "0001-01-01T00:00:00+23:59", "0001-06-01",
                 "9999-12-31", "9999-12-31T23:59:59Z", "9999-12-31T23:59:59-23:59")
        scripts = ((usage_report, ["--out", os.path.join(self.base, "r")], ("--since", "--until")),
                   (wi, ["--out", os.path.join(self.base, "w")], ("--since", "--until")),
                   (ac_measure, [], ("--since",)),
                   (qr, ["--repo", self.base], ("--since", "--until")))
        for module, extra, flags in scripts:
            for flag in flags:
                for value in edges:
                    with self.subTest(module.__name__, flag=flag, value=value):
                        argv = ["--root", self.root] + extra
                        if flag == "--until":
                            argv += ["--until", value]
                        else:
                            argv += ["--since", value]
                        rc, _, err = run_main(module, argv)
                        self.assertEqual(rc, 2, err)
                        self.assertIn("ISO-8601", err)

    def test_an_ordinary_old_or_far_date_is_still_accepted_as_a_flag(self):
        for value in ("0002-01-01", "1970-01-01", "2026-01-31T12:00:00Z", "9998-01-01"):
            self.assertIsNotNone(tr.parse_flag_ts(value), value)

    def test_ac_measure_raw_passes_survive_an_overflowing_timestamp(self):
        bad = {"type": "assistant", "timestamp": "0001-01-01T00:00:00+23:59", "message": {"content": []}}
        path = write_jsonl(os.path.join(self.proj, "w.jsonl"), [bad] + call_lines("a", ts(2), usage(inp=1)))
        s = tr.read_session(path, self.root)
        self.assertEqual(ac_measure.scan_session_ops(s), ([], {}))

    def test_ac_measure_raw_pass_survives_a_wrongly_typed_message(self):
        recs = [{"type": "assistant", "timestamp": ts(0), "message": "bad"}, {"type": "user", "timestamp": ts(1), "message": "bad"}]
        path = write_jsonl(os.path.join(self.proj, "w.jsonl"), recs + call_lines("a", ts(2), usage(inp=1)))
        s = tr.read_session(path, self.root)
        ops, results = ac_measure.scan_session_ops(s)
        self.assertEqual((ops, results), ([], {}))


class ImpossibleOffsets(Base):
    BAD = ("2026-01-01T00:00:00+99:99", "2026-01-01T00:00:00-99:99", "2026-01-01T00:00:00+24:00",
           "2026-01-01T00:00:00+23:60", "2026-01-01T00:00:00-0060", "2026-01-01T00:00:00+2400")

    def test_an_offset_no_clock_has_is_no_timestamp(self):
        for text in self.BAD:
            self.assertIsNone(tr.parse_ts(text), text)
            self.assertIsNone(tr.parse_flag_ts(text), text)

    def test_real_offsets_still_parse(self):
        for text in ("2026-01-01T12:00:00+23:59", "2026-01-01T12:00:00-12:00", "2026-01-01T12:00:00+0530"):
            self.assertIsNotNone(tr.parse_ts(text), text)
        self.assertEqual(tr.parse_ts("2026-01-01T12:00:00+05:30"), datetime(2026, 1, 1, 6, 30, tzinfo=timezone.utc))
        self.assertEqual(tr.parse_ts("2026-01-01T12:00:00-0930"), datetime(2026, 1, 1, 21, 30, tzinfo=timezone.utc))

    def test_every_script_refuses_such_a_window_flag(self):
        write_jsonl(os.path.join(self.proj, "w.jsonl"), call_lines("a", ts(0), usage(inp=1)))
        scripts = ((usage_report, ["--out", os.path.join(self.base, "r")], ("--since", "--until")),
                   (wi, ["--out", os.path.join(self.base, "w")], ("--since", "--until")),
                   (ac_measure, [], ("--since",)),
                   (qr, ["--repo", self.base], ("--since", "--until")))
        for module, extra, flags in scripts:
            for flag in flags:
                for value in self.BAD:
                    with self.subTest(module.__name__, flag=flag, value=value):
                        rc, _, err = run_main(module, ["--root", self.root] + extra + [flag, value])
                        self.assertEqual(rc, 2, err)
                        self.assertIn("ISO-8601", err)


class NumericFlags(Base):
    """A number a flag cannot use is refused with exit code 2 and a message naming the flag;
    it never reaches the arithmetic that would raise from inside the report."""

    def setUp(self):
        super().setUp()
        write_jsonl(os.path.join(self.proj, "w.jsonl"), synth.worker_records())

    def run_cli(self, module, argv):
        out, err = io.StringIO(), io.StringIO()
        try:
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                rc = module.main(argv)
        except SystemExit as exit_:                      # argparse's own refusal of a non-number
            rc = exit_.code
        return rc, err.getvalue()

    def base_argv(self, module):
        argv = ["--root", self.root]
        if module is ac_measure:
            argv += ["--since", "2000-01-01"]
        elif module is qr:
            argv += ["--repo", self.base]
        else:
            argv += ["--out", os.path.join(self.base, module.__name__)]
        return argv

    def refused(self, module, flag, value, name=None):
        with self.subTest(module.__name__, flag=flag, value=value):
            rc, err = self.run_cli(module, self.base_argv(module) + [flag, value])
            self.assertEqual(rc, 2, err)
            self.assertIn(name or flag, err)

    def test_weights_must_be_finite_and_within_bounds(self):
        for module in (usage_report, wi, ac_measure, qr):
            for flag in ("--wcw", "--wcr", "--wout"):
                for value in ("nan", "inf", "-inf", "-1", "1e300", "1001"):
                    self.refused(module, flag, value)

    def test_model_weights_must_be_finite_and_within_bounds(self):
        for module in (usage_report, wi, ac_measure, qr):
            for value in ("m=nan", "m=inf", "m=-1", "m=1e300", "m=abc", "m=", "=2"):
                self.refused(module, "--model-weight", value)

    def test_durations_must_be_finite_and_within_bounds(self):
        cases = ((ac_measure, "--like-for-like-hours", ("nan", "inf", "-1", "0", "1e300", "8761")),
                 (qr, "--fix-days", ("nan", "inf", "-1", "1e300", "366")),
                 (qr, "--horizon-h", ("nan", "inf", "-1", "1e300", "8761")),
                 (qr, "--merge-gap-min", ("nan", "inf", "-1", "1e300", "525601")))
        for module, flag, values in cases:
            for value in values:
                self.refused(module, flag, value)

    def test_counts_and_thresholds_must_be_within_bounds(self):
        for value in ("-1", "1000001", "abc", "1.5"):
            self.refused(usage_report, "--top", value)
            self.refused(wi, "--min-compactions", value)
        for value in ("abc", "", ",", "0", "-5", "1e3", "100,abc", "99999999999999999999", "2000000000"):
            self.refused(wi, "--thresholds", value)

    def test_the_documented_bounds_themselves_are_accepted(self):
        cases = ((usage_report, "--top", ("0", "1000000")), (wi, "--min-compactions", ("0", "1000000")),
                 (wi, "--thresholds", ("1", "1000000000", "150000,250000")),
                 (usage_report, "--wcw", ("0", "1000")), (wi, "--model-weight", ("m=0", "m=1000")),
                 (ac_measure, "--like-for-like-hours", ("0.5", "8760")))
        for module, flag, values in cases:
            for value in values:
                with self.subTest(module.__name__, flag=flag, value=value):
                    rc, err = self.run_cli(module, self.base_argv(module) + [flag, value])
                    self.assertEqual(rc, 0, err)


class AbsurdValues(Base):
    """Fields of the right type whose values no calendar or clock can use."""

    def compaction_run(self, when, duration, since, first=None):
        recs = call_lines("a", first or ts(0), usage(inp=1)) + [
            {"type": "system", "subtype": "compact_boundary", "timestamp": when,
             "compactMetadata": {"trigger": "auto", "preTokens": 100, "postTokens": 10, "durationMs": duration}},
        ] + call_lines("b", ts(9), usage(inp=1))
        write_jsonl(os.path.join(self.proj, "w.jsonl"), recs)
        return run_main(ac_measure, ["--root", self.root, "--since", since])

    def test_a_compaction_duration_beyond_the_clock_does_not_stop_ac_measure(self):
        for duration in (1e300, -1e300, 1e999):
            with self.subTest(duration=duration):
                rc, stdout, err = self.compaction_run(ts(5), duration, "2000-01-01")
                self.assertEqual(rc, 0, err)
                self.assertIn("started ~-Z", stdout)       # the start is unknown, said so, and the line is still printed

    def test_a_compaction_started_before_year_1_does_not_stop_ac_measure(self):
        # 400 days of compaction that began on day 3 of year 2: its start would be in year 0
        rc, stdout, err = self.compaction_run("0002-01-03T00:00:01Z", 3.5e10, "0002-01-03", first="0002-01-03T00:00:00Z")
        self.assertEqual(rc, 0, err)
        self.assertIn("started ~-Z", stdout)


class SendOrdering(Base):
    def records(self):
        recs = call_lines("m0", ts(0), usage(cr=1000), tools=[(TOOL + "check_messages", "c1", {})])
        recs.append(synth.tool_result(ts(1), "c1", delivery(("director — to you", "do it"))))
        # the errand's only call: ONE record that carries both the usage and the report
        recs.append(synth.assistant("m1", ts(2), [{"type": "tool_use", "id": "s1", "name": TOOL + "send_to_peer",
                                                   "input": {"to": "director", "text": "done"}}],
                                    u=usage(inp=5, out=5, cr=1000)))
        return recs

    def test_a_one_call_errand_whose_record_carries_the_send_keeps_its_call(self):
        write_jsonl(os.path.join(self.proj, "w.jsonl"), self.records())
        sessions = tr.load_sessions(self.root)
        errands, _ = tr.prepare(sessions, tr.Weights())
        self.assertEqual([e.calls for e in errands], [["m1"]])
        self.assertEqual(sessions[0].between, ["m0"])

    def test_it_reaches_errands_csv_and_what_if_a(self):
        write_jsonl(os.path.join(self.proj, "w.jsonl"), self.records())
        out = os.path.join(self.base, "o")
        rc, _, err = run_main(usage_report, ["--root", self.root, "--out", out])
        self.assertEqual(rc, 0, err)
        with open(os.path.join(out, "errands.csv"), encoding="utf-8") as fh:
            rows = list(csv.DictReader(fh))
        self.assertEqual([(r["calls"], r["sends"]) for r in rows], [("1", "1")])
        sessions = tr.load_sessions(self.root)
        tr.prepare(sessions, tr.Weights())
        wi.attach_order(sessions[0])
        self.assertEqual(wi.counterfactual_a(sessions[0], 500, tr.Weights())["errands"], 1)

    def test_director_turns_order_call_before_send_too(self):
        recs = [user_text(ts(0), "start"),
                synth.assistant("d1", ts(1), [{"type": "tool_use", "id": "s1", "name": TOOL + "send_to_peer",
                                               "input": {"to": "worker-1", "text": "go"}},
                                              {"type": "tool_use", "id": "n1", "name": TOOL + "name_member", "input": {}}],
                                u=usage(inp=3))]
        write_jsonl(os.path.join(self.proj, "d.jsonl"), recs)
        sessions = tr.load_sessions(self.root)
        errands, _ = tr.prepare(sessions, tr.Weights())
        self.assertEqual(sessions[0].role, "director")
        self.assertEqual([(e.calls, e.sends) for e in errands], [(["d1"], 1)])


class SelfDescribingOutputs(Base):
    def setUp(self):
        super().setUp()
        write_jsonl(os.path.join(self.proj, "w.jsonl"), synth.worker_records())

    def rows(self, path):
        with open(path, encoding="utf-8") as fh:
            return list(csv.DictReader(fh))

    def test_every_csv_with_a_weighted_cost_names_the_weights_it_used(self):
        out = os.path.join(self.base, "u")
        self.assertEqual(run_main(usage_report, ["--root", self.root, "--out", out])[0], 0)
        wout = os.path.join(self.base, "w")
        self.assertEqual(run_main(wi, ["--root", self.root, "--out", wout])[0], 0)
        paths = [os.path.join(out, n) for n in ("calls.csv", "errands.csv", "sessions.csv")] + [os.path.join(wout, "whatif_A.csv")]
        for path in paths:
            with self.subTest(os.path.basename(path)):
                rows = self.rows(path)
                self.assertTrue(rows)
                self.assertEqual({r["weights_assumed"] for r in rows}, {DEFAULT_BASIS})

    def test_the_basis_follows_the_flags(self):
        out = os.path.join(self.base, "u")
        argv = ["--root", self.root, "--out", out, "--wcr", "0.2", "--model-weight", "model-a=2"]
        self.assertEqual(run_main(usage_report, argv)[0], 0)
        basis = {r["weights_assumed"] for r in self.rows(os.path.join(out, "errands.csv"))}
        self.assertEqual(basis, {"input=1 cache_write=1.25 cache_read=0.2 output=5 "
                                 "model_multipliers=fable=4,haiku=0.68,model-a=2,opus=1,sonnet=0.68 other_models=1"})


class WhatIfCarry(Base):
    def test_a_threshold_crossed_before_the_window_is_carried_into_it(self):
        write_jsonl(os.path.join(self.proj, "w.jsonl"), synth.worker_records())
        sessions = tr.load_sessions(self.root)
        w = tr.Weights(wcr=0.1)          # the hand arithmetic below is at 0.1; the default is 0.025
        tr.prepare(sessions, w, since=tr.parse_ts(ts(1)))          # m0 (minute 0) is before the window
        s = sessions[0]
        wi.attach_order(s)
        P = {"director": None, "worker": 20000, "other": None}
        out = wi.counterfactual_b([s], 50000, P, {"director": 0, "worker": 100, "other": 0}, w)
        actual, cf, n, extra = out["worker"]
        # m0 (context 100000) crossed T before the window: the modelled compaction, its extra call and
        # its cache write all happened there, so none is charged here, and the drop (80000) already
        # applies to m1 and m2.
        #   m1: 10 + 0.1 * 20000 + 50 = 2060
        #   m2: 10 + 1.25 * 1000 + 0.1 * 20000 + 50 = 3310
        # (without the carry, m1 would cross T itself: one modelled compaction, 10501 of extra call)
        self.assertEqual((n, extra), (0, 0))
        self.assertAlmostEqual(actual, 10060 + 11310)
        self.assertAlmostEqual(cf, 2060 + 3310)


class HelpTexts(unittest.TestCase):
    """--help is the only documentation a script ships next to itself, so what it writes
    and where must be stated there, and every name it uses must be defined there."""

    def help_of(self, module):
        # a fixed width keeps argparse from re-wrapping on a narrow terminal
        with mock.patch.dict(os.environ, {"COLUMNS": "200"}):
            return module.build_parser().format_help()

    def test_each_help_says_what_it_writes(self):
        self.assertIn("writes no file at all", self.help_of(ac_measure))
        wi_help = self.help_of(wi)
        self.assertIn("whatif_A.csv", wi_help)
        self.assertIn("no file is written", wi_help)
        for name in ("calls.csv", "sessions.csv", "errands.csv", "context_series.csv"):
            self.assertIn(name, self.help_of(usage_report))
        for name in ("report.txt", "candidates_a.txt", "candidates_b.txt", "errands_dump.txt", "changes.json"):
            self.assertIn(name, self.help_of(qr))

    def test_candidates_are_defined_where_the_flag_names_them(self):
        text = self.help_of(qr)
        self.assertIn("candidate (a)", text)
        self.assertIn("(a) candidates_a.txt:", text)
        self.assertIn("(b) candidates_b.txt:", text)

    def test_session_sections_are_named_where_the_flag_numbers_them(self):
        text = self.help_of(ac_measure)
        for label in ("  2   ", "  3b  ", "  4   ", "  4b  "):
            self.assertIn(label, text)

    def test_out_flag_is_spelled_alike_everywhere(self):
        for module in (usage_report, wi, qr):
            self.assertIn("--out OUT", self.help_of(module))
            self.assertNotIn("--out DIR", self.help_of(module))


if __name__ == "__main__":
    unittest.main()
