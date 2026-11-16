"""quality_report's effect metrics: what review caught, what escaped it, who triggered a Director
reversal, lead time, parallelism and the operator's effort. Synthetic data only: every id, text and
date below is invented."""
import json
import os
import re
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock
from collections import defaultdict
from datetime import datetime, timedelta, timezone

import synth
from synth import TOOL, call_lines, delivery, run_main, usage, user_text, write_jsonl

import quality_report as qr
import transcripts as tr

UTC = timezone.utc
ID_RX = re.compile(r"\b[AB]-[0-9a-f]{8}\b")


def at(h, m, s=0):
    return f"2025-03-10T{h:02d}:{m:02d}:{s:02d}.000Z"


def when(h, m, s=0):
    return datetime(2025, 3, 10, h, m, s, tzinfo=UTC)


class CandidateIds(unittest.TestCase):
    def test_an_id_is_the_same_on_every_call_and_differs_with_each_input(self):
        t = when(10, 0, 5)
        a = qr.candidate_id("sess-1", t, "a")
        self.assertEqual(a, qr.candidate_id("sess-1", t, "a"))
        self.assertRegex(a, r"^A-[0-9a-f]{8}$")
        self.assertRegex(qr.candidate_id("sess-1", t, "b-text"), r"^B-[0-9a-f]{8}$")
        others = {qr.candidate_id("sess-2", t, "a"), qr.candidate_id("sess-1", when(10, 0, 6), "a"),
                  qr.candidate_id("sess-1", t, "a", 1), qr.candidate_id("sess-1", t, "b-text"),
                  qr.candidate_id("sess-1", t, "b-send")}
        self.assertNotIn(a, others)
        self.assertEqual(len(others), 5)

    def test_the_microsecond_is_part_of_the_record_timestamp(self):
        self.assertNotEqual(qr.candidate_id("s", when(10, 0, 5), "a"), qr.candidate_id("s", when(10, 0, 5).replace(microsecond=1), "a"))


class CandidateOrdinals(unittest.TestCase):
    """An id must not depend on which word matched: only on the session, the timestamp, the kind and
    the position among same-kind records sharing that timestamp, matching or not."""

    def setUp(self):
        self.t = when(10, 5)
        self.changes = qr.group_changes([{"hash": "abc1234", "ts": when(10, 30), "subject": "feat: x", "files": ["a"]}], when(9, 0), 15)
        for ch in self.changes:
            ch["reviews"] = []
        self.release = [(when(10, 4), "release build (Director)")]

    def find(self, texts):
        evs = [("operator", self.t, x) for x in texts]
        a, _b = qr.find_candidates(self.changes, when(9, 0), evs, self.release, 12, "sess")
        return [x[4] for x in a[1]]

    def test_the_id_follows_the_position_not_the_matching_word_or_its_neighbours(self):
        want = qr.candidate_id("sess", self.t, "a", 1)
        self.assertEqual(self.find(["all fine", "it is broken"]), [want])
        self.assertEqual(self.find(["all fine", "it is wrong"]), [want])
        self.assertEqual(self.find(["it is a bug", "it is wrong"]), [qr.candidate_id("sess", self.t, "a", 0), want])

    def test_the_ids_of_retraction_candidates_follow_the_same_rule(self):
        evs = [("atext", self.t, "all fine"), ("atext", self.t, "I was wrong"), ("send", self.t, "w", "scratch that")]
        _a, b = qr.find_candidates(self.changes, when(9, 0), evs, self.release, 12, "sess")
        self.assertEqual([x[3] for x in b[1]], [qr.candidate_id("sess", self.t, "b-text", 1), qr.candidate_id("sess", self.t, "b-send", 0)])


class ProblemWords(unittest.TestCase):
    """The word lists as tuned against a person's first-pass judgments: words that matched no real
    candidate and several 'not' ones are gone, every word a real candidate matched is kept. The
    phrases here are invented."""

    def hits(self, text):
        return bool(qr.PROB_RX.search(qr.NEG_RX.sub("", text)))

    def test_words_judged_noise_no_longer_make_a_candidate(self):
        for removed in ("消え", "鳴ら"):
            self.assertNotIn(removed, qr.PROB_WORDS)
            self.assertFalse(self.hits(f"その{removed}。"), removed)

    def test_the_words_a_real_candidate_matched_are_still_there(self):
        for kept in ("出て", "出る", "問題", "まだ", "動いて", "壊れ", "崩れ"):
            self.assertIn(kept, qr.PROB_WORDS)
            self.assertTrue(self.hits(f"その{kept}。"), kept)

    def test_the_word_list_and_the_pattern_agree(self):
        self.assertEqual(qr.PROB_RX.pattern, "|".join(qr.PROB_WORDS))
        self.assertEqual(qr.RETRACT_RX.pattern, "|".join(qr.RETRACT_PHRASES))

    def test_retraction_phrases_judged_noise_no_longer_make_a_candidate(self):
        self.assertNotIn("取り消", qr.RETRACT_PHRASES)
        self.assertIsNone(qr.RETRACT_RX.search("その取り消し処理について"))
        self.assertIsNone(qr.RETRACT_RX.search("please ignore the previous summary"))

    def test_retraction_phrases_a_real_candidate_matched_are_still_there(self):
        for text in ("I was wrong about it", "my mistake", "I withdraw that", "訂正します", "誤りでした", "間違いでした", "失礼しました", "a correction follows"):
            self.assertIsNotNone(qr.RETRACT_RX.search(text), text)

    def test_the_english_words_still_match(self):
        for text in ("it is still broken", "that is wrong", "it does not work", "there is a crash"):
            self.assertTrue(self.hits(text), text)

    def test_a_negated_phrase_is_still_no_candidate(self):
        self.assertFalse(self.hits("no problem here"))


class JudgmentParsing(unittest.TestCase):
    def test_good_lines_comments_and_blank_lines(self):
        text = "# a comment\n\nA-0123abcd real  it broke the picker\nB-89abcdef not\n   B-00000001   unsure   \n#A-ffffffff real\n"
        j, warnings = qr.parse_judgments(text)
        self.assertEqual(warnings, [])
        self.assertEqual(j, {"A-0123abcd": ("real", "it broke the picker"), "B-89abcdef": ("not", ""), "B-00000001": ("unsure", "")})

    def test_verdict_and_id_case_do_not_matter(self):
        j, warnings = qr.parse_judgments("a-0123ABCD REAL\n")
        self.assertEqual(warnings, [])
        self.assertEqual(j, {"A-0123abcd": ("real", "")})

    def test_a_malformed_line_is_a_warning_with_its_line_number(self):
        j, warnings = qr.parse_judgments("A-0123abcd real\nA-89abcdef\nA-00000001 maybe it did\nA-00000002 real fine\n")
        self.assertEqual(sorted(j), ["A-00000002", "A-0123abcd"])
        self.assertEqual(len(warnings), 2)
        self.assertIn("line 2", warnings[0])
        self.assertIn("line 3", warnings[1])

    def test_a_repeated_id_keeps_the_last_judgment_and_warns(self):
        j, warnings = qr.parse_judgments("A-0123abcd real\nA-0123abcd not\n")
        self.assertEqual(j, {"A-0123abcd": ("not", "")})
        self.assertEqual(len(warnings), 1)
        self.assertIn("line 2", warnings[0])

    def test_judged_counts_split_a_list_of_ids(self):
        j = {"A-1": ("real", ""), "A-2": ("not", ""), "A-3": ("unsure", ""), "A-4": ("real", "")}
        self.assertEqual(qr.judged_counts(["A-1", "A-2", "A-3", "A-4", "A-5", "A-6"], j),
                         {"real": 2, "not": 1, "unsure": 1, "unjudged": 2})
        self.assertEqual(qr.judged_counts([], j), {"real": 0, "not": 0, "unsure": 0, "unjudged": 0})


