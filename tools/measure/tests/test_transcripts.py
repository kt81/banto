import os
import tempfile
import unittest
from datetime import datetime, timezone
from unittest import mock

import synth
from synth import TOOL, call_lines, compact, delivery, tool_result, ts, usage, user_text, write_jsonl

import transcripts as tr


class Base(unittest.TestCase):
    def setUp(self):
        self.root, self.proj = synth.make_root()
        self.addCleanup(synth.rmtree, self.root)

    def read(self, name, records, raw_lines=(), detail=False):
        path = write_jsonl(os.path.join(self.proj, name + ".jsonl"), records, raw_lines)
        return tr.read_session(path, self.root, detail)


class TimestampTests(unittest.TestCase):
    def test_forms(self):
        self.assertEqual(tr.parse_ts("2025-03-10T10:00:01Z"), datetime(2025, 3, 10, 10, 0, 1, tzinfo=timezone.utc))
        self.assertEqual(tr.parse_ts("2025-03-10T10:00:01.123Z").microsecond, 123000)
        self.assertEqual(tr.parse_ts("2025-03-10T12:00:00+02:00"), datetime(2025, 3, 10, 10, 0, tzinfo=timezone.utc))
        self.assertEqual(tr.parse_ts("2025-03-10"), datetime(2025, 3, 10, tzinfo=timezone.utc))

    def test_garbage_is_none(self):
        for bad in (None, "", "yesterday", "2025-13-45T00:00:00Z"):
            self.assertIsNone(tr.parse_ts(bad))


class DedupeTests(Base):
    def test_within_file_keeps_most_output_and_first_timestamp(self):
        # a streamed message: the same id on several lines, the last one final
        recs = [
            synth.assistant("m1", ts(0), [{"type": "text", "text": "a"}], u=usage(inp=5, out=3, cr=100)),
            synth.assistant("m1", ts(0, 5), [{"type": "text", "text": "b"}], u=usage(inp=5, out=40, cr=100)),
            synth.assistant("m1", ts(0, 9), [{"type": "text", "text": "c"}], u=usage(inp=5, out=20, cr=100)),
        ]
        s = self.read("a", recs)
        self.assertEqual(len(s.calls), 1)
        c = s.calls["m1"]
        self.assertEqual(c.out, 40)
        self.assertEqual(tr.iso(c.ts), "2025-03-10T10:00:00Z")

    def test_cross_file_first_listed_session_keeps_the_call(self):
        first = self.read("first", [synth.assistant("m1", ts(0), [], u=usage(out=1, cr=10)),
                                    synth.assistant("m2", ts(1), [], u=usage(out=2, cr=10))])
        second = self.read("second", [synth.assistant("m1", ts(0), [], u=usage(out=1, cr=10)),
                                      synth.assistant("m3", ts(5), [], u=usage(out=3, cr=10))])
        dups = tr.dedupe_calls([first, second], tr.Weights())
        self.assertEqual(dups, 1)
        self.assertFalse(first.calls["m1"].dropped)
        self.assertTrue(second.calls["m1"].dropped)
        self.assertEqual(sum(1 for s in (first, second) for c in s.calls.values() if not c.dropped), 3)

    def test_load_sessions_orders_by_first_timestamp_so_earliest_file_wins(self):
        write_jsonl(os.path.join(self.proj, "z-early.jsonl"), [synth.assistant("m1", ts(0), [], u=usage(out=1))])
        write_jsonl(os.path.join(self.proj, "a-late.jsonl"), [synth.assistant("m1", ts(0), [], u=usage(out=1)),
                                                              synth.assistant("m9", ts(30), [], u=usage(out=1))])
        sessions = tr.load_sessions(self.root)
        # both files start at minute 0: ties break by path, so a-late is first and keeps m1
        tr.dedupe_calls(sessions)
        kept = {s.id: sorted(m for m, c in s.calls.items() if not c.dropped) for s in sessions}
        self.assertEqual(kept, {"a-late": ["m1", "m9"], "z-early": []})
        # and with a clearly earlier file, that one wins
        write_jsonl(os.path.join(self.proj, "a-late.jsonl"), [synth.assistant("m1", ts(5), [], u=usage(out=1))])
        write_jsonl(os.path.join(self.proj, "z-early.jsonl"), [synth.assistant("m1", ts(0), [], u=usage(out=1))])
        sessions = tr.load_sessions(self.root)
        tr.dedupe_calls(sessions)
        kept = {s.id: sorted(m for m, c in s.calls.items() if not c.dropped) for s in sessions}
        self.assertEqual(kept, {"a-late": [], "z-early": ["m1"]})


