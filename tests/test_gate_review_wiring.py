from __future__ import annotations

import os
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import generate_inventory_docs as inventory

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

    def test_audit_shard_runs_severity_harness_regressions(self):
        source = (ROOT / "scripts/ci-script-checks.sh").read_text().splitlines()
        start = source.index('if run_check cargo "Maintainability audit tests"; then') + 1
        body = "\n".join(source[start:source.index("fi", start)])
        self.write("tests/__init__.py", "")
        self.write("tests/test_audit_maintainability.py", """import unittest
class FooterViewWritesCheck(unittest.TestCase):
    def test_fixture(self): pass
class GiantFileRatchetCheck(unittest.TestCase):
    def test_fixture(self): pass
class HarnessCli(unittest.TestCase):
    def test_warning_blocker_regression(self): self.fail('severity harness executed')
""")
        result = subprocess.run(["bash", "-euo", "pipefail", "-c", body], cwd=self.root,
                                env={**os.environ, "PYTHON": sys.executable},
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("severity harness executed", result.stderr)

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

    def real_generator_fixture(self, *, existing_snapshots):
        for relative in (".githooks/pre-push", "scripts/resolve-python-runner.sh",
                         "scripts/check_generated_doc_drift.py", "scripts/generate_inventory_docs.py",
                         "scripts/generate_env_reference.py", "scripts/generate_cli_reference.py"):
            target = self.root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / relative, target)
        for name in inventory.TOP_LEVEL_MODULE_PURPOSES:
            if name.endswith("/"):
                (self.root / "src" / name).mkdir(parents=True, exist_ok=True)
            else:
                self.write("src/" + name, "")
        self.write("src/lib.rs", "pub mod server;\npub mod cli;\n")
        self.write("src/server/mod.rs", "pub mod routes;\npub mod worker_registry;\n")
        self.write("src/server/routes/mod.rs",
                   "pub fn compose_api_router() -> Router { Router::new().merge(fixture::router()) }\n")
        self.write("src/server/routes/fixture.rs", """pub fn router() -> Router {
    Router::new().route("/fixture", get(handler))
}
async fn handler() {}
""")
        self.write("src/server/web_surface.rs", "pub fn router() -> Router { Router::new() }\n")
        self.write("src/server/worker_registry.rs", "pub(crate) const WORKER_SPECS: &[WorkerSpec] = [\n];\n")
        self.write("src/cli/mod.rs", "pub mod args;\n")
        self.write("src/cli/args.rs", """#[derive(Parser)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}
#[derive(Subcommand)]
pub enum Commands { Fixture }
""")
        self.write("scripts/giant_file_registry.toml",
                   "grandfathered = []\ngrandfathered_baseline_paths = []\n")
        self.write("scripts/giant_file_closed_issue_transition_list.txt", "")
        self.write("scripts/giant_file_issue_metadata.json", json.dumps({
            "schema_version": 2, "refreshed_at": "2026-10-13T00:00:00Z", "issues": [],
            "ratchets": {name: 0 for name in inventory.GIANT_FILE_ISSUE_RATCHET_KEYS},
        }))
        self.write("ARCHITECTURE.md", "\n".join(("# Reviewed architecture", "",
                   inventory.ARCHITECTURE_SRC_TREE_START, "stale", inventory.ARCHITECTURE_SRC_TREE_END,
                   inventory.ARCHITECTURE_TOP_LEVEL_MAP_START, "stale",
                   inventory.ARCHITECTURE_TOP_LEVEL_MAP_END, "")))
        for name in DOCS[1:]:
            self.write(name, "stale\n")
        if existing_snapshots:
            for name in ("module-inventory.md", "giant-file-registry.md"):
                self.write("docs/generated/" + name, "preserve checkout-local snapshot\n")
        self.git("add", ".")
        self.git("commit", "-qm", "real generator fixture")

    def all_file_bytes(self):
        return {path.relative_to(self.root).as_posix(): path.read_bytes()
                for path in self.root.rglob("*")
                if path.is_file() and ".git" not in path.relative_to(self.root).parts}

    def assert_real_output_scope(self, *, weekly):
        for existing in (False, True):
            with self.subTest(existing_snapshots=existing):
                self.real_generator_fixture(existing_snapshots=existing)
                before = self.all_file_bytes()
                env = {**os.environ, "PYTHON": sys.executable, "PYTHONDONTWRITEBYTECODE": "1"}
                if weekly:
                    workflow = yaml.safe_load((ROOT / ".github/workflows/regen-docs.yml").read_text())
                    step = next(step for step in workflow["jobs"]["regen"]["steps"]
                                if step.get("name") == "Generate inventory docs")
                    result = subprocess.run(["bash", "-euo", "pipefail", "-c", step["run"]],
                                            cwd=self.root, env=env, capture_output=True, text=True)
                else:
                    head = self.git("rev-parse", "HEAD").stdout.strip()
                    result = subprocess.run(["bash", str(self.root / ".githooks/pre-push")],
                                            input=f"refs/heads/topic {head} refs/heads/topic {self.base}\n",
                                            cwd=self.root, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
                after = self.all_file_bytes()
                changed = {path for path in before.keys() | after.keys()
                           if before.get(path) != after.get(path)}
                self.assertEqual(changed, set(DOCS), result.stdout)
                self.assertEqual(self.git("diff", "--cached", "--name-only").stdout, "")

    def test_push_hook_real_generators_write_only_five_docs(self):
        self.assert_real_output_scope(weekly=False)

    def test_weekly_real_generators_write_only_five_docs(self):
        self.assert_real_output_scope(weekly=True)


if __name__ == "__main__":
    unittest.main()