class TriggerKinds(unittest.TestCase):
    NUDGE = ("nudge", when(10, 1))
    OPERATOR = ("operator", when(10, 1), "typed")
    FROM_WORKER = ("deliver", when(10, 1), [("worker-2", "rev", "to you", "report")])
    FROM_GOINKYO = ("deliver", when(10, 1), [("goinkyo", None, "to you", "advice")])
    FROM_OPERATOR = ("deliver", when(10, 1), [("operator", None, "to you", "typed elsewhere")])
    FROM_DIRECTOR_ONLY = ("deliver", when(10, 1), [("director", None, "to you", "echo")])
    RESULT = ("result", when(10, 1))
    SYSTEM = ("system", when(10, 1))
    OWN = [("send", when(10, 1, 30), "worker-1", "go"), ("atext", when(10, 1, 40), "thinking")]
    SAY = ("atext", when(10, 2), "I was wrong")

    def trig(self, fn, *events):
        evs = list(events) + [self.SAY]
        return fn(evs, len(evs) - 1)

    def turn(self, *events):
        return self.trig(qr.turn_trigger, *events)

    def prev(self, *events):
        return self.trig(qr.predecessor_trigger, *events)

    # -- by turn: the primary classification
    def test_an_operator_message_starting_the_turn_is_operator(self):
        self.assertEqual(self.turn(self.OPERATOR), "operator")
        self.assertEqual(self.turn(self.FROM_OPERATOR), "operator")

    def test_a_relay_nudge_or_a_delivery_carrying_a_worker_or_goinkyo_message_starting_the_turn_is_worker(self):
        self.assertEqual(self.turn(self.NUDGE), "worker")
        self.assertEqual(self.turn(self.FROM_WORKER), "worker")
        self.assertEqual(self.turn(self.FROM_GOINKYO), "worker")

    def test_tool_results_system_text_and_the_directors_own_events_do_not_end_a_turn(self):
        self.assertEqual(self.turn(self.OPERATOR, self.RESULT, self.SYSTEM, *self.OWN), "operator")
        self.assertEqual(self.turn(self.NUDGE, self.FROM_WORKER, self.RESULT), "worker")
        self.assertEqual(self.turn(self.OPERATOR, self.FROM_DIRECTOR_ONLY), "operator")

    def test_the_most_recent_start_wins(self):
        self.assertEqual(self.turn(self.OPERATOR, self.NUDGE), "worker")
        self.assertEqual(self.turn(self.NUDGE, self.OPERATOR), "operator")

    def test_self_is_what_is_left_when_neither_started_the_turn(self):
        self.assertEqual(self.turn(), "self")
        self.assertEqual(self.turn(self.RESULT, self.SYSTEM), "self")
        self.assertEqual(self.turn(self.FROM_DIRECTOR_ONLY), "self")

    def test_a_correction_the_director_found_itself_still_counts_as_what_started_its_turn(self):
        # the rule cannot see self-correction: only a human reading the candidate can say it was self
        self.assertEqual(self.turn(self.OPERATOR, *self.OWN), "operator")
        self.assertEqual(self.turn(self.NUDGE, self.RESULT, *self.OWN), "worker")

    def test_a_candidate_is_never_its_own_input(self):
        evs = [self.OPERATOR, ("atext", when(10, 2), "I was wrong")]
        self.assertEqual(qr.turn_trigger(evs, 1), "operator")
        self.assertEqual(qr.turn_trigger([("atext", when(10, 2), "I was wrong")], 0), "self")

    # -- by predecessor: the secondary column, the most recent input of any kind
    def test_the_predecessor_is_the_most_recent_input_of_any_kind(self):
        self.assertEqual(self.prev(self.OPERATOR), "operator")
        self.assertEqual(self.prev(self.NUDGE), "worker")
        self.assertEqual(self.prev(self.FROM_WORKER), "worker")
        self.assertEqual(self.prev(self.OPERATOR, self.RESULT), "self")
        self.assertEqual(self.prev(self.RESULT, self.OPERATOR), "operator")
        self.assertEqual(self.prev(self.SYSTEM), "self")
        self.assertEqual(self.prev(self.FROM_DIRECTOR_ONLY), "self")
        self.assertEqual(self.prev(), "self")
        self.assertEqual(self.prev(self.OPERATOR, *self.OWN), "operator")


class OperatorMessageRule(unittest.TestCase):
    """What counts as a message the operator typed to the Director, read through the real parser."""

    def setUp(self):
        self.root, self.proj = synth.make_root()
        self.addCleanup(synth.rmtree, self.root)

    def events(self, records):
        path = write_jsonl(os.path.join(self.proj, "d.jsonl"), records)
        s = tr.read_session(path, self.root, True)
        return qr.window_events(s, when(9, 0), None)

    def test_only_typed_text_counts(self):
        recs = [
            user_text(at(10, 0, 1), "please start"),                                                    # typed
            {"type": "user", "timestamp": at(10, 0, 2), "origin": {"kind": "human"},
             "message": {"role": "user", "content": [{"type": "image", "source": {}}, {"type": "text", "text": "and this picture"}]}},   # typed with an image
            user_text(at(10, 1), "[banto relay] Your brigade peer sent you a message.", origin={"kind": "human"}),   # nudge: origin says human too
            user_text(at(10, 1, 1), "<system-reminder>x</system-reminder>"),
            user_text(at(10, 1, 2), "<local-command-stdout>ok</local-command-stdout>"),
            user_text(at(10, 1, 3), "<task-notification>done</task-notification>", origin={"kind": "task-notification"}),
            user_text(at(10, 1, 4), "[banto] This pane just started as a brigade member."),
            user_text(at(10, 1, 5), "meta", isMeta=True),
            user_text(at(10, 1, 6), "a compaction summary", isCompactSummary=True),
            user_text(at(10, 1, 7), "transcript only", isVisibleInTranscriptOnly=True),
            user_text(at(10, 1, 8), "[Request interrupted by user]", isMeta=True),
            user_text(at(10, 1, 9), "from a channel", origin={"kind": "channel"}),
            synth.tool_result(at(10, 2), "t1", "tool output that is not typed"),
            user_text(at(10, 3), "   "),
        ]
        ops = [e for e in self.events(recs) if e[0] == "operator"]
        self.assertEqual([(e[1], e[2]) for e in ops],
                         [(when(10, 0, 1), "please start"), (when(10, 0, 2), "and this picture")])

    def test_the_other_inputs_are_kept_as_what_they_are(self):
        recs = [
            user_text(at(10, 1), "[banto relay] Your brigade peer sent you a message.", origin={"kind": "human"}),
            user_text(at(10, 1, 1), "<local-command-stdout>ok</local-command-stdout>"),
            synth.tool_result(at(10, 2), "t1", "output"),
        ]
        self.assertEqual([e[0] for e in self.events(recs)], ["nudge", "system", "result"])

    def test_a_check_messages_answer_is_a_delivery_not_a_plain_result(self):
        recs = call_lines("d1", at(10, 0), usage(cr=1), tools=[(TOOL + "check_messages", "c1", {})])
        recs += [synth.tool_result(at(10, 0, 5), "c1", delivery(('worker-1 "b" — to you', "ready")))]
        recs += call_lines("d2", at(10, 1), usage(cr=1), tools=[(TOOL + "check_messages", "c2", {})])
        recs += [synth.tool_result(at(10, 1, 5), "c2", "No messages.")]
        self.assertEqual([e[0] for e in self.events(recs) if e[0] in ("deliver", "result")], ["deliver", "result"])


