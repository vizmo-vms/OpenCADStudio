"""Tests for release_order.py and the release workflow's publication order.

  python3 -m unittest discover -s packaging/secureplan -p 'test_*.py'
"""

import pathlib
import re
import unittest

import release_order

WORKFLOW = pathlib.Path(__file__).resolve().parents[2] / ".github/workflows/secureplan-release.yml"


class VersionOrder(unittest.TestCase):
    def test_only_a_strictly_newer_release_may_be_published(self):
        allowed = [
            ("secureplan-cad-v0.1.0", ""),
            ("secureplan-cad-v0.1.1", "secureplan-cad-v0.1.0"),
            ("secureplan-cad-v0.10.0", "secureplan-cad-v0.9.0"),
            ("secureplan-cad-v1.0.0", "secureplan-cad-v0.99.99"),
            ("secureplan-cad-v0.2.0", "secureplan-cad-v0.1.10"),
        ]
        for candidate, latest in allowed:
            self.assertTrue(release_order.may_publish(candidate, latest)[0], (candidate, latest))
        refused = [
            ("secureplan-cad-v0.2.0", "secureplan-cad-v0.3.0"),  # an older run finishing late
            ("secureplan-cad-v0.3.0", "secureplan-cad-v0.3.0"),
            ("secureplan-cad-v0.9.0", "secureplan-cad-v0.10.0"),
            ("secureplan-cad-v0.1.9", "secureplan-cad-v0.1.10"),
            ("secureplan-cad-v0.2.0", "v2026.38"),  # latest outside the scheme
            ("v0.2.0", ""),
            ("secureplan-cad-v256.0.0", ""),
            ("secureplan-cad-v0.02.0", ""),
            ("secureplan-cad-v0.2", ""),
        ]
        for candidate, latest in refused:
            self.assertFalse(release_order.may_publish(candidate, latest)[0], (candidate, latest))

    def test_the_command_line_exit_codes(self):
        self.assertEqual(release_order.main(["x", "secureplan-cad-v0.2.0", "secureplan-cad-v0.1.0"]), 0)
        self.assertEqual(release_order.main(["x", "secureplan-cad-v0.2.0", ""]), 0)
        self.assertEqual(release_order.main(["x", "secureplan-cad-v0.2.0"]), 0)
        self.assertEqual(release_order.main(["x", "secureplan-cad-v0.1.0", "secureplan-cad-v0.2.0"]), 1)
        self.assertEqual(release_order.main(["x"]), 2)


def job_block(text: str, name: str) -> str:
    """The lines of job `name` (two-space indented key under `jobs:`)."""
    lines = text.splitlines()
    begin = lines.index(f"  {name}:")
    block = []
    for line in lines[begin + 1:]:
        if re.match(r"^  \S", line) or re.match(r"^\S", line):
            break
        block.append(line)
    return "\n".join(block)


class PublicationOrder(unittest.TestCase):
    """The release workflow's publication rules, read as text (stdlib only)."""

    def setUp(self):
        self.text = WORKFLOW.read_text()
        self.publish = job_block(self.text, "publish")

    def test_publication_is_serialized_across_every_tag(self):
        match = re.search(r"\n    concurrency:\n      group: (\S+)\n      cancel-in-progress: (\S+)", self.publish)
        self.assertIsNotNone(match, "the publish job has its own concurrency group")
        self.assertNotIn("${{", match.group(1), "one group for every tag")
        self.assertEqual(match.group(2), "false")

    def test_the_order_is_checked_before_the_draft_and_again_before_promotion(self):
        checks = [m.start() for m in re.finditer(r"release_order\.py", self.publish)]
        create = self.publish.index("gh release create")
        promote = self.publish.index("gh release edit")
        self.assertIn("--latest", self.publish[promote:].splitlines()[0])
        self.assertTrue(checks and checks[0] < create, "checked before the draft is created")
        self.assertTrue(any(create < c < promote for c in checks), "checked again just before it is marked latest")
        # The second check is in the same step as the promotion.
        step = self.publish[:promote].rsplit("      - name:", 1)[1]
        self.assertIn("release_order.py", step)

    def test_only_a_tag_push_can_publish(self):
        condition = re.search(r"\n    if: (.*)", self.publish).group(1)
        self.assertIn("github.event_name == 'push'", condition)
        self.assertIn("github.ref_type == 'tag'", condition)
        self.assertRegex(self.text, r"\npermissions:\n  contents: read\n")
        self.assertEqual(self.text.count("contents: write"), 1)
        self.assertIn("contents: write", self.publish)

    def test_every_action_is_pinned_to_a_commit(self):
        for line in self.text.splitlines():
            if "uses:" in line:
                self.assertRegex(line, r"uses: [\w.-]+/[\w.-]+@[0-9a-f]{40} # ", line.strip())
        self.assertNotIn("toolchain: stable", self.text)
        self.assertRegex(self.text, r'RUST_TOOLCHAIN: "\d+\.\d+\.\d+"')


if __name__ == "__main__":
    unittest.main()
