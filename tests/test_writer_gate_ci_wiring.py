"""Discrimination tests for the external writer-gate wiring checker (#5308)."""

from __future__ import annotations

import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts/check_writer_gate_ci_wiring.py"
SPEC = importlib.util.spec_from_file_location("writer_gate_ci_wiring", SCRIPT)
guard = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = guard
SPEC.loader.exec_module(guard)

EXPECTED_COMMANDS = (
    '"$PYTHON" scripts/check_delivery_journal_raw_writer.py',
    '"$PYTHON" scripts/check_durable_frontier_writer_call_sites.py',
    '"$PYTHON" -m unittest tests.test_durable_frontier_writer_call_sites',
    '"$PYTHON" scripts/check_intake_outbox_done_writer_call_sites.py',
    '"$PYTHON" -m unittest tests.test_intake_outbox_done_writer_call_sites',
    "./scripts/check-ci-runner-hardening.sh",
    '"$PYTHON" -m unittest tests.test_fast_check_ci_wiring',
)

AGGREGATE_SELF_PROTECTION_COMMANDS = EXPECTED_COMMANDS[-2:]


class WriterGateCiWiringTests(unittest.TestCase):
    def fixture_text(self) -> str:
        return "\n".join(("#!/usr/bin/env bash", *EXPECTED_COMMANDS, ""))

    def run_process(self, text: str) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            scripts = root / "scripts"
            scripts.mkdir()
            (scripts / "ci-script-checks.sh").write_text(text, encoding="utf-8")
            return subprocess.run(
                [sys.executable, str(SCRIPT), "--repo-root", str(root)],
                text=True,
                capture_output=True,
                check=False,
            )

    def test_required_inventory_is_independently_pinned(self) -> None:
        self.assertEqual(
            tuple(invocation.command for invocation in guard.REQUIRED_INVOCATIONS),
            EXPECTED_COMMANDS,
        )

    def test_real_tree_passes(self) -> None:
        self.assertEqual(guard.check(REPO_ROOT), [])

    def test_each_required_invocation_deletion_fails(self) -> None:
        baseline = self.fixture_text()
        for command in EXPECTED_COMMANDS:
            with self.subTest(command=command):
                mutated = baseline.replace(f"{command}\n", "", 1)
                self.assertNotEqual(mutated, baseline)
                errors = guard.check_text(mutated)
                self.assertTrue(errors)
                self.assertTrue(
                    any(command in error and "found 0" in error for error in errors),
                    errors,
                )

    def test_process_rejects_each_aggregate_self_protection_deletion(self) -> None:
        baseline = self.fixture_text()
        for command in AGGREGATE_SELF_PROTECTION_COMMANDS:
            with self.subTest(command=command):
                mutated = baseline.replace(f"{command}\n", "", 1)
                self.assertNotEqual(mutated, baseline)
                result = self.run_process(mutated)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(command, result.stderr)
                self.assertIn("found 0", result.stderr)

    def test_comment_echo_and_indentation_do_not_count(self) -> None:
        command = EXPECTED_COMMANDS[0]
        decoys = (
            f"# {command}",
            f"echo '{command}'",
            f"  {command}",
        )
        for decoy in decoys:
            with self.subTest(decoy=decoy):
                mutated = self.fixture_text().replace(command, decoy, 1)
                errors = guard.check_text(mutated)
                self.assertTrue(any("found 0" in error for error in errors), errors)

    def test_duplicate_invocation_fails(self) -> None:
        command = EXPECTED_COMMANDS[1]
        errors = guard.check_text(self.fixture_text() + f"{command}\n")
        self.assertTrue(any("found 2" in error for error in errors), errors)

    def test_each_tested_gate_must_precede_its_unittest(self) -> None:
        pairs = ((1, 2), (3, 4))
        for gate_index, test_index in pairs:
            with self.subTest(gate=EXPECTED_COMMANDS[gate_index]):
                commands = list(EXPECTED_COMMANDS)
                commands[gate_index], commands[test_index] = (
                    commands[test_index],
                    commands[gate_index],
                )
                errors = guard.check_text("\n".join(commands))
                self.assertTrue(any("must run before" in error for error in errors), errors)

    def test_process_exit_code_maps_pass_and_failure(self) -> None:
        passing = self.run_process(self.fixture_text())
        self.assertEqual(passing.returncode, 0, passing.stderr)
        self.assertIn("7 exact aggregate invocations protected", passing.stdout)

        command = EXPECTED_COMMANDS[4]
        failing = self.run_process(self.fixture_text().replace(f"{command}\n", "", 1))
        self.assertNotEqual(failing.returncode, 0)
        self.assertIn("found 0", failing.stderr)


if __name__ == "__main__":
    unittest.main()
