"""what_ran.py: the effort a call ran with, the Goinkyo as a role, configured versus observed, and
the price what-if, on synthetic transcripts and a synthetic config."""
import csv
import os
import tempfile
import unittest

import synth
from synth import TOOL, call_lines, delivery, prompt_snapshot, run_main, ts, usage, user_text, write_jsonl

import transcripts as tr
import what_ran

WORKER_PROMPT = "You are worker-2, a Worker in banto brigade 5. Check mail."
GOINKYO_PROMPT = "You are the Goinkyo of banto brigade 5. Answer the Director."
U = usage(inp=100, out=10)          # raw 150; weighted 150 on opus, 102 on sonnet or haiku, 600 on fable
OPUS, SONNET, HAIKU, FABLE = "claude-opus-x", "claude-sonnet-x", "claude-haiku-x", "claude-fable-x"


def load(path):
    root = os.path.dirname(os.path.dirname(path))
    return tr.read_session(path, root)


def _path(base, *parts):
    """A path under `base`, its directories made."""
    p = os.path.join(base, *parts)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    return p


class EffortTests(unittest.TestCase):
    def setUp(self):
        self.root, self.proj = synth.make_root()
        self.addCleanup(synth.rmtree, self.root)

    def test_effort_is_read_from_the_top_of_the_assistant_record(self):
        path = write_jsonl(os.path.join(self.proj, "s.jsonl"),
                           call_lines("a", ts(0), U, effort="xhigh") + call_lines("b", ts(1), U, effort=" high ")
                           + call_lines("c", ts(2), U))
        s = load(path)
        self.assertEqual({k: c.effort for k, c in s.calls.items()}, {"a": "xhigh", "b": "high", "c": "?"})

    def test_a_malformed_effort_reads_as_unknown_and_costs_no_record(self):
        recs = call_lines("a", ts(0), U, effort=5) + call_lines("b", ts(1), U, effort="") + call_lines("c", ts(2), U, effort=["max"])
        s = load(write_jsonl(os.path.join(self.proj, "s.jsonl"), recs))
        self.assertEqual(s.skipped, 0)
        self.assertEqual([c.effort for c in s.calls.values()], ["?", "?", "?"])


