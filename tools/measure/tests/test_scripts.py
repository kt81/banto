"""The scripts' pure helpers and an end-to-end run of each main() on synthetic data."""
import contextlib
import csv
import io
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock

import synth
from synth import TOOL, call_lines, compact, delivery, run_main, ts, usage, user_text, write_jsonl

import ac_measure
import quality_report as qr
import transcripts as tr
import usage_report
import usage_whatif


class VerdictTests(unittest.TestCase):
    def test_explicit_verdicts(self):
        self.assertEqual(qr.verdict_of("Verdict: PASS - nothing to fix"), "PASS")
        self.assertEqual(qr.verdict_of("verdict FAIL after the second run"), "FAIL")
        self.assertEqual(qr.verdict_of("FAIL: two blockers below"), "FAIL")
        self.assertEqual(qr.verdict_of("PASS with two nits"), "PASS")

    def test_derived_verdicts_carry_a_star(self):
        self.assertEqual(qr.verdict_of("Review complete. Nothing to report."), "OK*")
        self.assertEqual(qr.verdict_of("There is no blocker in this change."), "OK*")
        self.assertEqual(qr.verdict_of("The invariant HOLDS on all three paths."), "OK*")
        self.assertEqual(qr.verdict_of("BLOCKER 1: the lock is taken twice."), "BLK*")
        self.assertEqual(qr.verdict_of("This DOES NOT HOLD for the second case."), "BLK*")

    def test_refusal_and_silence_are_no_verdict(self):
        self.assertIsNone(qr.verdict_of("I cannot verify this without the log. Verdict: PASS"))
        self.assertIsNone(qr.verdict_of("Looks fine to me."))
        self.assertIsNone(qr.verdict_of(""))

    def test_finding_tokens(self):
        text = "BLOCKER-1: the lock\nH2: the cache\n- NIT: spelling\nprose mentioning H3 inline"
        self.assertEqual(qr.finding_tokens(text), ["BLOCKER-1", "H2", "NIT"])
        self.assertEqual(qr.findings_of(text), (3, 1))


class ClassifyTests(unittest.TestCase):
    NAMES = dict(qr.ROLE_BY_NAME)

    def test_keyword_kinds(self):
        self.assertEqual(qr.classify("welcome aboard", None, self.NAMES), ("other", "keyword"))
        self.assertEqual(qr.classify("go A: survey the parser", None, self.NAMES), ("measurement/investigation", "keyword"))
        self.assertEqual(qr.classify("independent review of the change", None, self.NAMES), ("review", "keyword"))
        self.assertEqual(qr.classify("one sentence to fix in README.md", None, self.NAMES), ("docs", "keyword"))
        self.assertEqual(qr.classify("implement the thing", None, self.NAMES), ("implementation", "keyword"))
        self.assertEqual(qr.classify("hello", None, self.NAMES), ("other", "default"))

    def test_worker_name_beats_review_and_impl_keywords_but_not_the_first_two_lists(self):
        self.assertEqual(qr.classify("implement the thing", "reviewer", self.NAMES), ("review", "worker-name"))
        self.assertEqual(qr.classify("welcome, reviewer", "reviewer", self.NAMES), ("other", "keyword"))

    def test_a_review_instruction_beats_a_worker_name(self):
        """A Worker named for implementation, or docs, that is sent to review is doing a review."""
        for name in ("実装", "implementation", "docs", "builder"):
            names = dict(self.NAMES, builder="implementation")
            self.assertEqual(qr.classify("independent review of the change", name, names), ("review", "keyword"), name)
            self.assertEqual(qr.classify("Re-review of the fix round", name, names), ("review", "keyword"), name)

    def test_a_name_still_beats_the_docs_and_implementation_words_and_loses_to_the_first_two_lists(self):
        self.assertEqual(qr.classify("fix the typo in README.md", "実装", self.NAMES), ("implementation", "worker-name"))
        self.assertEqual(qr.classify("implement the thing", "docs", self.NAMES), ("docs", "worker-name"))
        self.assertEqual(qr.classify("go A: survey the parser", "reviewer", self.NAMES), ("measurement/investigation", "keyword"))
        self.assertEqual(qr.classify("welcome aboard", "実装", self.NAMES), ("other", "keyword"))

    def test_review_of_beats_a_name_only_when_it_opens_the_instruction(self):
        names = dict(self.NAMES, builder="implementation")
        self.assertEqual(qr.classify("Fix the parser; review of the old notes is below", "builder", names), ("implementation", "worker-name"))
        self.assertEqual(qr.classify("Review of the README wording, then apply the fix", "docs", names), ("review", "keyword"))
        self.assertEqual(qr.classify("Fix the parser; review of the old notes is below", None, self.NAMES), ("review", "keyword"))

    def test_thanks_for_a_review_does_not_make_a_review(self):
        self.assertEqual(qr.classify("Your review of X was useful. Now implement Y", None, self.NAMES), ("implementation", "keyword"))

    def test_role_name_extension(self):
        names = dict(self.NAMES, builder="implementation")
        self.assertEqual(qr.classify("anything", "builder", names), ("implementation", "worker-name"))


