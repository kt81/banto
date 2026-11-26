"""Director turn report tests using hand-made transcripts only."""
import os
import tempfile
import unittest

import synth
import director_turns as report
import transcripts as tr


class DirectorTurns(unittest.TestCase):
    def test_triggers_text_decisions_and_sidechain(self):
        records = [
            synth.user_text(synth.ts(0), "hello", origin={"kind": "human"}),
            synth.assistant("m1", synth.ts(1), [
                {"type": "thinking", "thinking": "hidden"},
                {"type": "text", "text": "one\ntwo"},
                {"type": "tool_use", "id": "d1", "name": synth.TOOL + "request_decision", "input": {"text": "hidden"}},
                {"type": "tool_use", "id": "d2", "name": synth.TOOL + "request_decision", "input": {}},
            ]),
            synth.assistant("side", synth.ts(2), [{"type": "text", "text": "not visible"}], side=True),
            synth.user_text(synth.ts(3), "[banto relay] wake", origin={"kind": "human"}),
            synth.assistant("m2", synth.ts(4), [{"type": "text", "text": "wake"}]),
            synth.user_text(synth.ts(5), "<task-notification>[banto relay] wake</task-notification>",
                            origin={"kind": "task-notification"}),
            synth.assistant("m3", synth.ts(6), [{"type": "text", "text": "done\n"}]),
            synth.user_text(synth.ts(7), "ambiguous origin"),
            synth.assistant("m4", synth.ts(8), [{"type": "text", "text": "?"}]),
        ]
        with tempfile.TemporaryDirectory() as root:
            path = synth.write_jsonl(os.path.join(root, "project", "director.jsonl"), records)
            s = tr.read_session(path, root, detail=True)
            turns = report.turns_for(s, set())
            self.assertEqual([t["trigger"] for t in turns],
                             ["operator-typed", "relay-woken", "relay-woken", "other/unknown"])
            self.assertEqual([(t["characters"], t["lines"]) for t in turns], [(7, 2), (4, 1), (5, 1), (1, 1)])
            self.assertEqual([t["decisions"] for t in turns], [2, 0, 0, 0])
            rc, out, err = synth.run_main(report, ["--root", root, "--since", synth.ts(3), "--until", synth.ts(7)])
            self.assertEqual((rc, err), (0, ""))
            self.assertIn("relay-woken: 2 turns, 9 characters, 2 lines", out)
            self.assertIn("operator-typed turns: 0; request_decision calls: 0", out)
            self.assertNotIn("wake</task-notification>", out)

    def test_missing_trigger_and_shared_record_deduplication(self):
        r = synth.assistant("m", synth.ts(0), [
            {"type": "text", "text": "orphan"},
            {"type": "tool_use", "id": "d", "name": synth.TOOL + "request_decision", "input": {}},
        ])
        r["uuid"] = "invented-uuid"
        with tempfile.TemporaryDirectory() as root:
            path = synth.write_jsonl(os.path.join(root, "project", "a.jsonl"), [r])
            other = synth.write_jsonl(os.path.join(root, "project", "b.jsonl"), [r])
            seen = set()
            a = report.turns_for(tr.read_session(path, root, detail=True), seen)
            b = report.turns_for(tr.read_session(other, root, detail=True), seen)
            self.assertEqual((a[0]["trigger"], a[0]["characters"], a[0]["decisions"]), ("other/unknown", 6, 1))
            self.assertEqual(b, [])

    def test_invalid_window(self):
        rc, _, err = synth.run_main(report, ["--since", "bad"])
        self.assertEqual(rc, 2)
        self.assertIn("ISO-8601", err)
