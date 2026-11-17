"""consult_report.py on synthetic transcripts and rollouts: what a consultation is, which
calls are not counted, how a day is cut, and that nothing of a conversation is printed."""
import os
import re
import unittest

import synth
from synth import TOOL, call_lines, run_main, tool_result, usage, write_jsonl

import consult_report as cr

# Invented moments. 2025-03-10 14:59:59Z is the last second of 10 March in JST (UTC+9);
# 15:00:00Z is the first second of 11 March there.
LAST_JST_SECOND = "2025-03-10T14:59:59.000Z"
FIRST_NEXT_JST = "2025-03-10T15:00:00.000Z"
SECRET = "PRIVATE-QUESTION-TEXT"


def consult_input(kind, question=SECRET):
    return {"kind": kind, "question": question, "my_case": "c", "settled": "s", "unsettled": "u", "blind_spot": "b"}


def director_file(proj, name, calls):
    """A Director session. calls: (when, tool, tool_use id, input, result) where result is
    None (no result in the file), "ok", or "error"."""
    recs = []
    for n, (when, tool, tid, inp, result) in enumerate(calls):
        # the message id comes from the call, not the file: a resumed file repeats the same lines
        recs += call_lines(f"m{tid}", when, usage(), tools=[(TOOL + tool, tid, inp)])
        if result == "ok":
            recs.append(tool_result(when, tid, "done", is_error=False))
        elif result == "error":
            recs.append(tool_result(when, tid, "refused", is_error=True))
    return write_jsonl(os.path.join(proj, name + ".jsonl"), recs)


def codex_end(when, tool, call_id, arguments, result, server="banto"):
    return {"timestamp": when, "type": "event_msg",
            "payload": {"type": "mcp_tool_call_end", "call_id": call_id,
                        "invocation": {"server": server, "tool": tool, "arguments": arguments},
                        "duration": {"secs": 0, "nanos": 1}, "result": result}}


def codex_ok(is_error=False):
    return {"Ok": {"content": [{"type": "text", "text": "x"}], "isError": is_error}}


class Rig(unittest.TestCase):
    def setUp(self):
        self.root, self.proj = synth.make_root("consult-")
        self.codex = os.path.join(self.root, "codex-sessions")
        os.makedirs(self.codex)
        self.addCleanup(synth.rmtree, self.root)

    def run_report(self, *flags, codex=True):
        argv = ["--root", self.root, "--utc-offset", "+09:00"]
        argv += ["--codex-root", self.codex] if codex else ["--no-codex"]
        rc, out, err = run_main(cr, argv + list(flags))
        self.assertEqual(rc, 0, err)
        return out

    def rows(self, out):
        """The per-session table as (day, agent, session, arb, design, other, dismissed, refused,
        nolink, session total)."""
        lines = out.split("PER DIRECTOR SESSION PER DAY")[1].split("PER DAY, ALL DIRECTORS")[0].splitlines()
        rows = []
        for line in lines:
            parts = line.split()
            if len(parts) == 10 and re.fullmatch(r"\d{4}-\d\d-\d\d|\?", parts[0]):
                rows.append((parts[0], parts[1], parts[2]) + tuple(int(x) for x in parts[3:]))
        return rows


