"""The default weights and per-model multipliers: what they are, how a model name finds its
multiplier, how a model that finds none is counted, and that every output names them.
Synthetic data only; the model names below are invented."""
import argparse
import csv
import json
import os
import tempfile
import unittest

import synth
from synth import call_lines, compact, run_main, ts, usage, write_jsonl

import ac_measure
import transcripts as tr
import usage_report
import usage_whatif

FABLE, OPUS, SONNET, HAIKU, OTHER = "synth-fable-1", "synth-opus-1", "synth-sonnet-1", "synth-haiku-1", "model-a"


def parse(*argv):
    ap = argparse.ArgumentParser()
    tr.add_weight_args(ap)
    return tr.weights_from_args(ap.parse_args(list(argv)))


class Defaults(unittest.TestCase):
    def test_default_token_weights(self):
        w = tr.Weights()
        self.assertEqual((w.wcw, w.wcr, w.wout), (1.25, 0.025, 5.0))

    def test_default_multipliers_by_family(self):
        w = tr.Weights()
        self.assertEqual(w.model_mult, {"opus": 1.0, "sonnet": 0.68, "fable": 4.0, "haiku": 0.68})
        self.assertEqual([w.mult(m) for m in (OPUS, SONNET, FABLE, HAIKU)], [1.0, 0.68, 4.0, 0.68])

    def test_a_model_matching_no_family_weighs_one(self):
        w = tr.Weights()
        for name in (OTHER, "?", "", "<synthetic>"):
            self.assertEqual(w.mult(name), 1.0, name)
            self.assertIsNone(w.key_for(name), name)

    def test_the_flag_default_agrees_with_the_class_default(self):
        w = parse()
        self.assertEqual((w.wcw, w.wcr, w.wout, w.model_mult), (1.25, 0.025, 5.0, tr.Weights().model_mult))


class Matching(unittest.TestCase):
    def test_a_family_is_a_substring_anywhere_in_the_name(self):
        w = tr.Weights()
        self.assertEqual(w.key_for("vendor.fable-9-preview"), "fable")
        self.assertEqual(w.key_for("claude-opus-4"), "opus")

    def test_matching_ignores_case(self):
        self.assertEqual(tr.Weights().mult("Synth-FABLE-1"), 4.0)

    def test_the_longest_matching_key_wins(self):
        w = tr.Weights(model_mult={"fable-1": 6.0})
        self.assertEqual(w.mult("synth-fable-1"), 6.0)
        self.assertEqual(w.mult("synth-fable-2"), 4.0)

    def test_equal_length_keys_resolve_the_same_way_every_time(self):
        w = tr.Weights(model_mult={"ab": 2.0, "bc": 3.0})
        self.assertEqual(w.mult("xabcx"), 2.0)

    def test_an_override_replaces_a_family_and_the_others_stay(self):
        w = tr.Weights(model_mult={"fable": 2.5})
        self.assertEqual((w.mult(FABLE), w.mult(OPUS), w.mult(SONNET)), (2.5, 1.0, 0.68))

    def test_a_new_key_adds_a_family(self):
        w = tr.Weights(model_mult={"model-a": 2.0})
        self.assertEqual((w.mult(OTHER), w.mult(FABLE)), (2.0, 4.0))

    def test_override_keys_are_case_folded_so_they_replace_rather_than_duplicate(self):
        w = tr.Weights(model_mult={"FABLE": 2.0})
        self.assertEqual(w.model_mult["fable"], 2.0)
        self.assertEqual(sum(1 for k in w.model_mult if k.lower() == "fable"), 1)

    def test_cost_applies_the_multiplier(self):
        w = tr.Weights()
        base = 100 + 40 * 1.25 + 1000 * 0.025 + 10 * 5       # input + cache write + cache read + output
        self.assertAlmostEqual(w.cost(OPUS, 100, 40, 1000, 10), base)
        self.assertAlmostEqual(w.cost(FABLE, 100, 40, 1000, 10), base * 4.0)
        self.assertAlmostEqual(w.cost(SONNET, 100, 40, 1000, 10), base * 0.68)
        self.assertAlmostEqual(w.cost(OTHER, 100, 40, 1000, 10), base)