class DecisionCounting(unittest.TestCase):
    def test_only_request_decision_calls_are_decisions(self):
        root, proj = synth.make_root()
        self.addCleanup(synth.rmtree, root)
        recs = call_lines("d1", at(10, 0), usage(cr=1), tools=[(TOOL + "request_decision", "r1", {"question": "?"})])
        recs += call_lines("d2", at(10, 1), usage(cr=1), tools=[(TOOL + "name_member", "n1", {}), (TOOL + "brigade_status", "b1", {})])
        recs += call_lines("d3", at(10, 2), usage(cr=1), tools=[(TOOL + "request_decision", "r2", {"question": "again?"}),
                                                                ("Bash", "x1", {"command": "ls"})])
        s = tr.read_session(write_jsonl(os.path.join(proj, "d.jsonl"), recs), root, True)
        self.assertEqual([e[1] for e in qr.window_events(s, when(9, 0), None) if e[0] == "decision"],
                         [when(10, 0), when(10, 2)])

    def test_the_new_events_leave_errand_boundaries_and_call_attribution_alone(self):
        root, proj = synth.make_root()
        self.addCleanup(synth.rmtree, root)
        recs = synth.worker_records() + [synth.tool_result(synth.ts(9), "zz", "late output")]
        path = write_jsonl(os.path.join(proj, "w.jsonl"), recs)
        plain = tr.read_session(path, root, False)
        detailed = tr.read_session(path, root, True)
        tr.attach_errands([plain])
        tr.attach_errands([detailed])
        self.assertEqual([(e.start, e.end, e.calls) for e in plain.errs], [(e.start, e.end, e.calls) for e in detailed.errs])


def errand(start, end, reported=True):
    return {"start": start, "end": end, "reports": [(end, "report")] if reported else []}


class OpenTime(unittest.TestCase):
    LO, HI = when(10, 0), when(11, 0)

    def test_no_errand_is_no_open_time(self):
        self.assertEqual(qr.open_time([], self.LO, self.HI), (0.0, 0.0))

    def test_non_overlapping_errands_are_never_parallel(self):
        es = [errand(when(10, 5), when(10, 15)), errand(when(10, 20), when(10, 30))]
        self.assertEqual(qr.open_time(es, self.LO, self.HI), (1200.0, 0.0))

    def test_overlapping_errands_count_the_overlap_once(self):
        es = [errand(when(10, 5), when(10, 25)), errand(when(10, 15), when(10, 35)), errand(when(10, 20), when(10, 22))]
        # open 10:05..10:35 = 30 min; two or more open 10:15..10:25 (the third adds nothing beyond that) = 10 min
        self.assertEqual(qr.open_time(es, self.LO, self.HI), (1800.0, 600.0))

    def test_errands_touching_end_to_start_do_not_overlap(self):
        es = [errand(when(10, 5), when(10, 15)), errand(when(10, 15), when(10, 25))]
        self.assertEqual(qr.open_time(es, self.LO, self.HI), (1200.0, 0.0))

    def test_errands_are_clipped_to_the_window(self):
        es = [errand(when(9, 30), when(10, 10)), errand(when(10, 5), when(12, 0))]
        self.assertEqual(qr.open_time(es, self.LO, self.HI), (3600.0, 300.0))

    def test_an_errand_outside_the_window_is_ignored(self):
        self.assertEqual(qr.open_time([errand(when(8, 0), when(9, 0)), errand(when(11, 0), when(12, 0))], self.LO, self.HI), (0.0, 0.0))


class LeadTime(unittest.TestCase):
    def test_from_the_first_errand_start_to_the_last_commit(self):
        ch = {"end": when(10, 30), "errands": [errand(when(10, 12), when(10, 20)), errand(when(10, 5), when(10, 9))]}
        self.assertEqual(qr.lead_time(ch), (timedelta(minutes=25), None))

    def test_no_errand_is_no_lead_time_and_says_so(self):
        td, note = qr.lead_time({"end": when(10, 30), "errands": []})
        self.assertIsNone(td)
        self.assertIn("no errand", note)

    def test_an_errand_that_never_reported_is_left_out(self):
        ch = {"end": when(10, 30), "errands": [errand(when(10, 1), when(10, 25), reported=False), errand(when(10, 12), when(10, 20))]}
        self.assertEqual(qr.lead_time(ch), (timedelta(minutes=18), None))

    def test_a_change_whose_errands_all_never_reported_has_none_and_says_why(self):
        td, note = qr.lead_time({"end": when(10, 30), "errands": [errand(when(10, 1), when(10, 25), reported=False)]})
        self.assertIsNone(td)
        self.assertIn("never reported", note)

    def test_never_reported_errands_are_counted_per_change(self):
        ch = {"errands": [errand(when(10, 1), when(10, 25), reported=False), errand(when(10, 12), when(10, 20)),
                          errand(when(10, 2), when(10, 3), reported=False)]}
        self.assertEqual(qr.never_reported(ch), 2)


