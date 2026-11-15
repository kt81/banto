import os
import unittest

import synth
from synth import write_jsonl

import transcripts as tr
import usage_whatif as wi


class WhatIfArithmetic(unittest.TestCase):
    """One Worker: m0 outside any errand, then an errand of m1 and m2.

        call  input  output  cache-write  cache-read   context
        m0        0       0            0      100000    100000   (between errands)
        m1       10      10            0      100000    100010
        m2       10      10         1000      100000    101010

    Weights: input 1, cache-write 1.25, cache-read 0.1, output 5. All figures
    below were worked out by hand.
    """

    def setUp(self):
        root, proj = synth.make_root()
        self.addCleanup(synth.rmtree, root)
        write_jsonl(os.path.join(proj, "w.jsonl"), synth.worker_records())
        self.sessions = tr.load_sessions(root)
        self.w = tr.Weights()
        tr.prepare(self.sessions, self.w)
        self.s = self.sessions[0]
        wi.attach_order(self.s)

    def test_actual_costs(self):
        c = self.s.calls
        self.assertAlmostEqual(c["m0"].weighted, 10000)                 # 100000 * 0.1
        self.assertAlmostEqual(c["m1"].weighted, 10 + 10000 + 50)       # input + cache-read + 10 output * 5
        self.assertAlmostEqual(c["m2"].weighted, 10 + 1250 + 10000 + 50)

    def test_baselines(self):
        self.assertEqual(self.s.first_ctx, 100000)
        self.assertEqual(wi.baseline(self.s, "first"), (100000, "first"))
        self.assertEqual(wi.baseline(self.s, "min")[0], 100000)  # no compaction measured: falls back to the first call

    def test_counterfactual_a_fresh_worker_per_errand(self):
        B = 10010
        r = wi.counterfactual_a(self.s, B, self.w)
        # delta = 100010 - B = 90000 comes off the cache reads of both errand calls;
        # the errand's first call also pays one cache write of B
        #   m1: 10 + 1.25 * 10010 + 0.1 * 10000 + 50 = 13572.5
        #   m2: 10 + 1.25 * 1000  + 0.1 * 10000 + 50 =  2310
        self.assertEqual(r["errands"], 1)
        self.assertAlmostEqual(r["counterfactual"], 13572.5 + 2310 + 10000)   # m0 stays as it was
        self.assertAlmostEqual(r["actual"], 10060 + 11310 + 10000)
        self.assertAlmostEqual(r["between"], 10000)
        self.assertAlmostEqual(r["between_at_b"], 1001)   # m0's cache read cut to B: 10010 * 0.1
        self.assertEqual(r["clamped"], 0)

    def test_counterfactual_a_clamps_cache_reads_at_zero(self):
        # B = 1 makes delta 100009, more than either errand call's 100000 cache-read tokens
        self.assertEqual(wi.counterfactual_a(self.s, 1, self.w)["clamped"], 2)
        self.assertEqual(wi.counterfactual_a(self.s, 10010, self.w)["clamped"], 0)

    def test_counterfactual_b_compact_at_threshold(self):
        # T = 50000, a compaction brings the context to P = 20000, and costs one extra
        # call: all of the pre-compaction context read from the cache plus 100 output tokens.
        P = {"director": None, "worker": 20000, "other": None}
        out = wi.counterfactual_b([self.s], 50000, P, {"director": 0, "worker": 100, "other": 0}, self.w)
        actual, cf, n, extra = out["worker"]
        # m0: context 100000 > T -> compaction; extra = 100000 * 0.1 + 100 * 5 = 10500;
        #     it then reads 20000 (not 100000) and pays one cache write of P: 1.25 * 20000 + 0.1 * 20000 = 27000
        # m1: reads 20000 -> 10 + 2000 + 50 = 2060
        # m2: reads 20000, writes 1000 -> 10 + 1250 + 2000 + 50 = 3310
        self.assertEqual(n, 1)
        self.assertAlmostEqual(extra, 10500)
        self.assertAlmostEqual(actual, 10000 + 10060 + 11310)
        self.assertAlmostEqual(cf, 27000 + 10500 + 2060 + 3310)

    def test_a_role_without_a_p_is_skipped(self):
        P = {"director": None, "worker": None, "other": None}
        self.assertEqual(dict(wi.counterfactual_b([self.s], 50000, P, {"worker": 0}, self.w)), {})


if __name__ == "__main__":
    unittest.main()
