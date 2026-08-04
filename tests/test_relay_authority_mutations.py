from __future__ import annotations

import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
MUTATION_SCRIPT = Path("scripts/run_relay_authority_mutations.sh")
TERMINAL_HANDOFF = Path("src/services/discord/session_relay_sink/terminal_handoff.rs")
SESSION_RELAY_SINK = Path("src/services/discord/session_relay_sink.rs")


class RelayAuthorityMutationScriptTests(unittest.TestCase):
    maxDiff = None

    def copy_fixture(self) -> Path:
        temp = Path(tempfile.mkdtemp(prefix="relay-authority-mutations-"))
        self.addCleanup(shutil.rmtree, temp, True)
        for relative in (MUTATION_SCRIPT, TERMINAL_HANDOFF, SESSION_RELAY_SINK):
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
    def write_fake_cargo(root: Path) -> Path:
        bin_dir = root / "fake-bin"
        bin_dir.mkdir()
        cargo = bin_dir / "cargo"
        cargo.write_text(
            """#!/usr/bin/env bash
set -euo pipefail
if [[ "${CARGO_TERM_COLOR-}" == "never" ]]; then
    printf '   Compiling agentdesk v0.1.0 (fake)\\n'
else
    printf '\\033[1m\\033[92m   Compiling\\033[0m agentdesk v0.1.0 (fake)\\n'
fi
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
        for relative in (TERMINAL_HANDOFF, SESSION_RELAY_SINK):
            test.assertEqual((root / relative).read_bytes(), (REPO_ROOT / relative).read_bytes())

    def test_color_neutralization_keeps_cache_proof_color_proof(self) -> None:
        root = self.copy_fixture()
        cargo = self.write_fake_cargo(root)

        result = self.run_script_with_fake_cargo(root, cargo)

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout.count("compiling_agentdesk=1"), 4, result.stdout)
        self.assertNotIn("cache-proof=invalid", result.stderr)

    def test_four_fixed_mutations_are_killed_and_sources_restore(self) -> None:
        root = self.copy_fixture()
        runner = self.write_runner(root, "exit 101\n")

        result = self.run_script(root, runner)

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        count_line = next(
            line for line in result.stdout.splitlines() if line.startswith("MUTATION_COUNT ")
        )
        fields = dict(item.split("=", 1) for item in count_line.split()[1:])
        self.assertEqual(int(fields["count"]), 4)
        self.assertEqual(int(fields["minimum"]), 4)
        self.assertEqual(
            result.stdout.count("status=KILLED rc=101"), int(fields["count"]), result.stdout
        )
        self.assertIn(
            f"MUTATION_SUMMARY killed={fields['count']} survived=0 minimum=4 status=PASS",
            result.stdout,
        )
        self.assert_sources_restored(self, root)

    def test_concurrent_run_fails_closed_without_modifying_sources(self) -> None:
        root = self.copy_fixture()
        runner = self.write_runner(root, "exit 101\n")
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
        runner = self.write_runner(root, "exit 101\n")
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
            'if [[ "$1" == "M6" ]]; then exit 0; fi\nexit 101\n',
        )

        result = self.run_script(root, runner)

        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("MUTATION_RESULT mutation=M6 status=SURVIVED rc=0", result.stderr)
        self.assertIn("ERROR mutation survived: M6", result.stderr)
        self.assert_sources_restored(self, root)

    def test_signal_exit_releases_lock_and_restores_sources(self) -> None:
        root = self.copy_fixture()
        runner = self.write_runner(root, 'kill -TERM "$PPID"\nsleep 1\nexit 101\n')
        lock_dir = root / "target/relay-authority-mutations.lock"

        result = self.run_script(root, runner)

        self.assertEqual(result.returncode, 143, result.stdout + result.stderr)
        self.assertFalse(lock_dir.exists())
        self.assert_sources_restored(self, root)

    def test_missing_mutation_anchor_fails_closed_and_restores_sources(self) -> None:
        root = self.copy_fixture()
        runner = self.write_runner(root, "exit 101\n")
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
        runner = self.write_runner(root, "exit 101\n")

        result = self.run_script(root, runner)

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        count_line = next(
            line for line in result.stdout.splitlines() if line.startswith("MUTATION_COUNT ")
        )
        fields = dict(item.split("=", 1) for item in count_line.split()[1:])
        self.assertEqual(int(fields["count"]), 4)
        self.assertEqual(int(fields["minimum"]), 4)
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