class Naming(unittest.TestCase):
    def test_basis_names_every_multiplier_and_the_unmatched_one(self):
        self.assertEqual(tr.Weights().basis(),
                         "input=1 cache_write=1.25 cache_read=0.025 output=5 "
                         "model_multipliers=fable=4,haiku=0.68,opus=1,sonnet=0.68 other_models=1")

    def test_basis_follows_flags(self):
        w = parse("--wcr", "0.2", "--model-weight", "fable=2", "--model-weight", "gpt=1.5")
        self.assertEqual(w.basis(),
                         "input=1 cache_write=1.25 cache_read=0.2 output=5 "
                         "model_multipliers=fable=2,gpt=1.5,haiku=0.68,opus=1,sonnet=0.68 other_models=1")

    def test_as_dict(self):
        self.assertEqual(tr.Weights().as_dict(), {
            "input": 1, "cache_write": 1.25, "cache_read": 0.025, "output": 5,
            "model_multipliers": {"fable": 4.0, "haiku": 0.68, "opus": 1.0, "sonnet": 0.68},
            "other_models": 1.0})

    def test_describe_names_the_multipliers_the_rule_and_the_unmatched_weight(self):
        text = tr.Weights().describe()
        for part in ("cache-read 0.025", "fable 4", "opus 1", "sonnet 0.68", "haiku 0.68", "substring", "matching none counts 1.0"):
            self.assertIn(part, text)

    def test_the_assumption_says_what_was_fitted_what_was_reported_and_what_is_unknown(self):
        text = tr.ASSUMPTION
        for part in ("Fable", "fitted", "usage-screen", "outside report", "unchecked", "Haiku", "unknown", "placeholder"):
            self.assertIn(part, text)
        self.assertNotIn("Sonnet's is a placeholder", text)         # Sonnet's 0.68 is the outside report's, not a placeholder

    def test_numbers_are_named_exactly_not_to_six_digits(self):
        self.assertEqual([tr.num(x) for x in (4.0, 0.68, 1000, 0.123456789, 1.25, 0)], ["4", "0.68", "1000", "0.123456789", "1.25", "0"])
        w = tr.Weights(wcw=1.2345678, wcr=0.0123456789, wout=5.000001, model_mult={"fable": 0.123456789})
        for text in (w.describe(), w.basis()):
            for part in ("0.123456789", "1.2345678", "0.0123456789", "5.000001"):
                self.assertIn(part, text)
        self.assertEqual(w.as_dict()["model_multipliers"]["fable"], 0.123456789)
        self.assertEqual(float(w.basis().split("fable=")[1].split(",")[0]), w.model_mult["fable"])   # what it names is what it used

    def test_a_shared_csv_names_the_value_the_cost_used(self):
        root, proj = synth.make_root()
        out = tempfile.mkdtemp(prefix="measure-out-")
        self.addCleanup(synth.rmtree, root)
        self.addCleanup(synth.rmtree, out)
        write_jsonl(os.path.join(proj, "s.jsonl"), call_lines("a1", ts(0), usage(inp=1000000), model=FABLE))
        rc, stdout, err = run_main(usage_report, ["--root", root, "--out", out, "--model-weight", "fable=0.123456789"])
        self.assertEqual(rc, 0, err)
        with open(os.path.join(out, "calls.csv"), encoding="utf-8") as fh:
            row = next(csv.DictReader(fh))
        self.assertIn("fable=0.123456789", row["weights_assumed"])
        self.assertIn("fable 0.123456789", stdout)
        self.assertAlmostEqual(float(row["weighted"]), 123456.79, places=2)


