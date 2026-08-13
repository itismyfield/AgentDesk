"""Contract and mutation tests for the #5071 T2 intake-outbox done-writer gate."""

from __future__ import annotations

import importlib.util
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check_intake_outbox_done_writer_call_sites.py"
SPEC = importlib.util.spec_from_file_location("intake_outbox_done_writer_guard", SCRIPT)
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)

EXPECTED_CALL_SITES = {
    "mark_done": {"src/services/cluster/intake_worker.rs": 1},
    "mark_done_from_delivery_proof": {"src/services/discord/intake_delivery_reconciler.rs": 1},
}


def write(root: Path, rel: str, body: str) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")


class SourceContractTests(unittest.TestCase):
    def test_real_tree_passes_and_reports_declared_limits(self):
        ok, message = guard.check(ROOT)
        self.assertTrue(ok, message)
        self.assertIn("2 production sites across 2 symbols", message)
        for limit in ("not Rust parsing", "direct SQL writers are NOT seen", "over-counted"):
            self.assertIn(limit, message)

    def test_allowlist_is_the_t2_done_writer_only(self):
        self.assertEqual(guard.EXPECTED_CALL_SITES, EXPECTED_CALL_SITES)

    def test_scan_root_is_all_of_src(self):
        self.assertEqual(guard.SCAN_ROOT.as_posix(), "src")

    def test_ci_script_checks_runs_the_gate_and_its_tests(self):
        """Check wiring spelling/order, not its own execution.

        When this module runs independently, deletion of the gate command fails
        below. It cannot protect deletion of its own unittest invocation from
        ci-script-checks.sh, because that prevents this test from running there.
        """
        wiring = (ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn("scripts/check_intake_outbox_done_writer_call_sites.py", wiring)
        self.assertIn("tests.test_intake_outbox_done_writer_call_sites", wiring)
        self.assertLess(
            wiring.index("scripts/check_intake_outbox_done_writer_call_sites.py"),
            wiring.index("tests.test_intake_outbox_done_writer_call_sites"),
        )

    def test_allowlisted_symbol_is_imported_by_its_owner_function_file(self):
        worker = (ROOT / "src/services/cluster/intake_worker.rs").read_text(encoding="utf-8")
        self.assertIn("pub(crate) async fn run_intake_worker_tick(", worker)
        self.assertIn("mark_done(pool, row.id, claim_owner)", worker)
        reconciler = (ROOT / "src/services/discord/intake_delivery_reconciler.rs").read_text(
            encoding="utf-8"
        )
        self.assertIn("async fn reconcile_row(", reconciler)
        self.assertIn("mark_done_from_delivery_proof(&mut tx, row_id)", reconciler)


class DiscriminationTests(unittest.TestCase):
    def fixture(self) -> Path:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        write(
            root,
            "src/services/cluster/intake_worker.rs",
            "use crate::db::intake_outbox::{mark_done, mark_spawned};\n"
            "fn run_intake_worker_tick() { mark_done(); }\n",
        )
        write(root, "src/services/discord/intake_delivery_reconciler.rs", "use crate::db::intake_outbox_delivery_proof::mark_done_from_delivery_proof;\nfn reconcile() { mark_done_from_delivery_proof(); }\n")
        return root

    def run_guard(self, root: Path, expected=None) -> tuple[bool, str]:
        original = guard.EXPECTED_CALL_SITES
        guard.EXPECTED_CALL_SITES = expected if expected is not None else EXPECTED_CALL_SITES
        try:
            return guard.check(root)
        finally:
            guard.EXPECTED_CALL_SITES = original

    def test_baseline_fixture_is_green(self):
        ok, message = self.run_guard(self.fixture())
        self.assertTrue(ok, message)

    def test_script_process_exit_code_maps_pass_and_failure(self):
        passing = subprocess.run(
            [sys.executable, str(SCRIPT)],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(passing.returncode, 0, passing.stderr)

        root = self.fixture()
        copied_script = root / "scripts/check_intake_outbox_done_writer_call_sites.py"
        copied_script.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(SCRIPT, copied_script)
        write(
            root,
            "src/services/cluster/receipt_sink.rs",
            "use crate::db::intake_outbox::mark_done;\n"
            "fn receipt() { mark_done(); }\n",
        )
        failing = subprocess.run(
            [sys.executable, str(copied_script)],
            cwd=root,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(failing.returncode, 0, failing.stdout + failing.stderr)
        self.assertIn("UNLISTED call site", failing.stderr)

    def test_allowlist_entry_deleted_is_fail_closed(self):
        ok, message = self.run_guard(self.fixture(), {"mark_done": {}})
        self.assertFalse(ok)
        self.assertIn("mark_done: UNLISTED call site", message)

        ok, message = self.run_guard(self.fixture(), {})
        self.assertFalse(ok)
        self.assertIn("mark_done: UNLISTED call site", message)

    def test_unlisted_writer_is_fail_closed(self):
        root = self.fixture()
        write(
            root,
            "src/services/cluster/receipt_sink.rs",
            "use crate::db::intake_outbox;\n"
            "fn receipt() { intake_outbox::mark_done(); }\n",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn(
            "mark_done: UNLISTED call site in src/services/cluster/receipt_sink.rs (1x)",
            message,
        )

    def test_allowlist_path_typo_is_fail_closed(self):
        typo = {"mark_done": {"src/services/cluster/intake_wroker.rs": 1}}
        ok, message = self.run_guard(self.fixture(), typo)
        self.assertFalse(ok)
        self.assertIn("call site GONE from src/services/cluster/intake_wroker.rs", message)
        self.assertIn("UNLISTED call site in src/services/cluster/intake_worker.rs", message)

    def test_proof_writer_removal_unlisted_path_and_qualified_forms_fail_closed(self):
        root = self.fixture()
        write(root, "src/services/discord/intake_delivery_reconciler.rs", "fn reconcile() {}\n")
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn("mark_done_from_delivery_proof: call site GONE", message)

        root = self.fixture()
        write(
            root,
            "src/services/discord/other.rs",
            "use crate::db::intake_outbox_delivery_proof::mark_done_from_delivery_proof;\n"
            "fn proof() { mark_done_from_delivery_proof(); }\n",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn("mark_done_from_delivery_proof: UNLISTED call site", message)

        root = self.fixture()
        write(
            root,
            "src/services/discord/qualified.rs",
            "fn proof() { crate::db::intake_outbox_delivery_proof::"
            "mark_done_from_delivery_proof(); }\n",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn("mark_done_from_delivery_proof: UNLISTED call site", message)

        typo = {
            **EXPECTED_CALL_SITES,
            "mark_done_from_delivery_proof": {
                "src/services/discord/intake_delivery_reconcile.rs": 1
            },
        }
        ok, message = self.run_guard(self.fixture(), typo)
        self.assertFalse(ok)
        self.assertIn("intake_delivery_reconcile.rs", message)

    def test_cfg_test_with_adjacent_attribute_is_not_a_production_site(self):
        root = self.fixture()
        write(
            root,
            "src/services/discord/test_only.rs",
            "use crate::db::intake_outbox_delivery_proof::mark_done_from_delivery_proof;\n"
            "#[cfg(test)] #[rustfmt::skip]\n"
            "mod tests { fn probe() { mark_done_from_delivery_proof(); } }\n",
        )
        ok, message = self.run_guard(root)
        self.assertTrue(ok, message)

    def test_cfg_test_writer_call_is_not_a_production_site(self):
        root = self.fixture()
        write(
            root,
            "src/services/cluster/test_only.rs",
            "use crate::db::intake_outbox::mark_done;\n"
            "#[cfg(test)]\nmod tests { fn probe() { mark_done(); } }\n",
        )
        ok, message = self.run_guard(root)
        self.assertTrue(ok, message)

    def test_lifetime_does_not_hide_later_fully_qualified_writer(self):
        root = self.fixture()
        write(
            root,
            "src/services/session_backend.rs",
            "fn receipt<'a>() { crate::db::intake_outbox::mark_done(); }\n",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn(
            "mark_done: UNLISTED call site in src/services/session_backend.rs (1x)",
            message,
        )


if __name__ == "__main__":
    unittest.main()