class ClaudeTests(Rig):
    def test_counts_by_kind_and_a_refused_call_is_not_a_consultation(self):
        director_file(self.proj, "aaaaaaaa-0000", [
            (synth.ts(1), "consult_goinkyo", "t1", consult_input("arbitration"), "ok"),
            (synth.ts(2), "consult_goinkyo", "t2", consult_input("design"), "ok"),
            (synth.ts(3), "consult_goinkyo", "t3", consult_input("design"), "error"),
            (synth.ts(4), "dismiss_goinkyo", "t4", {}, "ok"),
            (synth.ts(5), "dismiss_goinkyo", "t5", {}, "error"),
        ])
        self.assertEqual(self.rows(self.run_report()), [("2025-03-10", "claude", "aaaaaaaa", 1, 1, 0, 1, 2, 0, 2)])

    def test_a_call_with_no_result_is_counted_and_marked(self):
        director_file(self.proj, "bbbbbbbb-0000", [
            (synth.ts(1), "consult_goinkyo", "t1", consult_input("design"), None),
            (synth.ts(2), "dismiss_goinkyo", "t2", {}, None),
        ])
        self.assertEqual(self.rows(self.run_report()), [("2025-03-10", "claude", "bbbbbbbb", 0, 1, 0, 1, 0, 2, 1)])

    def test_a_consultation_whose_kind_is_neither_is_counted_as_other(self):
        director_file(self.proj, "cccccccc-0000", [
            (synth.ts(1), "consult_goinkyo", "t1", {"question": "q"}, "ok"),
            (synth.ts(2), "consult_goinkyo", "t2", {"kind": 7}, "ok"),
        ])
        self.assertEqual(self.rows(self.run_report()), [("2025-03-10", "claude", "cccccccc", 0, 0, 2, 0, 0, 0, 2)])

    def test_the_day_is_cut_in_the_zone_asked_for(self):
        director_file(self.proj, "dddddddd-0000", [
            (LAST_JST_SECOND, "consult_goinkyo", "t1", consult_input("arbitration"), "ok"),
            (FIRST_NEXT_JST, "consult_goinkyo", "t2", consult_input("design"), "ok"),
        ])
        jst = self.rows(self.run_report())
        self.assertEqual([(r[0], r[3], r[4]) for r in jst], [("2025-03-10", 1, 0), ("2025-03-11", 0, 1)])
        self.assertEqual({r[9] for r in jst}, {2})              # the session total is the same on both days
        utc = self.rows(self.run_report("--utc-offset", "+00:00"))
        self.assertEqual([(r[0], r[3], r[4]) for r in utc], [("2025-03-10", 1, 1)])

    def test_a_day_with_activity_and_no_consultation_is_a_row_of_zeros(self):
        director_file(self.proj, "eeeeeeee-0000", [
            (synth.ts(1, day=10), "request_decision", "t1", {}, "ok"),
            (synth.ts(1, day=11), "consult_goinkyo", "t2", consult_input("design"), "ok"),
            (synth.ts(1, day=12), "name_member", "t3", {}, "ok"),
        ])
        got = [(r[0], r[3] + r[4]) for r in self.rows(self.run_report())]
        self.assertEqual(got, [("2025-03-10", 0), ("2025-03-11", 1), ("2025-03-12", 0)])

    def test_a_session_with_no_director_only_tool_is_not_listed(self):
        recs = call_lines("w0", synth.ts(1), usage(), tools=[(TOOL + "check_messages", "c1", {})])
        write_jsonl(os.path.join(self.proj, "ffffffff-0000.jsonl"), recs)
        out = self.run_report()
        self.assertEqual(self.rows(out), [])
        self.assertIn("no Director session had a record in the window", out)

    def test_a_subagent_file_is_left_out(self):
        sub = os.path.join(self.proj, "parent", "subagents")
        director_file(sub, "agent-1", [(synth.ts(1), "consult_goinkyo", "t1", consult_input("design"), "ok")])
        self.assertEqual(self.rows(self.run_report()), [])

    def test_a_call_repeated_in_a_resumed_file_is_counted_once_in_the_earlier_file(self):
        calls = [(synth.ts(1), "consult_goinkyo", "t1", consult_input("design"), "ok")]
        director_file(self.proj, "11111111-0000", calls)
        # the later file repeats the call (same tool_use id and record content) and files one more
        later = calls + [(synth.ts(30), "consult_goinkyo", "t2", consult_input("arbitration"), "ok")]
        director_file(self.proj, "22222222-0000", later)
        out = self.run_report()
        by_session = {r[2]: (r[3], r[4], r[9]) for r in self.rows(out)}
        self.assertEqual(by_session["11111111"], (0, 1, 1))
        self.assertEqual(by_session["22222222"], (1, 0, 1))
        self.assertIn("copies dropped: 1 calls, 3 records", out)

    def test_a_record_repeated_by_uuid_is_dropped_even_when_its_line_differs(self):
        def lines(mid):
            recs = call_lines(mid, synth.ts(1), usage(), tools=[(TOOL + "consult_goinkyo", "t1", consult_input("design"))])
            for i, rec in enumerate(recs):
                rec["uuid"] = f"u{i}"
            return recs
        write_jsonl(os.path.join(self.proj, "11111111-0001.jsonl"), lines("mA"))
        write_jsonl(os.path.join(self.proj, "22222222-0001.jsonl"), lines("mB") + [tool_result(synth.ts(5), "t1", "ok", is_error=False)])
        out = self.run_report()
        self.assertIn("copies dropped: 1 calls, 2 records", out)
        self.assertEqual(sum(r[4] for r in self.rows(out)), 1)

    def test_a_result_in_a_later_file_with_no_director_only_call_still_links_to_its_call(self):
        director_file(self.proj, "33333333-0000", [(synth.ts(1), "consult_goinkyo", "t1", consult_input("design"), None)])
        # the later file is a plain session: a refusal and a call of a tool every member has
        later = [tool_result(synth.ts(2), "t1", "no", is_error=True)]
        later += call_lines("x", synth.ts(3), usage(), tools=[(TOOL + "check_messages", "t9", {})])
        write_jsonl(os.path.join(self.proj, "44444444-0000.jsonl"), later)
        got = self.rows(self.run_report())
        self.assertEqual([(r[2], r[4], r[7], r[8], r[9]) for r in got], [("33333333", 0, 1, 0, 0)])

    def test_a_non_error_result_in_a_later_file_links_the_call(self):
        director_file(self.proj, "33333333-0001", [(synth.ts(1), "consult_goinkyo", "t1", consult_input("design"), None)])
        write_jsonl(os.path.join(self.proj, "44444444-0001.jsonl"), [tool_result(synth.ts(2), "t1", "filed", is_error=False)])
        self.assertEqual([(r[4], r[7], r[8]) for r in self.rows(self.run_report())], [(1, 0, 0)])

    def test_the_ends_of_the_calendar_do_not_stop_the_run(self):
        moments = ["0001-01-01T00:00:00Z", "1960-01-01T00:00:00Z", "9999-12-31T23:59:59Z", synth.ts(1)]
        director_file(self.proj, "99999999-0000", [(m, "consult_goinkyo", f"t{n}", consult_input("design"), "ok")
                                                   for n, m in enumerate(moments)])
        for offset in ("+09:00", "-05:00", "+00:00", "local"):
            rows = self.rows(self.run_report(f"--utc-offset={offset}"))
            self.assertEqual(sum(r[4] for r in rows), 4, offset)             # every call is in some row
        by_day = {r[0]: r[4] for r in self.rows(self.run_report("--utc-offset=-05:00"))}
        self.assertEqual(by_day["?"], 1)                                     # 0001-01-01 minus 5 h underflows
        by_day = {r[0]: r[4] for r in self.rows(self.run_report("--utc-offset", "+09:00"))}
        self.assertEqual(by_day["?"], 1)                                     # 9999-12-31 23:59:59 plus 9 h overflows

    def test_an_until_at_the_start_of_the_calendar_does_not_stop_a_local_run(self):
        director_file(self.proj, "aaaaaaaa-1111", [(synth.ts(1), "consult_goinkyo", "t1", consult_input("design"), "ok")])
        rc, out, err = run_main(cr, ["--root", self.root, "--no-codex", "--until", "0002-01-01"])
        self.assertEqual(rc, 0, err)
        self.assertIn("days: the machine's own zone", out)

    def test_a_call_with_no_timestamp_is_in_the_question_mark_row_even_inside_a_window(self):
        recs = call_lines("mq1", None, usage(), tools=[(TOOL + "consult_goinkyo", "t1", consult_input("design"))])
        for rec in recs:
            rec.pop("timestamp", None)
        recs.append(tool_result(synth.ts(1), "t1", "ok", is_error=False))
        recs += call_lines("mq2", synth.ts(1), usage(), tools=[(TOOL + "consult_goinkyo", "t2", consult_input("arbitration"))])
        recs.append(tool_result(synth.ts(2), "t2", "ok", is_error=False))
        write_jsonl(os.path.join(self.proj, "bbbbbbbb-1111.jsonl"), recs)
        for flags in ((), ("--since", "2025-03-10", "--until", "2025-03-11")):
            got = {r[0]: (r[3], r[4], r[9]) for r in self.rows(self.run_report(*flags))}
            self.assertEqual(got, {"2025-03-10": (1, 0, 2), "?": (0, 1, 2)}, flags)

    def test_the_default_zone_is_the_machines_own(self):
        director_file(self.proj, "cccccccc-1111", [("2025-03-10T12:00:00Z", "consult_goinkyo", "t1", consult_input("design"), "ok")])
        rc, out, err = run_main(cr, ["--root", self.root, "--no-codex"])
        self.assertEqual(rc, 0, err)
        self.assertIn("days: the machine's own zone", out)
        rows = self.rows(out)
        self.assertEqual([r[4] for r in rows], [1])
        self.assertIn(rows[0][0], ("2025-03-10", "2025-03-09", "2025-03-11"))    # 12:00Z is within a day of 10 March in any zone

    def test_a_negative_offset_cuts_the_day_the_other_way(self):
        director_file(self.proj, "dddddddd-1111", [
            ("2025-03-10T04:59:59Z", "consult_goinkyo", "t1", consult_input("design"), "ok"),       # 23:59:59 on the 9th at -05:00
            ("2025-03-10T05:00:00Z", "consult_goinkyo", "t2", consult_input("arbitration"), "ok"),  # 00:00:00 on the 10th
        ])
        got = [(r[0], r[3], r[4]) for r in self.rows(self.run_report("--utc-offset=-05:00"))]
        self.assertEqual(got, [("2025-03-09", 0, 1), ("2025-03-10", 1, 0)])

    def test_since_is_inclusive_and_until_is_exclusive_and_both_are_utc_instants(self):
        director_file(self.proj, "eeeeeeee-1111", [
            ("2025-03-10T10:00:00Z", "consult_goinkyo", "t1", consult_input("design"), "ok"),
            ("2025-03-10T11:00:00Z", "consult_goinkyo", "t2", consult_input("design"), "ok"),
            ("2025-03-10T12:00:00Z", "consult_goinkyo", "t3", consult_input("design"), "ok"),
        ])
        got = self.rows(self.run_report("--since", "2025-03-10T10:00:00Z", "--until", "2025-03-10T12:00:00Z"))
        self.assertEqual(sum(r[4] for r in got), 2)                      # the 10:00 call in, the 12:00 call out
        jst_day = self.run_report("--since", "2025-03-09T15:00:00Z", "--until", "2025-03-10T15:00:00Z")
        self.assertEqual(sum(r[4] for r in self.rows(jst_day)), 3)       # the JST day of 10 March, bounds in UTC
        none = self.run_report("--since", "2025-03-10T12:00:01Z")
        self.assertEqual(sum(r[4] for r in self.rows(none)), 0)

    def test_a_resumed_file_keeps_the_days_of_its_own_records_even_with_no_consultation(self):
        first = [(synth.ts(1, day=10), "consult_goinkyo", "t1", consult_input("design"), "ok")]
        director_file(self.proj, "00000000-a", first)
        director_file(self.proj, "ffffffff-a", first + [
            (synth.ts(1, day=12), "request_decision", "t2", {}, "ok"),
            (synth.ts(1, day=13), "request_decision", "t3", {}, "ok"),
        ])
        out = self.run_report()
        self.assertEqual([(r[0], r[2], r[4], r[9]) for r in self.rows(out)],
                         [("2025-03-10", "00000000", 1, 1), ("2025-03-12", "ffffffff", 0, 0), ("2025-03-13", "ffffffff", 0, 0)])
        self.assertIn("copies dropped: 1 calls", out)

    def test_a_file_that_is_only_a_copy_of_an_earlier_one_shows_no_row_and_no_director(self):
        calls = [(synth.ts(1, day=10), "consult_goinkyo", "t1", consult_input("design"), "ok")]
        director_file(self.proj, "00000000-1111", calls)
        director_file(self.proj, "ffffffff-1111", calls)       # same moments, so the path breaks the tie
        out = self.run_report()
        self.assertEqual([(r[0], r[2], r[4]) for r in self.rows(out)], [("2025-03-10", "00000000", 1)])
        per_day = out.split("PER DAY, ALL DIRECTORS")[1].split("WHAT THIS CANNOT SEE")[0]
        self.assertRegex(per_day, r"2025-03-10\s+1\s+0\s+1\b")

    def test_the_window_limits_the_rows_but_not_the_session_total(self):
        director_file(self.proj, "55555555-0000", [
            (synth.ts(1, day=10), "consult_goinkyo", "t1", consult_input("design"), "ok"),
            (synth.ts(1, day=12), "consult_goinkyo", "t2", consult_input("design"), "ok"),
        ])
        got = self.rows(self.run_report("--since", "2025-03-11", "--until", "2025-03-13"))
        self.assertEqual([(r[0], r[4], r[9]) for r in got], [("2025-03-12", 1, 2)])

    def test_no_conversation_text_reaches_the_output(self):
        director_file(self.proj, "66666666-0000", [(synth.ts(1), "consult_goinkyo", "t1", consult_input("design"), "ok")])
        self.assertNotIn(SECRET, self.run_report())

    def test_a_broken_line_is_skipped_not_fatal(self):
        path = director_file(self.proj, "77777777-0000", [(synth.ts(1), "consult_goinkyo", "t1", consult_input("design"), "ok")])
        with open(path, "a", encoding="utf-8") as fh:
            fh.write("{not json\n[1,2]\n")
        out = self.run_report()
        self.assertEqual(len(self.rows(out)), 1)
        self.assertIn("SKIPPED (broken) 2", out)

    def test_spaced_json_is_read_like_compact_json(self):
        recs = call_lines("m0", synth.ts(1), usage(), tools=[(TOOL + "consult_goinkyo", "t1", consult_input("design"))])
        recs.append(tool_result(synth.ts(2), "t1", "ok", is_error=False))
        write_jsonl(os.path.join(self.proj, "88888888-0000.jsonl"), recs, spaced=True)
        self.assertEqual(sum(r[4] for r in self.rows(self.run_report())), 1)


