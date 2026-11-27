"""Synthetic inbox examples and a canary against banto's format strings."""
import os
import re
import unittest

import synth
import transcripts as tr
import plan_metrics as pm


class InboxFormat(unittest.TestCase):
    def test_real_framing_shapes_and_sender_headers(self):
        examples = [
            (("banto — to you", "notice"),),
            (("worker-2 \"impl\" — to you", "report"),),
            (("banto — to you", "notice"), ("worker-2 — broadcast", "report")),
            (("goinkyo — to you", "review"),),
            (("planner — to you", "plan"),),
        ]
        for messages in examples:
            text = synth.delivery(*messages, recipient="director")
            self.assertIsNotNone(tr.DELIVERED_RE.match(text))
            self.assertEqual([p[0] for p in tr.split_delivery(text)], [h.split()[0] for h, _ in messages])
        self.assertIn("[from banto (banto's own record, not another AI's words) — to you]",
                      synth.delivery(("banto — to you", "notice")))

    def test_rust_format_string_canary(self):
        repo = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
        with open(os.path.join(repo, "crates", "banto", "src", "mcp.rs"), encoding="utf-8") as fh:
            inbox = fh.read()
        with open(os.path.join(repo, "crates", "banto-core", "src", "plan.rs"), encoding="utf-8") as fh:
            plans = fh.read()
        def fmt_after(source, marker):
            fragment = source[source.index(marker):]
            match = re.search(r'format!\(\s*"((?:[^"\\]|\\[\s\S])*)"', fragment)
            self.assertIsNotNone(match, marker)
            text = re.sub(r'\\\r?\n\s*', '', match.group(1))
            return text.replace(r'\u{2014}', '—').replace(r'\n', '\n')

        approval_fmt = fmt_after(plans, "pub fn approval_notice(")
        release_fmt = fmt_after(plans, "pub fn release_notice(")
        approval = approval_fmt.format("review", number=3, version=4)
        release = release_fmt.format(number=3, version=5, summary="synthetic")
        self.assertEqual(pm.APPROVAL.match(approval).groups(), ("3", "4"))
        self.assertEqual(pm.RELEASE.match(release).groups(), ("3", "5"))
        self.assertIsNone(pm.APPROVAL.match(approval.replace("version 4,", "version 4x,")))
        self.assertIsNone(pm.RELEASE.match(release.replace("version 5 —", "version 5x —")))

        banto_preface = fmt_after(inbox, "let mut out = if messages.iter().all(from_banto)").format(1)
        mixed_preface = fmt_after(inbox, "} else if messages.iter().any(from_banto)").format(2, own=1, peer="Worker")
        peer_preface = fmt_after(inbox, "    } else {\n        format!(\n            \"{} message(s) relayed by banto from your brigade").format(1, peer="Worker")
        origin = re.search(r'let origin = if from_banto\(message\) \{\s*"([^"]*)"', inbox).group(1)
        header = fmt_after(inbox, "out.push_str(&format!(\n            \"\\n[from")
        for preface in (banto_preface, mixed_preface, peer_preface):
            self.assertIsNotNone(tr.DELIVERED_RE.match(preface))
        for sender, sender_origin in (("banto", origin), ("worker-1", "")):
            text = banto_preface + header.format(sender, "synthetic", origin=sender_origin, addressing="to you", interrupted="")
            self.assertEqual(tr.split_delivery(text)[0][0], sender)

        # The line a page that left mail unread ends with: rendered from the Rust string, it is
        # what synth reproduces, and it is not the last message's body.
        waiting = fmt_after(inbox, "fn more_waiting_line").replace("{remaining}", "3")
        page = banto_preface + header.format("worker-1", "last body", origin="", addressing="to you", interrupted="") + waiting
        self.assertEqual(tr.split_delivery(page), [("worker-1", None, "to you", "last body")])
        self.assertEqual(tr.instruction_label(page), "last body")
        synthetic = synth.delivery(("worker-1 — to you", "last body"), waiting=3)
        self.assertTrue(synthetic.endswith(waiting), (synthetic, waiting))
        self.assertEqual(tr.split_delivery(synthetic), [("worker-1", None, "to you", "last body")])
        # Only the end of the answer is the line: the same words inside a body stay in it.
        inside = synth.delivery(("worker-1 — to you", "quoted:" + waiting + "after"))
        self.assertIn("quoted:", tr.split_delivery(inside)[0][3])
        self.assertIn("after", tr.split_delivery(inside)[0][3])
        self.assertIn("more message(s) waiting", tr.split_delivery(inside)[0][3])
        # Known limitation (tools/measure/README.md): a last body that itself ends with the
        # line is shortened though banto appended none. Pinned so nobody reads the parser as
        # exact; if it ever learns to tell them apart, this is the assertion to flip.
        lookalike = synth.delivery(("worker-1 — to you", "see:" + waiting), waiting=0)
        self.assertEqual(tr.split_delivery(lookalike)[0][3], "see:")
        # A later page's last message, and a page with several messages.
        two = synth.delivery(("worker-1 — to you", "a"), ("worker-2 — broadcast", "b"), waiting=12)
        self.assertEqual([p[3] for p in tr.split_delivery(two)], ["a", "b"])

    def test_update_progress_alone_identifies_director(self):
        import tempfile
        with tempfile.TemporaryDirectory() as root:
            path = synth.write_jsonl(os.path.join(root, "p", "director.jsonl"),
                                    synth.call_lines("m", synth.ts(0), synth.usage(out=1),
                                                     tools=[(synth.TOOL + "update_progress", "t", {})]))
            self.assertEqual(tr.read_session(path, root).role, "director")