class CompactionTests(Base):
    def test_compaction_record_is_parsed_and_ordered(self):
        recs = synth.call_lines("m1", ts(0), usage(cr=900)) + [compact(ts(1), 950, 60, 24000, "auto")] \
            + synth.call_lines("m2", ts(2), usage(cc=60))
        s = self.read("c", recs)
        self.assertEqual(len(s.compactions), 1)
        when, trigger, pre, post, dur = s.compactions[0]
        self.assertEqual((tr.iso(when), trigger, pre, post, dur), ("2025-03-10T10:01:00Z", "auto", 950, 60, 24000))
        self.assertEqual([e[0] for e in s.events], ["call", "compact", "call"])

    def test_compaction_without_metadata_is_still_a_record(self):
        s = self.read("c", [{"type": "system", "subtype": "compact_boundary", "timestamp": ts(1)}])
        self.assertEqual(s.compactions, [(tr.parse_ts(ts(1)), None, None, None, None)])


class LenientParsingTests(Base):
    def test_malformed_lines_are_skipped_not_fatal(self):
        raw = ['{"type":"assistant", broken', '[1,2,3]', '"type":"user" not json', ""]
        s = self.read("m", call_lines("m1", ts(0), usage(inp=1, out=2)), raw_lines=raw)
        self.assertEqual(len(s.calls), 1)
        self.assertGreaterEqual(s.skipped, 2)  # the broken line and the non-object line were counted, not raised

    def test_unknown_record_types_and_fields_are_ignored(self):
        recs = [{"type": "something-new", "x": 1}, {"type": "assistant", "timestamp": ts(0),
                "message": {"id": "m1", "model": "m", "usage": usage(out=1), "future_field": {"a": 1}}, "extra": True}]
        s = self.read("u", recs)
        self.assertEqual(s.calls["m1"].out, 1)

    def test_missing_usage_and_bad_numbers(self):
        recs = [synth.assistant("m1", ts(0), [], u={"input_tokens": None, "output_tokens": 7})]
        s = self.read("n", recs)
        self.assertEqual((s.calls["m1"].inp, s.calls["m1"].out), (0, 7))


class RoleTests(Base):
    def test_roles(self):
        worker = self.read("w", synth.worker_records())
        self.assertEqual(worker.role, "worker")
        director = self.read("d", call_lines("m1", ts(0), usage(inp=1), tools=[(TOOL + "request_decision", "t1", {})]))
        self.assertEqual(director.role, "director")
        solo = self.read("s", call_lines("m1", ts(0), usage(inp=1), tools=[("Bash", "t1", {"command": "ls"})]))
        self.assertEqual(solo.role, "solo")
        sub_path = write_jsonl(os.path.join(self.proj, "parent", "subagents", "agent-1.jsonl"), call_lines("m1", ts(0), usage(inp=1)))
        self.assertEqual(tr.read_session(sub_path, self.root).role, "subagent")


