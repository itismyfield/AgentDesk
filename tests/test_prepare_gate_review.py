from __future__ import annotations

import io
import subprocess
import sys
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import prepare_gate_review as PREPARE
import check_legacy_supervision_census as CENSUS
import test_only_module_skip_pin as PIN
import generate_inventory_docs as GEN


class PrepareGateReviewTest(unittest.TestCase):
    def test_commands_regenerate_only_review_artifacts_without_commit_or_repin(self):
        with patch.object(PREPARE.subprocess, "run", return_value=SimpleNamespace(returncode=0)) as run:
            self.assertEqual(PREPARE.prepare(ROOT), 0)
        commands = [call.args[0][2:] for call in run.call_args_list]
        self.assertEqual(commands, [
            ["--sync-skips"], ["--review-docs-only", "--sync-giant-docs"], [], [],
        ])
        self.assertTrue(all(call.kwargs["cwd"] == ROOT for call in run.call_args_list))
        self.assertEqual([Path(call.args[0][1]).name for call in run.call_args_list], [command[0] for command in PREPARE.COMMANDS])

    def test_subprocess_failure_and_signal_stop_preparation(self):
        for status in (1, 2, -9):
            with self.subTest(status=status), patch.object(PREPARE.subprocess, "run", return_value=SimpleNamespace(returncode=status)) as run:
                self.assertNotEqual(PREPARE.prepare(ROOT), 0)
                self.assertEqual(run.call_count, 1)

    def test_process_launch_exception_never_reports_ready(self):
        output = io.StringIO()
        with patch.object(PREPARE.subprocess, "run", side_effect=OSError("cannot launch")), redirect_stdout(output):
            with self.assertRaises(OSError):
                PREPARE.prepare(ROOT)
        self.assertNotIn("REVIEW_ARTIFACTS_READY", output.getvalue())

    def test_child_python_exception_fails_without_ready_output(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "scripts").mkdir()
            (root / "scripts/check_legacy_supervision_census.py").write_text("raise ValueError('classification crashed')\n")
            result = subprocess.run([sys.executable, str(ROOT / "scripts/prepare_gate_review.py"),
                                     "--repo-root", str(root)], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("classification crashed", result.stderr)
            self.assertNotIn("REVIEW_ARTIFACTS_READY", result.stdout)

    def test_drift_fixture_is_corrected_and_recheck_succeeds(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            scripts = root / "scripts"
            scripts.mkdir()
            docs = ["ARCHITECTURE.md", "docs/generated/route-inventory.md", "docs/generated/worker-inventory.md",
                    "docs/generated/env-reference.md", "docs/generated/cli-reference.md"]
            writes = [[], docs[:3], docs[3:4], docs[4:]]
            for command, outputs in zip(PREPARE.COMMANDS, writes):
                (scripts / command[0]).write_text(
                    "from pathlib import Path\n" +
                    "for name in " + repr(outputs) + ":\n"
                    " p = Path(name); p.parent.mkdir(parents=True, exist_ok=True); p.write_text('fresh\\n')\n")
            untouched = root / "docs/generated/manual.md"
            untouched.parent.mkdir(parents=True)
            untouched.write_text("manual\n")
            for name in docs:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("drift\n")
            self.assertEqual(PREPARE.prepare(root), 0)
            self.assertTrue(all((root / name).read_text() == "fresh\n" for name in docs))
            self.assertEqual(untouched.read_text(), "manual\n")
            self.assertEqual(PREPARE.prepare(root), 0)


class SafeAdditiveSkipTest(unittest.TestCase):
    def _fixture(self, root, *, pinned=True, child=True, production=False):
        (root / "src").mkdir()
        (root / "scripts").mkdir()
        (root / "src/lib.rs").write_text(("" if production else "#[cfg(test)]\n") + "mod harness;\n")
        if child:
            (root / "src/harness.rs").write_text("#[test]\nfn fixture() {}\n")
        existing = {"src/harness.rs"} if pinned else set()
        source = 'PINNED_RESOLVER_TEST_ONLY_FILES = frozenset(\n    {\n'
        if pinned:
            source += '        "src/harness.rs",\n'
        path = root / "scripts/test_only_module_skip_pin.py"
        path.write_text(source + '    }\n)\n')
        pin = SimpleNamespace(_lexical_rust_files=PIN._lexical_rust_files, _INVENTORY=GEN,
                              is_basename_test_file=PIN.is_basename_test_file,
                              validate_basename_mounts=PIN.validate_basename_mounts,
                              PINNED_RESOLVER_TEST_ONLY_FILES=frozenset(existing))
        return path, pin

    def test_verified_test_only_addition_is_registered(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            path, pin = self._fixture(root, pinned=False)
            before = path.read_bytes()
            with patch.object(CENSUS.classifier, "_SKIP_PIN", pin):
                CENSUS.sync_skips(root)
            fragment = root / "scripts/test_only_module_skip_pin.d" / CENSUS.registry.auto_fragment("src/harness.rs")
            self.assertEqual(fragment.read_text(), "resolver | src/harness.rs\n")
            self.assertEqual(path.read_bytes(), before)

    def test_deletion_and_production_promotion_are_refused_unchanged(self):
        for kwargs in ({"child": False}, {"production": True}):
            with self.subTest(kwargs=kwargs), TemporaryDirectory() as tmp:
                root = Path(tmp)
                path, pin = self._fixture(root, **kwargs)
                before = path.read_bytes()
                with patch.object(CENSUS.classifier, "_SKIP_PIN", pin), self.assertRaisesRegex(RuntimeError, "refusing pin removal"):
                    CENSUS.sync_skips(root)
                self.assertEqual(path.read_bytes(), before)

    def test_unknown_symlink_classification_is_refused_unchanged(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            path, pin = self._fixture(root, pinned=False)
            (root / "src/unknown.rs").symlink_to(root / "src/harness.rs")
            before = path.read_bytes()
            with patch.object(CENSUS.classifier, "_SKIP_PIN", pin), self.assertRaisesRegex(RuntimeError, "symlinks"):
                CENSUS.sync_skips(root)
            self.assertEqual(path.read_bytes(), before)


class GiantDocumentationSyncTest(unittest.TestCase):
    def _module(self, path, lines):
        return GEN.ModuleEntry(path, path, lines, lines, 0, ("giant-file",) if lines >= 1000 else ())

    def test_sync_corrects_only_generated_block_and_recheck_passes(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = root / "docs/agent-maintenance/change-surfaces.md"
            path.parent.mkdir(parents=True)
            manual = "# Manual purpose\n\nPreserve reviewed ownership.\n"
            path.write_text(manual)
            with patch.object(GEN, "REPO_ROOT", root):
                documents = dict([GEN.giant_change_surface_document([self._module("src/large.rs", 1001)])])
                self.assertEqual(GEN.write_documents(documents, check=True), 1)
                self.assertEqual(GEN.write_documents(documents, check=False), 0)
                self.assertEqual(GEN.write_documents(documents, check=True), 0)
                updated = GEN.giant_change_surface_document([self._module("src/large.rs", 999)])[1]
            self.assertTrue(updated.startswith(manual.rstrip()))
            self.assertNotIn("src/large.rs", updated)
            self.assertIn("Preserve reviewed ownership.", updated)

    def test_ambiguous_block_and_unreadable_doc_fail_closed(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = root / "docs/agent-maintenance/change-surfaces.md"
            path.parent.mkdir(parents=True)
            with patch.object(GEN, "REPO_ROOT", root), self.assertRaises(OSError):
                GEN.giant_change_surface_document([])
            path.write_text("<!-- BEGIN GENERATED: GIANT PATHS -->\nmanual\n")
            before = path.read_bytes()
            with patch.object(GEN, "REPO_ROOT", root), self.assertRaises(GEN.ParseError):
                GEN.giant_change_surface_document([])
            self.assertEqual(path.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