class Flags(unittest.TestCase):
    def test_model_weight_overrides_and_repeats(self):
        w = parse("--model-weight", "fable=2", "--model-weight", "FABLE=3", "--model-weight", "gpt=0")
        self.assertEqual((w.mult(FABLE), w.mult("gpt-5"), w.mult(OPUS)), (3.0, 0.0, 1.0))

    def test_malformed_and_out_of_range_values_are_refused(self):
        for bad in ("fable", "=2", " =2", "fable=", "fable=abc", "fable=nan", "fable=inf", "fable=-1", "fable=1000.5"):
            with self.assertRaises(ValueError, msg=bad):
                parse("--model-weight", bad)

    def test_the_bounds_are_inclusive(self):
        self.assertEqual(parse("--model-weight", "x=0").mult("x"), 0.0)
        self.assertEqual(parse("--model-weight", "x=1000").mult("x"), 1000.0)

    def test_every_script_turns_a_bad_multiplier_into_exit_two(self):
        root, _ = synth.make_root()
        self.addCleanup(synth.rmtree, root)
        for module, extra in ((usage_report, []), (usage_whatif, []), (ac_measure, ["--since", ts(0)])):
            rc, _, err = run_main(module, ["--root", root, "--model-weight", "fable=nan"] + extra)
            self.assertEqual(rc, 2, module.__name__)
            self.assertIn("--model-weight", err)


class Unmatched(unittest.TestCase):
    def setUp(self):
        self.root, self.proj = synth.make_root()
        self.out = tempfile.mkdtemp(prefix="measure-out-")
        self.addCleanup(synth.rmtree, self.root)
        self.addCleanup(synth.rmtree, self.out)
        recs = []
        recs += call_lines("a1", ts(0), usage(inp=10, out=10, cr=1000), model=FABLE)
        recs += call_lines("a2", ts(1), usage(inp=10, out=10, cr=1000), model=OPUS)
        recs += call_lines("a3", ts(2), usage(inp=10, out=10, cr=1000), model=OTHER)
        recs += call_lines("a4", ts(3), usage(inp=10, out=10, cr=1000), model=OTHER)
        recs += call_lines("a5", ts(4), usage(), model="<synthetic>")           # no tokens: nothing weighed
        recs += call_lines("a6", ts(5), usage(inp=5, cc=1), model=SONNET)
        write_jsonl(os.path.join(self.proj, "s.jsonl"), recs)

    def sessions(self, since=None):
        sessions = tr.load_sessions(self.root, None)
        tr.prepare(sessions, tr.Weights(), since, None)
        return sessions

    def test_unmatched_models_are_counted_with_their_tokens(self):
        got = tr.unmatched_models(self.sessions(), tr.Weights())
        self.assertEqual(got, {OTHER: {"calls": 2, "tokens": 2 * 1020}})

    def test_a_call_outside_the_window_is_not_counted(self):
        got = tr.unmatched_models(self.sessions(tr.parse_ts(ts(3))), tr.Weights())
        self.assertEqual(got, {OTHER: {"calls": 1, "tokens": 1020}})

    def test_a_model_the_flags_name_is_no_longer_unmatched(self):
        got = tr.unmatched_models(self.sessions(), tr.Weights(model_mult={"model-a": 2.0}))
        self.assertEqual(got, {})

    def test_the_line_names_them_or_says_none(self):
        self.assertEqual(tr.unmatched_line({}), "models matching no multiplier (weighed at 1.0): none")
        self.assertEqual(tr.unmatched_line({OTHER: {"calls": 2, "tokens": 2040}}),
                         "models matching no multiplier (weighed at 1.0): model-a (2 calls, 2,040 tokens)")

    def test_usage_report_names_the_multipliers_and_the_unmatched_model(self):
        rc, stdout, err = run_main(usage_report, ["--root", self.root, "--out", self.out])
        self.assertEqual(rc, 0, err)
        self.assertIn("fable 4", stdout)
        self.assertIn("models matching no multiplier (weighed at 1.0): model-a (2 calls, 2,040 tokens)", stdout)
        with open(os.path.join(self.out, "calls.csv"), encoding="utf-8") as fh:
            rows = list(csv.DictReader(fh))
        weighted = {r["message_id"]: float(r["weighted"]) for r in rows}
        base = 10 + 1000 * 0.025 + 10 * 5
        self.assertAlmostEqual(weighted["a1"], base * 4.0)
        self.assertAlmostEqual(weighted["a2"], base)
        self.assertAlmostEqual(weighted["a3"], base)
        self.assertAlmostEqual(weighted["a6"], (5 + 1.25) * 0.68)
        for name in ("calls.csv", "sessions.csv"):                   # errands.csv: see test_scripts, where an errand exists
            with open(os.path.join(self.out, name), encoding="utf-8") as fh:
                column = {r["weights_assumed"] for r in csv.DictReader(fh)}
            self.assertEqual(column, {tr.Weights().basis()}, name)

    def test_usage_report_flag_changes_both_the_cost_and_the_column(self):
        rc, _, err = run_main(usage_report, ["--root", self.root, "--out", self.out, "--model-weight", "fable=2"])
        self.assertEqual(rc, 0, err)
        with open(os.path.join(self.out, "calls.csv"), encoding="utf-8") as fh:
            rows = {r["message_id"]: r for r in csv.DictReader(fh)}
        self.assertAlmostEqual(float(rows["a1"]["weighted"]), (10 + 25 + 50) * 2.0)
        self.assertIn("fable=2", rows["a1"]["weights_assumed"])

    def test_usage_report_says_none_when_every_model_matches(self):
        rc, stdout, _ = run_main(usage_report, ["--root", self.root, "--out", self.out, "--model-weight", "model-a=1"])
        self.assertEqual(rc, 0)
        self.assertIn("models matching no multiplier (weighed at 1.0): none", stdout)

    def test_usage_whatif_names_them_in_the_text_and_the_csv(self):
        rc, stdout, err = run_main(usage_whatif, ["--root", self.root, "--out", self.out, "--thresholds", "500"])
        self.assertEqual(rc, 0, err)
        self.assertIn("fable 4", stdout)
        self.assertIn("models matching no multiplier (weighed at 1.0): model-a (2 calls, 2,040 tokens)", stdout)
        path = os.path.join(self.out, "whatif_A.csv")
        if os.path.exists(path):
            with open(path, encoding="utf-8") as fh:
                for r in csv.DictReader(fh):
                    self.assertEqual(r["weights_assumed"], tr.Weights().basis())

    def test_ac_measure_names_them(self):
        rc, stdout, err = run_main(ac_measure, ["--root", self.root, "--since", ts(0)])
        self.assertEqual(rc, 0, err)
        self.assertIn("fable 4", stdout)
        self.assertIn("models matching no multiplier (weighed at 1.0): model-a (2 calls, 2,040 tokens)", stdout)


