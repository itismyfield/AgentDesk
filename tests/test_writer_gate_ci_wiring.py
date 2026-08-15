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
    '"$PYTHON" scripts/check_sql_execution_surface_inventory.py --check',
    "git diff --exit-code HEAD -- scripts/sql_execution_surface_inventory.json",
    '"$PYTHON" -m unittest tests.test_sql_execution_surface_inventory',
    "./scripts/check-ci-runner-hardening.sh",
    '"$PYTHON" -m unittest tests.test_fast_check_ci_wiring',
)

AGGREGATE_SELF_PROTECTION_COMMANDS = EXPECTED_COMMANDS[-2:]

EXPECTED_HARDENING_SNIPPETS = (
    '''unless execution_contract(script_check_execution, expected_script_check_execution)
  expected = JSON.generate(canonical_yaml(expected_script_check_execution))
  found = JSON.generate(canonical_yaml(script_check_execution))
  warn "#{path}: Script checks aggregate effective execution changed; expected #{expected}; found #{found}"
  exit 1
end''',
    '''unless execution_contract(writer_wiring_execution, expected_writer_wiring_execution)
  expected = JSON.generate(canonical_yaml(expected_writer_wiring_execution))
  found = JSON.generate(canonical_yaml(writer_wiring_execution))
  warn "#{path}: writer gate aggregate wiring effective execution changed; expected #{expected}; found #{found}"
  exit 1
end''',
)


class WriterGateCiWiringTests(unittest.TestCase):
    def fixture_text(self) -> str:
        return "\n".join(("#!/usr/bin/env bash", *EXPECTED_COMMANDS, ""))

    def run_process(
        self, text: str, hardening_text: str | None = None
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            scripts = root / "scripts"
            scripts.mkdir()
            (scripts / "ci-script-checks.sh").write_text(text, encoding="utf-8")
            (scripts / "check-ci-runner-hardening.sh").write_text(
                hardening_text or "\n\n".join(EXPECTED_HARDENING_SNIPPETS) + "\n",
                encoding="utf-8",
            )
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
        self.assertEqual(guard.REQUIRED_HARDENING_SNIPPETS, EXPECTED_HARDENING_SNIPPETS)

    def test_real_tree_passes(self) -> None:
        self.assertEqual(guard.check(REPO_ROOT), [])

    def test_effective_execution_assertions_are_independently_pinned(self) -> None:
        self.assertEqual(
            guard.check_hardening_text("\n\n".join(EXPECTED_HARDENING_SNIPPETS)),
            [],
        )
        for snippet in EXPECTED_HARDENING_SNIPPETS:
            with self.subTest(snippet=snippet.splitlines()[0]):
                mutated = "\n\n".join(
                    candidate for candidate in EXPECTED_HARDENING_SNIPPETS if candidate != snippet
                )
                errors = guard.check_hardening_text(mutated)
                self.assertTrue(errors)
                self.assertIn("found 0", errors[0])

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
        for command in (EXPECTED_COMMANDS[1], EXPECTED_COMMANDS[5], EXPECTED_COMMANDS[6], EXPECTED_COMMANDS[7]):
            errors = guard.check_text(self.fixture_text() + f"{command}\n")
            self.assertTrue(any("found 2" in error for error in errors), errors)

    def test_aggregate_repin_is_forbidden_even_as_a_decoy(self) -> None:
        baseline = self.fixture_text()
        variants = (
            baseline.replace(
                EXPECTED_COMMANDS[5],
                EXPECTED_COMMANDS[5].replace("--check", "--write-baseline") + "\n" + EXPECTED_COMMANDS[5],
                1,
            ),
            baseline + "# --write-baseline\n",
        )
        for mutated in variants:
            with self.subTest(mutated=mutated.splitlines()[-1]):
                errors = guard.check_text(mutated)
                self.assertTrue(any("must not contain" in error for error in errors), errors)

    def test_each_tested_gate_must_precede_its_unittest(self) -> None:
        pairs = ((1, 2), (3, 4), (5, 7))
        for gate_index, test_index in pairs:
            with self.subTest(gate=EXPECTED_COMMANDS[gate_index]):
                commands = list(EXPECTED_COMMANDS)
                commands[gate_index], commands[test_index] = (
                    commands[test_index],
                    commands[gate_index],
                )
                errors = guard.check_text("\n".join(commands))
                self.assertTrue(any("must run before" in error for error in errors), errors)

    def test_dirty_worktree_guard_must_immediately_follow_check(self) -> None:
        mutated = self.fixture_text().replace(
            EXPECTED_COMMANDS[5] + "\n" + EXPECTED_COMMANDS[6],
            EXPECTED_COMMANDS[5] + "\necho decoy\n" + EXPECTED_COMMANDS[6],
            1,
        )
        errors = guard.check_text(mutated)
        self.assertTrue(any("must run immediately after" in error for error in errors), errors)

    def test_process_exit_code_maps_pass_and_failure(self) -> None:
        passing = self.run_process(self.fixture_text())
        self.assertEqual(passing.returncode, 0, passing.stderr)
        self.assertIn(
            "10 exact aggregate invocations and 2 effective-execution assertions protected",
            passing.stdout,
        )

        command = EXPECTED_COMMANDS[4]
        failing = self.run_process(self.fixture_text().replace(f"{command}\n", "", 1))
        self.assertNotEqual(failing.returncode, 0)
        self.assertIn("found 0", failing.stderr)


if __name__ == "__main__":
    unittest.main()