class WorkerErrandTests(Base):
    def test_errand_runs_from_delivery_to_first_send(self):
        s = self.read("w", synth.worker_records())
        errs, between = tr.build_errands(s)
        self.assertEqual(len(errs), 1)
        e = errs[0]
        self.assertEqual((e.kind, e.calls, e.sends, e.open), ("errand", ["m1", "m2"], 1, False))
        self.assertEqual(between, ["m0"])
        self.assertEqual(e.label, "implement the thing")

    def test_unfinished_errand_is_open(self):
        recs = synth.worker_records()[:-2]  # drop m2 and its send
        s = self.read("w", recs)
        errs, _ = tr.build_errands(s)
        self.assertEqual(len(errs), 1)
        self.assertTrue(errs[0].open)
        self.assertEqual(errs[0].calls, ["m1"])

    def test_no_delivery_means_no_errand(self):
        recs = call_lines("m0", ts(0), usage(cr=5), tools=[(TOOL + "check_messages", "c1", {})]) \
            + [tool_result(ts(1), "c1", "No new messages.")]
        s = self.read("w", recs)
        errs, between = tr.build_errands(s)
        self.assertEqual((errs, between), ([], ["m0"]))


class DirectorTurnTests(Base):
    def test_task_notification_wake_opens_a_nudge_turn(self):
        recs = [user_text(ts(1), "start"),
                user_text(ts(2), "<task-notification>[banto relay] check messages</task-notification>",
                          origin={"kind": "task-notification"}),
                user_text(ts(3), "<task-notification>other task</task-notification>",
                          origin={"kind": "task-notification"})]
        s = self.read("d", recs)
        s.role = "director"
        errs, _ = tr.build_errands(s)
        self.assertEqual([e.nudge for e in errs], [False, True])

    def director(self):
        recs = []
        recs += call_lines("pre", ts(0), usage(cr=10), tools=[(TOOL + "name_member", "n1", {})])  # before any turn
        recs.append(user_text(ts(1), "please start the work"))
        recs += call_lines("t1", ts(2), usage(cr=20))
        recs.append(user_text(ts(3), "[banto relay] Mail waiting. Call check_messages."))
        recs += call_lines("t2a", ts(3, 5), usage(cr=30), tools=[(TOOL + "check_messages", "c1", {})])
        recs.append(tool_result(ts(4), "c1", delivery(('worker-1 "impl" — to you', "report text"))))
        recs += call_lines("t2b", ts(5), usage(cr=40))
        recs.append(user_text(ts(6), "<task-notification>background done</task-notification>"))
        recs += call_lines("t2c", ts(7), usage(cr=50))
        recs.append(user_text(ts(8), "what is the status?"))
        recs += call_lines("t3", ts(9), usage(cr=60))
        return recs

    def test_turns_open_at_operator_messages_and_nudges_only(self):
        s = self.read("d", self.director())
        self.assertEqual(s.role, "director")
        errs, between = tr.build_errands(s)
        self.assertEqual(between, ["pre"])
        self.assertEqual([e.kind for e in errs], ["turn"] * 3)
        self.assertEqual([e.calls for e in errs], [["t1"], ["t2a", "t2b", "t2c"], ["t3"]])  # the task notification does not split a turn
        self.assertEqual([e.nudge for e in errs], [False, True, False])
        self.assertEqual(errs[1].label, "relay nudge: report text")
        self.assertEqual(errs[0].label, "please start the work")
        self.assertTrue(errs[2].open)


