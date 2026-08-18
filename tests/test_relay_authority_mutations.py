from __future__ import annotations

import json
import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
MUTATION_SCRIPT = Path("scripts/run_relay_authority_mutations.sh")
CONTRACT_MANIFEST = Path("scripts/relay_authority_contract_targets.json")
TERMINAL_HANDOFF = Path("src/services/discord/session_relay_sink/terminal_handoff.rs")
SESSION_RELAY_SINK = Path("src/services/discord/session_relay_sink.rs")
WATCHER_REGISTRY = Path("src/services/discord/tmux_watcher_registry.rs")
DESTRUCTIVE_CANCEL_GATE = Path("src/services/discord/destructive_cancel_gate.rs")
# Every file the script mutates; it backs up and hash-verifies all of them on
# every row, so the fixture tree has to carry the whole set.
MUTATION_FILES = (
    TERMINAL_HANDOFF,
    SESSION_RELAY_SINK,
    WATCHER_REGISTRY,
    DESTRUCTIVE_CANCEL_GATE,
)
# #5071 relay-tail S4 raised this from four and its r2 repair added S4-m7; the
# declared condition-3 floor stays four.
MUTATION_COUNT = 7
MUTATION_NAMES = ("M10", "M6", "M8", "anchor-drop", "S4-m5", "S4-m6", "S4-m7")


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
if [[ "${{CARGO_TERM_COLOR-}}" == "never" ]]; then
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
    def run_script_with_fake_cargo(root: Path, cargo: Path) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.pop("RELAY_AUTHORITY_MUTATION_TEST_MODE", None)
        env.pop("RELAY_AUTHORITY_MUTATION_FIXTURE_RUNNER", None)
        env["CARGO_TERM_COLOR"] = "always"
        env["PATH"] = str(cargo.parent) + os.pathsep + env.get("PATH", "")
        return subprocess.run(
            ["bash", str(root / MUTATION_SCRIPT)],
            cwd=root,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    @staticmethod
    def assert_sources_restored(test: unittest.TestCase, root: Path) -> None:
        for relative in MUTATION_FILES:
            test.assertEqual((root / relative).read_bytes(), (REPO_ROOT / relative).read_bytes())

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


if __name__ == "__main__":
    unittest.main()