class RoleTests(unittest.TestCase):
    def setUp(self):
        self.root, self.proj = synth.make_root()
        self.addCleanup(synth.rmtree, self.root)

    def member(self, name, records):
        return load(write_jsonl(os.path.join(self.proj, name + ".jsonl"), records))

    def mail(self, mid="a"):
        return call_lines(mid, ts(1), U, tools=[(TOOL + "check_messages", "c1", {})])

    def test_a_goinkyo_is_told_apart_by_the_briefing_in_its_first_snapshot(self):
        s = self.member("g", [prompt_snapshot(ts(0), GOINKYO_PROMPT)] + self.mail())
        self.assertEqual(s.role, "goinkyo")
        self.assertEqual(s.members, [("goinkyo", "5")])

    def test_a_worker_with_a_worker_briefing_stays_a_worker_and_is_named(self):
        s = self.member("w", [prompt_snapshot(ts(0), WORKER_PROMPT)] + self.mail())
        self.assertEqual((s.role, s.members), ("worker", [("worker-2", "5")]))

    def test_a_system_prompt_of_text_blocks_is_searched_too(self):
        blocks = [{"type": "text", "text": "intro"}, {"type": "text", "text": GOINKYO_PROMPT}]
        self.assertEqual(self.member("g", [prompt_snapshot(ts(0), blocks)] + self.mail()).role, "goinkyo")

    def test_quoted_briefing_text_does_not_make_a_goinkyo(self):
        """A Worker that read the Goinkyo briefing while working on it, in a tool result."""
        recs = [prompt_snapshot(ts(0), WORKER_PROMPT)] + self.mail()
        recs.append(synth.tool_result(ts(2), "c1", delivery(("director — to you", GOINKYO_PROMPT))))
        recs.append(user_text(ts(3), GOINKYO_PROMPT))
        self.assertEqual(self.member("w", recs).role, "worker")

    def test_the_first_snapshot_decides_and_a_later_one_is_not_read(self):
        """A first snapshot with no briefing makes a worker, whatever a later snapshot carries."""
        recs = [prompt_snapshot(ts(0), "You are a helpful assistant."), prompt_snapshot(ts(1), GOINKYO_PROMPT)] + self.mail()
        s = self.member("late", recs)
        self.assertEqual((s.role, s.members), ("worker", []))

    def test_no_snapshot_means_a_worker(self):
        """Claude Code before 2.1.265 wrote none: such a Goinkyo cannot be told from a Worker."""
        s = self.member("old", [user_text(ts(0), GOINKYO_PROMPT)] + self.mail())
        self.assertEqual((s.role, s.members), ("worker", []))

    def test_a_director_is_a_director_whatever_it_quotes(self):
        recs = [prompt_snapshot(ts(0), GOINKYO_PROMPT)] + call_lines("a", ts(1), U, tools=[(TOOL + "name_member", "n1", {})])
        self.assertEqual(self.member("d", recs).role, "director")

    def test_the_goinkyo_gets_errands_like_a_worker(self):
        recs = [prompt_snapshot(ts(0), GOINKYO_PROMPT)]
        recs += call_lines("a", ts(1), U, tools=[(TOOL + "check_messages", "c1", {})])
        recs.append(synth.tool_result(ts(2), "c1", delivery(("director — to you", "a question"))))
        recs += call_lines("b", ts(3), U, tools=[(TOOL + "send_to_peer", "s1", {"to": "director", "text": "an answer"})])
        sessions = [self.member("g", recs)]
        errands, _ = tr.prepare(sessions, tr.Weights())
        self.assertEqual([(e.role, e.kind, len(e.calls)) for e in errands], [("goinkyo", "errand", 1)])


