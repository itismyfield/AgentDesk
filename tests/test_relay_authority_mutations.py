from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import yaml


REPO_ROOT = Path(__file__).resolve().parents[1]
MUTATION_SCRIPT = Path("scripts/run_relay_authority_mutations.sh")
CONTRACT_MANIFEST = Path("scripts/relay_authority_contract_targets.json")
TERMINAL_HANDOFF = Path("src/services/discord/session_relay_sink/terminal_handoff.rs")
SESSION_RELAY_SINK = Path("src/services/discord/session_relay_sink.rs")
# #5457 moved the S4 fence layer into this child module, and both S4 fence rows
# anchor on text that went with it, so the registry root is no longer mutated.
WATCHER_FENCES = Path("src/services/discord/tmux_watcher_registry/fences.rs")
DESTRUCTIVE_CANCEL_GATE = Path("src/services/discord/destructive_cancel_gate.rs")
# Every file the script mutates; it backs up and hash-verifies all of them on
# every row, so the fixture tree has to carry the whole set.
MUTATION_FILES = (
    TERMINAL_HANDOFF,
    SESSION_RELAY_SINK,
    WATCHER_FENCES,
    DESTRUCTIVE_CANCEL_GATE,
)
# #5071 relay-tail S4 raised this from four and its r2 repair added S4-m7; the
# declared condition-3 floor stays four.
MUTATION_COUNT = 7
MUTATION_NAMES = ("M10", "M6", "M8", "anchor-drop", "S4-m5", "S4-m6", "S4-m7")
PR_WORKFLOW = Path(".github/workflows/ci-pr.yml")
MUTATION_JOB = "relay-authority-contract"
MUTATION_STEP = "Require relay-authority mutations to be killed"
FILTER_ID = "mutation_paths"
FILTER_NAME = "mutation_sources"
STEP_CONDITION = f"steps.{FILTER_ID}.outputs.{FILTER_NAME} != 'false'"
# The file that DEFINES each row's judging test. Six of the seven rows are
# judged from a file they do not mutate, so #5997's CI filter has to select
# these as well: a diff that only weakens a judge must still run the gate.
JUDGE_FILES = {
    "M10": "src/services/discord/session_relay_sink/delivery_orchestration_tests.rs",
    "M6": "src/services/discord/session_relay_sink/delivery_orchestration_tests.rs",
    "M8": "src/services/discord/session_relay_sink/delivery_orchestration_tests.rs",
    "anchor-drop": "src/services/discord/session_relay_sink/delivery_orchestration_tests.rs",
    "S4-m5": "src/services/discord/relay_recovery/tests.rs",
    "S4-m6": "src/services/discord/destructive_cancel_gate.rs",
    "S4-m7": "src/services/discord/tmux_watcher_registry_restore_tests.rs",
}
# The gate's own wiring: editing either can change what the step proves.
WIRING_FILES = ("scripts/run_relay_authority_mutations.sh", ".github/workflows/ci-pr.yml")
# A judge's fixtures reach it through `use super::*` (its own parent module) or
# `use super::<mod>::` (a sibling module); either can empty a judgment while
# JUDGE_FILES and MUTATION_FILES both stay untouched, so the filter has to
# select them too. `use super::{Item, ...}` is deliberately NOT followed: its
# names mix re-exported items with sibling modules -- `inflight` in
# destructive_cancel_gate.rs is src/services/discord/inflight.rs, 6046 lines --
# and selecting those returns the job to the unconditional cost this filter
# exists to remove.
SUPER_IMPORT = re.compile(r"^use super::(\*|[a-z_][a-z0-9_]*::)", re.M)


def _module_file(module: Path) -> str | None:
    """`foo/bar.rs` and `foo/bar/mod.rs` are two spellings of one module."""
    for candidate in (module.with_suffix(".rs"), module / "mod.rs"):
        if (REPO_ROOT / candidate).is_file():
            return candidate.as_posix()
    return None


def judge_fixture_owners() -> frozenset[str]:
    """Every module a judging test pulls fixtures from in a file-top import."""
    owners: set[str] = set()
    for judge in sorted(set(JUDGE_FILES.values())):
        parent = Path(judge).parent
        source = (REPO_ROOT / judge).read_text(encoding="utf-8")
        for token in SUPER_IMPORT.findall(source):
            segment = token.rstrip(":")
            if segment == "super":  # `use super::super::` leaves the subtree.
                continue
            owner = _module_file(parent if segment == "*" else parent / segment)
            if owner is not None:
                owners.add(owner)
    return frozenset(owners)


