"""Phase 3 metrics on invented transcripts."""
import os
import tempfile
import unittest

import synth
import plan_metrics as pm


def use(mid, minute, name, ident, args=None, text=""):
    return synth.assistant(mid, synth.ts(minute), [
        {"type": "text", "text": text},
        {"type": "tool_use", "id": ident, "name": synth.TOOL + name, "input": args or {}},
    ], u=synth.usage(out=7))


class PlanMetrics(unittest.TestCase):
    def test_all_metrics_and_api_blob_exclusion(self):
        with tempfile.TemporaryDirectory() as root:
            records = [
                synth.prompt_snapshot(synth.ts(0), "You are the Director of banto brigade 9."),
                synth.user_text(synth.ts(0), "initial"),
                use("m1", 1, "request_decision", "d1"),
                synth.tool_result(synth.ts(2), "d1", "Recorded"),
                synth.user_text(synth.ts(3), "answer"),
                synth.user_text(synth.ts(4), "another intervention"),
                use("m2", 5, "request_decision", "d2"),
                synth.tool_result(synth.ts(6), "d2", "refused", is_error=True),
                synth.user_text(synth.ts(7), "not an answer"),
                synth.user_text(synth.ts(8), "[banto relay] wake"),
                use("m3", 9, "check_messages", "c1", text="hello"),
                synth.tool_result(synth.ts(10), "c1", synth.delivery(
                    ("worker-1 — to you", "synthetic report"), ("worker-2 — to you", "another"), recipient="director")),
                synth.tool_result(synth.ts(10), "c1", synth.delivery(
                    ("worker-1 — to you", "synthetic report"), ("worker-2 — to you", "another"), recipient="director")),
                synth.assistant("m4", synth.ts(11), [{"type": "text", "text": "done"}], u=synth.usage(out=3)),
                use("m5", 12, "update_progress", "p1", {"stage": "work", "off_plan": ["one", "two"]}),
                synth.tool_result(synth.ts(13), "p1", "Recorded"),
                use("m6", 14, "update_progress", "p2", {"stage": "work", "off_plan": ["ignored"]}),
                synth.tool_result(synth.ts(15), "p2", "refused", is_error=True),
                synth.user_text(synth.ts(16), "[banto relay] notice"),
                use("m7", 17, "check_messages", "c2"),
                synth.tool_result(synth.ts(18), "c2", synth.delivery(
                    ("banto — to you", "The operator approved Plan 3, version 1, with the key"), recipient="director")),
                synth.user_text(synth.ts(19), "[banto relay] notice"),
                use("m8", 20, "check_messages", "c3"),
                synth.tool_result(synth.ts(21), "c3", synth.delivery(
                    ("banto — to you", "The Planner released Plan 3, version 2 — split"), recipient="director")),
                synth.user_text(synth.ts(22), "[banto relay] notice"),
                use("m9", 23, "check_messages", "c4"),
                synth.tool_result(synth.ts(24), "c4", synth.delivery(
                    ("banto — to you", "The operator approved Plan 3, version 3, with the key"),
                    ("banto — to you", "The operator approved Plan 3, version 3, with the key"), recipient="director")),
                {"type": "api-request-blob", "message": {"content": [{"type": "tool_result", "content": "fake"}]}},
            ]
            synth.write_jsonl(os.path.join(root, "p", "director.jsonl"), records)
            planner = [synth.prompt_snapshot(synth.ts(0), pm.tr.PLANNER_PROMPT_MARKERS[1]),
                       synth.user_text(synth.ts(1), "planner input")]
            synth.write_jsonl(os.path.join(root, "p", "planner.jsonl"), planner)
            rc, out, err = synth.run_main(pm, ["--root", root])
            self.assertEqual((rc, err), (0, ""))
            self.assertIn("operator: 1 answers, 3 own step-ins; 2 Worker reports; 1 active UTC days; Planner inputs 1", out)
            self.assertIn("2 reports, 24 output tokens, 9 pane characters; tokens/report 12.00", out)
            self.assertIn("brigade 9: off-plan items 2", out)
            self.assertNotIn("off-plan items 1", out)
            self.assertIn("Plan 3: operator-approved revisions 1, Planner releases 1", out)
            self.assertNotIn("synthetic report", out)
            rc, out, err = synth.run_main(pm, ["--root", root, "--since", synth.ts(19)])
            self.assertEqual((rc, err), (0, ""))
            self.assertIn("Plan 3: operator-approved revisions 1, Planner releases 1", out)

    def test_operator_turn_reports_and_unknown_results(self):
        with tempfile.TemporaryDirectory() as root:
            records = [synth.user_text(synth.ts(0), "work"), use("a", 1, "check_messages", "c"),
                       synth.tool_result(synth.ts(2), "c", synth.delivery(("worker-1 — to you", "done"))),
                       use("b", 3, "update_progress", "p", {"stage": "stage", "off_plan": ["x"]}),
                       synth.user_text(synth.ts(4), "next")]
            synth.write_jsonl(os.path.join(root, "p", "d.jsonl"), records)
            rc, out, err = synth.run_main(pm, ["--root", root])
            self.assertEqual((rc, err), (0, ""))
            self.assertIn("reports in operator turns 1", out)
            self.assertIn("tokens/report N/A", out)
            self.assertNotIn("off-plan items 1", out)

    def test_successful_close_and_progress_are_interleaved(self):
        with tempfile.TemporaryDirectory() as root:
            records = [synth.user_text(synth.ts(0), "work"),
                       use("a", 1, "update_progress", "p", {"stage": "a", "off_plan": []}),
                       synth.tool_result(synth.ts(2), "p", "ok"),
                       use("b", 3, "close_planner", "c"), synth.tool_result(synth.ts(4), "c", "ok")]
            synth.write_jsonl(os.path.join(root, "p", "d.jsonl"), records)
            rc, out, err = synth.run_main(pm, ["--root", root])
            self.assertEqual((rc, err), (0, ""))
            self.assertLess(out.index("off-plan items 0"), out.index("Director close_planner call"))

    def test_suggestion_sources_are_counted_as_operator_inputs(self):
        with tempfile.TemporaryDirectory() as root:
            records = [synth.user_text(synth.ts(0), "initial", promptSource="typed"),
                       synth.user_text(synth.ts(1), "suggested", promptSource="suggestion_accepted"),
                       use("m", 2, "request_decision", "d"),
                       synth.tool_result(synth.ts(3), "d", "ok"),
                       synth.user_text(synth.ts(4), "answer", promptSource="suggestion_accepted"),
                       synth.user_text(synth.ts(5), "queued", promptSource="queued")]
            synth.write_jsonl(os.path.join(root, "p", "d.jsonl"), records)
            rc, out, err = synth.run_main(pm, ["--root", root])
            self.assertEqual((rc, err), (0, ""))
            self.assertIn("operator: 1 answers, 3 own step-ins", out)
            self.assertIn("suggestion-accepted operator turns: 2; answers 1, own step-ins 1", out)

    def test_planner_brigade_from_snapshot(self):
        with tempfile.TemporaryDirectory() as root:
            prompt = "You are the Planner of banto brigade 19. " + pm.tr.PLANNER_PROMPT_MARKERS[1]
            synth.write_jsonl(os.path.join(root, "p", "planner.jsonl"),
                              [synth.prompt_snapshot(synth.ts(0), prompt), synth.user_text(synth.ts(1), "work")])
            rc, out, err = synth.run_main(pm, ["--root", root])
            self.assertEqual((rc, err), (0, ""))
            self.assertIn("role planner brigade 19", out)

    def test_approval_and_release_for_same_version_both_count(self):
        with tempfile.TemporaryDirectory() as root:
            records = [synth.prompt_snapshot(synth.ts(0), "You are the Director of banto brigade 9."),
                       synth.user_text(synth.ts(0), "work"),
                       synth.user_text(synth.ts(0), "[banto relay] wake"),
                       use("progress", 0, "update_progress", "p"),
                       use("m", 1, "check_messages", "c"),
                       synth.tool_result(synth.ts(2), "c", synth.delivery(
                           ("banto — to you", "The operator approved Plan 3, version 1, with the key"),
                           ("banto — to you", "The Planner released Plan 3, version 1 — split")))]
            synth.write_jsonl(os.path.join(root, "p", "d.jsonl"), records)
            rc, out, err = synth.run_main(pm, ["--root", root])
            self.assertEqual((rc, err), (0, ""))
            self.assertIn("Plan 3 version 1 approval", out)
            self.assertIn("Plan 3 version 1 release", out)
            self.assertIn("Plan 3: operator-approved revisions 0, Planner releases 1", out)

    def test_close_call_and_operator_notice_have_distinct_labels(self):
        with tempfile.TemporaryDirectory() as root:
            records = [synth.user_text(synth.ts(0), "work"),
                       use("m1", 1, "close_planner", "close"),
                       synth.tool_result(synth.ts(2), "close", "ok"),
                       use("m2", 3, "check_messages", "check"),
                       synth.tool_result(synth.ts(4), "check", synth.delivery(
                           ("banto — to you", "The operator closed Plan 3.")))]
            synth.write_jsonl(os.path.join(root, "p", "d.jsonl"), records)
            rc, out, err = synth.run_main(pm, ["--root", root])
            self.assertEqual((rc, err), (0, ""))
            self.assertIn("Director close_planner call", out)
            self.assertIn("operator closed Plan 3", out)

    def test_resumed_prefix_replays_successful_request_before_new_answer(self):
        with tempfile.TemporaryDirectory() as root:
            prefix = [synth.user_text(synth.ts(0), "initial"),
                      use("m", 1, "request_decision", "d"),
                      synth.tool_result(synth.ts(2), "d", "ok")]
            for i, record in enumerate(prefix):
                record["uuid"] = f"prefix-{i}"
            answer = synth.user_text(synth.ts(3), "answer", uuid="answer")
            synth.write_jsonl(os.path.join(root, "p", "a.jsonl"), prefix)
            synth.write_jsonl(os.path.join(root, "p", "b.jsonl"), prefix + [answer])
            rc, out, err = synth.run_main(pm, ["--root", root])
            self.assertEqual((rc, err), (0, ""))
            self.assertIn("operator: 1 answers, 1 own step-ins", out)

    def test_resumed_prefix_replays_pending_checks_and_progress(self):
        with tempfile.TemporaryDirectory() as root:
            prefix = [synth.user_text(synth.ts(0), "[banto relay] wake"),
                      use("m1", 1, "check_messages", "c"),
                      use("m2", 2, "update_progress", "p", {"stage": "work", "off_plan": ["x"]})]
            for i, record in enumerate(prefix):
                record["uuid"] = f"prefix-{i}"
            results = [synth.tool_result(synth.ts(3), "c", synth.delivery(("worker-1 — to you", "done"))),
                       synth.tool_result(synth.ts(4), "p", "ok")]
            for i, record in enumerate(results):
                record["uuid"] = f"result-{i}"
            synth.write_jsonl(os.path.join(root, "p", "a.jsonl"), prefix)
            synth.write_jsonl(os.path.join(root, "p", "b.jsonl"), prefix + results)
            rc, out, err = synth.run_main(pm, ["--root", root])
            self.assertEqual((rc, err), (0, ""))
            self.assertIn("1 Worker reports", out)
            self.assertIn("1 reports, 14 output tokens", out)
            self.assertEqual(out.count("off-plan items 1"), 1)
