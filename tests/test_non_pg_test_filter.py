"""Execution contract for the shared non-PG libtest filter."""

from __future__ import annotations

import importlib.util
import os
import shlex
import subprocess
import sys
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
FILTER = ROOT / "scripts/ci/non-pg-test-filter.sh"
MEMBERSHIP = ROOT / "scripts/check_pg_test_lane_membership.py"


def load_membership_module():
    spec = importlib.util.spec_from_file_location("non_pg_filter_membership", MEMBERSHIP)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class NonPgTestFilter(unittest.TestCase):
    def test_source_exports_filter_and_replays_the_generated_selection(self) -> None:
        script = r'''
cargo() {
  printf 'cargo'
  printf ' <%s>' "$@"
  printf '\n'
}
source "$1"
printf 'filter'
printf ' <%s>' "${NON_PG_SKIP_ARGS[@]}"
printf '\n'
printf 'include'
printf ' <%s>' "${PG_INCLUDE_ARGS[@]}"
printf '\n'
run_non_pg_filter_replay
'''
        result = subprocess.run(
            ["bash", "-c", script, "bash", str(FILTER)],
            check=True,
            capture_output=True,
            text=True,
        )
        lines = result.stdout.splitlines()
        membership = load_membership_module()
        args = membership.load_non_pg_skip_args(ROOT)
        self.assertEqual(lines[0], "filter" + "".join(f" <{arg}>" for arg in args))
        self.assertEqual(
            lines[1], "include" + "".join(f" <{arg}>" for arg in args[1::2])
        )
        replay = membership.load_non_pg_filter_replay(ROOT)
        self.assertEqual(replay, membership.non_pg_selection(ROOT)[1])
        prefix = "cargo <test> <--lib> <--> <--exact>"
        replayed: list[str] = []
        for line in lines[2:]:
            self.assertTrue(line.startswith(prefix + " <"), line)
            replayed.extend(line.removeprefix(prefix + " <")[:-1].split("> <"))
        self.assertEqual(tuple(replayed), replay)

    def test_pg_shards_partition_the_unsharded_selection(self) -> None:
        """Main's two PG jobs together run each PG-selected test exactly once."""
        membership = load_membership_module()
        coverage = membership._load_coverage_module(ROOT)
        inventory = membership.load_lib_test_inventory(ROOT)
        script = 'source "$1" && printf "%s\\n" "${PG_INCLUDE_ARGS[@]}"'

        def selection(shard: str | None) -> set[str]:
            env = {key: value for key, value in os.environ.items() if key != "PG_INCLUDE_SHARD"}
            if shard is not None:
                env["PG_INCLUDE_SHARD"] = shard
            result = subprocess.run(
                ["bash", "-c", script, "bash", str(FILTER)],
                check=True, capture_output=True, text=True, env=env,
            )
            command = shlex.join(["cargo", "test", "--lib", "--", *result.stdout.splitlines()])
            return coverage.cargo_test_filter(command).selected_tests(inventory)

        whole, shard_0, shard_1 = selection(None), selection("0"), selection("1")
        self.assertTrue(shard_0 and shard_1)
        self.assertEqual(shard_0 & shard_1, set())
        self.assertEqual(shard_0 | shard_1, whole)
        for shard in ("2", "01", " "):
            with self.subTest(shard=shard):
                env = {**os.environ, "PG_INCLUDE_SHARD": shard}
                result = subprocess.run(
                    ["bash", "-c", script, "bash", str(FILTER)],
                    capture_output=True, text=True, env=env,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertIn("PG_INCLUDE_SHARD must be 0 or 1", result.stderr)

    def test_generated_selection_does_not_depend_on_hash_seed(self) -> None:
        """A set-ordered shard split would flip the checked-in file between runs."""
        code = (
            "import sys; sys.path.insert(0, 'scripts');"
            "import check_pg_test_lane_membership as m; from pathlib import Path;"
            "print('\\n'.join(m.non_pg_selection_plan(Path('.'))[3]))"
        )
        outputs = {
            subprocess.run(
                [sys.executable, "-c", code], cwd=ROOT, check=True,
                capture_output=True, text=True, env={**os.environ, "PYTHONHASHSEED": seed},
            ).stdout
            for seed in ("0", "1", "2")
        }
        lines = FILTER.read_text("utf-8").splitlines()
        membership = load_membership_module()
        begin = lines.index(membership.NON_PG_SELECTION_BEGIN)
        end = lines.index(membership.NON_PG_SELECTION_END)
        self.assertEqual(outputs, {"\n".join(lines[begin:end + 1]) + "\n"})


if __name__ == "__main__":
    unittest.main()