def script_mutation_files(script: str) -> tuple[str, ...]:
    """The `MUTATION_FILES` array as the shell script really declares it.

    Parsed rather than restated: comparing the workflow filter against a second
    hand-written copy of the list would pass while the script itself drifted.
    """
    constants = dict(re.findall(r'^readonly ([A-Z0-9_]+)="([^"]+)"$', script, re.M))
    body = re.search(
        r"^readonly -a MUTATION_FILES=\(\n(.*?)^\)$", script, re.M | re.S
    )
    assert body, "MUTATION_FILES array literal not found in the script"
    return tuple(constants[name] for name in re.findall(r'"\$([A-Z0-9_]+)"', body.group(1)))


def mutation_filter_patterns() -> tuple[str, ...]:
    """The `mutation_sources` pattern list from the job's own filter step."""
    job = yaml.safe_load((REPO_ROOT / PR_WORKFLOW).read_text(encoding="utf-8"))["jobs"][
        MUTATION_JOB
    ]
    step = next(
        candidate
        for candidate in job["steps"]
        if str(candidate.get("uses", "")).startswith("dorny/paths-filter")
    )
    assert step["id"] == FILTER_ID, step["id"]
    return tuple(yaml.safe_load(step["with"]["filters"])[FILTER_NAME])


def setUpModule() -> None:
    subprocess.run(
        [sys.executable, str(REPO_ROOT / "scripts/check_relay_mutation_sources.py")],
        check=True,
    )


def _result_log(outcome: str = "failed") -> str:
    return 'test ! -s "${!#}" || exit 42\n' + f"printf '%s %s\\n' {outcome} \"$4\" > \"${{!#}}\"\n"


def _cargo_log(body: str) -> str:
    """A fixture-runner body that replays a realistic `cargo test --lib` log.

    #5243: the oracle grades the log of the single cargo invocation, so a fixture
    runner that prints nothing is no longer a stand-in for a killed mutant.
    """
    return "cat <<'RELAY_AUTHORITY_LOG'\n" + body + "RELAY_AUTHORITY_LOG\n"


COMPILED_HEADER = """   Compiling agentdesk v0.1.2 (/repo)
    Finished test profile [unoptimized + debuginfo] target(s) in 12.34s
     Running unittests src/lib.rs (target/debug/deps/agentdesk-0123456789abcdef)

"""

# The test named for the mutation ran and failed: a real kill.
KILLED_RUNNER = _cargo_log(
    COMPILED_HEADER
    + """running 1 test
test the_named_target ... FAILED

failures:
    the_named_target

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 130 filtered out; finished in 0.02s
"""
) + "exit 101\n"

# The test ran and passed: the mutation survived.
SURVIVED_RUNNER = _cargo_log(
    COMPILED_HEADER
    + """running 1 test
test the_named_target ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 130 filtered out; finished in 0.02s
"""
) + "exit 0\n"

# rustc rejected the mutant: cargo still answers 101, and the build leaves no
# fingerprint, so the cache proof still sees compiling=1 fresh=0.
BUILD_BROKEN_RUNNER = _cargo_log(
    """   Compiling agentdesk v0.1.2 (/repo)
error[E0425]: cannot find value `terminal_not_delivered` in this scope
   --> src/services/discord/session_relay_sink/terminal_handoff.rs:111:31

error: aborting due to 1 previous error

error: could not compile `agentdesk` (lib test) due to 1 previous error
"""
) + "exit 101\n"

# The named test no longer exists: the filter matches nothing and cargo says rc=0.
NO_TEST_RAN_RUNNER = _cargo_log(
    COMPILED_HEADER
    + """running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 131 filtered out; finished in 0.00s
"""
) + "exit 0\n"


