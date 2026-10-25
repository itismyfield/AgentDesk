"""Tests for the relay-authority named-target selection-floor gate over the nextest list."""

from __future__ import annotations

import contextlib
import copy
import functools
import importlib.util
import io
import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import yaml

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "check_relay_authority_contract.py"
_spec = importlib.util.spec_from_file_location("check_relay_authority_contract", SCRIPT)
assert _spec and _spec.loader
contract = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = contract
_spec.loader.exec_module(contract)
sys.path.insert(0, str(REPO_ROOT / "scripts" / "ci"))
import nextest_required as required  # noqa: E402


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
    mutation_steps = (
        [{"name": "Run condition-3 mutations", "run": mutation_step}]
        if mutation_step is not None else []
    )
    workflow = repo_root / contract.PR_WORKFLOW
    workflow.parent.mkdir(parents=True, exist_ok=True)
    workflow.write_text(
        yaml.safe_dump({"jobs": {
            "changes": {"outputs": {"rust_tests_skip": contract.VERIFIED_RUST_SKIP}},
            contract.JUDGE_JOB: {
                "if": "${{ !cancelled() }}", "needs": list(contract.JUDGE_NEEDS),
                "strategy": {"fail-fast": False, "matrix": {"context": required.matrix_expression()}},
                "steps": [{"run": "python3 scripts/ci/nextest_ci.py required-context x"}],
            },
            contract.BUILD_JOB: {"steps": [{"name": "Archive, list, compare and plan",
                                            "run": "set -euo pipefail\n" + contract.FLOOR_COMMAND + "\n"}]},
            contract.RELAY_AUTHORITY_MUTATIONS_JOB: {
                "needs": "changes", "if": contract.RUST_REQUIRED_IF,
                "strategy": {"fail-fast": False, "matrix": {"shard": [0, 1, 2]}},
                "env": {
                    "RELAY_AUTHORITY_MUTATION_SHARD_INDEX": "${{ matrix.shard }}",
                    "RELAY_AUTHORITY_MUTATION_SHARD_TOTAL": "3",
                },
                "steps": mutation_steps,
            },
        }}),
        encoding="utf-8",
    )


