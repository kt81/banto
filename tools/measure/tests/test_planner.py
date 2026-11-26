"""Synthetic Planner and banto-sender fixtures."""
import os
import tempfile
import unittest

import synth
import transcripts as tr
import director_turns
import quality_report as qr
import what_ran
import usage_whatif


class PlannerAndBanto(unittest.TestCase):
    def read(self, root, name, records):
        path = synth.write_jsonl(os.path.join(root, "project", name + ".jsonl"), records)
        return tr.read_session(path, root, detail=True)

    def test_only_planner_tools_identify_planner_and_director_tools_identify_director(self):
        with tempfile.TemporaryDirectory() as root:
            for tool in ("write_plan", "release_plan"):
                records = synth.call_lines("m-" + tool, synth.ts(0), synth.usage(out=2),
                                           tools=[(synth.TOOL + tool, "t", {})])
                s = self.read(root, tool, records)
                self.assertEqual(s.role, "planner")
                self.assertEqual(usage_whatif.role_of(s), "planner")
                self.assertEqual(what_ran.member_label(s), "planner")
            for tool in ("open_planner", "close_planner"):
                records = synth.call_lines("m-" + tool, synth.ts(0), synth.usage(out=2),
                                           tools=[(synth.TOOL + tool, "t", {})])
                self.assertEqual(self.read(root, tool, records).role, "director")

    def test_planner_markers_only_in_first_snapshot_system_prompt(self):
        with tempfile.TemporaryDirectory() as root:
            for i, marker in enumerate(tr.PLANNER_PROMPT_MARKERS):
                records = [synth.prompt_snapshot(synth.ts(0), "editable text\n" + marker),
                           synth.user_text(synth.ts(1), "hello")]
                s = self.read(root, "marker" + str(i), records)
                self.assertEqual(s.role, "planner")
                self.assertIn("snapshot", s.evidence)
                records = [synth.prompt_snapshot(synth.ts(0), "ordinary prompt"),
                           synth.prompt_snapshot(synth.ts(1), marker),
                           synth.user_text(synth.ts(2), marker)]
                self.assertEqual(self.read(root, "late" + str(i), records).role, "solo")

    def test_banto_sender_and_notice_do_not_open_or_cut_worker_errand(self):
        parts = tr.split_delivery(synth.delivery(("banto — to you", "notice"), ("planner — to you", "plan")))
        self.assertEqual([p[0] for p in parts], ["banto", "planner"])
        records = [synth.assistant("a", synth.ts(0), [{"type": "tool_use", "id": "c1", "name": synth.TOOL + "check_messages", "input": {}}]),
                   synth.tool_result(synth.ts(1), "c1", synth.delivery(("director — to you", "work"))),
                   synth.assistant("b", synth.ts(2), [{"type": "tool_use", "id": "c2", "name": synth.TOOL + "check_messages", "input": {}}]),
                   synth.tool_result(synth.ts(3), "c2", synth.delivery(("banto — to you", "approved"))),
                   synth.assistant("c", synth.ts(4), [{"type": "tool_use", "id": "s1", "name": synth.TOOL + "send_to_peer", "input": {"to": "director", "text": "done"}}])]
        with tempfile.TemporaryDirectory() as root:
            s = self.read(root, "worker", records)
            errs, _ = tr.build_errands(s)
            self.assertEqual(len(errs), 1)
            events = qr.window_events(s, tr.parse_ts(synth.ts(0)), None)
            analysis = qr.Analysis(tr.parse_ts(synth.ts(0)), None, 3, {})
            analysis.worker_session_errands(s, "worker-1", events)
            self.assertEqual(len(analysis.errands), 1)
            self.assertEqual(len(analysis.errands[0]["reports"]), 1)

    def test_planner_request_read_identifies_planner_on_both_path_styles(self):
        with tempfile.TemporaryDirectory() as root:
            for i, path in enumerate(("/tmp/banto/mcp/planner/2.txt", r"C:\banto\mcp\planner\2.txt")):
                records = synth.call_lines("r" + str(i), synth.ts(0), synth.usage(out=1),
                                           tools=[("Read", "read", {"file_path": path})])
                s = self.read(root, "planner" + str(i), records)
                self.assertEqual(s.role, "planner")
            shared = synth.call_lines("shared", synth.ts(0), synth.usage(out=1),
                                      tools=[(synth.TOOL + "read_plan", "read", {})])
            self.assertEqual(self.read(root, "shared", shared).role, "worker")

    def test_director_wake_from_banto_and_mixed_delivery(self):
        records = [synth.user_text(synth.ts(0), "[banto relay] wake", origin={"kind": "human"}),
                   synth.assistant("a", synth.ts(1), [{"type": "tool_use", "id": "c", "name": synth.TOOL + "check_messages", "input": {}}]),
                   synth.tool_result(synth.ts(2), "c", synth.delivery(("banto — to you", "approved"))),
                   synth.assistant("b", synth.ts(3), [{"type": "text", "text": "ok"}, {"type": "tool_use", "id": "d", "name": synth.TOOL + "open_planner", "input": {}}])]
        with tempfile.TemporaryDirectory() as root:
            s = self.read(root, "director", records)
            self.assertEqual(director_turns.turns_for(s, set())[0]["trigger"], "banto-notice")
            records[2] = synth.tool_result(synth.ts(2), "c", synth.delivery(("banto — to you", "ok"), ("worker-1 — to you", "done")))
            s = self.read(root, "mixed", records)
            self.assertEqual(director_turns.turns_for(s, set())[0]["trigger"], "relay-woken")