class RelayAuthorityMutationScriptTests(unittest.TestCase):
    maxDiff = None

    def copy_fixture(self) -> Path:
        temp = Path(tempfile.mkdtemp(prefix="relay-authority-mutations-"))
        self.addCleanup(shutil.rmtree, temp, True)
        for relative in (MUTATION_SCRIPT, *MUTATION_FILES):
            destination = temp / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(REPO_ROOT / relative, destination, follow_symlinks=False)
        return temp

    @staticmethod
    def run_script(root: Path, runner: Path) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.update(
            RELAY_AUTHORITY_MUTATION_TEST_MODE="fixture",
            RELAY_AUTHORITY_MUTATION_FIXTURE_RUNNER=str(runner),
        )
        return subprocess.run(
            ["bash", str(root / MUTATION_SCRIPT)],
            cwd=root,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    @staticmethod
    def write_runner(root: Path, body: str) -> Path:
        runner = root / "fixture-runner.sh"
        runner.write_text("#!/usr/bin/env bash\nset -euo pipefail\n" + body, encoding="utf-8")
        runner.chmod(0o755)
        return runner

    @staticmethod
    def write_fake_cargo(root: Path, *, cached: bool = False) -> Path:
        bin_dir = root / "fake-bin"
        bin_dir.mkdir()
        cargo = bin_dir / "cargo"
        marker = "Fresh" if cached else "Compiling"
        cargo.write_text(
            f"""#!/usr/bin/env bash
set -euo pipefail
{_result_log()}if [[ "${{CARGO_TERM_COLOR-}}" == "never" ]]; then
    printf '   {marker} agentdesk v0.1.0 (fake)\\n'
else
    printf '\\033[1m\\033[92m   {marker}\\033[0m agentdesk v0.1.0 (fake)\\n'
fi
printf '     Running unittests src/lib.rs (target/debug/deps/agentdesk-0123456789ab)\\n'
printf 'running 1 test\\n'
printf 'test the_named_target ... FAILED\\n'
printf 'test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1 filtered out\\n'
exit 101
""",
            encoding="utf-8",
        )
        cargo.chmod(0o755)
        return cargo

    @staticmethod
    def run_script_with_fake_cargo(
        root: Path,
        cargo: Path,
        extra_env: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.pop("RELAY_AUTHORITY_MUTATION_TEST_MODE", None)
        env.pop("RELAY_AUTHORITY_MUTATION_FIXTURE_RUNNER", None)
        env["CARGO_TERM_COLOR"] = "always"
        env["PATH"] = str(cargo.parent) + os.pathsep + env.get("PATH", "")
        env.update(extra_env or {})
        return subprocess.run(
            ["bash", str(root / MUTATION_SCRIPT)],
            cwd=root,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    @staticmethod
    def write_recording_cargo(root: Path, record: Path) -> Path:
        bin_dir = root / "fake-bin"
        bin_dir.mkdir()
        cargo = bin_dir / "cargo"
        cargo.write_text(
            f"""#!/usr/bin/env bash
set -euo pipefail
{_result_log()}{{
  printf 'ARGV %s\\n' "$*"
  printf 'CARGO_INCREMENTAL=%s\\n' "${{CARGO_INCREMENTAL-<unset>}}"
  printf 'RUSTC_WRAPPER=%s\\n' "${{RUSTC_WRAPPER-<unset>}}"
}} >>"{record}"
printf '   Compiling agentdesk v0.1.0 (fake)\\n'
printf '     Running unittests src/lib.rs (target/debug/deps/agentdesk-0123456789ab)\\n'
printf 'running 1 test\\n'
printf 'test the_named_target ... FAILED\\n'
printf 'test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1 filtered out\\n'
exit 101
""",
            encoding="utf-8",
        )
        cargo.chmod(0o755)
        return cargo

    @staticmethod
    def assert_sources_restored(test: unittest.TestCase, root: Path) -> None:
        for relative in MUTATION_FILES:
            test.assertEqual((root / relative).read_bytes(), (REPO_ROOT / relative).read_bytes())

    def run_cargo_body(self, body: str, outcome: str = "failed") -> tuple[Path, subprocess.CompletedProcess[str]]:
        root = self.copy_fixture()
        cargo = root / "fake-bin/cargo"
        cargo.parent.mkdir()
        cargo.write_text("#!/usr/bin/env bash\nset -euo pipefail\n" + _result_log(outcome) + body)
        cargo.chmod(0o755)
        result = self.run_script_with_fake_cargo(root, cargo)
        self.assert_sources_restored(self, root)
        return root, result

    def test_default_cargo_entrypoint_grades_valid_and_invalid_runs(self) -> None:
        for body, rc, status in (
            (KILLED_RUNNER, 0, "KILLED"),
            (SURVIVED_RUNNER, 1, "SURVIVED"),
            (BUILD_BROKEN_RUNNER, 95, "BUILD-BROKEN"),
            (NO_TEST_RAN_RUNNER, 94, "NO-TEST-RAN"),
        ):
            with self.subTest(status=status):
                _, result = self.run_cargo_body(body, "ok" if status == "SURVIVED" else "failed")
                output = result.stdout + result.stderr
                self.assertEqual(result.returncode, rc, output)
                self.assertIn(f"status={status}", output)
                self.assertEqual("MUTATION_SUMMARY" in output, rc == 0, output)

    def test_default_cargo_entrypoint_rejects_contradictory_verdicts(self) -> None:
        for body in (SURVIVED_RUNNER.replace("exit 0", "exit 101"),
                     KILLED_RUNNER.replace("exit 101", "exit 0")):
            with self.subTest(body=body):
                _, result = self.run_cargo_body(body)
                output = result.stdout + result.stderr
                self.assertEqual(result.returncode, 93, output)
                self.assertIn("status=NO-VERDICT", output)
                self.assertNotIn("MUTATION_RESULT", output)
                self.assertNotIn("MUTATION_SUMMARY", output)

    def test_nested_child_failure_does_not_decide_the_parent_verdict(self) -> None:
        child = "{\n" + KILLED_RUNNER.replace(COMPILED_HEADER, "").replace("exit 101\n", "") + "} >&2\n"
        for parent, expected in ((KILLED_RUNNER, 0), (SURVIVED_RUNNER, 1)):
            with self.subTest(parent_rc=expected):
                _, result = self.run_cargo_body(child + parent, "ok" if expected == 1 else "failed")
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)

    def test_nested_child_output_cannot_fill_missing_parent_evidence(self) -> None:
        child = "{\n" + KILLED_RUNNER.replace(COMPILED_HEADER, "").replace("exit 101\n", "") + "} >&2\n"
        for parent in (
            _cargo_log(COMPILED_HEADER + "running 1 test\n") + "exit 101\n",
            ': > "${!#}"\n' + KILLED_RUNNER,
            'printf "failed wrong_target\\n" > "${!#}"\n' + KILLED_RUNNER,
            'printf "failed %s\\n" "$4" >> "${!#}"\n' + KILLED_RUNNER,
            'printf "ok %s\\n" "$4" > "${!#}"\n' + KILLED_RUNNER,
            SURVIVED_RUNNER.replace("exit 0", "exit 101"),
            KILLED_RUNNER.replace("exit 101", "exit 0"),
            child.replace("} >&2", "}") + KILLED_RUNNER,
        ):
            with self.subTest(parent=parent):
                _, result = self.run_cargo_body(child + parent)
                output = result.stdout + result.stderr
                self.assertEqual(result.returncode, 93, output)
                self.assertIn("status=NO-VERDICT", output)
                self.assertNotIn("MUTATION_RESULT", output)
                self.assertNotIn("MUTATION_SUMMARY", output)

    def test_default_cargo_entrypoint_rejects_incomplete_runs(self) -> None:
        for body in (
            KILLED_RUNNER.replace("running 1 test", "running 2 tests"),
            KILLED_RUNNER.replace("running 1 test\n", ""),
            KILLED_RUNNER.replace("0 ignored", "1 ignored"),
            KILLED_RUNNER.replace("0 measured", "1 measured"),
            KILLED_RUNNER.replace("running 1 test", "running 1 test\nrunning 1 test"),
            KILLED_RUNNER.replace("exit 101", "printf 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 130 filtered out\\n'\nexit 101"),
            _cargo_log(COMPILED_HEADER + "running 1 test\n") + "exit 101\n",
        ):
            with self.subTest(body=body):
                _, result = self.run_cargo_body(body)
                output = result.stdout + result.stderr
                self.assertEqual(result.returncode, 93, output)
                self.assertIn("status=NO-VERDICT", output)
                self.assertNotIn("MUTATION_RESULT", output)
                self.assertNotIn("MUTATION_SUMMARY", output)

    def test_source_provenance_allows_local_edits_but_rejects_ci_drift(self) -> None:
        root = self.copy_fixture()
        test_path = Path("tests/test_relay_authority_mutations.py")
        for relative in (test_path, Path("scripts/check_relay_mutation_sources.py"),
                         Path("scripts/ci-script-checks.sh")):
            (root / relative).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(REPO_ROOT / relative, root / relative)
        for args in (("init", "-q"), ("add", "."),
                     ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                      "-c", "core.hooksPath=/dev/null", "commit", "-qm", "fixture")):
            subprocess.run(["git", *args], cwd=root, check=True, capture_output=True)

        python_shim = root / "python-shim"
        python_shim.write_text(
            f"#!{sys.executable}\nimport os, sys\n"
            "if sys.argv[1:] == ['scripts/check_relay_mutation_sources.py']:\n"
            "    os.execv(sys.executable, [sys.executable, *sys.argv[1:]])\n"
            "assert sys.argv[1:] == ['-m', 'unittest', 'tests.test_relay_authority_mutations']\n"
            f"os.execv(sys.executable, [sys.executable, {str(root / test_path)!r}, "
            "'RelayAuthorityMutationScriptTests.test_script_mode_is_executable'])\n"
        )
        python_shim.chmod(0o755)

        def check(verify: bool, expected_rc: int) -> subprocess.CompletedProcess[str]:
            script = (root / "scripts/ci-script-checks.sh").read_text()
            section = script.split('banner "Relay-authority fixed mutation gate (#5071)"\n', 1)[1].split('\nbanner ', 1)[0]
            env = {**os.environ, "PYTHON": str(python_shim), "GITHUB_ACTIONS": str(verify).lower()}
            result = subprocess.run(
                ["bash", "-euc", section], cwd=root, env=env, text=True, capture_output=True,
            )
            self.assertEqual(result.returncode, expected_rc, result.stdout + result.stderr)
            self.assertIn("MUTATION_SOURCE", result.stdout)
            return result

        check(True, 0)
        for relative in (MUTATION_SCRIPT, test_path, Path("scripts/check_relay_mutation_sources.py"),
                         Path("scripts/ci-script-checks.sh")):
            with self.subTest(path=relative):
                original = (root / relative).read_bytes()
                try:
                    (root / relative).write_bytes(original + b"\n# local development edit\n")
                    local = check(False, 0)
                    digest = hashlib.sha256((root / relative).read_bytes()).hexdigest()
                    self.assertIn(f"path={relative} sha256={digest}", local.stdout)
                    check(True, 1)
                    subprocess.run(["git", "add", str(relative)], cwd=root, check=True)
                    check(True, 1)
                finally:
                    (root / relative).write_bytes(original)
                    subprocess.run(["git", "add", str(relative)], cwd=root, check=True)
        check(True, 0)
        (root / test_path).write_text(
            "import unittest\n"
            "class RelayAuthorityMutationScriptTests(unittest.TestCase):\n"
            "    def test_script_mode_is_executable(self): pass\n"
            "unittest.main()\n"
        )
        check(True, 1)

    def test_color_neutralization_keeps_cache_proof_color_proof(self) -> None:
        root = self.copy_fixture()
        cargo = self.write_fake_cargo(root)

        result = self.run_script_with_fake_cargo(root, cargo)

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            result.stdout.count("compiling_agentdesk=1"), MUTATION_COUNT, result.stdout
        )
        self.assertNotIn("cache-proof=invalid", result.stderr)
        # #5243 case E control: a freshly built, genuinely killed mutant must not
        # be misgraded by the new build/test gates.
        self.assertNotIn("status=BUILD-BROKEN", result.stderr)
        self.assertNotIn("status=NO-TEST-RAN", result.stderr)
        self.assertEqual(
            result.stdout.count("compile_ok=yes tests_passed=0 tests_failed=1"),
            MUTATION_COUNT,
            result.stdout,
        )

    def test_rows_build_incrementally_and_never_through_the_sccache_wrapper(self) -> None:
        """The row loop pays one crate build per mutation, so the build shape is
        the step's whole cost. Incremental keeps consecutive rows cheap; the
        unset wrapper is what lets incremental run without voiding the cache
        proof, so both are pinned together against a silent revert."""
        root = self.copy_fixture()
        record = root / "cargo-invocations.txt"
        cargo = self.write_recording_cargo(root, record)
        targets = [
            row["target"]
            for row in json.loads(
                (REPO_ROOT / CONTRACT_MANIFEST).read_text(encoding="utf-8")
            )["condition3_mutations"]
        ]

        result = self.run_script_with_fake_cargo(
            root, cargo, extra_env={"RUSTC_WRAPPER": "sccache", "CARGO_INCREMENTAL": "0"}
        )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        lines = record.read_text(encoding="utf-8").splitlines()
        self.assertEqual(lines.count("CARGO_INCREMENTAL=1"), MUTATION_COUNT, lines)
        self.assertEqual(lines.count("RUSTC_WRAPPER=<unset>"), MUTATION_COUNT, lines)
        self.assertEqual(
            [line.rsplit(" --logfile ", 1)[0] for line in lines if line.startswith("ARGV ")],
            [
                f"ARGV test --offline --lib {target} -- --exact --test-threads=1 --no-capture"
                for target in targets
            ],
        )

        result_logs = [Path(line.rsplit(" --logfile ", 1)[1])
                       for line in lines if line.startswith("ARGV ")]
        self.assertEqual(len(set(result_logs)), MUTATION_COUNT)
        self.assertTrue(all(not path.exists() for path in result_logs))
        self.assertTrue(all(not Path(str(path).removesuffix(".results") + ".stdout").exists()
                            for path in result_logs))

    def test_cache_proof_still_trips_on_a_cached_tree(self) -> None:
        root = self.copy_fixture()
        cargo = self.write_fake_cargo(root, cached=True)

        result = self.run_script_with_fake_cargo(root, cargo)

        self.assertEqual(result.returncode, 96, result.stdout + result.stderr)
        self.assertIn("cache-proof=invalid", result.stderr)
        self.assertNotIn("status=BUILD-BROKEN", result.stderr)
        self.assertNotIn("status=NO-TEST-RAN", result.stderr)
        self.assert_sources_restored(self, root)

    def test_killed_mutation_records_the_evidence_that_killed_it(self) -> None:
        root = self.copy_fixture()
        runner = self.write_runner(root, KILLED_RUNNER)

        result = self.run_script(root, runner)

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for mutation in MUTATION_NAMES:
            self.assertIn(
                f"MUTATION_ORACLE mutation={mutation} compile_ok=yes "
                "tests_passed=0 tests_failed=1",
                result.stdout,
            )

    def test_mutant_that_does_not_compile_is_build_broken_not_killed(self) -> None:
        root = self.copy_fixture()
        runner = self.write_runner(
            root,
            'if [[ "$1" == "M8" ]]; then\n' + BUILD_BROKEN_RUNNER + "fi\n" + KILLED_RUNNER,
        )

        result = self.run_script(root, runner)

        self.assertEqual(result.returncode, 95, result.stdout + result.stderr)
        self.assertIn("MUTATION_ORACLE mutation=M8 compile_ok=no", result.stdout)
        self.assertIn("status=BUILD-BROKEN", result.stderr)
        self.assertIn("mutation=M8", result.stderr)
        self.assertNotIn("MUTATION_RESULT mutation=M8 status=KILLED", result.stdout)
        self.assertNotIn("MUTATION_SUMMARY", result.stdout)
        self.assertIn("could not compile `agentdesk`", result.stderr)
        self.assert_sources_restored(self, root)

    def test_lost_test_name_is_no_test_ran_not_a_survived_report(self) -> None:
        root = self.copy_fixture()
        runner = self.write_runner(
            root,
            'if [[ "$1" == "M6" ]]; then\n' + NO_TEST_RAN_RUNNER + "fi\n" + KILLED_RUNNER,
        )

        result = self.run_script(root, runner)

        self.assertEqual(result.returncode, 94, result.stdout + result.stderr)
        self.assertIn("status=NO-TEST-RAN", result.stderr)
        self.assertIn("mutation=M6", result.stderr)
        self.assertNotIn("mutation survived", result.stderr)
        self.assertNotIn("status=SURVIVED", result.stderr)
        self.assertNotIn("MUTATION_SUMMARY", result.stdout)
        self.assert_sources_restored(self, root)

    def test_every_fixed_mutation_is_killed_and_sources_restore(self) -> None:
        root = self.copy_fixture()
        runner = self.write_runner(root, KILLED_RUNNER)

        result = self.run_script(root, runner)

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        count_line = next(
            line for line in result.stdout.splitlines() if line.startswith("MUTATION_COUNT ")
        )
        fields = dict(item.split("=", 1) for item in count_line.split()[1:])
        self.assertEqual(int(fields["count"]), MUTATION_COUNT)
        self.assertEqual(int(fields["minimum"]), 4)
        self.assertEqual(
            result.stdout.count("status=KILLED rc=101"), int(fields["count"]), result.stdout
        )
        self.assertIn(
            f"MUTATION_SUMMARY killed={fields['count']} survived=0 minimum=4 status=PASS",
            result.stdout,
        )
        self.assert_sources_restored(self, root)

    def test_manifest_declares_the_same_mutation_rows_the_script_runs(self) -> None:
        """The manifest's `condition3_mutations` list is documentation until
        something compares it to the script. Compare it to a real run: the names
        come from the run's own `MUTATION_RESULT` markers, and the files and
        named targets must literally appear in the script that produced them."""
        manifest = json.loads((REPO_ROOT / CONTRACT_MANIFEST).read_text(encoding="utf-8"))
        declared = manifest["condition3_mutations"]
        root = self.copy_fixture()
        runner = self.write_runner(root, KILLED_RUNNER)

        result = self.run_script(root, runner)

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        executed = [
            line.split("mutation=", 1)[1].split(maxsplit=1)[0]
            for line in result.stdout.splitlines()
            if line.startswith("MUTATION_RESULT mutation=")
        ]
        self.assertEqual([row["name"] for row in declared], executed)
        self.assertEqual(executed, list(MUTATION_NAMES))
        script = (REPO_ROOT / MUTATION_SCRIPT).read_text(encoding="utf-8")
        for row in declared:
            with self.subTest(mutation=row["name"]):
                self.assertIn(row["file"], script)
                self.assertIn(row["target"], script)
        self.assertEqual(
            {row["file"] for row in declared},
            {relative.as_posix() for relative in MUTATION_FILES},
        )

    def test_concurrent_run_fails_closed_without_modifying_sources(self) -> None:
        root = self.copy_fixture()
        runner = self.write_runner(root, KILLED_RUNNER)
        lock_dir = root / "target/relay-authority-mutations.lock"
        lock_dir.mkdir(parents=True)
        before = {
            relative: (root / relative).read_bytes()
            for relative in (TERMINAL_HANDOFF, SESSION_RELAY_SINK)
        }

        result = self.run_script(root, runner)

        self.assertEqual(result.returncode, 75, result.stdout + result.stderr)
        self.assertIn("another relay-authority mutation run holds lock", result.stderr)
        for relative, expected in before.items():
            self.assertEqual((root / relative).read_bytes(), expected)

    def test_normal_exit_releases_lock_for_subsequent_run(self) -> None:
        root = self.copy_fixture()
        runner = self.write_runner(root, KILLED_RUNNER)
        lock_dir = root / "target/relay-authority-mutations.lock"

        first = self.run_script(root, runner)
        second = self.run_script(root, runner)

        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.assertFalse(lock_dir.exists())
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertFalse(lock_dir.exists())
        self.assert_sources_restored(self, root)

    def test_surviving_mutation_makes_the_gate_red_and_restores_sources(self) -> None:
        root = self.copy_fixture()
        runner = self.write_runner(
            root,
            'if [[ "$1" == "M6" ]]; then\n' + SURVIVED_RUNNER + "fi\n" + KILLED_RUNNER,
        )

        result = self.run_script(root, runner)

        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("MUTATION_RESULT mutation=M6 status=SURVIVED rc=0", result.stderr)
        self.assertIn("ERROR mutation survived: M6", result.stderr)
        # #5243 case D: a test that ran and passed is still SURVIVED, not
        # NO-TEST-RAN — the two rc=0 shapes must stay distinguishable.
        self.assertIn(
            "MUTATION_ORACLE mutation=M6 compile_ok=yes tests_passed=1 tests_failed=0",
            result.stdout,
        )
        self.assertNotIn("status=NO-TEST-RAN", result.stderr)
        self.assert_sources_restored(self, root)

    def test_signal_exit_releases_lock_and_restores_sources(self) -> None:
        root = self.copy_fixture()
        runner = self.write_runner(root, 'kill -TERM "$PPID"\nsleep 1\n' + KILLED_RUNNER)
        lock_dir = root / "target/relay-authority-mutations.lock"

        result = self.run_script(root, runner)

        self.assertEqual(result.returncode, 143, result.stdout + result.stderr)
        self.assertFalse(lock_dir.exists())
        self.assert_sources_restored(self, root)

    def test_missing_mutation_anchor_fails_closed_and_restores_sources(self) -> None:
        root = self.copy_fixture()
        runner = self.write_runner(root, KILLED_RUNNER)
        source = root / TERMINAL_HANDOFF
        source.write_text(
            source.read_text(encoding="utf-8").replace(
                "delivery_frontier::SinkDeliveryProofResult::Persisted => Self::Delivered,",
                "delivery_frontier::SinkDeliveryProofResult::Persisted => Self::NotDelivered,",
                1,
            ),
            encoding="utf-8",
        )
        expected = source.read_bytes()

        result = self.run_script(root, runner)

        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("matches=0", result.stderr)
        self.assertEqual(source.read_bytes(), expected)

    def test_mutation_count_floor_is_at_least_four(self) -> None:
        root = self.copy_fixture()
        runner = self.write_runner(root, KILLED_RUNNER)

        result = self.run_script(root, runner)

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        count_line = next(
            line for line in result.stdout.splitlines() if line.startswith("MUTATION_COUNT ")
        )
        fields = dict(item.split("=", 1) for item in count_line.split()[1:])
        # The floor is what this test names; the exact row count is pinned by
        # `test_every_fixed_mutation_is_killed_and_sources_restore`, so adding a
        # mutation does not have to be edited in twice.
        self.assertEqual(int(fields["minimum"]), 4)
        self.assertGreaterEqual(int(fields["count"]), int(fields["minimum"]))
        self.assertEqual(result.stdout.count("MUTATION_RESULT mutation="), int(fields["count"]))

    @staticmethod
    def validate_script(path: Path) -> int:
        if path.is_symlink() or not path.is_file():
            return 1
        if path.stat().st_size == 0 or not os.access(path, os.X_OK):
            return 1
        commands = [
            line.strip()
            for line in path.read_text(encoding="utf-8").splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        ]
        if not commands or commands in (["exit 0"], ["#!/usr/bin/env bash", "exit 0"]):
            return 1
        return 0

    def test_guard_rejects_degenerate_script_files(self) -> None:
        cases = ("absent", "empty", "non-executable", "symlink", "exit-zero-only")
        for case in cases:
            with self.subTest(case=case):
                root = self.copy_fixture()
                script = root / MUTATION_SCRIPT
                inert = root / "inert.sh"
                inert.write_text("#!/usr/bin/env bash\nexit 0\n", encoding="utf-8")
                inert.chmod(0o755)
                if case == "absent":
                    script.unlink()
                elif case == "empty":
                    script.write_bytes(b"")
                elif case == "non-executable":
                    script.chmod(script.stat().st_mode & ~(stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH))
                elif case == "symlink":
                    script.unlink()
                    script.symlink_to(inert)
                elif case == "exit-zero-only":
                    script.write_text("exit 0\n", encoding="utf-8")
                    script.chmod(0o755)
                self.assertEqual(self.validate_script(script), 1)

    def test_guard_accepts_the_real_script(self) -> None:
        self.assertEqual(self.validate_script(REPO_ROOT / MUTATION_SCRIPT), 0)

    def test_script_mode_is_executable(self) -> None:
        self.assertTrue(os.access(REPO_ROOT / MUTATION_SCRIPT, os.X_OK))