class CodexTests(Rig):
    def rollout(self, ident, records):
        return write_jsonl(os.path.join(self.codex, "2025", "03", "10", f"rollout-2025-03-10T01-00-00-{ident}.jsonl"), records)

    def test_calls_are_read_from_mcp_tool_call_end_records(self):
        self.rollout("99999999-aaaa-bbbb-cccc-000000000001", [
            codex_end(synth.ts(1), "consult_goinkyo", "k1", consult_input("arbitration"), codex_ok()),
            codex_end(synth.ts(2), "consult_goinkyo", "k2", consult_input("design"), codex_ok()),
            codex_end(synth.ts(3), "consult_goinkyo", "k3", consult_input("design"), codex_ok(is_error=True)),
            codex_end(synth.ts(4), "consult_goinkyo", "k4", consult_input("design"), {"Err": "transport closed"}),
            codex_end(synth.ts(5), "dismiss_goinkyo", "k5", {}, codex_ok()),
            codex_end(synth.ts(6), "consult_goinkyo", "k6", consult_input("design"), {"Weird": 1}),
        ])
        # arbitration 1, design 2 (k2 and the unlinked k6), dismissed 1, refused 2 (k3, k4), nolink 1, session 3
        self.assertEqual(self.rows(self.run_report()), [("2025-03-10", "codex", "99999999", 1, 2, 0, 1, 2, 1, 3)])

    def test_arguments_given_as_a_json_string_are_read(self):
        self.rollout("99999999-aaaa-bbbb-cccc-000000000002",
                     [codex_end(synth.ts(1), "consult_goinkyo", "k1", '{"kind": "arbitration"}', codex_ok())])
        self.assertEqual(sum(r[3] for r in self.rows(self.run_report())), 1)

    def test_another_server_or_another_record_type_is_not_a_consultation(self):
        self.rollout("99999999-aaaa-bbbb-cccc-000000000003", [
            codex_end(synth.ts(1), "consult_goinkyo", "k1", consult_input("design"), codex_ok(), server="other"),
            {"timestamp": synth.ts(2), "type": "response_item",
             "payload": {"type": "function_call", "name": "consult_goinkyo", "arguments": "{}", "call_id": "k2"}},
            codex_end(synth.ts(3), "check_messages", "k3", {}, codex_ok()),
        ])
        self.assertEqual(self.rows(self.run_report()), [])

    def test_a_codex_director_with_only_other_director_tools_is_a_row_of_zeros(self):
        self.rollout("99999999-aaaa-bbbb-cccc-000000000004", [codex_end(synth.ts(1), "request_decision", "k1", {}, codex_ok())])
        self.assertEqual(self.rows(self.run_report()), [("2025-03-10", "codex", "99999999", 0, 0, 0, 0, 0, 0, 0)])

    def test_the_day_is_cut_in_the_zone_asked_for(self):
        self.rollout("99999999-aaaa-bbbb-cccc-000000000005", [
            codex_end(LAST_JST_SECOND, "consult_goinkyo", "k1", consult_input("design"), codex_ok()),
            codex_end(FIRST_NEXT_JST, "consult_goinkyo", "k2", consult_input("design"), codex_ok()),
        ])
        self.assertEqual([(r[0], r[4]) for r in self.rows(self.run_report())], [("2025-03-10", 1), ("2025-03-11", 1)])

    def test_no_codex_leaves_the_rollouts_unread(self):
        self.rollout("99999999-aaaa-bbbb-cccc-000000000006",
                     [codex_end(synth.ts(1), "consult_goinkyo", "k1", consult_input("design"), codex_ok())])
        self.assertEqual(self.rows(self.run_report(codex=False)), [])


class FlagTests(unittest.TestCase):
    def test_offsets(self):
        self.assertEqual(cr.parse_offset("+09:00").utcoffset(None).total_seconds(), 9 * 3600)
        self.assertEqual(cr.parse_offset("-05:30").utcoffset(None).total_seconds(), -(5 * 3600 + 1800))
        self.assertEqual(cr.parse_offset("+9").utcoffset(None).total_seconds(), 9 * 3600)
        self.assertEqual(cr.parse_offset("local"), "local")
        for bad in ("", "JST", "+15:00", "+09:60", "09:00", None):
            self.assertIsNone(cr.parse_offset(bad), bad)

    def test_bad_flags_are_refused_with_exit_2(self):
        for argv in (["--utc-offset", "JST", "--no-codex"], ["--since", "yesterday", "--no-codex"]):
            rc, _out, err = run_main(cr, argv)
            self.assertEqual(rc, 2, argv)
            self.assertTrue(err)


if __name__ == "__main__":
    unittest.main()