class NameLearningTests(unittest.TestCase):
    def analysis(self):
        return qr.Analysis(tr.parse_ts(ts(0)), None, 7, dict(qr.ROLE_BY_NAME))

    def deliver(self, minute, *senders):
        return ("deliver", tr.parse_ts(ts(minute)), [(s, n, "to you", "text") for s, n in senders])

    def test_a_cleared_name_is_forgotten(self):
        """banto prints no name in the header of a member whose name the Director cleared."""
        an = self.analysis()
        an.learn_names([self.deliver(1, ("worker-1", "reviewer"), ("worker-2", "docs")),
                        self.deliver(5, ("worker-1", None)),
                        self.deliver(9, ("worker-1", "docs"))])
        at = lambda w, m: an.name_of(w, tr.parse_ts(ts(m)))
        self.assertEqual([at("worker-1", m) for m in (0, 1, 4, 5, 8, 9, 30)], [None, "reviewer", "reviewer", None, None, "docs", "docs"])
        self.assertEqual(at("worker-2", 30), "docs")           # worker-2 sent nothing since: nothing shows its name is gone

    def test_a_worker_that_never_had_a_name_has_none(self):
        an = self.analysis()
        an.learn_names([self.deliver(1, ("worker-3", None))])
        self.assertIsNone(an.name_of("worker-3", tr.parse_ts(ts(2))))


class ChangeGroupingTests(unittest.TestCase):
    def test_commits_within_the_gap_merge_and_changes_own_the_time_before_them(self):
        def commit(h, minute):
            return {"hash": h, "ts": tr.parse_ts(ts(minute)), "subject": h, "files": [h + ".rs"]}
        commits = [commit("a1", 0), commit("a2", 10), commit("b1", 40)]
        since = tr.parse_ts(ts(0)) - (tr.parse_ts(ts(1)) - tr.parse_ts(ts(0))) * 5
        changes = qr.group_changes(commits, since, 15)
        self.assertEqual([[c["hash"] for c in ch["commits"]] for ch in changes], [["a1", "a2"], ["b1"]])
        self.assertEqual(changes[1]["start"], changes[0]["end"])
        self.assertEqual(changes[1]["prev_hash"], "a2")
        self.assertEqual(changes[0]["files"], ["a1.rs", "a2.rs"])
        self.assertIs(qr.change_of(changes, since, tr.parse_ts(ts(5))), changes[0])
        self.assertIs(qr.change_of(changes, since, tr.parse_ts(ts(30))), changes[1])
        self.assertIsNone(qr.change_of(changes, since, tr.parse_ts(ts(50))))   # after the last commit
        self.assertIsNone(qr.change_of(changes, since, since))                  # window start is exclusive

    def test_parse_git_log_skips_malformed_blocks(self):
        raw = "\x01abc1234\x022025-03-10T10:00:00+00:00\x02fix: one\nsrc/a.rs\nsrc/b.rs\n\n" \
              "\x01broken block without separators\n" \
              "\x01def5678\x02not-a-date\x02fix: two\n"
        commits = qr.parse_git_log(raw)
        self.assertEqual([(c["hash"], c["subject"], c["files"]) for c in commits], [("abc1234", "fix: one", ["src/a.rs", "src/b.rs"])])