class MutationPathFilterContractTests(unittest.TestCase):
    """#5997: the mutation step is the only path-gated step in an otherwise
    unconditional required job, so its filter must stay exactly as wide as what
    the step grades. A filter that drifts narrow skips the gate silently and CI
    stays green, so the two lists are compared rather than trusted."""

    maxDiff = None

    def setUp(self) -> None:
        self.patterns = mutation_filter_patterns()
        self.declared = script_mutation_files(
            (REPO_ROOT / MUTATION_SCRIPT).read_text(encoding="utf-8")
        )

    def test_the_script_array_is_the_list_this_fixture_copies(self) -> None:
        self.assertEqual(self.declared, tuple(p.as_posix() for p in MUTATION_FILES))

    def test_filter_is_exactly_the_mutated_judging_and_wiring_files(self) -> None:
        self.assertEqual(
            set(self.patterns),
            set(self.declared)
            | set(JUDGE_FILES.values())
            | judge_fixture_owners()
            | set(WIRING_FILES),
        )
        self.assertEqual(len(self.patterns), len(set(self.patterns)))

    def test_the_fixture_owners_are_read_off_the_judges_not_restated(self) -> None:
        """The equality above is only a real comparison while this derivation
        finds something: a regex that matched nothing would make the fixture
        group vanish from both sides at once. These three are the demonstrated
        channels -- `delivery_orchestration_tests.rs` builds the M6/M8/M10
        verdicts from `terminal_frame_offset` in a file it never mutates,
        S4-m5's judge takes its post-gate hook out of `relay_recovery.rs` the
        same way, and S4-m7's judge takes `TerminalDeliveryFence` out of
        `tmux_watcher_registry.rs`."""
        owners = judge_fixture_owners()
        self.assertIn("src/services/discord/session_relay_sink/tests.rs", owners)
        self.assertIn("src/services/discord/relay_recovery.rs", owners)
        self.assertIn("src/services/discord/tmux_watcher_registry.rs", owners)
        self.assertTrue(owners.issubset(set(self.patterns)), sorted(owners))

    def test_every_pattern_is_a_literal_path_that_exists(self) -> None:
        """Set equality above is only a real comparison while every pattern is
        a literal; one glob would make it silently over- or under-match."""
        for pattern in self.patterns:
            with self.subTest(pattern=pattern):
                self.assertNotIn("*", pattern)
                self.assertFalse(pattern.startswith("!"))
                self.assertTrue((REPO_ROOT / pattern).is_file(), pattern)

    def test_each_row_is_judged_from_a_file_the_filter_selects(self) -> None:
        """The mutated file and the file defining the test that grades it are
        different for six of the seven rows; both have to select the lane."""
        manifest = json.loads((REPO_ROOT / CONTRACT_MANIFEST).read_text(encoding="utf-8"))
        for row in manifest["condition3_mutations"]:
            with self.subTest(mutation=row["name"]):
                judge = JUDGE_FILES[row["name"]]
                self.assertIn(row["file"], self.patterns)
                self.assertIn(judge, self.patterns)
                named = row["target"].rsplit("::", 1)[1]
                source = (REPO_ROOT / judge).read_text(encoding="utf-8")
                self.assertRegex(source, rf"\bfn {re.escape(named)}\b")

    def test_only_the_mutation_step_is_gated_on_the_filter(self) -> None:
        job = yaml.safe_load((REPO_ROOT / PR_WORKFLOW).read_text(encoding="utf-8"))["jobs"][
            MUTATION_JOB
        ]
        # `check-ci-runner-hardening.sh` forbids both keys here so the #5321
        # backstop stays independent of the `changes` job; that is why this is a
        # step-level filter and not a job-level one.
        self.assertNotIn("if", job)
        self.assertNotIn("needs", job)
        gated = {
            step["name"]: step["if"]
            for step in job["steps"]
            if "name" in step and FILTER_ID in str(step.get("if", ""))
        }
        self.assertEqual(gated, {MUTATION_STEP: STEP_CONDITION})

    def test_the_condition_runs_the_gate_unless_the_filter_said_unrelated(self) -> None:
        """The negative form is load-bearing: a missing or empty filter output
        has to run the mutation gate, not skip it. Read off the workflow rather
        than off STEP_CONDITION, which this file builds itself: deleting the
        `if:` line outright has to fail here and not only next door."""
        job = yaml.safe_load((REPO_ROOT / PR_WORKFLOW).read_text(encoding="utf-8"))["jobs"][
            MUTATION_JOB
        ]
        step = next(s for s in job["steps"] if s.get("name") == MUTATION_STEP)
        self.assertTrue(str(step["if"]).endswith("!= 'false'"), step.get("if"))

    def test_ci_script_checks_runs_this_contract(self) -> None:
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn("unittest tests.test_relay_authority_mutations", script)


if __name__ == "__main__":
    unittest.main()