class ConfigTests(unittest.TestCase):
    TEXT = """# a comment
workers = 9
[brigade]
workers = 3
worker_model = "haiku"   # trailing comment
worker_effort = 'low'
goinkyo_model = ""
goinkyo_effort = max
[other]
worker_model = "elsewhere"
"""

    def test_only_the_brigade_table_is_read(self):
        got = what_ran.parse_brigade_table(self.TEXT)
        self.assertEqual((got["worker_model"], got["worker_effort"], got["goinkyo_model"], got["goinkyo_effort"]),
                         ("haiku", "low", "", "max"))
        self.assertEqual(got["workers"], "3")

    def test_the_brigade_header_may_have_blanks_inside_its_brackets(self):
        got = what_ran.parse_brigade_table('[ brigade ]\nworker_model = "opus"\n[ brigade.extra ]\nworker_model = "haiku"\n')
        self.assertEqual(got, {"worker_model": "opus"})

    def test_a_product_table_is_read_by_its_own_name(self):
        text = ('[brigade]\nworker_model = "flat"\n[ brigade . claude ]\nworker_model = "haiku"  # c\n'
                'goinkyo_effort = \'low\'\n[brigade.codex]\nworker_model = "terra"\n')
        self.assertEqual(what_ran.parse_table(text, "brigade.claude"), {"worker_model": "haiku", "goinkyo_effort": "low"})
        self.assertEqual(what_ran.parse_table(text, "brigade.codex"), {"worker_model": "terra"})
        self.assertEqual(what_ran.parse_brigade_table(text), {"worker_model": "flat"})

    def write(self, text):
        d = tempfile.mkdtemp(prefix="measure-cfg-")
        self.addCleanup(synth.rmtree, d)
        return self._put(os.path.join(d, "config.toml"), text)

    def _put(self, path, text):
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(text)
        return path

    def test_the_built_in_defaults_when_there_is_no_file(self):
        expected, note = what_ran.load_expected(os.path.join(tempfile.gettempdir(), "no-such-measure-config.toml"))
        self.assertEqual(expected, {"worker": ("sonnet", "high"), "goinkyo": ("opus", "xhigh")})
        self.assertIn("--config names a missing file", note)
        self.assertIn("banto would not start", note)

    def test_the_file_replaces_a_default_and_an_empty_string_expects_nothing(self):
        expected, note = what_ran.load_expected(self.write(self.TEXT))
        self.assertEqual(expected["worker"], ("haiku", "low"))
        self.assertEqual(expected["goinkyo"], (None, "max"))
        self.assertIn("modified", note)

    def test_a_key_the_file_leaves_out_keeps_its_default(self):
        expected, _ = what_ran.load_expected(self.write("[brigade]\nworker_model = \"opus\"\n"))
        self.assertEqual(expected["worker"], ("opus", "high"))

    def test_the_claude_table_is_read_and_beats_the_legacy_key_one_key_at_a_time(self):
        table_only = '[brigade.claude]\nworker_model = "haiku"\nworker_effort = "low"\ngoinkyo_model = ""\n'
        expected, _ = what_ran.load_expected(self.write(table_only))
        self.assertEqual(expected["worker"], ("haiku", "low"))
        self.assertEqual(expected["goinkyo"], (None, "xhigh"))
        both = ('[brigade]\nworker_model = "flat"\nworker_effort = "medium"\ngoinkyo_effort = "max"\n'
                '[brigade.claude]\nworker_model = "haiku"\ngoinkyo_model = ""\n')
        expected, _ = what_ran.load_expected(self.write(both))
        self.assertEqual(expected["worker"], ("haiku", "medium"))      # table model, legacy effort
        self.assertEqual(expected["goinkyo"], (None, "max"))           # "" in the table is a value
        # A Codex table, or a product banto has not heard of, changes nothing here.
        other = '[brigade.codex]\nworker_model = "terra"\n[brigade.antigravity]\nworker_model = "x"\n'
        expected, _ = what_ran.load_expected(self.write(other))
        self.assertEqual(expected, {"worker": ("sonnet", "high"), "goinkyo": ("opus", "xhigh")})

    def test_the_search_order_follows_banto(self):
        """banto: $XDG_CONFIG_HOME, ~/.config, then the platform directory (%APPDATA% on Windows,
        Application Support on macOS); an explicit path is not in this list."""
        home = os.path.join(os.sep, "h")
        env = {"BANTO_CONFIG": "/explicit.toml", "XDG_CONFIG_HOME": "/xdg", "APPDATA": "/appdata"}
        self.assertEqual(what_ran.config_candidates(env, home, platform="win32"), [
            os.path.join("/xdg", "banto", "config.toml"),
            os.path.join(home, ".config", "banto", "config.toml"),
            os.path.join("/appdata", "banto", "config.toml"),
        ])
        self.assertEqual(what_ran.config_candidates({}, home, platform="darwin"), [
            os.path.join(home, ".config", "banto", "config.toml"),
            os.path.join(home, "Library", "Application Support", "banto", "config.toml"),
        ])
        self.assertEqual(what_ran.config_candidates({"APPDATA": "/appdata"}, home, platform="linux"),
                         [os.path.join(home, ".config", "banto", "config.toml")])

    def test_the_first_existing_candidate_wins(self):
        home, xdg = tempfile.mkdtemp(prefix="measure-home-"), tempfile.mkdtemp(prefix="measure-xdg-")
        self.addCleanup(synth.rmtree, home)
        self.addCleanup(synth.rmtree, xdg)
        home_cfg = self._put(_path(home, ".config", "banto", "config.toml"), '[brigade]\nworker_effort = "xhigh"\n')
        xdg_cfg = self._put(_path(xdg, "banto", "config.toml"), '[brigade]\nworker_effort = "low"\n')
        env = {"XDG_CONFIG_HOME": xdg}
        expected, note = what_ran.load_expected(None, env=env, home=home, platform="linux")
        self.assertEqual(expected["worker"], ("sonnet", "low"))                 # both exist: XDG is first
        self.assertIn(xdg_cfg, note)
        os.remove(xdg_cfg)
        expected, note = what_ran.load_expected(None, env=env, home=home, platform="linux")
        self.assertEqual(expected["worker"], ("sonnet", "xhigh"))               # XDG gone: ~/.config
        self.assertIn(home_cfg, note)

    def test_the_platform_directory_is_read_only_where_banto_reads_it(self):
        home = tempfile.mkdtemp(prefix="measure-home-")
        appdata = tempfile.mkdtemp(prefix="measure-appdata-")
        self.addCleanup(synth.rmtree, home)
        self.addCleanup(synth.rmtree, appdata)
        self._put(_path(appdata, "banto", "config.toml"), '[brigade]\nworker_effort = "low"\n')
        env = {"APPDATA": appdata}
        on_linux, note = what_ran.load_expected(None, env=env, home=home, platform="linux")
        self.assertEqual(on_linux["worker"], ("sonnet", "high"))                # not read on Linux
        self.assertIn("no config.toml found", note)
        on_windows, note = what_ran.load_expected(None, env=env, home=home, platform="win32")
        self.assertEqual(on_windows["worker"], ("sonnet", "low"))
        self.assertIn(os.path.join(appdata, "banto", "config.toml"), note)

    def test_a_missing_explicit_path_is_not_replaced_by_a_discovered_file(self):
        home = tempfile.mkdtemp(prefix="measure-home-")
        self.addCleanup(synth.rmtree, home)
        self._put(_path(home, ".config", "banto", "config.toml"), '[brigade]\nworker_model = "haiku"\n')
        missing = os.path.join(home, "missing.toml")
        expected, note = what_ran.load_expected(None, env={"BANTO_CONFIG": missing}, home=home, platform="linux")
        self.assertEqual(expected["worker"], ("sonnet", "high"))                # ~/.config is not read
        self.assertIn(f"BANTO_CONFIG names a missing file ({missing}); banto would not start", note)
        _, note = what_ran.load_expected(missing, env={"BANTO_CONFIG": home}, home=home, platform="linux")
        self.assertIn("--config names a missing file", note)                    # --config beats BANTO_CONFIG
        _, note = what_ran.load_expected(None, env={"BANTO_CONFIG": ""}, home=home, platform="linux")
        self.assertIn("BANTO_CONFIG names a missing file", note)                # empty is an explicit path, as in banto

    def test_no_config_anywhere_means_the_built_in_defaults(self):
        home = tempfile.mkdtemp(prefix="measure-home-")
        self.addCleanup(synth.rmtree, home)
        expected, note = what_ran.load_expected(None, env={}, home=home, platform="linux")
        self.assertEqual(expected["goinkyo"], ("opus", "xhigh"))
        self.assertIn("no config.toml found", note)

    def test_expect_flags_replace_and_add_roles(self):
        got = what_ran.apply_expect_flags({"worker": ("sonnet", "high")}, ["worker=opus", "director=Opus/xhigh", "solo=haiku"])
        self.assertEqual(got, {"worker": ("opus", None), "director": ("Opus", "xhigh"), "solo": ("haiku", None)})
        for bad in ("worker", "nobody=opus", "worker=", "=opus"):
            with self.assertRaises(ValueError, msg=bad):
                what_ran.apply_expect_flags({}, [bad])

    def test_what_differs(self):
        exp = ("sonnet", "high")
        d = what_ran.differences
        self.assertEqual(d("claude-sonnet-5-5", "high", exp), [])
        self.assertEqual(d("claude-SONNET-5", "HIGH", exp), [])
        self.assertEqual(d("claude-haiku-5-5", "high", exp), ["model sonnet expected, claude-haiku-5-5 ran"])
        self.assertEqual(d("claude-sonnet-5-5", "xhigh", exp), ["effort high expected, xhigh ran"])
        self.assertEqual(len(d("claude-haiku-5-5", "low", exp)), 2)
        self.assertEqual(d("claude-sonnet-5-5", "?", exp), [])                              # no recorded effort: unrecorded, not DIFFERS
        self.assertEqual(d("claude-haiku-5-5", "?", exp), ["model sonnet expected, claude-haiku-5-5 ran"])
        self.assertEqual(d("anything", "?", None), [])
        self.assertEqual(d("claude-haiku-5-5", "low", (None, None)), [])


