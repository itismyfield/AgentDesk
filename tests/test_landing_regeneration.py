"""Merged documentation policy, real Git fixtures and required CI execution contracts."""
from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import yaml

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/landing/regen_after_merge.py"
sys.path.insert(0, str(SCRIPT.parent))
SPEC = importlib.util.spec_from_file_location("landing_regen", SCRIPT)
REGEN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(REGEN)


def block(name: str, body: str = "base") -> str:
    return f"<!-- BEGIN GENERATED: {name} -->\n{body}\n<!-- END GENERATED: {name} -->\n"


class LandingRegeneration(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="landing-regen-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.count = 0
        self.git("init", "-b", "main")
        self.git("config", "user.name", "Landing fixture")
        self.git("config", "user.email", "landing@example.invalid")
        self.write("ARCHITECTURE.md", "manual header\n" + "stable prefix\n" * 8 + block("SRC TREE") + "manual middle\n" * 8
                   + block("TOP LEVEL MODULE MAP") + "stable suffix\n" * 8 + "manual footer\n")
        self.write("docs/agent-maintenance/change-surfaces.md", "manual giant\n" + block("GIANT PATHS"))
        for name in REGEN.WHOLE_FILES:
            self.write(name, "base\n")
        self.write("docs/architecture/layers.md", "manual layers\n")
        self.write("docs/generated/README.md", "manual readme\n")
        self.write(".gitattributes", "docs/generated/route-inventory.md merge=regen-inventory\n")
        generator = '''from pathlib import Path
import re
for name in ('ARCHITECTURE.md', 'docs/agent-maintenance/change-surfaces.md'):
    p=Path(name)
    p.write_text(re.sub(r'(<!-- BEGIN GENERATED: [^>]+ -->)\\n.*?(<!-- END GENERATED: [^>]+ -->)',
                        r'\\1\\ncanonical\\n\\2', p.read_text(), flags=re.S))
for name in ('route-inventory', 'worker-inventory', 'env-reference', 'cli-reference'):
    Path('docs/generated/'+name+'.md').write_text('canonical\\n')
'''
        self.write("scripts/generate_inventory_docs.py", generator)
        self.write("scripts/generate_env_reference.py", "")
        self.write("scripts/generate_cli_reference.py", "")
        self.git("add", ".")
        self.git("commit", "-m", "base")
        self.base = self.git("rev-parse", "HEAD").stdout.strip()

    def write(self, name: str, value: str) -> None:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(value)

    def git(self, *args: str, expected: int = 0) -> subprocess.CompletedProcess:
        done = subprocess.run(["git", *args], cwd=self.root, text=True, capture_output=True)
        self.assertEqual(done.returncode, expected, done.stdout + done.stderr)
        return done

    def run_regen(self, expected: int = 0, check: bool = False, check_merge: bool = False) -> dict:
        self.count += 1
        artifact = self.root / f"artifacts/{self.count}"
        done = subprocess.run([sys.executable, str(SCRIPT), "--worktree", str(self.root),
                               "--artifact-dir", str(artifact), *(["--check"] if check else []),
                               *(["--check-merge"] if check_merge else [])],
                              text=True, capture_output=True)
        self.assertEqual(done.returncode, expected, done.stdout + done.stderr)
        report = json.loads((artifact / "manifest.json").read_text())
        self.assertEqual(report["exit_code"], expected)
        return report

    def merge_fixture(self, manual_conflict: bool = False) -> None:
        self.git("switch", "-c", "ours")
        for name in ("ARCHITECTURE.md", "docs/generated/route-inventory.md"):
            p = self.root / name
            p.write_text(p.read_text().replace("base", "ours"))
        p = self.root / "ARCHITECTURE.md"
        p.write_text(p.read_text().replace("manual header", "ours header"))
        self.git("commit", "-am", "ours")
        self.git("switch", "-c", "theirs", self.base)
        for name in ("ARCHITECTURE.md", "docs/generated/route-inventory.md"):
            p = self.root / name
            p.write_text(p.read_text().replace("base", "theirs"))
        p = self.root / "ARCHITECTURE.md"
        p.write_text(p.read_text().replace("manual header" if manual_conflict else "manual footer",
                                          "theirs header" if manual_conflict else "theirs footer"))
        self.git("commit", "-am", "theirs")
        self.git("switch", "ours")
        self.git("merge", "--no-commit", "theirs", expected=1)

    def test_generated_conflict_preserves_both_manual_edits_and_rerun_bytes(self) -> None:
        self.merge_fixture()
        report = self.run_regen()
        self.assertEqual(report["status"], "REGEN_OK")
        text = (self.root / "ARCHITECTURE.md").read_text()
        for phrase in ("ours header", "theirs footer", "canonical"):
            self.assertIn(phrase, text)
        self.assertEqual(self.git("diff", "--name-only", "--diff-filter=U").stdout, "")
        before = REGEN.snapshot(self.root)
        self.run_regen()
        self.assertEqual(REGEN.snapshot(self.root), before)
        self.assertEqual((self.root / "artifacts/2/regeneration.diff").read_bytes(), b"")
        self.assertIn("<<<<<<<", (self.root / "artifacts/1/regeneration.diff").read_text())
        self.assertEqual((self.root / "docs/architecture/layers.md").read_text(), "manual layers\n")

    def test_merge_check_accepts_generated_conflicts_without_staging(self) -> None:
        self.merge_fixture()
        index = self.git("ls-files", "--stage").stdout
        report = self.run_regen(check_merge=True)
        self.assertEqual(report["status"], "REGEN_OK")
        self.assertEqual(self.git("ls-files", "--stage").stdout, index)
        self.assertTrue((self.root / "artifacts/1/regeneration.diff").read_bytes())

    def test_driver_registration_does_not_change_final_bytes(self) -> None:
        self.merge_fixture()
        self.run_regen()
        expected = REGEN.snapshot(self.root)
        self.git("reset", "--hard", "ours")
        self.git("config", "merge.regen-inventory.driver",
                 f"bash {ROOT}/scripts/git-merge-regen-inventory.sh %O %A %B %P")
        self.git("merge", "--no-commit", "theirs", expected=1)
        self.assertNotIn("docs/generated/route-inventory.md",
                         self.git("diff", "--name-only", "--diff-filter=U").stdout)
        self.run_regen()
        self.assertEqual(REGEN.snapshot(self.root), expected)

    def test_duplicate_markers_fail_before_generator(self) -> None:
        self.merge_fixture()
        p = self.root / "ARCHITECTURE.md"
        p.write_text(p.read_text() + block("SRC TREE", "extra"))
        before = REGEN.snapshot(self.root)
        self.run_regen(expected=2)
        self.assertEqual(REGEN.snapshot(self.root), before)
        self.assertFalse((self.root / "artifacts/1/generator.log").exists())

    def test_missing_reversed_nested_or_conflicted_markers_fail(self) -> None:
        self.merge_fixture()
        p = self.root / "ARCHITECTURE.md"
        original = p.read_text()
        mutations = [
            original.replace("<!-- END GENERATED: SRC TREE -->", ""),
            original.replace("<!-- BEGIN GENERATED: SRC TREE -->", "END_PLACEHOLDER")
                    .replace("<!-- END GENERATED: SRC TREE -->", "<!-- BEGIN GENERATED: SRC TREE -->")
                    .replace("END_PLACEHOLDER", "<!-- END GENERATED: SRC TREE -->"),
            "<!-- BEGIN GENERATED: SRC TREE -->\n" + block("TOP LEVEL MODULE MAP")
                    + "<!-- END GENERATED: SRC TREE -->\n",
            original.replace("<<<<<<< HEAD\n", "<<<<<<< HEAD\n<!-- BEGIN GENERATED: SRC TREE -->\n"),
            original + block("UNKNOWN"),
            original.replace("<!-- BEGIN GENERATED: SRC TREE -->", "inline <!-- BEGIN GENERATED: SRC TREE -->"),
        ]
        for data in mutations:
            with self.subTest(data=data[:80]):
                p.write_text(data)
                self.run_regen(expected=2)
                self.assertEqual(p.read_text(), data)

    def test_manual_conflict_and_unowned_path_conflict_fail(self) -> None:
        self.merge_fixture(manual_conflict=True)
        self.run_regen(expected=2)
        self.assertIn("<<<<<<<", (self.root / "ARCHITECTURE.md").read_text())
        self.git("merge", "--abort")
        self.git("switch", "-c", "manual-ours", self.base)
        self.write("docs/architecture/layers.md", "ours\n")
        self.git("commit", "-am", "manual ours")
        self.git("switch", "-c", "manual-theirs", self.base)
        self.write("docs/architecture/layers.md", "theirs\n")
        self.git("commit", "-am", "manual theirs")
        self.git("switch", "manual-ours")
        self.git("merge", "--no-commit", "manual-theirs", expected=1)
        self.run_regen(expected=2)

    def test_staged_manual_conflict_markers_still_fail(self) -> None:
        self.write("docs/architecture/layers.md", "<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\n")
        self.git("add", "docs/architecture/layers.md")
        self.run_regen(expected=2)

    def test_generator_cannot_change_manual_regions(self) -> None:
        p = self.root / "scripts/generate_inventory_docs.py"
        p.write_text(p.read_text() + "\np=Path('ARCHITECTURE.md')\np.write_text(p.read_text()+'lost manual boundary\\n')\n")
        before = REGEN.snapshot(self.root)
        self.run_regen(expected=2)
        self.assertEqual(REGEN.snapshot(self.root), before)
        self.assertIn("lost manual boundary", (self.root / "artifacts/1/regeneration.diff").read_text())

    def test_generator_failure_is_not_no_change(self) -> None:
        self.write("scripts/generate_env_reference.py", "raise SystemExit(9)\n")
        report = self.run_regen(expected=2, check=True)
        self.assertEqual(report["status"], "CHECK_FAIL")
        self.assertIn("exit status 9", report["error"])
        self.assertFalse((self.root / "artifacts/1/head.diff").exists())

    def test_check_rejects_drift_against_head_even_after_prior_refresh(self) -> None:
        self.run_regen()
        self.run_regen(expected=1, check=True)
        self.assertTrue((self.root / "artifacts/2/head.diff").read_bytes())
        self.assertEqual((self.root / "artifacts/2/regeneration.diff").read_bytes(), b"")
        self.git("add", "--", *REGEN.OUTPUTS)
        self.git("commit", "-m", "reviewed generated correction")
        self.run_regen(check=True)

    def test_check_rejects_committed_output_deletion_and_records_addition(self) -> None:
        self.run_regen()
        self.git("add", "--", *REGEN.OUTPUTS)
        self.git("commit", "-m", "current generated docs")
        name = "docs/generated/env-reference.md"
        self.git("rm", "--", name)
        self.git("commit", "-m", "delete generated output")
        report = self.run_regen(expected=1, check=True)
        self.assertEqual(report["status"], "DRIFT")
        diff = (self.root / "artifacts/2/head.diff").read_text()
        self.assertIn("--- /dev/null", diff)
        self.assertIn("+++ b/" + name, diff)
        self.assertIn("+canonical", diff)
        self.assertEqual((self.root / name).read_text(), "canonical\n")

    def test_evidence_failure_and_restore_write_errors_do_not_stop_other_restores(self) -> None:
        original_diff = REGEN.write_diff
        original_write = Path.write_bytes
        cases = ((2, True, None), (3, True, None), (2, True, 0),
                 (3, True, 2), (2, False, 1), (3, False, 2))
        for number, (failed_generator, diff_error, failed_restore) in enumerate(cases):
            with self.subTest(generator=failed_generator, diff_error=diff_error, restore=failed_restore):
                for command in REGEN.COMMANDS[1:]:
                    self.write(command[0], "")
                self.write(REGEN.COMMANDS[failed_generator - 1][0], "raise SystemExit(9)\n")
                before = REGEN.snapshot(self.root)
                state = {"restoring": False, "attempts": []}
                artifact = self.root / f"fault-artifacts/{number}"

                def record_diff(*args):
                    state["restoring"] = True
                    if diff_error:
                        raise OSError("injected diff I/O error")
                    return original_diff(*args)

                def restore_write(path, data):
                    if state["restoring"] and str(path.relative_to(self.root)) in REGEN.OUTPUTS:
                        name = str(path.relative_to(self.root))
                        state["attempts"].append(name)
                        if failed_restore is not None and name == REGEN.OUTPUTS[failed_restore]:
                            raise OSError("injected restore I/O error")
                    return original_write(path, data)

                with patch.object(REGEN, "write_diff", side_effect=record_diff), \
                     patch.object(Path, "write_bytes", new=restore_write):
                    self.assertEqual(REGEN.regenerate(self.root, artifact, check=True), 2)
                self.assertEqual(state["attempts"], list(REGEN.OUTPUTS))
                report = json.loads((artifact / "manifest.json").read_text())
                self.assertEqual((report["status"], report["exit_code"]), ("CHECK_FAIL", 2))
                if diff_error:
                    self.assertIn("injected diff I/O error", str(report["evidence_errors"]))
                expected_errors = {} if failed_restore is None else {
                    REGEN.OUTPUTS[failed_restore]: "injected restore I/O error"}
                self.assertEqual(report.get("restore_errors", {}), expected_errors)
                for name in REGEN.OUTPUTS:
                    if failed_restore is None or name != REGEN.OUTPUTS[failed_restore]:
                        self.assertEqual((self.root / name).read_bytes(), before[name])
                for name, data in before.items():
                    (self.root / name).write_bytes(data)

    def test_markerless_legacy_doc_is_not_silently_overwritten(self) -> None:
        self.write("ARCHITECTURE.md", "legacy manual architecture\n")
        self.run_regen(expected=2)
        self.assertEqual((self.root / "ARCHITECTURE.md").read_text(), "legacy manual architecture\n")

    def test_unexpected_tracked_write_fails(self) -> None:
        p = self.root / "scripts/generate_cli_reference.py"
        p.write_text("from pathlib import Path\nPath('docs/generated/README.md').write_text('oops')\n")
        self.run_regen(expected=2)


class LandingRequiredCI(unittest.TestCase):
    def test_head_check_and_diff_upload_are_required_and_unconditional(self) -> None:
        jobs = yaml.safe_load((ROOT / ".github/workflows/ci-pr.yml").read_text())["jobs"]
        self.assertIn("scripts", jobs["required_contexts"]["needs"])
        job = jobs["scripts"]
        self.assertNotIn("continue-on-error", job)
        steps = job["steps"]
        gate = next(step for step in steps if step.get("name") == "Check generated documentation at PR head")
        self.assertNotIn("if", gate)
        self.assertNotIn("continue-on-error", gate)
        self.assertIn("set -euo pipefail", gate["run"])
        self.assertIn('git worktree add --detach "$RUNNER_TEMP/landing-regen-head" "$PR_HEAD_SHA"', gate["run"])
        self.assertEqual(gate["env"]["PR_HEAD_SHA"], "${{ github.event.pull_request.head.sha }}")
        self.assertIn("python3 scripts/landing/regen_after_merge.py --check", gate["run"])
        upload = next(step for step in steps if step.get("name") == "Upload landing regeneration diff")
        self.assertEqual(upload["if"], "always()")
        self.assertEqual(upload["uses"], "actions/upload-artifact@v4")
        self.assertEqual(upload["with"]["path"], "target/landing-regen/")
        self.assertEqual(upload["with"]["if-no-files-found"], "error")
        self.assertIn('if run_check contracts "Landing regeneration contracts"; then\n'
                      '"$PYTHON" -m unittest -v tests.test_landing_regeneration\nfi',
                      (ROOT / "scripts/ci-script-checks.sh").read_text())


if __name__ == "__main__":
    unittest.main()
