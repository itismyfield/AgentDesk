from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import yaml

ROOT = Path(__file__).resolve().parents[1]
DOCS = ("ARCHITECTURE.md", "docs/generated/route-inventory.md",
        "docs/generated/worker-inventory.md", "docs/generated/env-reference.md",
        "docs/generated/cli-reference.md")


class GateReviewWiring(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.test")
        for path in DOCS:
            self.write(path, "committed\n")
        self.write("docs/generated/manual.md", "manual\n")
        self.git("add", ".")
        self.git("commit", "-qm", "base")
        self.base = self.git("rev-parse", "HEAD").stdout.strip()

    def git(self, *args):
        return subprocess.run(["git", "-C", str(self.root), *args],
                              check=True, capture_output=True, text=True)

    def write(self, path, text):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def drift(self, root=None):
        return subprocess.run([sys.executable, str(ROOT / "scripts/check_generated_doc_drift.py"),
                               "--repo-root", str(root or self.root)], capture_output=True, text=True)

    def test_drift_warns_with_candidate_and_exact_files(self):
        self.write(DOCS[3], "regenerated\n")
        result = self.drift()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("::warning", result.stdout)
        self.assertIn(DOCS[3], result.stdout)
        self.assertIn(self.base, result.stdout)
        self.assertEqual(self.git("rev-parse", "HEAD").stdout.strip(), self.base)

    def test_manual_generated_directory_file_is_outside_drift_scope(self):
        self.write("docs/generated/manual.md", "changed manual text\n")
        result = self.drift()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("::warning", result.stdout)

    def test_git_exception_is_failure(self):
        result = self.drift(self.root / "absent")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("::warning", result.stdout)

    def test_diff_exception_is_not_classified_as_drift(self):
        probe = self.root / "bin/git"
        probe.parent.mkdir()
        probe.write_text("#!/bin/sh\ncase \"$3\" in\nrev-parse) echo " + self.base +
                         ";;\n*) echo 'fixture git diff crashed' >&2; exit 128;;\nesac\n")
        probe.chmod(0o755)
        result = subprocess.run([sys.executable, str(ROOT / "scripts/check_generated_doc_drift.py"),
                                 "--repo-root", str(self.root)], capture_output=True, text=True,
                                env={**os.environ, "PATH": f"{probe.parent}:{os.environ['PATH']}"})
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("::warning", result.stdout)
        self.assertIn("fixture git diff crashed", result.stderr)

    def test_weekly_regeneration_git_failure_is_fatal(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/regen-docs.yml").read_text())
        step = next(step for step in workflow["jobs"]["regen"]["steps"] if step.get("id") == "drift")
        with tempfile.TemporaryDirectory() as tmp:
            result = subprocess.run(["bash", "-c", step["run"]], cwd=tmp,
                                    env={**os.environ, "GITHUB_OUTPUT": str(Path(tmp) / "output")},
                                    capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)

    def hook(self, *, fail=False):
        for path in (".githooks/pre-push", "scripts/resolve-python-runner.sh"):
            target = self.root / path
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / path, target)
        shutil.copyfile(ROOT / "scripts/check_generated_doc_drift.py",
                        self.root / "scripts/check_generated_doc_drift.py")
        for generator, targets in (("inventory_docs", DOCS[:3]),
                                   ("env_reference", DOCS[3:4]),
                                   ("cli_reference", DOCS[4:])):
            body = "raise RuntimeError('fixture scanner crashed')\n" if fail else (
                "from pathlib import Path\n" +
                "\n".join(f"Path({path!r}).write_text('regenerated\\n')" for path in targets))
            self.write(f"scripts/generate_{generator}.py", body)
        self.write("src/lib.rs", "fn changed() {}\n")
        self.git("add", ".")
        self.git("commit", "-qm", "change source")
        head = self.git("rev-parse", "HEAD").stdout.strip()
        result = subprocess.run(["bash", str(self.root / ".githooks/pre-push")],
                                input=f"refs/heads/topic {head} refs/heads/topic {self.base}\n",
                                cwd=self.root, env={**os.environ, "PYTHON": sys.executable,
                                                   "GIT_AUTO_REGEN_DOCS": "0"},
                                capture_output=True, text=True)
        return head, result

    def test_push_hook_warns_without_amending_or_staging(self):
        head, result = self.hook()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("::warning", result.stdout)
        self.assertEqual(self.git("rev-parse", "HEAD").stdout.strip(), head)
        self.assertEqual(self.git("diff", "--cached", "--name-only").stdout, "")
        self.assertEqual(set(self.git("diff", "--name-only").stdout.splitlines()), set(DOCS))

    def test_push_hook_generator_exception_fails(self):
        head, result = self.hook(fail=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fixture scanner crashed", result.stderr + result.stdout)
        self.assertEqual(self.git("rev-parse", "HEAD").stdout.strip(), head)


if __name__ == "__main__":
    unittest.main()