class RecordKeyedIds(unittest.TestCase):
    """An id comes from the transcript record's own uuid (and, inside one record, the block it came
    from), so no other record, matching or not, can rename it. A record without a uuid falls back to
    the position among same-kind records at its timestamp, and those ids are not stable."""

    def setUp(self):
        self.root, self.proj = synth.make_root()
        self.addCleanup(synth.rmtree, self.root)
        self.t = when(10, 5)
        self.changes = qr.group_changes([{"hash": "abc1234", "ts": when(10, 30), "subject": "feat: x", "files": ["a"]}], when(9, 0), 15)
        for ch in self.changes:
            ch["reviews"] = []
        self.release = [(when(10, 4), "release build (Director)")]

    def op(self, uuid, text):
        return user_text(at(10, 5), text, uuid=uuid)

    def text_rec(self, uuid, mid, *texts):
        r = synth.assistant(mid, at(10, 5), [{"type": "text", "text": x} for x in texts])
        r["uuid"] = uuid
        return r

    def send_rec(self, uuid, mid, *sends):
        """sends: (tool_use id, to, text)"""
        r = synth.assistant(mid, at(10, 5), [{"type": "tool_use", "id": tid, "name": TOOL + "send_to_peer", "input": {"to": to, "text": text}}
                                             for tid, to, text in sends])
        r["uuid"] = uuid
        return r

    def ids(self, records):
        path = write_jsonl(os.path.join(self.proj, "d.jsonl"), records)
        s = tr.read_session(path, self.root, True)
        evs = qr.window_events(s, when(9, 0), None)
        a, b = qr.find_candidates(self.changes, when(9, 0), evs, self.release, 12, s.id)
        return [x[4] for x in a[1]], [x[3] for x in b[1]]

    def test_a_keyed_id_ignores_the_timestamp_and_the_ordinal(self):
        t1, t2 = when(10, 0), when(11, 0)
        a = qr.candidate_id("s", t1, "a", 0, key="u-1")
        self.assertEqual(a, qr.candidate_id("s", t2, "a", 7, key="u-1"))
        self.assertRegex(a, r"^A-[0-9a-f]{8}$")
        self.assertRegex(qr.candidate_id("s", t1, "b-send", key="u-1"), r"^B-[0-9a-f]{8}$")
        others = {qr.candidate_id("s2", t1, "a", key="u-1"), qr.candidate_id("s", t1, "a", key="u-2"),
                  qr.candidate_id("s", t1, "b-text", key="u-1"), qr.candidate_id("s", t1, "b-send", key="u-1"), qr.candidate_id("s", t1, "a")}
        self.assertNotIn(a, others)
        self.assertEqual(len(others), 5)

    def test_operator_ids_survive_other_records_at_the_same_timestamp(self):
        base = [self.op("u1", "all fine"), self.op("u2", "it is broken"), self.op("u3", "it is a bug")]
        want = self.ids(base)[0]
        self.assertEqual(len(want), 2)
        self.assertEqual(len(set(want)), 2)
        self.assertEqual(self.ids(base[1:])[0], want)                                           # a record removed
        self.assertEqual(self.ids([self.op("u0", "hello")] + base)[0], want)                    # a non-matching one added
        self.assertEqual(self.ids([self.op("u0", "it is wrong")] + base)[0][1:], want)          # a matching one added
        self.assertEqual(self.ids([self.op("u1", "it is a bug")] + base[1:])[0][1:], want)      # the first one edited into a match
        self.assertEqual(self.ids([base[0], self.op("u2", "it is wrong"), base[2]])[0][0], want[0])   # a word changed: same id
        self.assertEqual(self.ids([base[0], base[2], base[1]])[0], [want[1], want[0]])          # records reordered: ids follow the records

    def test_retraction_text_ids_survive_other_records_at_the_same_timestamp(self):
        base = [self.text_rec("u1", "m1", "all fine"), self.text_rec("u2", "m2", "I was wrong"), self.text_rec("u3", "m3", "my mistake")]
        want = self.ids(base)[1]
        self.assertEqual(len(set(want)), 2)
        self.assertEqual(self.ids(base[1:])[1], want)
        self.assertEqual(self.ids([self.text_rec("u0", "m0", "hello")] + base)[1], want)
        self.assertEqual(self.ids([self.text_rec("u0", "m0", "I was wrong")] + base)[1][1:], want)
        self.assertEqual(self.ids([self.text_rec("u1", "m1", "I was wrong")] + base[1:])[1][1:], want)

    def test_send_ids_survive_other_records_at_the_same_timestamp(self):
        base = [self.send_rec("u1", "m1", ("t1", "w", "all fine")), self.send_rec("u2", "m2", ("t2", "w", "scratch that")),
                self.send_rec("u3", "m3", ("t3", "w", "I was wrong"))]
        want = self.ids(base)[1]
        self.assertEqual(len(set(want)), 2)
        self.assertEqual(self.ids(base[1:])[1], want)
        self.assertEqual(self.ids([self.send_rec("u0", "m0", ("t0", "w", "hello"))] + base)[1], want)
        self.assertEqual(self.ids([self.send_rec("u0", "m0", ("t0", "w", "I was wrong"))] + base)[1][1:], want)

    def test_blocks_of_one_record_get_their_own_ids(self):
        _a, b = self.ids([self.text_rec("u1", "m1", "I was wrong", "my mistake")])
        self.assertEqual(len(set(b)), 2)
        _a, b = self.ids([self.send_rec("u1", "m1", ("t1", "w", "I was wrong"), ("t2", "w", "my mistake"))])
        self.assertEqual(len(set(b)), 2)

    def test_a_text_block_and_a_send_of_one_record_do_not_share_an_id(self):
        r = synth.assistant("m1", at(10, 5), [{"type": "text", "text": "I was wrong"},
                                              {"type": "tool_use", "id": "t1", "name": TOOL + "send_to_peer", "input": {"to": "w", "text": "my mistake"}}])
        r["uuid"] = "u1"
        _a, b = self.ids([r])
        self.assertEqual(len(set(b)), 2)

    def test_a_record_without_a_uuid_falls_back_to_the_position_among_same_kind_records(self):
        a, _b = self.ids([user_text(at(10, 5), "all fine"), user_text(at(10, 5), "it is broken")])
        self.assertEqual(a, [qr.candidate_id("d", self.t, "a", 1)])
        a, _b = self.ids([user_text(at(10, 5), "it is broken", uuid=5)])     # a uuid that is not a string is no uuid
        self.assertEqual(a, [qr.candidate_id("d", self.t, "a", 0)])

    def test_a_send_without_a_tool_use_id_is_keyed_by_its_block_position_and_that_is_not_stable(self):
        def keys(records):
            path = write_jsonl(os.path.join(self.proj, "k.jsonl"), records)
            return list(tr.read_session(path, self.root, True).keys.values())

        for missing in (None, ""):
            r = synth.assistant("m1", at(10, 5), [{"type": "tool_use", "name": TOOL + "send_to_peer", "input": {"to": "w", "text": "I was wrong"}}])
            if missing is not None:
                r["message"]["content"][0]["id"] = missing
            r["uuid"] = "u1"
            self.assertEqual(keys([r]), ["u1#b0"])
        text_first = synth.assistant("m1", at(10, 5), [{"type": "text", "text": "hello"},
                                                       {"type": "tool_use", "name": TOOL + "send_to_peer", "input": {"to": "w", "text": "I was wrong"}}])
        text_first["uuid"] = "u1"
        self.assertEqual(keys([text_first]), ["u1#t0", "u1#b1"])        # moves with the block order: the documented instability
        with_id = synth.assistant("m1", at(10, 5), [{"type": "tool_use", "id": "t9", "name": TOOL + "send_to_peer", "input": {"to": "w", "text": "I was wrong"}}])
        with_id["uuid"] = "u1"
        self.assertEqual(keys([with_id]), ["u1#ut9"])                    # a tool_use id does not depend on the position

    def test_a_keyed_and_an_unkeyed_record_never_share_an_id(self):
        a, _b = self.ids([user_text(at(10, 5), "it is broken"), self.op("u2", "it is a bug")])
        self.assertEqual(len(set(a)), 2)

    def test_a_keyed_id_is_not_the_position_based_one(self):
        keyed = self.ids([self.op("u1", "it is broken")])[0]
        self.assertRegex(keyed[0], r"^A-[0-9a-f]{8}$")
        self.assertNotEqual(keyed[0], qr.candidate_id("d", self.t, "a", 0))