class AcMeasureArithmetic(unittest.TestCase):
    """ac_measure used to price with the raw token weights; each place must now carry the multiplier."""

    def test_summary_cost_estimate_uses_the_model_multiplier(self):
        w = tr.Weights()
        ot, a, b, c = ac_measure.summary_cost_estimate(1000, 350, w, FABLE)
        self.assertAlmostEqual(ot, 100)
        self.assertAlmostEqual(a, (1000 * 0.025 + 100 * 5) * 4.0)
        self.assertAlmostEqual(b, (1000 * 1.0 + 100 * 5) * 4.0)
        self.assertAlmostEqual(c, (1000 * 1.25 + 100 * 5) * 4.0)

    def test_summary_cost_estimate_without_a_model_is_the_unscaled_price(self):
        ot, a, b, c = ac_measure.summary_cost_estimate(1000, 350, tr.Weights())
        self.assertAlmostEqual(a, 1000 * 0.025 + 100 * 5)

    def test_the_summarising_model_is_the_session_model_at_the_compaction(self):
        root, proj = synth.make_root()
        self.addCleanup(synth.rmtree, root)
        recs = call_lines("c1", ts(0), usage(inp=1), model=OPUS)
        recs += call_lines("c2", ts(1), usage(inp=1), model=FABLE)
        recs += [compact(ts(2), 100000, 8000, 20000)]
        recs += call_lines("c3", ts(3), usage(inp=1), model=OPUS)
        write_jsonl(os.path.join(proj, "s.jsonl"), recs)
        sessions = tr.load_sessions(root, None)
        tr.prepare(sessions, tr.Weights())
        s = sessions[0]
        self.assertEqual(ac_measure.summarising_model(s, tr.parse_ts(ts(2))), FABLE)
        self.assertEqual(ac_measure.summarising_model(s, tr.parse_ts(ts(0, 0))), OPUS)     # nothing before: the first call

    def synthetic_after_fable(self):
        root, proj = synth.make_root()
        self.addCleanup(synth.rmtree, root)
        recs = call_lines("c1", ts(0), usage(cr=1000), model=FABLE)
        recs += call_lines("c2", ts(1), usage(), model="<synthetic>")            # a placeholder: no tokens
        recs += [compact(ts(2), 1000, 200, 5000)]
        recs += call_lines("c3", ts(3), usage(cr=10), model=OPUS)
        write_jsonl(os.path.join(proj, "s.jsonl"), recs)
        return root

    def test_a_zero_usage_placeholder_is_not_taken_for_the_summarising_model(self):
        root = self.synthetic_after_fable()
        sessions = tr.load_sessions(root, None)
        tr.prepare(sessions, tr.Weights())
        s, when = sessions[0], tr.parse_ts(ts(2))
        self.assertEqual(ac_measure.summarising_model(s, when), FABLE)
        ot, a, _, _ = ac_measure.summary_cost_estimate(1000, 0, tr.Weights(), ac_measure.summarising_model(s, when))
        self.assertAlmostEqual(a, 1000 * 0.025 * 4.0)               # 100, not 25

    def test_the_first_call_after_is_the_fallback_and_it_too_skips_placeholders(self):
        root, proj = synth.make_root()
        self.addCleanup(synth.rmtree, root)
        recs = call_lines("c0", ts(0), usage(), model="<synthetic>") + [compact(ts(1), 1000, 200, 5000)]
        recs += call_lines("c1", ts(2), usage(), model="<synthetic>") + call_lines("c2", ts(3), usage(cr=10), model=FABLE)
        write_jsonl(os.path.join(proj, "s.jsonl"), recs)
        sessions = tr.load_sessions(root, None)
        tr.prepare(sessions, tr.Weights())
        self.assertEqual(ac_measure.summarising_model(sessions[0], tr.parse_ts(ts(1))), FABLE)

    def test_no_call_with_tokens_means_no_model(self):
        root, proj = synth.make_root()
        self.addCleanup(synth.rmtree, root)
        write_jsonl(os.path.join(proj, "s.jsonl"), call_lines("c0", ts(0), usage(), model="<synthetic>"))
        sessions = tr.load_sessions(root, None)
        tr.prepare(sessions, tr.Weights())
        self.assertEqual(ac_measure.summarising_model(sessions[0], tr.parse_ts(ts(1))), "")

    def test_the_output_says_the_model_is_inferred_and_names_it(self):
        root = self.synthetic_after_fable()
        rc, stdout, err = run_main(ac_measure, ["--root", root, "--since", ts(0)])
        self.assertEqual(rc, 0, err)
        lines = stdout.splitlines()
        head = next(i for i, ln in enumerate(lines) if "estimate of the summarising call" in ln)
        self.assertIn("inferred", lines[head])
        self.assertNotIn("the session's model at the compaction", stdout)     # stated as a fact: wrong
        row = next(ln for ln in lines[head:] if FABLE in ln)
        self.assertIn("(inferred)", row)
        self.assertIn(" 100 ", row)                                           # A = 1000 * 0.025 * 4

    def test_visible_cost_and_decomposition_carry_the_multiplier(self):
        root, proj = synth.make_root()
        self.addCleanup(synth.rmtree, root)
        recs = call_lines("c1", ts(0), usage(inp=0, cr=1000), model=FABLE)
        recs += [compact(ts(1), 10000, 2000, 5000)]
        recs += call_lines("c2", ts(2), usage(inp=0, out=10, cc=100, cr=500), model=FABLE)
        recs += call_lines("c3", ts(3), usage(inp=0, cr=500), model=FABLE)
        write_jsonl(os.path.join(proj, "s.jsonl"), recs)
        sessions = tr.load_sessions(root, None)
        w = tr.Weights()
        tr.prepare(sessions, w)
        lines = []
        ac_measure.section_visible_cost(lines.append, sessions[0], tr.parse_ts(ts(0)), w)
        text = "\n".join(lines)
        dropped = 10000 - 2000
        saved = 2 * dropped * 0.025 * 4.0               # two later calls, each reading the dropped tokens
        self.assertIn(f"{saved:,.0f}", text)
        self.assertIn("model multiplier", text)          # the saving's line says it is scaled
        first = 100 * 1.25 * 4.0                         # the one first call's cache write
        self.assertIn(f"summed visible first-call cache-write cost: {first:,.0f}", text)