class WeightTests(unittest.TestCase):
    def test_default_weights(self):
        w = tr.Weights()
        # input 1 + cache-write 1.25 + cache-read 0.025 + output 5; "m" matches no model family
        self.assertAlmostEqual(w.cost("m", 10, 100, 1000, 20), 10 + 125 + 25 + 100)

    def test_model_multiplier_substring_and_longest_key(self):
        w = tr.Weights(model_mult={"fam": 2.0, "fam-big": 3.0, "exact-model": 0.5})
        self.assertEqual(w.mult("exact-model"), 0.5)
        self.assertEqual(w.mult("fam-big-1"), 3.0)
        self.assertEqual(w.mult("fam-small"), 2.0)
        self.assertEqual(w.mult("a-fam-b"), 2.0)          # a substring anywhere, not a prefix
        self.assertEqual(w.mult("other"), 1.0)
        self.assertAlmostEqual(w.cost("fam-small", 10, 100, 1000, 20), 260 * 2.0)

    def test_weights_from_args(self):
        import argparse
        ap = argparse.ArgumentParser()
        tr.add_weight_args(ap)
        w = tr.weights_from_args(ap.parse_args(["--wcw", "2", "--model-weight", "a=4"]))
        self.assertEqual((w.wcw, w.wcr, w.wout, w.mult("a-1")), (2.0, 0.025, 5.0, 4.0))
        with self.assertRaises(ValueError):
            tr.weights_from_args(ap.parse_args(["--model-weight", "nonsense"]))

    def test_prepare_prices_calls_and_marks_window(self):
        root, proj = synth.make_root()
        self.addCleanup(synth.rmtree, root)
        write_jsonl(os.path.join(proj, "a.jsonl"), call_lines("m1", ts(0), usage(inp=10, cc=100, cr=1000, out=20))
                    + call_lines("m2", ts(30), usage(inp=1)))
        sessions = tr.load_sessions(root)
        tr.prepare(sessions, tr.Weights(), since=tr.parse_ts(ts(10)))
        c1, c2 = sessions[0].calls["m1"], sessions[0].calls["m2"]
        self.assertAlmostEqual(c1.weighted, 260)
        self.assertEqual((c1.in_window, c2.in_window), (False, True))
        self.assertEqual(tr.live_calls(sessions[0]), [c2])


class OutDirTests(unittest.TestCase):
    def setUp(self):
        self.base = tempfile.mkdtemp(prefix="measure-out-")
        self.addCleanup(synth.rmtree, self.base)
        self.repo = os.path.join(self.base, "repo")
        os.makedirs(os.path.join(self.repo, ".git"))  # a .git directory is enough to be a working tree

    def test_refuses_inside_a_git_working_tree(self):
        target = os.path.join(self.repo, "deep", "out")
        with self.assertRaises(tr.OutDirError):
            tr.prepare_out_dir(target, "t")
        self.assertFalse(os.path.exists(target))  # refused before anything was created

    def test_force_allows_it(self):
        target = os.path.join(self.repo, "out")
        self.assertEqual(tr.prepare_out_dir(target, "t", force=True), target)
        self.assertTrue(os.path.isdir(target))

    def test_git_file_marks_a_worktree_too(self):
        other = os.path.join(self.base, "wt")
        os.makedirs(other)
        with open(os.path.join(other, ".git"), "w") as fh:  # a linked worktree has a .git file
            fh.write("gitdir: elsewhere")
        with self.assertRaises(tr.OutDirError):
            tr.prepare_out_dir(os.path.join(other, "out"), "t")

    def test_outside_a_repository_is_fine_and_default_is_under_temp(self):
        plain = os.path.join(self.base, "plain", "out")
        self.assertEqual(tr.prepare_out_dir(plain, "t"), plain)
        # the system temp directory is pointed at our own, so the "banto-measure" parent that a default
        # run creates is removed with it instead of being left behind
        with mock.patch.object(tempfile, "tempdir", self.base):
            default = tr.prepare_out_dir(None, "unit-test", now=0)
        self.assertTrue(os.path.realpath(default).startswith(os.path.realpath(self.base)))
        self.assertIn("unit-test", default)


class RootTests(unittest.TestCase):
    def test_config_dir_override_and_default(self):
        saved = os.environ.get("CLAUDE_CONFIG_DIR")
        self.addCleanup(lambda: os.environ.__setitem__("CLAUDE_CONFIG_DIR", saved) if saved is not None
                        else os.environ.pop("CLAUDE_CONFIG_DIR", None))
        os.environ["CLAUDE_CONFIG_DIR"] = os.path.join("somewhere", "cfg")
        self.assertEqual(tr.default_root(), os.path.join("somewhere", "cfg", "projects"))
        os.environ.pop("CLAUDE_CONFIG_DIR")
        self.assertEqual(tr.default_root(), os.path.join(os.path.expanduser("~"), ".claude", "projects"))

    def test_project_dir_name(self):
        name = tr.project_dir_name(os.path.join(os.sep, "a b", "c.d"))
        self.assertNotRegex(name, r"[^A-Za-z0-9-]")


if __name__ == "__main__":
    unittest.main()