class DefinitionsText(unittest.TestCase):
    """What the README says about the metrics, the report's own definitions must say as well."""

    def setUp(self):
        self.text = " ".join(" ".join(qr.DEFINITIONS).split())

    def test_the_word_lists_are_said_to_be_tuned_on_one_window_by_one_judge_without_a_holdout(self):
        for phrase in ("one window", "one judge", "no holdout"):
            self.assertIn(phrase, self.text)

    def test_lead_time_is_said_to_start_at_the_first_errand_that_reported(self):
        self.assertIn("first errand that reported", self.text)

    def test_the_id_source_and_the_unstable_fallback_are_said(self):
        self.assertIn("uuid", self.text)
        self.assertIn("not stable", self.text)

    def test_the_append_only_invariant_the_ids_rely_on_is_said(self):
        self.assertIn("appending transcript records and never rewriting them", self.text)
        self.assertIn("block index", self.text)

    def test_every_unstable_fallback_is_named(self):
        self.assertIn("without a uuid", self.text)
        self.assertIn("without a tool_use id", self.text)
        self.assertIn("not stable", self.text)

    def test_the_id_collision_check_is_said(self):
        self.assertIn("collision", self.text)
        self.assertIn("ambiguous", self.text)

    def test_how_left_out_errands_are_assigned_and_counted_is_said(self):
        self.assertIn("by the change it started in", self.text)
        self.assertIn("owned by no change", self.text)


class IdCollisions(unittest.TestCase):
    """Two distinct candidates with one id: counted, warned about, and no judgment is applied to either."""

    def test_the_ids_that_name_more_than_one_candidate_are_listed(self):
        a = {1: [(when(10, 0), "k", when(9, 0), "x", "A-00000001"), (when(10, 1), "k", when(9, 0), "x", "A-00000002")], 2: [(when(10, 2), "k", when(9, 0), "x", "A-00000001")]}
        b = {1: [(when(10, 3), "to operator", "x", "B-00000003", "self", "self"), (when(10, 4), "to w", "x", "B-00000004", "self", "self")]}
        self.assertEqual(qr.id_collisions(a, b), ["A-00000001"])
        self.assertEqual(qr.id_collisions({}, {}), [])

    def test_a_judgment_for_a_colliding_id_is_dropped_with_a_warning_and_the_others_stay(self):
        judgments = {"A-00000001": ("real", ""), "A-00000002": ("not", "")}
        usable, warnings = qr.refuse_ambiguous(judgments, ["A-00000001"])
        self.assertEqual(usable, {"A-00000002": ("not", "")})
        self.assertEqual(len(warnings), 1)
        self.assertIn("A-00000001", warnings[0])
        self.assertIn("ambiguous", warnings[0])
        self.assertEqual(judgments["A-00000001"], ("real", ""))       # the caller's dict is not changed
        self.assertEqual(qr.refuse_ambiguous(judgments, ["A-00000009"])[1], [])


    """The per-change count assigns an errand by its start; the summary counts every unreported errand
    whose time parallelism left out, owned by a change or not."""

    SINCE = when(10, 0)

    def run_effects(self, errands):
        changes = qr.group_changes([{"hash": "h1", "ts": when(10, 30), "subject": "feat: x", "files": ["a"]}], self.SINCE, 15)
        for ch in changes:
            ch["reviews"], ch["later_fixes"], ch["usage"] = [], [], {}
            ch["errands"] = [e for e in errands if qr.change_of(changes, self.SINCE, e["start"]) is ch]
        none = defaultdict(list)
        sm = qr.compute_effects(changes, errands, [], self.SINCE, none, none, None)
        return changes[0]["effect"], sm

    def test_an_unreported_errand_starting_exactly_at_since_is_counted_in_the_summary(self):
        ef, sm = self.run_effects([errand(self.SINCE, self.SINCE + timedelta(minutes=30), reported=False)])
        self.assertEqual(ef["left_out_errands"], 0)             # no change owns it: the per-change count is by start
        self.assertEqual(ef["parallel"]["open_s"], 0)           # but its 1800 s were left out of parallelism
        self.assertEqual(sm["left_out_errands"], {"errands": 1, "changes": 0, "unowned": 1})

    def test_an_unreported_errand_carried_in_from_before_since_is_counted_when_its_time_was_left_out(self):
        _ef, sm = self.run_effects([errand(self.SINCE - timedelta(hours=1), self.SINCE + timedelta(minutes=10), reported=False)])
        self.assertEqual(sm["left_out_errands"], {"errands": 1, "changes": 0, "unowned": 1})

    def test_an_unreported_errand_after_the_last_commit_left_out_no_time_and_is_not_counted(self):
        _ef, sm = self.run_effects([errand(when(11, 0), when(11, 20), reported=False)])
        self.assertEqual(sm["left_out_errands"], {"errands": 0, "changes": 0, "unowned": 0})

    def test_an_owned_errand_is_counted_once_in_its_change_and_once_in_the_summary(self):
        ef, sm = self.run_effects([errand(when(10, 5), when(10, 20), reported=False),
                                   errand(self.SINCE, self.SINCE + timedelta(minutes=30), reported=False)])
        self.assertEqual(ef["left_out_errands"], 1)
        self.assertEqual(sm["left_out_errands"], {"errands": 2, "changes": 1, "unowned": 1})

    def test_a_reported_errand_at_since_is_not_left_out_and_its_time_counts(self):
        ef, sm = self.run_effects([errand(self.SINCE, self.SINCE + timedelta(minutes=30))])
        self.assertEqual(ef["parallel"]["open_s"], 1800.0)
        self.assertEqual(sm["left_out_errands"], {"errands": 0, "changes": 0, "unowned": 0})


class DecisionAvailability(unittest.TestCase):
    def test_before_straddling_and_after_the_commit(self):
        cutoff = qr.REQUEST_DECISION_AVAILABLE
        starts = [cutoff - timedelta(hours=2), cutoff - timedelta(hours=1), cutoff]
        ends = [cutoff - timedelta(hours=1), cutoff, cutoff + timedelta(hours=1)]
        changes = [{"id": i + 1, "start": start, "end": end, "errands": [], "reviews": [],
                    "later_fixes": [], "usage": {}} for i, (start, end) in enumerate(zip(starts, ends))]
        events = [("decision", cutoff - timedelta(minutes=30)), ("decision", cutoff + timedelta(minutes=5))]
        empty = defaultdict(list)
        summary = qr.compute_effects(changes, [], events, starts[0], empty, empty, None)
        self.assertEqual([ch["effect"]["decisions"] for ch in changes], [None, None, 1])
        self.assertEqual(summary["operator"]["decisions"], 1)
        self.assertEqual(summary["operator"]["decisions_eligible_changes"], 1)
        self.assertEqual(summary["operator"]["decisions_per_change"], 1)


