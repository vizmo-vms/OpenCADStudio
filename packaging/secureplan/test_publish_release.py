"""publish-release.sh against a stub `gh` and `curl` (stdlib only).

  python3 -m unittest discover -s packaging/secureplan -p 'test_*.py'

The stub answers `gh api repos/<repo>/releases/latest` from a scripted list
(a tag, GitHub's 404, another HTTP error, or a network failure), records every
`gh release` command, and serves uploaded assets back to `gh release download`
and `curl`. The tests show that only a confirmed 404 counts as "no release",
and that any failed or unfavourable lookup stops before a draft is created or
before the release is promoted.
"""

import hashlib
import os
import pathlib
import subprocess
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
SCRIPT = HERE / "publish-release.sh"
ASSETS = ["SecurePlanCAD-macos-arm64.dmg", "SecurePlanCAD-macos-x64.dmg", "SecurePlanCAD-windows-x64.msi", "SecurePlanCAD-source.tar.gz"]
TAG = "secureplan-cad-v0.2.0"

GH_STUB = r'''#!/usr/bin/env python3
import json, os, pathlib, shutil, sys
state = pathlib.Path(os.environ["STUB_STATE"])
args = sys.argv[1:]
with open(state / "log", "a") as log:
    log.write(" ".join(args) + "\n")
uploaded = state / "uploaded"
if args[:1] == ["api"] and args[1].endswith("/releases/latest"):
    if (state / "published").exists():
        print((state / "published").read_text())
        sys.exit(0)
    answers = json.loads(os.environ["STUB_LATEST"])
    count_file = state / "latest-count"
    count = int(count_file.read_text()) if count_file.exists() else 0
    count_file.write_text(str(count + 1))
    answer = answers[min(count, len(answers) - 1)]
    if answer == "404":
        print("gh: Not Found (HTTP 404)", file=sys.stderr)
        sys.exit(1)
    if answer == "502":
        print("gh: Bad Gateway (HTTP 502)", file=sys.stderr)
        sys.exit(1)
    if answer == "offline":
        print("error connecting to api.github.com", file=sys.stderr)
        sys.exit(1)
    print(answer)
    sys.exit(0)
if args[:1] == ["api"] and args[1].endswith("/releases"):
    print(os.environ.get("STUB_EXISTING", ""))
    sys.exit(0)
if args[:2] == ["release", "create"]:
    (state / "created").write_text(args[2])
    sys.exit(0)
if args[:2] == ["release", "upload"]:
    uploaded.mkdir(exist_ok=True)
    for path in args[3:]:
        shutil.copy(path, uploaded)
    sys.exit(0)
if args[:2] == ["release", "download"]:
    target = pathlib.Path(args[args.index("--dir") + 1])
    target.mkdir(parents=True, exist_ok=True)
    for path in uploaded.iterdir():
        shutil.copy(path, target)
    sys.exit(0)
if args[:2] == ["release", "edit"]:
    (state / "published").write_text(args[2])
    sys.exit(0)
print("stub gh: unexpected " + " ".join(args), file=sys.stderr)
sys.exit(3)
'''

CURL_STUB = r'''#!/usr/bin/env python3
import os, pathlib, shutil, sys
state = pathlib.Path(os.environ["STUB_STATE"])
args = sys.argv[1:]
out = args[args.index("-o") + 1]
shutil.copy(state / "uploaded" / args[-1].rsplit("/", 1)[1], out)
'''


class PublishRelease(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(self.tmp.name)
        self.state = root / "state"
        self.state.mkdir()
        bin_dir = root / "bin"
        bin_dir.mkdir()
        for name, text in (("gh", GH_STUB), ("curl", CURL_STUB)):
            (bin_dir / name).write_text(text)
            (bin_dir / name).chmod(0o755)
        self.dist = root / "dist"
        self.dist.mkdir()
        sums = []
        for name in ASSETS:
            data = f"synthetic {name}".encode()
            (self.dist / name).write_bytes(data)
            sums.append(f"{hashlib.sha256(data).hexdigest()}  {name}\n")
        (self.dist / "SHA256SUMS").write_text("".join(sums))
        self.notes = root / "notes.md"
        self.notes.write_text("Notes.\n")
        self.path = f"{bin_dir}{os.pathsep}{os.environ['PATH']}"

    def tearDown(self):
        self.tmp.cleanup()

    def publish(self, latest, existing=""):
        env = dict(os.environ, PATH=self.path, STUB_STATE=str(self.state), STUB_LATEST=__import__("json").dumps(latest),
                   STUB_EXISTING=existing, GH_REPO="vizmo-vms/OpenCADStudio", GH_TOKEN="stub")
        env.pop("GITHUB_STEP_SUMMARY", None)
        return subprocess.run(["bash", str(SCRIPT), TAG, "0.2.0", str(self.dist), str(self.notes)], env=env, capture_output=True, text=True)

    def commands(self):
        log = self.state / "log"
        return log.read_text().splitlines() if log.exists() else []

    def created(self):
        return any(c.startswith("release create") for c in self.commands())

    def promoted(self):
        return any(c.startswith("release edit") for c in self.commands())

    def test_a_confirmed_first_release_is_created_verified_and_promoted(self):
        result = self.publish(["404"])
        self.assertEqual(result.returncode, 0, result.stderr)
        commands = self.commands()
        order = [c.split()[1] for c in commands if c.startswith("release ")]
        self.assertEqual(order, ["create", "upload", "download", "edit"])
        self.assertIn("--latest", next(c for c in commands if c.startswith("release edit")))

    def test_a_newer_release_over_an_older_latest_is_published(self):
        result = self.publish(["secureplan-cad-v0.1.0"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.created() and self.promoted())

    def test_a_failed_lookup_creates_nothing(self):
        for failure in ("502", "offline"):
            with self.subTest(failure=failure):
                self.tearDown()
                self.setUp()
                result = self.publish([failure])
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("could not be read", result.stderr)
                self.assertFalse(self.created() or self.promoted(), self.commands())

    def test_an_older_or_equal_release_creates_nothing(self):
        for latest in ("secureplan-cad-v0.3.0", TAG):
            with self.subTest(latest=latest):
                self.tearDown()
                self.setUp()
                result = self.publish([latest])
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.created() or self.promoted(), self.commands())

    def test_a_lookup_failure_before_promotion_never_promotes(self):
        # First lookup: no release yet; by promotion time the lookup fails,
        # or a newer release has been published.
        for second in ("502", "offline", "secureplan-cad-v0.3.0"):
            with self.subTest(second=second):
                self.tearDown()
                self.setUp()
                result = self.publish(["404", second])
                self.assertNotEqual(result.returncode, 0)
                self.assertTrue(self.created(), "the draft is created first")
                self.assertFalse(self.promoted(), f"promoted after {second}: {self.commands()}")

    def test_an_existing_draft_for_the_tag_stops_publication(self):
        result = self.publish(["404"], existing="12345")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.created() or self.promoted())


if __name__ == "__main__":
    unittest.main()