class AcMeasureHelperTests(unittest.TestCase):
    def test_bash_read_paths(self):
        self.assertEqual(ac_measure.bash_read_paths("cat src/main.rs | grep foo"), ["src/main.rs"])
        self.assertEqual(ac_measure.bash_read_paths("ls src && head -5 notes.txt"), ["notes.txt"])
        self.assertEqual(ac_measure.bash_read_paths("echo hi > out.txt"), [])

    def test_paths_are_normalised_against_the_session_cwd(self):
        cwd = "c:/work/proj/"
        self.assertEqual(ac_measure.norm("C:\\work\\proj\\src\\A.rs", cwd), "src/a.rs")
        self.assertEqual(ac_measure.norm("/c/work/proj/src/a.rs", cwd), "src/a.rs")
        self.assertEqual(ac_measure.norm("./src/a.rs", cwd), "src/a.rs")

    def test_summary_cost_estimate(self):
        # 1000 tokens in, 350 characters out at 3.5 characters per token = 100 output tokens
        ot, a, b, c = ac_measure.summary_cost_estimate(1000, 350, tr.Weights())
        self.assertAlmostEqual(ot, 100)
        self.assertAlmostEqual(a, 1000 * 0.025 + 100 * 5)   # input as cache read
        self.assertAlmostEqual(b, 1000 * 1.0 + 100 * 5)     # input uncached
        self.assertAlmostEqual(c, 1000 * 1.25 + 100 * 5)    # input as cache write


class EndToEnd(unittest.TestCase):
    def setUp(self):
        self.root, self.proj = synth.make_root()
        self.out = tempfile.mkdtemp(prefix="measure-out-")
        self.addCleanup(synth.rmtree, self.root)
        self.addCleanup(synth.rmtree, self.out)
        recs = synth.worker_records() + [compact(ts(4), 101010, 8000, 20000)] + call_lines("m3", ts(5), usage(cc=8000))
        write_jsonl(os.path.join(self.proj, "worker.jsonl"), recs)

    def test_usage_report(self):
        rc, stdout, _ = run_main(usage_report, ["--root", self.root, "--out", self.out])
        self.assertEqual(rc, 0)
        self.assertIn("USAGE REPORT", stdout)
        self.assertIn(tr.ASSUMPTION, stdout)           # the weights caveat stays in the output
        with open(os.path.join(self.out, "calls.csv"), encoding="utf-8") as fh:
            self.assertEqual(len(list(csv.DictReader(fh))), 4)
        with open(os.path.join(self.out, "errands.csv"), encoding="utf-8") as fh:
            rows = list(csv.DictReader(fh))
        self.assertEqual([(r["calls"], r["sends"], r["weighted"]) for r in rows], [("2", "1", "6370.0")])
        self.assertEqual({r["weights_assumed"] for r in rows}, {tr.Weights().basis()})

    def test_window_filters_calls(self):
        rc, stdout, _ = run_main(usage_report, ["--root", self.root, "--out", self.out, "--since", ts(3)])
        self.assertEqual(rc, 0)
        self.assertIn("calls in window 2;", stdout)    # m2 (minute 3) and m3 (minute 5)

    def test_out_inside_a_git_working_tree_is_refused(self):
        repo = os.path.join(self.out, "repo")
        os.makedirs(os.path.join(repo, ".git"))
        target = os.path.join(repo, "results")
        for module, extra in ((usage_report, []), (usage_whatif, []), (qr, ["--repo", self.root])):
            rc, _, err = run_main(module, ["--root", self.root, "--out", target] + extra)
            self.assertEqual(rc, 2, module.__name__)
            self.assertIn("git working tree", err)
        self.assertFalse(os.path.exists(target))
        rc, _, _ = run_main(usage_report, ["--root", self.root, "--out", target, "--force-out"])
        self.assertEqual(rc, 0)

    def test_bad_arguments_are_errors_not_tracebacks(self):
        rc, _, err = run_main(usage_report, ["--root", self.root, "--out", self.out, "--since", "not-a-date"])
        self.assertEqual(rc, 2)
        rc, _, err = run_main(usage_report, ["--root", self.root, "--out", self.out, "--model-weight", "oops"])
        self.assertEqual(rc, 2)

    def test_usage_whatif(self):
        rc, stdout, _ = run_main(usage_whatif, ["--root", self.root, "--out", self.out, "--thresholds", "50000"])
        self.assertEqual(rc, 0)
        self.assertIn("COUNTERFACTUAL A", stdout)
        self.assertIn(tr.ASSUMPTION, stdout)
        self.assertTrue(os.path.exists(os.path.join(self.out, "whatif_A.csv")))

    def test_ac_measure(self):
        rc, stdout, err = run_main(ac_measure, ["--root", self.root, "--since", ts(2)])
        self.assertEqual(rc, 0, err)
        self.assertIn("compactions since T", stdout)
        self.assertIn("101,010", stdout)               # the compaction's preTokens

    def test_ac_measure_requires_since(self):
        with self.assertRaises(SystemExit) as cm, contextlib.redirect_stderr(io.StringIO()):
            ac_measure.main(["--root", self.root])
        self.assertEqual(cm.exception.code, 2)