class DirectorSideFollowups(unittest.TestCase):
    def test_followups_belong_to_the_first_report_until_next_instruction(self):
        an = qr.Analysis(when(9, 0), when(12, 0), 3, {})
        events = [("send", when(10, 0), "worker-9", "review errand"),
                  ("deliver", when(10, 1), [("worker-9", None, None, "FAIL\n1. High — a.rs:1")]),
                  ("deliver", when(10, 2), [("worker-9", None, None, "H2: follow-up")]),
                  ("send", when(10, 3), "worker-9", "review errand"),
                  ("deliver", when(10, 4), [("worker-9", None, None, "PASS")])]
        an.director_side_errands(events)
        self.assertEqual(len(an.errands), 2)
        self.assertEqual(an.errands[0]["end"], when(10, 1))
        self.assertEqual(len(an.errands[0]["reports"]), 2)
        self.assertEqual(qr.neutral_findings(an.errands[0]["reports"]), 2)
        self.assertEqual(len(an.errands[1]["reports"]), 1)


    def test_followup_explicit_verdict_stops_a_director_side_review(self):
        since = when(9, 0)
        an = qr.Analysis(since, when(12, 0), 3, {})
        an.director_side_errands([
            ("send", when(10, 0), "worker-9", "review errand"),
            ("deliver", when(10, 1), [("worker-9", None, None, "Checking the change.")]),
            ("deliver", when(10, 2), [("worker-9", None, None, "Verdict: FAIL\n1. High — synthetic.rs:12")]),
        ])
        commits = [{"hash": "abc1234", "ts": when(10, 30), "subject": "feat: x", "files": ["x"]}]
        changes = qr.group_changes(commits, since, 15)
        qr.aggregate(changes, an, commits, [], {})
        review = changes[0]["reviews"][0]
        self.assertEqual((review["t"], review["verdict"]), (when(10, 2), "FAIL"))
        result = qr.review_catch([review])
        self.assertEqual((result["stopped_rounds"], result["stopped_findings"]), (1, 1))

    def test_explicit_verdict_takes_precedence_over_earlier_derived_mark(self):
        reports = [(when(10, 1), "BLOCKER in this case"), (when(10, 2), "Verdict: PASS")]
        self.assertEqual(qr.selected_review_report(reports)[2], "PASS")
        self.assertEqual(qr.selected_review_report(reports[:1])[2], "BLK*")


class NeutralFindingHeadings(unittest.TestCase):
    def test_numbered_severity_not_numbered_checks(self):
        text = "PASS checks:\n1. verified the output\n2. checked the boundary\nFAIL\n1. High — sample.rs:12\n2. Medium — sample.rs:20"
        self.assertEqual(qr.neutral_findings([(None, text)]), 2)

    def test_empty_severity_heading_is_not_a_finding(self):
        self.assertEqual(qr.neutral_findings([(None, "BLOCKER: none.\nSHOULD-FIX: none found.")]), 0)

    def test_bare_severity_and_numbered_severity_are_not_findings(self):
        for text in ("HIGH", "HIGH:", "1. HIGH", "**HIGH**:"):
            with self.subTest(text=text):
                self.assertEqual(qr.neutral_findings([(None, text)]), 0)
        for text in ("HIGH: no handling for missing input", "1. HIGH — missing input"):
            with self.subTest(text=text):
                self.assertEqual(qr.neutral_findings([(None, text)]), 1)

    def test_negated_clear_words_remain_findings(self):
        for text in ("H1: not fixed", "H1: failure was not resolved", "H1: never fixed"):
            with self.subTest(text=text):
                self.assertEqual(qr.neutral_findings([(None, text)]), 1)
        for text in ("H1: fixed", "H1: failure was resolved", "H1: passed"):
            with self.subTest(text=text):
                self.assertEqual(qr.neutral_findings([(None, text)]), 0)

    def test_unlisted_finding_sentence_with_location(self):
        self.assertEqual(qr.neutral_findings([(None, "New finding, low — a.rs:12. Broken case.")]), 1)

    def test_labels_clear_checks_and_followups(self):
        reports = [(None, "BLOCKER 1 — a.rs:1\nH2: unhandled case\nH3: holds on test"),
                   (None, "H2: still unhandled\nH4: another issue")]
        self.assertEqual(qr.neutral_findings(reports), 3)


class ReviewCatch(unittest.TestCase):
    def test_fail_rounds_findings_and_derived_marks(self):
        reviews = [{"verdict": "FAIL", "findings": 3, "neutral_findings": 2, "blockers": 1}, {"verdict": "PASS", "findings": 1, "blockers": 0},
                   {"verdict": "FAIL", "findings": 2, "blockers": 2}, {"verdict": "BLK*", "findings": 4, "neutral_findings": 1, "blockers": 1},
                   {"verdict": None, "findings": 0, "blockers": 0}]
        self.assertEqual(qr.review_catch(reviews), {"rounds": 5, "fail_rounds": 2, "fail_findings": 5, "fail_blockers": 3, "derived_blk_rounds": 1, "stopped_rounds": 3, "stopped_findings": 3})

    def test_no_review_is_all_zero(self):
        self.assertEqual(qr.review_catch([]), {"rounds": 0, "fail_rounds": 0, "fail_findings": 0, "fail_blockers": 0, "derived_blk_rounds": 0, "stopped_rounds": 0, "stopped_findings": 0})


GIT_DATES = ("2025-03-10T10:30:00+00:00", "2025-03-10T11:30:00+00:00")