def write_nextest_list(path: Path, names: list[str], ignored: tuple[str, ...] = ()) -> Path:
    """An unfiltered `cargo nextest list --message-format json` of one lib binary."""
    cases = {name: {"ignored": name in ignored, "filter-match": {"status": "matches"}} for name in names}
    path.write_text(json.dumps({"rust-suites": {"agentdesk": {
        "binary-id": "agentdesk", "status": "listed", "testcases": cases}}}), encoding="utf-8")
    return path


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
    def test_execution_graph_cannot_skip_contract_lanes(self) -> None:
        lanes, _ = contract.load_active_lanes(REPO_ROOT / contract.DEFAULT_MANIFEST)
        original = yaml.safe_load((REPO_ROOT / contract.PR_WORKFLOW).read_text(encoding="utf-8"))
        judge_needs = original["jobs"][contract.JUDGE_JOB]["needs"]
        mutations = [
            (contract.JUDGE_JOB, "if", None, "must always judge"),
            (contract.JUDGE_JOB, "if", "success()", "must always judge"),
            (contract.JUDGE_JOB, "continue-on-error", True, "must always judge"),
            (contract.JUDGE_JOB, "needs", [need for need in judge_needs if need != contract.RELAY_AUTHORITY_MUTATIONS_JOB], "must always judge"),
            (contract.JUDGE_JOB, "needs", [need for need in judge_needs if need != "test_evidence"], "must always judge"),
            (contract.JUDGE_JOB, "strategy", {"fail-fast": False, "matrix": {"context": ["Lint"]}}, "must always judge"),
            (contract.JUDGE_JOB, "strategy", {"fail-fast": False, "matrix": {"context": list(required.CONTEXTS)}}, "must always judge"),
            (contract.RELAY_AUTHORITY_MUTATIONS_JOB, "needs", ["changes"], "must use the verified skip"),
            (contract.RELAY_AUTHORITY_MUTATIONS_JOB, "if", "false", "must use the verified skip"),
            (contract.RELAY_AUTHORITY_MUTATIONS_JOB, "if", contract.RUST_REQUIRED_IF.replace("!= 'true'", "== 'false'"), "must use the verified skip"),
            (contract.RELAY_AUTHORITY_MUTATIONS_JOB, "continue-on-error", True, "must use the verified skip"),
        ]
        for job_id, key, value, message in mutations:
            with self.subTest(job=job_id, key=key, value=value), tempfile.TemporaryDirectory() as temporary:
                payload = copy.deepcopy(original)
                job = payload["jobs"][job_id]
                if value is None:
                    job.pop(key)
                else:
                    job[key] = value
                root = Path(temporary)
                workflow = root / contract.PR_WORKFLOW
                workflow.parent.mkdir(parents=True)
                workflow.write_text(yaml.safe_dump(payload), encoding="utf-8")
                with self.assertRaisesRegex(contract.ManifestError, message):
                    contract.validate_workflow_contract(root, lanes, True)

    def test_judge_matrix_must_be_the_canonical_two_branch_expression(self) -> None:
        lanes, _ = contract.load_active_lanes(REPO_ROOT / contract.DEFAULT_MANIFEST)
        original = yaml.safe_load((REPO_ROOT / contract.PR_WORKFLOW).read_text(encoding="utf-8"))
        native = list(required.CONTEXTS)
        without_relay = [name for name in native if name != contract.RELAY_AUTHORITY_CONTEXT]
        canonical = required.matrix_expression()
        self.assertEqual(original["jobs"][contract.JUDGE_JOB]["strategy"]["matrix"]["context"], canonical)
        rejected = {
            "fork list without relay": required.matrix_expression(fork=without_relay),
            "fork list with relay twice": required.matrix_expression(fork=native + [contract.RELAY_AUTHORITY_CONTEXT]),
            "same-repo list renders a required name": required.matrix_expression(same_repo=[required.PUBLISHER, "Lint"]),
            "same-repo list without the publisher": required.matrix_expression(same_repo=native),
            "arbitrary string": "relay-authority-contract",
            "expression from another predicate": canonical.replace("github.repository", "github.event.repository.full_name"),
            "non-canonical JSON spacing": canonical.replace("'[\"Required", "'[ \"Required"),
        }
        for label, matrix in rejected.items():
            with self.subTest(label), tempfile.TemporaryDirectory() as temporary:
                payload = copy.deepcopy(original)
                payload["jobs"][contract.JUDGE_JOB]["strategy"]["matrix"]["context"] = matrix
                root = Path(temporary)
                workflow = root / contract.PR_WORKFLOW
                workflow.parent.mkdir(parents=True)
                workflow.write_text(yaml.safe_dump(payload), encoding="utf-8")
                with self.assertRaisesRegex(contract.ManifestError, "must always judge"):
                    contract.validate_workflow_contract(root, lanes, True)

    def test_verified_skip_output_rejects_missing_or_partial_proofs(self) -> None:
        temporary, path = manifest_path([active_lane()])
        with temporary:
            root = Path(temporary.name)
            workflow = root / contract.PR_WORKFLOW
            original = yaml.safe_load(workflow.read_text(encoding="utf-8"))
            lanes, _ = contract.load_active_lanes(path, root)
            for output in (None, "", "true", True,
                           contract.VERIFIED_RUST_SKIP.replace("steps.comment_only.outcome == 'success' && ", ""),
                           contract.VERIFIED_RUST_SKIP.replace("steps.comment_only.outputs.comment_only == 'true' && ", ""),
                           contract.VERIFIED_RUST_SKIP.replace("steps.comment_only.outputs.rust_tests_skip == 'true' && ", "")):
                with self.subTest(output=output):
                    payload = copy.deepcopy(original)
                    payload["jobs"]["changes"]["outputs"]["rust_tests_skip"] = output
                    workflow.write_text(yaml.safe_dump(payload), encoding="utf-8")
                    with self.assertRaisesRegex(contract.ManifestError, "Rust skip output must fail closed"):
                        contract.validate_workflow_contract(root, lanes, False)

    def test_build_job_must_run_one_fail_closed_floor_check(self) -> None:
        temporary, path = manifest_path([active_lane()])
        with temporary:
            root = Path(temporary.name)
            workflow = root / contract.PR_WORKFLOW
            original = yaml.safe_load(workflow.read_text(encoding="utf-8"))
            for name, change in (
                ("missing", lambda steps: steps.clear()),
                ("conditional", lambda steps: steps[0].update({"if": "false"})),
                ("masked", lambda steps: steps[0].update({"continue-on-error": True})),
                ("other list", lambda steps: steps[0].update({"run": steps[0]["run"].replace("universe.json", "other.json")})),
                ("duplicated", lambda steps: steps.append(dict(steps[0]))),
            ):
                with self.subTest(mutation=name):
                    payload = copy.deepcopy(original)
                    change(payload["jobs"][contract.BUILD_JOB]["steps"])
                    workflow.write_text(yaml.safe_dump(payload), encoding="utf-8")
                    with self.assertRaisesRegex(contract.ManifestError, "relay floor check on its nextest list"):
                        contract.load_active_lanes(path, root)

    def test_mutation_matrix_and_shard_environment_cannot_drop_rows(self) -> None:
        lanes, _ = contract.load_active_lanes(REPO_ROOT / contract.DEFAULT_MANIFEST)
        original = yaml.safe_load((REPO_ROOT / contract.PR_WORKFLOW).read_text(encoding="utf-8"))
        mutations = [
            ("strategy", {"fail-fast": False, "matrix": {"shard": [0, 1]}}),
            ("strategy", {"fail-fast": False, "matrix": {"shard": [0, 1, 1]}}),
            ("strategy", {"fail-fast": True, "matrix": {"shard": [0, 1, 2]}}),
            ("env", {"RELAY_AUTHORITY_MUTATION_SHARD_INDEX": "0", "RELAY_AUTHORITY_MUTATION_SHARD_TOTAL": "3"}),
            ("env", {"RELAY_AUTHORITY_MUTATION_SHARD_INDEX": "${{ matrix.shard }}", "RELAY_AUTHORITY_MUTATION_SHARD_TOTAL": "4"}),
        ]
        for key, value in mutations:
            with self.subTest(key=key, value=value), tempfile.TemporaryDirectory() as temporary:
                payload = copy.deepcopy(original)
                payload["jobs"][contract.RELAY_AUTHORITY_MUTATIONS_JOB][key] = value
                root = Path(temporary)
                workflow = root / contract.PR_WORKFLOW
                workflow.parent.mkdir(parents=True)
                workflow.write_text(yaml.safe_dump(payload), encoding="utf-8")
                with self.assertRaisesRegex(contract.ManifestError, "must execute all three|must pass its shard"):
                    contract.validate_workflow_contract(root, lanes, True)

    def test_checked_in_manifest_declares_active_and_gap_rows(self) -> None:
        lanes, gaps = contract.load_active_lanes(
            REPO_ROOT / "scripts" / "relay_authority_contract_targets.json",
            REPO_ROOT,
        )
        self.assertEqual([lane.name for lane in lanes], [
            "t1-sink-terminal-handoff",
            "t4-single-actor-recovery-decision",
            "t5-s4-missing-row-cohort-lifecycle",
            "t5-s4-same-authority-watcher-epoch",
            "t5-s7a-entry-outcome-matrix",
            "t5-s7a-no-anchor-no-visible-mutation",
            "t5-s7a-detached-rowless-state-preserved",
            "t5-c1-rowless-terminal-ledger-and-lease",
            "t5-native-recovered-preview-terminal",
            "relay-e2e-local-model-queue-wake",
            "relay-e2e-scenario-census",
        ])
        self.assertEqual({gap["boundary"] for gap in gaps}, {"T2", "T3", "T5"})
        self.assertTrue(all(lane.minimum > 0 for lane in lanes))

    def test_omitting_any_checked_in_witness_from_the_run_is_rejected(self) -> None:
        lanes, gaps = contract.load_active_lanes(
            REPO_ROOT / "scripts" / "relay_authority_contract_targets.json",
            REPO_ROOT,
        )
        selected = [lane for lane in lanes if lane.name.startswith(("t5-s4-", "t5-s7a-", "t5-c1-", "t5-native-"))]
        self.assertEqual(len(selected), 7)
        self.assertEqual([lane.minimum for lane in selected], [1, 1, 1, 1, 1, 15, 1])
        self.assertIn("t5-structural-signal-authority-teardown", {gap["name"] for gap in gaps})
        # One listed test per declared floor unit, so dropping a lane's tests drops it below its floor.
        names = [f"{lane.command[-1]}::case{index}" for lane in lanes for index in range(lane.minimum)]
        self.assertEqual([contract.failures_for(contract.select_lane(lane, names)) for lane in lanes], [[]] * len(lanes))
        for lane in selected:
            with self.subTest(lane=lane.name):
                kept = [name for name in names if not name.startswith(lane.command[-1])]
                self.assertTrue(contract.failures_for(contract.select_lane(lane, kept)))

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
            payload = yaml.safe_load(workflow.read_text(encoding="utf-8"))
            payload["jobs"][contract.RELAY_AUTHORITY_MUTATIONS_JOB]["steps"].append({
                "name": "Run condition-3 mutations",
                "if": "${{ false }}",
                "run": contract.CONDITION3_MUTATION_COMMAND,
            })
            workflow.write_text(
                yaml.safe_dump(payload),
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


RELAY_C1_LANE = "t5-c1-rowless-terminal-ledger-and-lease"


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    assert text.count(old) == 1, f"{old!r} occurs {text.count(old)} times in {path}"
    path.write_text(text.replace(old, new), encoding="utf-8")


def entry(payload: dict, path: str) -> dict:
    return next(item for item in payload["authority_surface"] if item["path"] == path)


def add_undeclared_row(payload: dict, root: Path) -> None:
    payload["condition3_mutations"].append({
        "name": "new-row", "file": "src/services/discord/inflight.rs",
        "target": "services::discord::inflight::tests::x", "judge": "src/services/discord/inflight.rs",
    })


def add_script_only_file(payload: dict, root: Path) -> None:
    script = root / contract.CONDITION3_MUTATION_SCRIPT
    replace_once(script, '  "$DESTRUCTIVE_CANCEL_GATE"\n', '  "$DESTRUCTIVE_CANCEL_GATE"\n  "$NEW_AUTHORITY"\n')
    replace_once(script, "readonly -a MUTATION_FILES=(", 'readonly NEW_AUTHORITY="src/new_authority.rs"\nreadonly -a MUTATION_FILES=(')


def add_undeclared_lane(payload: dict, root: Path) -> None:
    payload["lanes"].append(active_lane(
        command=["cargo", "test", "--lib", "services::discord::inflight"], minimum=1,
    ) | {"name": "new-lane"})


def drop_entry(path: str):
    return lambda payload, root: payload["authority_surface"].remove(entry(payload, path))


def set_field(path: str, key: str, value: object):
    return lambda payload, root: entry(payload, path).__setitem__(key, value)


def move_row_guard(payload: dict, root: Path) -> None:
    entry(payload, "src/services/discord/destructive_cancel_gate.rs")["guards"] = ["known_gap:#1"]
    entry(payload, "src/services/discord/relay_recovery.rs")["guards"].append("mutation_row:S4-m6")


def drop_filter_judge(payload: dict, root: Path) -> None:
    replace_once(root / contract.PR_WORKFLOW, "              - 'src/services/discord/relay_recovery/tests.rs'\n", "")


class AuthoritySurfaceContract(unittest.TestCase):
    """Drive `--check-manifest` over a copy of the real manifest, script, workflow and sources,
    so every declaration is graded against the files the gate actually reads."""

    def check(self, mutate=None) -> tuple[int, str]:
        payload = json.loads((REPO_ROOT / contract.DEFAULT_MANIFEST).read_text(encoding="utf-8"))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            copied = {contract.DEFAULT_MANIFEST.as_posix(), contract.CONDITION3_MUTATION_SCRIPT.as_posix(),
                      contract.PR_WORKFLOW.as_posix()}
            copied |= {item["path"] for item in payload["authority_surface"]}
            copied |= {row["judge"] for row in payload["condition3_mutations"]}
            for relative in copied:
                (root / relative).parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(REPO_ROOT / relative, root / relative)
            if mutate is not None:
                mutate(payload, root)
            (root / contract.DEFAULT_MANIFEST).write_text(json.dumps(payload), encoding="utf-8")
            stdout, stderr = io.StringIO(), io.StringIO()
            # The copy holds only the declared files, so entry tests resolve against this tree's sources.
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr), \
                    mock.patch.object(contract, "lib_test_ids", return_value=self.real_test_ids()):
                rc = contract.main(["--repo-root", str(root), "--check-manifest"])
        return rc, stdout.getvalue() + stderr.getvalue()

    @staticmethod
    @functools.cache
    def real_test_ids() -> frozenset[str]:
        return frozenset(contract.lib_test_ids(REPO_ROOT))

    def test_entry_tests_resolve_only_to_lib_tests_the_linux_build_compiles(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text('[package]\nname = "fixture"\n', encoding="utf-8")
            (root / "src/lib.rs").write_text(
                "#[cfg(test)]\nmod tests {\n#[test] fn linux_case() {}\n"
                "#[cfg(windows)]\n#[test] fn windows_case() {}\n}\n// #[test] fn commented() {}\n",
                encoding="utf-8")
            self.assertEqual(contract.lib_test_ids(root), {"tests::linux_case"})

    def test_checked_in_surface_passes_the_declaration_check(self) -> None:
        rc, output = self.check()
        self.assertEqual(rc, 0, output)
        self.assertRegex(output, r"surface=[1-9][0-9]* ")

    def test_an_undeclared_or_unresolvable_authority_declaration_fails(self) -> None:
        rowless = "src/services/discord/tmux_watcher/rowless_delivery_authority.rs"
        cases = (
            ("new mutation row not declared", add_undeclared_row, "declare every condition3 mutation row"),
            ("new file in the script array only", add_script_only_file, "MUTATION_FILES must equal"),
            ("new active lane not declared", add_undeclared_lane, "does not declare active lanes ['new-lane']"),
            ("mutation row declaration deleted", drop_entry("src/services/discord/tmux_watcher_registry/fences.rs"),
             "declare every condition3 mutation row"),
            ("named target declaration deleted", drop_entry("src/services/discord/relay_recovery.rs"),
             "does not declare active lanes ['t4-single-actor-recovery-decision']"),
            ("surface section deleted", lambda payload, root: payload.pop("authority_surface"),
             "non-empty authority_surface"),
            ("declared path moved", set_field(rowless, "path", rowless.replace(".rs", "_moved.rs")),
             "is not a regular file"),
            ("entry test renamed", set_field("src/services/discord/catch_up.rs", "guards",
                                             ["entry_test:services::discord::catch_up::no_such_test"]),
             "is not a linux lib test"),
            ("known gap without an issue", set_field("src/server/routes/health_api.rs", "guards", ["known_gap:5996"]),
             "must cite an issue"),
            ("unknown guard kind", set_field(rowless, "guards", ["reviewed:yes"]), "unknown guard"),
            ("mutation row moved to a file it does not mutate", move_row_guard,
             "mutation row S4-m6 does not mutate it"),
            ("row without a judge", lambda payload, root: payload["condition3_mutations"][0].pop("judge"),
             "must name an existing judge file"),
            ("path filter drops a judge", drop_filter_judge, "mutation path filter must select"),
        )
        for name, mutate, message in cases:
            with self.subTest(case=name):
                rc, output = self.check(mutate)
                self.assertEqual(rc, 2, output)
                self.assertIn(message, output)

    def test_checked_in_surface_states_why_rowless_delivery_is_not_a_mutation_row(self) -> None:
        """The rowless soft-terminal authority sits outside MUTATION_FILES; its files must stay
        declared under the named target that guards them, with the reason recorded."""
        payload = json.loads((REPO_ROOT / contract.DEFAULT_MANIFEST).read_text(encoding="utf-8"))
        for path in (
            "src/services/discord/tmux_watcher/rowless_delivery_authority.rs",
            "src/services/discord/tmux_watcher/terminal_relay_plan.rs",
            "src/services/discord/tmux_watcher/turn_identity/soft_terminal_authority.rs",
        ):
            with self.subTest(path=path):
                self.assertIn(f"named_target:{RELAY_C1_LANE}", entry(payload, path)["guards"])
        self.assertTrue(entry(payload, "src/services/discord/tmux_watcher/rowless_delivery_authority.rs")["note"])


class SelectionContract(unittest.TestCase):
    def test_selection_counts_runnable_names_containing_any_filter(self) -> None:
        lane = contract.Lane("lane", "T1", "fixture", ("cargo", "test", "--lib", "fixture::a", "fixture::b"), 1)
        names = ["fixture::a::one", "x::fixture::b::two", "fixture::c::three"]
        self.assertEqual(contract.select_lane(lane, names).selected, 2)

    def test_zero_selection_is_fatal(self) -> None:
        lane = contract.Lane("zero", "T1", "fixture", ("cargo", "test", "--lib", "missing"), 1)
        self.assertEqual(contract.failures_for(contract.select_lane(lane, ["fixture::one"])), [
            "selected 0 tests",
            "selected 0 below declared minimum 1",
        ])

    def test_selection_below_floor_is_fatal(self) -> None:
        lane = contract.Lane("floor", "T4", "fixture", ("cargo", "test", "--lib", "fixture"), 3)
        self.assertEqual(contract.failures_for(contract.select_lane(lane, ["fixture::one", "fixture::two"])), [
            "selected 2 below declared minimum 3"
        ])


class FloorGateEndToEnd(unittest.TestCase):
    """Drives `main` over a written nextest list, so a refactor that stops feeding the list into the floor is caught."""

    def _run(self, *, listed: int, minimum: int, ignored: int = 0, nextest_list: bool = True) -> tuple[int, str, str]:
        temporary, manifest = manifest_path([active_lane(minimum=minimum)])
        with temporary:
            repo_root = Path(temporary.name)
            names = [f"fixture::module::t{index}" for index in range(listed)] + ["other::module::t"]
            listing = write_nextest_list(repo_root / "universe.json", names, tuple(names[:ignored]))
            stdout, stderr = io.StringIO(), io.StringIO()
            argv = ["--repo-root", str(repo_root), "--manifest", str(manifest)]
            argv += ["--nextest-list", str(listing)] if nextest_list else []
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                rc = contract.main(argv)
        return rc, stdout.getvalue(), stderr.getvalue()

    def test_listing_below_the_declared_floor_makes_the_gate_red(self) -> None:
        rc, stdout, stderr = self._run(listed=2, minimum=3)
        self.assertEqual(rc, 1, stdout + stderr)
        self.assertIn("selected=2 minimum=3", stdout)
        self.assertIn("selected 2 below declared minimum 3", stderr)

    def test_listing_that_meets_the_declared_floor_stays_green(self) -> None:
        rc, stdout, stderr = self._run(listed=3, minimum=3)
        self.assertEqual(rc, 0, stdout + stderr)
        self.assertIn("selected=3 minimum=3", stdout)
        self.assertEqual(stderr, "")

    def test_ignored_tests_do_not_count_toward_the_floor(self) -> None:
        rc, stdout, stderr = self._run(listed=3, minimum=3, ignored=1)
        self.assertEqual(rc, 1, stdout + stderr)
        self.assertIn("selected 2 below declared minimum 3", stderr)

    def test_an_empty_selection_is_red(self) -> None:
        rc, stdout, stderr = self._run(listed=0, minimum=1)
        self.assertEqual(rc, 1, stdout + stderr)
        self.assertIn("selected 0 tests", stderr)

    def test_a_missing_list_is_a_usage_error_not_a_pass(self) -> None:
        rc, stdout, stderr = self._run(listed=3, minimum=1, nextest_list=False)
        self.assertEqual(rc, 2, stdout + stderr)
        self.assertIn("--nextest-list is required", stderr)


if __name__ == "__main__":
    unittest.main()