PROJECT = "q-proj"


@unittest.skipUnless(shutil.which("git"), "git is not installed")
class QualityEndToEnd(unittest.TestCase):
    def setUp(self):
        self.base = tempfile.mkdtemp(prefix="measure-quality-")
        self.addCleanup(synth.rmtree, self.base)
        self.root = os.path.join(self.base, "claude", "projects")
        self.repo = os.path.join(self.base, "repo")
        os.makedirs(self.repo)
        # A project directory named after the repository's path (what Claude Code does, and what
        # project_files derives) is as long as that path, which lies under the temp directory: twice
        # a deep TEMP is past Windows' 260 characters. So these tests select it with --project and
        # a short name; the derived name has a test of its own on a short path.
        self.proj = os.path.join(self.root, PROJECT)
        os.makedirs(self.proj)
        self.out = os.path.join(self.base, "out")

        # Worker: briefed into brigade 7, one errand from the Director, reports a PASS
        write_jsonl(os.path.join(self.proj, "worker-session.jsonl"),
                    [user_text(ts(0), "You are worker-1, a Worker in banto brigade 7. Check mail.")] + synth.worker_records())
        # Director: names a member, hears from worker-1 (display name 'builder'), is told "thanks", asks the operator nothing
        d = []
        d += call_lines("d1", ts(0, 10), usage(cr=50), tools=[(TOOL + "name_member", "n1", {})])
        d.append(user_text(ts(0, 20), "please start"))
        d += call_lines("d2", ts(0, 30), usage(cr=60), tools=[(TOOL + "check_messages", "c1", {})])
        d.append(synth.tool_result(ts(0, 40), "c1", delivery(('worker-1 "builder" — to you', "ready"))))
        d += call_lines("d3", ts(4), usage(cr=70), tools=[(TOOL + "check_messages", "c2", {})])
        d.append(synth.tool_result(ts(4, 10), "c2", delivery(('worker-1 "builder" — to you', "Verdict: PASS"))))
        d.append(user_text(ts(6), "it is still broken, nothing appears"))
        write_jsonl(os.path.join(self.proj, "director-session.jsonl"), d)

        env = dict(os.environ, GIT_AUTHOR_DATE="2025-03-10T10:30:00+00:00", GIT_COMMITTER_DATE="2025-03-10T10:30:00+00:00")
        git = ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false", "-C", self.repo]
        subprocess.run(["git", "-C", self.repo, "init", "-q"], check=True, capture_output=True)
        with open(os.path.join(self.repo, "a.txt"), "w") as fh:
            fh.write("x")
        subprocess.run(git + ["add", "a.txt"], check=True, capture_output=True)
        subprocess.run(git + ["commit", "-q", "-m", "fix: the thing"], check=True, capture_output=True, env=env)

    def run_quality(self, *extra):
        return run_main(qr, ["--repo", self.repo, "--root", self.root, "--project", PROJECT, "--out", self.out,
                             "--since", "2025-03-10T09:00:00Z", "--until", "2025-03-10T12:00:00Z"] + list(extra))

    def test_the_project_directory_is_found_by_the_repository_path(self):
        repo = os.path.abspath(os.path.join(os.sep, "r"))        # never created, only named: its derived name is short
        pdir = os.path.join(self.root, tr.project_dir_name(repo), "sub")
        os.makedirs(pdir)
        path = write_jsonl(os.path.join(pdir, "s.jsonl"), call_lines("a", ts(0), usage(inp=1)))
        self.assertEqual(qr.project_files(self.root, None, repo)[1], [path])
        self.assertEqual(qr.project_files(self.root, "no-such-project", repo)[1], [])

    def test_one_change_with_its_errand_review_and_cost(self):
        rc, stdout, err = self.run_quality("--role-name", "builder=implementation")
        self.assertEqual(rc, 0, err)
        self.assertIn("brigade 7", stdout)             # taken from the Worker's briefing; the Director's own transcript has none
        with open(os.path.join(self.out, "changes.json"), encoding="utf-8") as fh:
            changes = json.load(fh)["changes"]
        self.assertEqual(len(changes), 1)
        ch = changes[0]
        self.assertEqual([(e[0], e[1], e[2]) for e in ch["errands"]], [("worker-1", "implementation", "worker-name")])
        self.assertEqual([r["verdict"] for r in ch["reviews"]], [])      # an implementation errand is not a review
        self.assertEqual(ch["usage"]["worker-1"][0], 3)
        self.assertAlmostEqual(ch["usage"]["worker-1"][1], 8870)
        for name in ("report.txt", "candidates_a.txt", "candidates_b.txt", "errands_dump.txt"):
            self.assertTrue(os.path.exists(os.path.join(self.out, name)), name)

    def test_a_cleared_name_no_longer_classifies_the_errand(self):
        d = []
        d += call_lines("d1", ts(0, 10), usage(cr=50), tools=[(TOOL + "name_member", "n1", {})])
        d += call_lines("d2", ts(0, 30), usage(cr=60), tools=[(TOOL + "check_messages", "c1", {})])
        d.append(synth.tool_result(ts(0, 40), "c1", delivery(('worker-1 "builder" — to you', "ready"))))
        d += call_lines("d3", ts(0, 45), usage(cr=60), tools=[(TOOL + "check_messages", "c2", {})])
        d.append(synth.tool_result(ts(0, 50), "c2", delivery(('worker-1 — to you', "ready again"))))       # the name was cleared
        write_jsonl(os.path.join(self.proj, "director-session.jsonl"), d)
        rc, _, err = self.run_quality("--role-name", "builder=implementation")
        self.assertEqual(rc, 0, err)
        with open(os.path.join(self.out, "changes.json"), encoding="utf-8") as fh:
            ch = json.load(fh)["changes"][0]
        self.assertEqual([(e[1], e[2]) for e in ch["errands"]], [("implementation", "keyword")])      # by the instruction's own word, not the old name

    def test_the_change_says_what_ran(self):
        rc, stdout, err = self.run_quality()
        self.assertEqual(rc, 0, err)
        with open(os.path.join(self.out, "changes.json"), encoding="utf-8") as fh:
            ch = json.load(fh)["changes"][0]
        ran = {(r["role"], r["model"], r["effort"]): r["calls"] for r in ch["ran"]}
        self.assertEqual(ran, {("worker", "model-a", "?"): 3, ("director", "model-a", "?"): 3})
        self.assertIn("what ran (role model/effort: calls, weighted): ", stdout)

    def test_a_bad_model_weight_is_refused_before_anything_is_written(self):
        for value in ("fable", "fable=nan", "=2", "fable=1001"):
            rc, _, err = self.run_quality("--model-weight", value)
            self.assertEqual(rc, 2, value)
            self.assertIn("--model-weight", err)
        self.assertFalse(os.path.exists(self.out))

    def test_without_a_role_name_the_keyword_decides(self):
        rc, _, err = self.run_quality()
        self.assertEqual(rc, 0, err)
        with open(os.path.join(self.out, "changes.json"), encoding="utf-8") as fh:
            ch = json.load(fh)["changes"][0]
        self.assertEqual([(e[1], e[2]) for e in ch["errands"]], [("implementation", "keyword")])

    def test_director_is_found_on_its_own_and_can_be_named(self):
        rc, stdout, _ = self.run_quality()
        self.assertEqual(rc, 0)
        self.assertIn("Director candidates", stdout)
        self.assertIn("director-session"[:8], stdout)
        synth.rmtree(self.out)
        rc, stdout, _ = self.run_quality("--director", "director-s")
        self.assertEqual(rc, 0)
        self.assertNotIn("Director candidates", stdout)

    def test_changes_json_names_the_weights_and_the_assumption(self):
        rc, _, err = self.run_quality()
        self.assertEqual(rc, 0, err)
        with open(os.path.join(self.out, "changes.json"), encoding="utf-8") as fh:
            data = json.load(fh)
        self.assertEqual(data["weights"], {"input": 1, "cache_write": 1.25, "cache_read": 0.025, "output": 5,
                                           "model_multipliers": {"fable": 4, "haiku": 0.68, "opus": 1, "sonnet": 0.68},
                                           "other_models": 1})
        # the synthetic model name matches no family: counted and named, not silently weighed at 1.0
        self.assertEqual(list(data["unmatched_models"]), ["model-a"])
        self.assertGreaterEqual(data["unmatched_models"]["model-a"]["calls"], 3)
        self.assertEqual(data["assumption"], tr.ASSUMPTION)
        with open(os.path.join(self.out, "report.txt"), encoding="utf-8") as fh:
            report = fh.read()
        self.assertIn(tr.ASSUMPTION, report)
        self.assertIn("fable 4", report)
        self.assertIn("models matching no multiplier (weighed at 1.0): model-a (", report)

    def test_a_symlinked_output_file_is_never_written_through(self):
        os.makedirs(self.out)
        victim = os.path.join(self.base, "victim")
        with open(victim, "w") as fh:
            fh.write("SENTINEL")
        try:
            os.symlink(victim, os.path.join(self.out, "report.txt"))
        except (OSError, NotImplementedError, AttributeError):
            self.skipTest("this platform cannot create symlinks here")
        rc, _, err = self.run_quality()
        self.assertEqual(rc, 2, err)
        with open(victim) as fh:
            self.assertEqual(fh.read(), "SENTINEL")

    def test_an_agent_home_is_refused_as_out(self):
        home = os.path.join(self.base, "home")
        with mock.patch.dict(os.environ, {"HOME": home, "USERPROFILE": home}):
            for force in ([], ["--force-out"]):
                rc, _, err = run_main(qr, ["--repo", self.repo, "--root", self.root,
                                           "--out", os.path.join(home, ".codex", "o")] + force)
                self.assertEqual(rc, 2)
                self.assertIn("agent", err)

    def test_bad_role_name_is_an_error(self):
        rc, _, err = self.run_quality("--role-name", "x=bogus")
        self.assertEqual(rc, 2)
        self.assertIn("--role-name", err)

    def test_operator_problem_report_after_a_pass_is_a_candidate(self):
        # the operator says "still broken" at minute 6, three minutes after the review-free PASS above:
        # no review errand exists, so there is no PASS trigger and no candidate
        rc, _, _ = self.run_quality()
        self.assertEqual(rc, 0)
        with open(os.path.join(self.out, "candidates_a.txt"), encoding="utf-8") as fh:
            self.assertIn("(0)", fh.read())


if __name__ == "__main__":
    unittest.main()