@unittest.skipUnless(shutil.which("git"), "git is not installed")
class EffectEndToEnd(unittest.TestCase):
    """Two changes. Change 1 (window 09:00..10:30): a builder errand and a reviewer errand that overlap
    for one minute, the reviewer FAILs with two findings, the Director is nudged, hears from a Worker and
    retracts three times (after an operator message, after a Worker's report, after a bare tool result),
    asks the operator one decision, builds a release and the operator reports it still broken. Change 2
    (10:30..11:30): no errand; the operator reports a crash after another release build. A later `fix`
    commit touches change 1's file."""

    def setUp(self):
        self.base = tempfile.mkdtemp(prefix="measure-effect-")
        self.addCleanup(synth.rmtree, self.base)
        self.root = os.path.join(self.base, "claude", "projects")
        self.repo = os.path.join(self.base, "repo")
        os.makedirs(self.repo)
        self.proj = os.path.join(self.root, "q-proj")
        os.makedirs(self.proj)
        self.n = 0

        write_jsonl(os.path.join(self.proj, "builder.jsonl"),
                    [user_text(synth.ts(0), "You are worker-1, a Worker in banto brigade 7. Check mail.")] + synth.worker_records())
        rv = [user_text(at(10, 0), "You are worker-2, a Worker in banto brigade 7. Check mail.")]
        rv += call_lines("r0", at(10, 1, 50), usage(cr=1000), tools=[(TOOL + "check_messages", "rc1", {})])
        rv.append(synth.tool_result(at(10, 2), "rc1", delivery(("director — to you", "independent review of the change"))))
        rv += call_lines("r1", at(10, 5), usage(cr=1000))
        rv += call_lines("r2", at(10, 8), usage(cr=1000),
                         tools=[(TOOL + "send_to_peer", "rs1", {"to": "director", "text": "Verdict: FAIL\nF1: bad\nF2: worse"})])
        write_jsonl(os.path.join(self.proj, "reviewer.jsonl"), rv)
        # Two errands that never send a report: worker-3's opens in change 1 before every other errand
        # and runs for a quarter of an hour, worker-4's is the only errand of change 2.
        for who, h, m, last in ((3, 10, 0, (10, 15)), (4, 11, 0, (11, 20))):
            w = [user_text(at(h, m), f"You are worker-{who}, a Worker in banto brigade 7. Check mail.")]
            w += call_lines(f"x{who}", at(h, m, 20), usage(cr=500), tools=[(TOOL + "check_messages", f"xc{who}", {})])
            w.append(synth.tool_result(at(h, m, 30), f"xc{who}", delivery(("director — to you", "survey the other thing"))))
            w += call_lines(f"y{who}", at(*last), usage(cr=500))
            write_jsonl(os.path.join(self.proj, f"never-{who}.jsonl"), w)

        d = []
        d += call_lines("d1", at(10, 0, 10), usage(cr=50), tools=[(TOOL + "name_member", "n1", {})])
        d.append(user_text(at(10, 0, 20), "please start"))
        d += call_lines("d2", at(10, 0, 30), usage(cr=60), tools=[(TOOL + "check_messages", "c1", {})])
        d.append(synth.tool_result(at(10, 0, 40), "c1", delivery(('worker-1 "builder" — to you', "ready"))))
        d.append(user_text(at(10, 5), "[banto relay] Your brigade peer sent you a message. Call the check_messages tool now.",
                           origin={"kind": "human"}))
        d.append(user_text(at(10, 5, 2), "<system-reminder>background</system-reminder>"))
        d.append(user_text(at(10, 5, 4), "<task-notification>done</task-notification>", origin={"kind": "task-notification"}))
        d.append(user_text(at(10, 5, 6), "meta", isMeta=True))
        d.append(user_text(at(10, 20), "ok go"))
        d += call_lines("d3", at(10, 20, 5), usage(cr=70), text="I was wrong, scratch that.")
        d.append(user_text(at(10, 21), "[banto relay] Your brigade peer sent you a message. Call the check_messages tool now.",
                           origin={"kind": "human"}))
        d += call_lines("d4", at(10, 21, 2), usage(cr=70), tools=[(TOOL + "check_messages", "c2", {})])
        d.append(synth.tool_result(at(10, 21, 5), "c2", delivery(('worker-2 "rev" — to you', "report"))))
        d += call_lines("d5", at(10, 21, 10), usage(cr=70), text="Correction: the report was fine.")
        d.append(synth.tool_result(at(10, 22), "x9", "some command output"))
        d += call_lines("d6", at(10, 22, 5), usage(cr=70), text="my mistake")
        d += call_lines("d7", at(10, 23), usage(cr=80), tools=[(TOOL + "request_decision", "r1", {"question": "which?"})], text="asking")
        d += call_lines("d8", at(10, 24), usage(cr=90), tools=[("Bash", "b1", {"command": "cargo build --release"})], text="building")
        d.append(user_text(at(10, 25), "it is still broken, nothing appears"))
        d += call_lines("d9", at(10, 58), usage(cr=90), tools=[("Bash", "b2", {"command": "cargo build --release"})], text="building")
        d.append(user_text(at(11, 0), "wrong again, it crashes"))
        write_jsonl(os.path.join(self.proj, "director.jsonl"), d)

        self.git = ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false", "-C", self.repo]
        subprocess.run(["git", "-C", self.repo, "init", "-q"], check=True, capture_output=True)
        self.commit("a.txt", "feat: one", GIT_DATES[0])
        self.commit("a.txt", "fix: two", GIT_DATES[1])

    def commit(self, name, subject, date):
        self.n += 1
        with open(os.path.join(self.repo, name), "w") as fh:
            fh.write(str(self.n))
        env = dict(os.environ, GIT_AUTHOR_DATE=date, GIT_COMMITTER_DATE=date)
        subprocess.run(self.git + ["add", name], check=True, capture_output=True)
        subprocess.run(self.git + ["commit", "-q", "-m", subject], check=True, capture_output=True, env=env)

    def run_quality(self, out, *extra):
        return run_main(qr, ["--repo", self.repo, "--root", self.root, "--project", "q-proj", "--out", os.path.join(self.base, out),
                             "--since", "2025-03-10T09:00:00Z", "--until", "2025-03-10T12:00:00Z"] + list(extra))

    def load(self, out):
        with open(os.path.join(self.base, out, "changes.json"), encoding="utf-8") as fh:
            return json.load(fh)

    def ids(self, out, name):
        with open(os.path.join(self.base, out, name), encoding="utf-8") as fh:
            return ID_RX.findall(fh.read())

    def test_ids_are_identical_across_two_runs_and_printed_in_both_files(self):
        self.assertEqual(self.run_quality("o1")[0], 0)
        self.assertEqual(self.run_quality("o2")[0], 0)
        for name, n in (("candidates_a.txt", 2), ("candidates_b.txt", 3)):
            first, second = self.ids("o1", name), self.ids("o2", name)
            self.assertEqual(len(first), n, name)
            self.assertEqual(first, second, name)
            self.assertEqual(len(set(first)), n, name)
        doc1, doc2 = self.load("o1"), self.load("o2")
        self.assertEqual(doc1["changes"][0]["effect"], doc2["changes"][0]["effect"])
        self.assertEqual(doc1["summary"], doc2["summary"])

    def test_quality_and_speed_and_effort_without_judgments(self):
        rc, stdout, err = self.run_quality("o1")
        self.assertEqual(rc, 0, err)
        doc = self.load("o1")
        c1, c2 = (ch["effect"] for ch in doc["changes"])
        self.assertEqual(c1["review"], {"rounds": 1, "fail_rounds": 1, "fail_findings": 2, "fail_blockers": 0, "derived_blk_rounds": 0, "stopped_rounds": 1, "stopped_findings": 2})
        self.assertEqual(c2["review"]["rounds"], 0)
        self.assertEqual((c1["escapes"]["candidates_a"], c2["escapes"]["candidates_a"]), (1, 1))
        self.assertEqual((c1["escapes"]["fix_commits"], c2["escapes"]["fix_commits"]), (1, 0))
        self.assertIsNone(c1["escapes"]["judged_a"])
        self.assertEqual(c1["reversals"]["by_trigger"], {"operator": 1, "worker": 2, "self": 0})
        self.assertEqual(c1["reversals"]["by_predecessor"], {"operator": 1, "worker": 1, "self": 1})
        self.assertEqual([(c["trigger"], c["predecessor"]) for c in c1["reversals"]["candidates"]],
                         [("operator", "operator"), ("worker", "worker"), ("worker", "self")])
        self.assertIsNone(c1["reversals"]["judged"])
        self.assertEqual(c2["reversals"]["by_trigger"], {"operator": 0, "worker": 0, "self": 0})
        self.assertEqual(c1["lead_time_s"], 29 * 60)        # first errand opened 10:01, last commit 10:30
        self.assertIsNone(c2["lead_time_s"])
        self.assertIn("never reported", c2["lead_time_note"])
        self.assertEqual((c1["left_out_errands"], c2["left_out_errands"]), (1, 1))
        self.assertEqual((c1["parallel"]["open_s"], c1["parallel"]["multi_s"]), (420, 60))   # 10:01..10:08 open, 10:02..10:03 two at once
        self.assertEqual((c2["parallel"]["open_s"], c2["parallel"]["share"]), (0, None))
        self.assertEqual((c1["operator_messages"], c2["operator_messages"]), (3, 1))         # nudges, tags, meta text excluded
        self.assertEqual((c1["decisions"], c2["decisions"]), (None, None))
        self.assertGreater(c1["cost"], 0)
        s = doc["summary"]
        self.assertEqual(s["review"], {"changes_with_review": 1, "changes_with_fail": 1, "fail_rounds": 1, "fail_findings": 2, "derived_blk_rounds": 0, "changes_with_stopped": 1, "stopped_rounds": 1, "stopped_findings": 2})
        self.assertEqual(s["escapes"]["candidates_a"], 2)
        self.assertIsNone(s["escapes"]["judged_a"])
        self.assertEqual(s["escapes"]["changes_with_candidate_a"], 2)
        self.assertEqual((s["escapes"]["changes_with_fix_commits"], s["escapes"]["fix_commits"]), (1, 1))
        self.assertEqual(s["reversals"]["candidates_b"], 3)
        self.assertEqual(s["reversals"]["by_trigger"], {"operator": 1, "worker": 2, "self": 0})
        self.assertEqual(s["reversals"]["by_predecessor"], {"operator": 1, "worker": 1, "self": 1})
        self.assertEqual(s["lead_time"], {"median_s": 29 * 60, "max_s": 29 * 60, "changes_with_lead_time": 1, "changes_without_lead_time": 1})
        self.assertEqual(s["left_out_errands"], {"errands": 2, "changes": 2, "unowned": 0})
        self.assertEqual(s["parallelism"]["share"], 60 / 420)
        self.assertEqual((s["operator"]["messages"], s["operator"]["decisions"]), (4, None))
        self.assertEqual(s["operator"]["decisions_eligible_changes"], 0)
        self.assertIsNone(s["operator"]["decisions_per_change"])
        self.assertEqual(s["operator"]["messages_per_change"], 2)
        self.assertIsNone(s["judgments"])
        self.assertIn("WINDOW SUMMARY", stdout)
        self.assertIn("DEFINITIONS", stdout)
        self.assertIn("no --judgments file", stdout)

    def test_judgments_turn_candidate_counts_into_judged_counts(self):
        self.assertEqual(self.run_quality("o1")[0], 0)
        a_ids, b_ids = self.ids("o1", "candidates_a.txt"), self.ids("o1", "candidates_b.txt")
        doc = self.load("o1")
        cands = doc["changes"][0]["effect"]["reversals"]["candidates"]
        self.assertEqual(sorted(c["id"] for c in cands), sorted(b_ids))
        first, second, third = (c["id"] for c in cands)    # triggers: operator, worker, worker
        path = os.path.join(self.base, "judgments.txt")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write("# my verdicts\n\n")
            fh.write(f"{a_ids[0]} real broke the picker\n")
            fh.write(f"{a_ids[1]} not\n")
            fh.write(f"{first} real\n")
            fh.write(f"{second} unsure\n")
            fh.write("A-deadbeef real\n")            # an id no candidate has
            fh.write("only-one-token\n")             # malformed
            fh.write(f"{third} maybe\n")  # not a verdict: this candidate stays unjudged
        rc, stdout, err = self.run_quality("o2", "--judgments", path)
        self.assertEqual(rc, 0, err)
        doc = self.load("o2")
        e1 = doc["changes"][0]["effect"]
        self.assertEqual(e1["escapes"]["judged_a"], {"real": 1, "not": 0, "unsure": 0, "unjudged": 0})
        self.assertEqual(doc["changes"][1]["effect"]["escapes"]["judged_a"], {"real": 0, "not": 1, "unsure": 0, "unjudged": 0})
        self.assertEqual(e1["reversals"]["judged"], {"operator": {"real": 1, "not": 0, "unsure": 0, "unjudged": 0},
                                                      "worker": {"real": 0, "not": 0, "unsure": 1, "unjudged": 1},
                                                      "self": {"real": 0, "not": 0, "unsure": 0, "unjudged": 0}})
        s = doc["summary"]
        self.assertEqual(s["escapes"]["judged_a"], {"real": 1, "not": 1, "unsure": 0, "unjudged": 0})
        self.assertEqual(s["reversals"]["judged"]["operator"]["real"], 1)
        warnings = s["judgments"]["warnings"]
        self.assertEqual(len(warnings), 3)
        self.assertTrue(any("A-deadbeef" in w for w in warnings))
        self.assertTrue(any("line 8" in w for w in warnings))       # the malformed line
        self.assertTrue(any("line 9" in w for w in warnings))       # 'maybe'
        self.assertIn("A-deadbeef", err)                             # warnings reach stderr too
        self.assertIn("warning", stdout.lower())

    FORCED = "A-00000001"

    def run_forced(self, out, *extra):
        """A run in which every (a) candidate gets one and the same id."""
        real = qr.candidate_id

        def fake(session_id, ts, kind, n=0, key=None):
            return self.FORCED if kind == "a" else real(session_id, ts, kind, n, key=key)
        with mock.patch.object(qr, "candidate_id", fake):
            return self.run_quality(out, *extra)

    def test_a_normal_run_reports_zero_collisions_and_no_warning(self):
        rc, stdout, err = self.run_quality("o0")
        self.assertEqual(rc, 0, err)
        self.assertEqual(self.load("o0")["summary"]["id_collisions"], 0)
        self.assertNotIn("collision", err)
        self.assertIn("0 id collision(s)", stdout)

    def test_a_collision_is_counted_in_the_summary_and_warned_about_on_stderr(self):
        rc, stdout, err = self.run_forced("o1")
        self.assertEqual(rc, 0, err)
        self.assertEqual(self.load("o1")["summary"]["id_collisions"], 1)
        self.assertIn(self.FORCED, err)
        self.assertIn("collision", err)
        self.assertIn("1 id collision(s)", stdout)

    def test_a_judgment_naming_the_colliding_id_is_refused_and_counts_neither_candidate(self):
        path = os.path.join(self.base, "judgments.txt")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(f"{self.FORCED} real\n")
        rc, stdout, err = self.run_forced("o2", "--judgments", path)
        self.assertEqual(rc, 0, err)
        doc = self.load("o2")
        self.assertEqual(doc["summary"]["escapes"]["judged_a"], {"real": 0, "not": 0, "unsure": 0, "unjudged": 2})
        warnings = doc["summary"]["judgments"]["warnings"]
        self.assertEqual(len(warnings), 1)
        self.assertIn("ambiguous", warnings[0])
        self.assertNotIn("matches no candidate", warnings[0])
        self.assertIn("ambiguous", err)

    def test_a_missing_judgments_file_is_an_error_before_anything_is_written(self):
        rc, _, err = self.run_quality("o1", "--judgments", os.path.join(self.base, "nope.txt"))
        self.assertEqual(rc, 2)
        self.assertIn("--judgments", err)
        self.assertFalse(os.path.exists(os.path.join(self.base, "o1")))

    def test_the_existing_report_lines_and_json_keys_are_still_there(self):
        rc, stdout, err = self.run_quality("o1")
        self.assertEqual(rc, 0, err)
        for needle in ("PER-CHANGE TABLE", "rework rounds", "weighted cost", "later fix commits", "candidates: (a)", "attribution:", "review reports over all changes"):
            self.assertIn(needle, stdout)
        ch = self.load("o1")["changes"][0]
        for key in ("id", "commits", "start", "end", "errands", "reviews", "rework", "usage", "later_fixes"):
            self.assertIn(key, ch)


if __name__ == "__main__":
    unittest.main()
