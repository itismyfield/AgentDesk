"""Tests for the relay-authority named-target selection-floor gate (#5071)."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "check_relay_authority_contract.py"
_spec = importlib.util.spec_from_file_location("check_relay_authority_contract", SCRIPT)
assert _spec and _spec.loader
contract = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = contract
_spec.loader.exec_module(contract)


def active_lane(*, command: list[str] | None = None, minimum: int = 2) -> dict[str, object]:
    return {
        "name": "t1-fixture",
        "boundary": "T1",
        "status": "active",
        "module": "fixture::module",
        "command": command or ["cargo", "test", "--lib", "fixture::module"],
        "minimum": minimum,
        "derivation": "fixture",
    }


def write_workflow(
    repo_root: Path,
    lanes: list[dict[str, object]],
    *,
    mutation_step: str | None = None,
) -> None:
    commands = [
        "env -u AGENTDESK_ROOT_DIR "
        + " ".join(lane["command"])
        + " -- --test-threads=1"
        for lane in lanes
        if lane.get("status") == "active"
    ]
    steps: list[dict[str, object]] = [{
        "name": contract.RELAY_TARGET_STEP,
        "run": "\n".join(commands) + "\n",
    }]
    if mutation_step is not None:
        steps.append({"name": "Run condition-3 mutations", "run": mutation_step})
    workflow = repo_root / contract.PR_WORKFLOW
    workflow.parent.mkdir(parents=True, exist_ok=True)
    workflow.write_text(
        "jobs:\n"
        f"  {contract.RELAY_AUTHORITY_JOB}:\n"
        "    steps:\n"
        + "".join(
            f"      - name: {step['name']}\n        run:"
            + (f" |\n          {str(step['run']).replace(chr(10), chr(10) + '          ').rstrip()}\n"
               if "\n" in str(step["run"])
               else f" {step['run']}\n")
            for step in steps
        ),
        encoding="utf-8",
    )


def manifest_path(
    lanes: list[dict[str, object]],
    *,
    condition3_mutations_present: bool = False,
    mutation_step: str | None = None,
) -> tuple[tempfile.TemporaryDirectory[str], Path]:
    temporary = tempfile.TemporaryDirectory()
    repo_root = Path(temporary.name)
    path = repo_root / "targets.json"
    path.write_text(
        json.dumps({
            "schema_version": 1,
            "condition3_mutations_present": condition3_mutations_present,
            "lanes": lanes,
        }),
        encoding="utf-8",
    )
    write_workflow(repo_root, lanes, mutation_step=mutation_step)
    return temporary, path


class ManifestContract(unittest.TestCase):
    def test_checked_in_manifest_declares_active_and_gap_rows(self) -> None:
        lanes, gaps = contract.load_active_lanes(
            REPO_ROOT / "scripts" / "relay_authority_contract_targets.json",
            REPO_ROOT,
        )
        self.assertEqual([lane.name for lane in lanes], [
            "t1-sink-terminal-handoff",
            "t4-single-actor-recovery-decision",
            "relay-e2e-local-model-queue-wake",
        ])
        self.assertEqual({gap["boundary"] for gap in gaps}, {"T2", "T3", "T5"})
        self.assertTrue(all(lane.minimum > 0 for lane in lanes))

    def test_condition3_false_rejects_existing_mutation_script(self) -> None:
        temporary, path = manifest_path([active_lane()])
        with temporary:
            repo_root = Path(temporary.name)
            mutation_script = repo_root / "scripts" / "run_relay_authority_mutations.sh"
            mutation_script.parent.mkdir(exist_ok=True)
            mutation_script.touch()
            with self.assertRaisesRegex(
                contract.ManifestError,
                "condition3_mutations_present is false but .*run_relay_authority_mutations.sh exists",
            ):
                contract.load_active_lanes(path, repo_root)

    def test_condition3_true_rejects_missing_mutation_script(self) -> None:
        temporary, path = manifest_path(
            [active_lane()], condition3_mutations_present=True
        )
        with temporary:
            with self.assertRaisesRegex(
                contract.ManifestError,
                "condition3_mutations_present is true but .*run_relay_authority_mutations.sh is missing",
            ):
                contract.load_active_lanes(path, Path(temporary.name))

    def test_condition3_true_rejects_missing_unconditional_workflow_step(self) -> None:
        temporary, path = manifest_path(
            [active_lane()], condition3_mutations_present=True
        )
        with temporary:
            repo_root = Path(temporary.name)
            mutation_script = repo_root / contract.CONDITION3_MUTATION_SCRIPT
            mutation_script.parent.mkdir(exist_ok=True)
            mutation_script.write_text("#!/usr/bin/env bash\nset -euo pipefail\ntrue\n", encoding="utf-8")
            mutation_script.chmod(0o755)
            with self.assertRaisesRegex(
                contract.ManifestError,
                "must contain exactly one unconditional run step",
            ):
                contract.load_active_lanes(path, repo_root)

    def test_condition3_true_accepts_script_and_unconditional_workflow_step(self) -> None:
        temporary, path = manifest_path(
            [active_lane()],
            condition3_mutations_present=True,
            mutation_step=contract.CONDITION3_MUTATION_COMMAND,
        )
        with temporary:
            repo_root = Path(temporary.name)
            mutation_script = repo_root / contract.CONDITION3_MUTATION_SCRIPT
            mutation_script.parent.mkdir(exist_ok=True)
            mutation_script.write_text("#!/usr/bin/env bash\nset -euo pipefail\ntrue\n", encoding="utf-8")
            mutation_script.chmod(0o755)
            lanes, _ = contract.load_active_lanes(path, repo_root)
            self.assertEqual([lane.name for lane in lanes], ["t1-fixture"])

    def test_condition3_true_rejects_if_guarded_workflow_step(self) -> None:
        temporary, path = manifest_path(
            [active_lane()], condition3_mutations_present=True
        )
        with temporary:
            repo_root = Path(temporary.name)
            mutation_script = repo_root / contract.CONDITION3_MUTATION_SCRIPT
            mutation_script.parent.mkdir(exist_ok=True)
            mutation_script.write_text("#!/usr/bin/env bash\nset -euo pipefail\ntrue\n", encoding="utf-8")
            mutation_script.chmod(0o755)
            workflow = repo_root / contract.PR_WORKFLOW
            text = workflow.read_text(encoding="utf-8")
            workflow.write_text(
                text + (
                    "      - name: Run condition-3 mutations\n"
                    "        if: ${{ false }}\n"
                    f"        run: {contract.CONDITION3_MUTATION_COMMAND}\n"
                ),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                contract.ManifestError,
                "must contain exactly one unconditional run step",
            ):
                contract.load_active_lanes(path, repo_root)

    def test_condition3_true_rejects_empty_script(self) -> None:
        temporary, path = manifest_path(
            [active_lane()],
            condition3_mutations_present=True,
            mutation_step=contract.CONDITION3_MUTATION_COMMAND,
        )
        with temporary:
            repo_root = Path(temporary.name)
            mutation_script = repo_root / contract.CONDITION3_MUTATION_SCRIPT
            mutation_script.parent.mkdir(exist_ok=True)
            mutation_script.touch(mode=0o755)
            with self.assertRaisesRegex(contract.ManifestError, "must not be empty"):
                contract.load_active_lanes(path, repo_root)

    def test_condition3_true_rejects_exit_zero_only_script(self) -> None:
        temporary, path = manifest_path(
            [active_lane()],
            condition3_mutations_present=True,
            mutation_step=contract.CONDITION3_MUTATION_COMMAND,
        )
        with temporary:
            repo_root = Path(temporary.name)
            mutation_script = repo_root / contract.CONDITION3_MUTATION_SCRIPT
            mutation_script.parent.mkdir(exist_ok=True)
            mutation_script.write_text("#!/usr/bin/env bash\nexit 0\n", encoding="utf-8")
            mutation_script.chmod(0o755)
            with self.assertRaisesRegex(contract.ManifestError, "exit-0-only placeholder"):
                contract.load_active_lanes(path, repo_root)

    def test_condition3_true_rejects_non_executable_script(self) -> None:
        temporary, path = manifest_path(
            [active_lane()],
            condition3_mutations_present=True,
            mutation_step=contract.CONDITION3_MUTATION_COMMAND,
        )
        with temporary:
            repo_root = Path(temporary.name)
            mutation_script = repo_root / contract.CONDITION3_MUTATION_SCRIPT
            mutation_script.parent.mkdir(exist_ok=True)
            mutation_script.write_text("#!/usr/bin/env bash\ntrue\n", encoding="utf-8")
            mutation_script.chmod(0o644)
            with self.assertRaisesRegex(contract.ManifestError, "must be executable"):
                contract.load_active_lanes(path, repo_root)

    def test_condition3_true_rejects_symlink_script(self) -> None:
        temporary, path = manifest_path(
            [active_lane()],
            condition3_mutations_present=True,
            mutation_step=contract.CONDITION3_MUTATION_COMMAND,
        )
        with temporary:
            repo_root = Path(temporary.name)
            target = repo_root / "unrelated.sh"
            target.write_text("#!/usr/bin/env bash\ntrue\n", encoding="utf-8")
            target.chmod(0o755)
            mutation_script = repo_root / contract.CONDITION3_MUTATION_SCRIPT
            mutation_script.parent.mkdir(exist_ok=True)
            mutation_script.symlink_to(target)
            with self.assertRaisesRegex(contract.ManifestError, "must not be a symlink"):
                contract.load_active_lanes(path, repo_root)

    def test_manifest_command_must_match_workflow_command(self) -> None:
        temporary, path = manifest_path([active_lane()])
        with temporary:
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["lanes"][0]["command"][-1] = "fixture::other"
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(contract.ManifestError, "must exactly match"):
                contract.load_active_lanes(path, Path(temporary.name))

    def test_unfiltered_command_is_rejected(self) -> None:
        temporary, path = manifest_path([active_lane(command=["cargo", "test", "--lib"])])
        with temporary:
            with self.assertRaisesRegex(contract.ManifestError, "explicit test filter"):
                contract.load_active_lanes(path)

    def test_all_targets_command_is_rejected(self) -> None:
        temporary, path = manifest_path([
            active_lane(command=["cargo", "test", "--lib", "--all-targets", "fixture"])
        ])
        with temporary:
            with self.assertRaisesRegex(contract.ManifestError, "--all-targets"):
                contract.load_active_lanes(path)

    def test_zero_floor_is_rejected(self) -> None:
        temporary, path = manifest_path([active_lane(minimum=0)])
        with temporary:
            with self.assertRaisesRegex(contract.ManifestError, "integer >= 1"):
                contract.load_active_lanes(path)


class SelectionContract(unittest.TestCase):
    def test_list_count_uses_test_ids_not_cargo_summary(self) -> None:
        output = "a::one: test\na::two: test\n2 tests, 0 benchmarks\n"
        self.assertEqual(contract.count_test_ids(output), 2)

    def test_zero_selection_is_fatal_even_when_cargo_exits_zero(self) -> None:
        lane = contract.Lane("zero", "T1", "fixture", ("cargo", "test", "--lib", "missing"), 1)
        result = contract.LaneResult(lane, 0, 0, contract.list_command(lane), "0 tests")
        self.assertEqual(contract.failures_for(result), [
            "selected 0 tests",
            "selected 0 below declared minimum 1",
        ])

    def test_selection_below_floor_is_fatal(self) -> None:
        lane = contract.Lane("floor", "T4", "fixture", ("cargo", "test", "--lib", "fixture"), 3)
        result = contract.LaneResult(lane, 2, 0, contract.list_command(lane), "")
        self.assertEqual(contract.failures_for(result), [
            "selected 2 below declared minimum 3"
        ])

    def test_run_lane_appends_list_and_removes_agentdesk_root(self) -> None:
        lane = contract.Lane("lane", "T1", "fixture", ("cargo", "test", "--lib", "fixture"), 1)
        observed: dict[str, object] = {}

        def runner(command, **kwargs):
            observed["command"] = command
            observed["env"] = kwargs["env"]
            return subprocess.CompletedProcess(command, 0, "fixture::one: test\n", "")

        with mock.patch.dict("os.environ", {"AGENTDESK_ROOT_DIR": "/wrong"}):
            result = contract.run_lane(lane, REPO_ROOT, runner=runner)
        self.assertEqual(tuple(observed["command"]), contract.list_command(lane))
        self.assertNotIn("AGENTDESK_ROOT_DIR", observed["env"])
        self.assertEqual(result.selected, 1)


class MainContract(unittest.TestCase):
    def _run(self, result: contract.LaneResult) -> tuple[int, str, str]:
        temporary, manifest = manifest_path([active_lane(minimum=result.lane.minimum)])
        with temporary, mock.patch.object(contract, "run_lane", return_value=result):
            stdout, stderr = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                rc = contract.main([
                    "--repo-root", str(Path(temporary.name)),
                    "--manifest", str(manifest),
                ])
        return rc, stdout.getvalue(), stderr.getvalue()

    def test_zero_selection_main_returns_one(self) -> None:
        lane = contract.Lane("t1-fixture", "T1", "fixture", ("cargo", "test", "--lib", "missing"), 1)
        rc, stdout, stderr = self._run(
            contract.LaneResult(lane, 0, 0, contract.list_command(lane), "0 tests")
        )
        self.assertEqual(rc, 1)
        self.assertIn("selected=0 minimum=1 rc=0", stdout)
        self.assertIn("selected 0 tests", stderr)

    def test_floor_failure_main_returns_one(self) -> None:
        lane = contract.Lane("t1-fixture", "T1", "fixture", ("cargo", "test", "--lib", "fixture"), 3)
        rc, stdout, stderr = self._run(
            contract.LaneResult(lane, 2, 0, contract.list_command(lane), "")
        )
        self.assertEqual(rc, 1)
        self.assertIn("selected=2 minimum=3", stdout)
        self.assertIn("below declared minimum 3", stderr)

    def test_clean_selection_main_returns_zero(self) -> None:
        lane = contract.Lane("t1-fixture", "T1", "fixture", ("cargo", "test", "--lib", "fixture"), 2)
        rc, stdout, stderr = self._run(
            contract.LaneResult(lane, 2, 0, contract.list_command(lane), "")
        )
        self.assertEqual(rc, 0)
        self.assertIn("selected=2 minimum=2", stdout)
        self.assertEqual(stderr, "")


if __name__ == "__main__":
    unittest.main()