class WhatRanEndToEnd(unittest.TestCase):
    """A Director, a Worker that changes model in the middle of its first day, and a Goinkyo whose
    second call ran at a lower effort; every figure is worked out by hand in the comments."""

    def setUp(self):
        self.root, self.proj = synth.make_root()
        self.addCleanup(synth.rmtree, self.root)
        self.out = os.path.join(self.root, "out")
        self.cfg = os.path.join(self.root, "config.toml")
        with open(self.cfg, "w", encoding="utf-8") as fh:
            fh.write('[brigade]\nworker_model = "sonnet"\nworker_effort = "high"\ngoinkyo_model = "fable"\ngoinkyo_effort = "max"\n')
        # Director: opus on day 10 (150), fable on day 11 (600): a role with nothing configured
        d = call_lines("d1", ts(0), U, model=OPUS, effort="xhigh", tools=[(TOOL + "name_member", "n1", {})])
        d += call_lines("d2", ts(30, day=11), U, model=FABLE, effort="high")
        write_jsonl(os.path.join(self.proj, "director.jsonl"), d)
        # Worker: w0 outside an errand; the errand (delivery .. send_to_peer) holds w1 on sonnet and w2 on haiku;
        # w3 on day 11, on haiku. Each 102.
        w = [prompt_snapshot(ts(0), WORKER_PROMPT)]
        w += call_lines("w0", ts(1), U, model=SONNET, effort="high", tools=[(TOOL + "check_messages", "c1", {})])
        w.append(synth.tool_result(ts(2), "c1", delivery(("director — to you", "implement it"))))
        w += call_lines("w1", ts(3), U, model=SONNET, effort="high")
        w += call_lines("w2", ts(4), U, model=HAIKU, effort="high", tools=[(TOOL + "send_to_peer", "s1", {"to": "director", "text": "done"})])
        w += call_lines("w3", ts(5, day=11), U, model=HAIKU, effort="high")
        write_jsonl(os.path.join(self.proj, "worker.jsonl"), w)
        # Goinkyo: fable/max (600), fable/high (600), and a placeholder with no tokens
        g = [prompt_snapshot(ts(0), GOINKYO_PROMPT)]
        g += call_lines("g1", ts(10), U, model=FABLE, effort="max", tools=[(TOOL + "check_messages", "c2", {})])
        g += call_lines("g2", ts(11), U, model=FABLE, effort="high")
        g += call_lines("g3", ts(12), usage(), model="<synthetic>")
        write_jsonl(os.path.join(self.proj, "goinkyo.jsonl"), g)

    def run_ledger(self, *extra):
        return run_main(what_ran, ["--root", self.root, "--out", self.out, "--config", self.cfg] + list(extra))

    def rows(self, name):
        with open(os.path.join(self.out, name), encoding="utf-8", newline="") as fh:
            return list(csv.DictReader(fh))

    def test_role_model_effort_table(self):
        rc, stdout, err = self.run_ledger()
        self.assertEqual(rc, 0, err)
        got = {(r["role"], r["model"], r["effort"]): (int(r["calls"]), float(r["weighted"])) for r in self.rows("ledger_roles.csv")}
        self.assertEqual(got, {
            ("director", OPUS, "xhigh"): (1, 150.0), ("director", FABLE, "high"): (1, 600.0),
            ("worker", SONNET, "high"): (2, 204.0), ("worker", HAIKU, "high"): (2, 204.0),
            ("goinkyo", FABLE, "max"): (1, 600.0), ("goinkyo", FABLE, "high"): (1, 600.0),
        })
        self.assertIn("calls that ran 8;", stdout)
        self.assertIn("left out as placeholders (synthetic or no tokens) 1", stdout)
        self.assertIn("goinkyo: 2 calls, weighted 1,200 = 50.9% of all", stdout)       # 1200 of 2358
        for line in self.rows("ledger_roles.csv"):
            self.assertIn("model_multipliers=", line["weights_assumed"])

    def test_configured_versus_observed_flags_what_differs(self):
        _, stdout, _ = self.run_ledger()
        section = stdout.split("4. CONFIGURED VERSUS OBSERVED")[1].split("5. PRICE WHAT-IF")[0]
        self.assertIn("expected: director none configured; worker sonnet/high; goinkyo fable/max", section)
        flagged = [ln for ln in section.splitlines() if "DIFFERS" in ln]
        self.assertEqual(len(flagged), 3)
        self.assertEqual(sum(HAIKU in ln and "model sonnet expected" in ln for ln in flagged), 2)
        self.assertEqual(sum(FABLE in ln and "effort max expected, high ran" in ln for ln in flagged), 1)
        self.assertIn("7 session-day-model-effort rows; 3 differ", section)
        # the Worker's first day: sonnet 2 calls, then haiku 1, each with its time span
        day10 = [ln for ln in section.splitlines() if "worker-2 b5" in ln and "2025-03-10" in ln]
        self.assertEqual(len(day10), 2)
        self.assertTrue(any(SONNET in ln and "10:01-10:03Z" in ln and ln.rstrip().endswith("ok") for ln in day10), day10)
        self.assertTrue(any(HAIKU in ln and "10:04-10:04Z" in ln and "DIFFERS" in ln for ln in day10), day10)
        by_session = {(r["role"], r["day"], r["model"], r["effort"]): r["differs"] for r in self.rows("ledger_sessions.csv")}
        self.assertEqual(by_session[("worker", "2025-03-11", HAIKU, "high")], f"model sonnet expected, {HAIKU} ran")
        self.assertEqual(by_session[("director", "2025-03-11", FABLE, "high")], "")      # nothing configured for a Director
        self.assertEqual(by_session[("goinkyo", "2025-03-10", FABLE, "max")], "")

    def test_expect_flag_gives_the_director_something_to_differ_from(self):
        _, stdout, _ = self.run_ledger("--expect", "director=opus/xhigh")
        section = stdout.split("4. CONFIGURED VERSUS OBSERVED")[1]
        self.assertIn("director opus/xhigh", section)
        self.assertEqual(len([ln for ln in section.splitlines() if "director" in ln and "DIFFERS" in ln]), 1)

    def test_the_config_decides_what_is_expected(self):
        with open(self.cfg, "w", encoding="utf-8") as fh:
            fh.write('[brigade]\nworker_model = "haiku"\nworker_effort = ""\ngoinkyo_model = "fable"\ngoinkyo_effort = ""\n')
        _, stdout, _ = self.run_ledger()
        section = stdout.split("4. CONFIGURED VERSUS OBSERVED")[1].split("5. PRICE WHAT-IF")[0]
        flagged = [ln for ln in section.splitlines() if "DIFFERS" in ln]
        self.assertEqual(len(flagged), 1)                      # now the Worker's sonnet row; its haiku rows are the expected ones
        self.assertIn(SONNET, flagged[0])

    def test_errands_show_a_model_change_inside_one(self):
        _, stdout, _ = self.run_ledger()
        section = stdout.split("3. PER ERRAND")[1].split("4. CONFIGURED")[0]
        self.assertIn("on more than one", section)
        rows = [r for r in self.rows("ledger_errands.csv") if r["role"] == "worker"]
        self.assertEqual(sorted((r["model"], r["calls"]) for r in rows), [(HAIKU, "1"), (SONNET, "1")])
        self.assertEqual(len({r["errand_id"] for r in rows}), 1)

    def test_price_what_if_keeps_the_tokens_and_reprices_the_named_roles(self):
        rc, stdout, err = self.run_ledger("--reprice", "worker=opus", "--reprice", "goinkyo=opus")
        self.assertEqual(rc, 0, err)
        section = stdout.split("5. PRICE WHAT-IF")[1]
        line = {ln.split()[0]: ln.split() for ln in section.splitlines() if ln.strip() and ln.split()[0] in ("director", "worker", "goinkyo", "all")}
        # actual: director 750, worker 408, goinkyo 1200 = 2358; re-priced: 750, 600, 300 = 1650
        self.assertEqual(line["director"][2:], ["750", "31.8%", "750", "45.5%", "x1.00"])
        self.assertEqual(line["worker"][2:], ["408", "17.3%", "600", "36.4%", "x1.47"])
        self.assertEqual(line["goinkyo"][2:], ["1,200", "50.9%", "300", "18.2%", "x0.25"])
        self.assertEqual(line["all"][2:], ["2,358", "100.0%", "1,650", "100.0%", "x0.70"])

    def test_without_reprice_the_section_says_how_to_ask(self):
        _, stdout, _ = self.run_ledger()
        self.assertIn("none asked for: pass --reprice ROLE=MODEL", stdout)

    def test_an_unknown_reprice_model_or_role_is_refused_before_anything_is_written(self):
        for bad in ("worker=gemini", "nobody=opus", "worker", "worker="):
            rc, _, err = self.run_ledger("--reprice", bad)
            self.assertEqual(rc, 2, bad)
            self.assertIn("--reprice", err)
        self.assertFalse(os.path.exists(self.out))

    def test_a_model_weight_makes_a_new_model_priceable(self):
        rc, stdout, err = self.run_ledger("--reprice", "worker=gemini", "--model-weight", "gemini=2")
        self.assertEqual(rc, 0, err)
        self.assertIn("worker -> gemini", stdout)

    def test_the_window_cuts_calls(self):
        _, stdout, _ = self.run_ledger("--since", "2025-03-11")
        self.assertIn("calls that ran 2;", stdout)             # d2 and w3

    def test_malformed_flags_are_refused(self):
        for argv in (["--since", "yesterday"], ["--expect", "worker"], ["--top", "-1"]):
            rc, _, err = self.run_ledger(*argv)
            self.assertEqual(rc, 2, argv)
            self.assertTrue(err)

    def test_nothing_is_written_to_the_transcripts_or_the_config(self):
        def read(path):
            with open(path, "rb") as fh:
                return fh.read()
        before = {p: read(p) for p in (self.cfg, *(os.path.join(self.proj, n) for n in os.listdir(self.proj)))}
        self.run_ledger()
        for p, data in before.items():
            self.assertEqual(read(p), data)

    def test_the_other_reports_see_the_goinkyo_as_a_role(self):
        import usage_report
        rc, stdout, err = run_main(usage_report, ["--root", self.root, "--out", os.path.join(self.root, "u")])
        self.assertEqual(rc, 0, err)
        self.assertIn("TOTALS BY ROLE", stdout)
        by_role = stdout.split("TOTALS BY ROLE")[1].split("TOTALS BY DAY")[0]
        self.assertEqual(sorted(ln.split()[0] for ln in by_role.splitlines()[1:] if ln.strip()), ["director", "goinkyo", "worker"])
        with open(os.path.join(self.root, "u", "sessions.csv"), encoding="utf-8", newline="") as fh:
            self.assertEqual(sorted(r["role"] for r in csv.DictReader(fh)), ["director", "goinkyo", "worker"])


if __name__ == "__main__":
    unittest.main()
