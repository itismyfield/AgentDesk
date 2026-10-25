"""Execution contract for the shared non-PG libtest filter."""

from __future__ import annotations

import importlib.util
import os
import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


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
    def setUp(self) -> None:
        # A runner's artifact input would send run_prepared_lib_test past the shell cargo stubs;
        # the shared-harness cases set it explicitly.
        environment = mock.patch.dict(os.environ)
        environment.start()
        self.addCleanup(environment.stop)
        os.environ.pop("ADK_LIB_TEST_ARTIFACT", None)

    def test_pg_matrix_comes_from_the_partition_count(self) -> None:
        import json
        result = subprocess.run([sys.executable, str(MEMBERSHIP), "--pg-shard-matrix"],
                                check=True, capture_output=True, text=True)
        membership = load_membership_module()
        self.assertEqual(json.loads(result.stdout), {"shard": [*range(membership.PG_SHARD_COUNT), "continuity"]})

    def test_failed_list_cannot_run_an_unsharded_selection(self) -> None:
        membership = load_membership_module()
        ids = [next(f"suite::case_{i}" for i in range(100) if membership.pg_test_shard(f"suite::case_{i}") == shard)
               for shard in range(membership.PG_SHARD_COUNT)]
        listed = "".join(name + ": test\n" for name in ids) + f"\n{len(ids)} tests, 0 benchmarks\n"
        script = 'cargo() { printf "%s" "$PG_LIST_TEXT"; return 7; }; source "$1" && printf "UNSAFE\\n"'
        result = subprocess.run(["bash", "-c", script, "bash", str(FILTER)],
                                capture_output=True, text=True,
                                env={**os.environ, "PG_INCLUDE_SHARD": "0", "PG_LIST_TEXT": listed})
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("UNSAFE", result.stdout)

    def test_incomplete_duplicate_and_empty_lists_fail_closed(self) -> None:
        membership = load_membership_module()
        for listed in ("", "suite::a: test\n", "suite::a: test\n\n2 tests, 0 benchmarks\n",
                       "suite::a: test\nsuite::a: test\n\n2 tests, 0 benchmarks\n",
                       ": test\n\n1 test, 0 benchmarks\n", "--ignored: test\n\n1 test, 0 benchmarks\n",
                       "\n0 tests, 0 benchmarks\n"):
            with self.subTest(listed=listed):
                with self.assertRaises(ValueError):
                    membership.partition_pg_test_list(listed, "0")

    def test_new_tests_do_not_move_existing_assignments(self) -> None:
        from unittest.mock import patch
        membership = load_membership_module()
        old = {f"suite::case_{index}" for index in range(50)}
        for count in (2, 3):
            with patch.object(membership, "PG_SHARD_COUNT", count):
                before = membership.pg_include_shards(("suite::",), old)
                after = membership.pg_include_shards(("suite::",), old | {"suite::new_pg_case"})
                for left, right in zip(before, after):
                    self.assertEqual(set(left[1:]), set(right[1:]) & old)
                self.assertEqual(set().union(*(set(args[1:]) for args in after)), old | {"suite::new_pg_case"})
                self.assertEqual(sum(len(args) - 1 for args in after), len(old) + 1)

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
        """Main's PG jobs together run each PG-selected test exactly once."""
        membership = load_membership_module()
        coverage = membership._load_coverage_module(ROOT)
        inventory = membership.load_lib_test_inventory(ROOT)
        skips = membership.load_non_pg_skip_args(ROOT)[1::2]
        script = 'cargo() { cat "$PG_LIST"; }; source "$1" && printf "%s\\n" "${PG_INCLUDE_ARGS[@]}"'

        def selection(shard: str | None) -> set[str]:
            env = {key: value for key, value in os.environ.items() if key != "PG_INCLUDE_SHARD"}
            if shard is not None:
                env["PG_INCLUDE_SHARD"] = shard
            with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8") as listed:
                ids = sorted(name for name in inventory if any(value in name for value in skips))
                listed.write("".join(name + ": test\n" for name in ids) + f"\n{len(ids)} tests, 0 benchmarks\n")
                listed.flush()
                env["PG_LIST"] = listed.name
                result = subprocess.run(
                    ["bash", "-c", script, "bash", str(FILTER)],
                    check=True, capture_output=True, text=True, env=env,
                )
            command = shlex.join(["cargo", "test", "--lib", "--", *result.stdout.splitlines()])
            return coverage.cargo_test_filter(command).selected_tests(inventory)

        whole = selection(None)
        shards = [selection(str(index)) for index in range(membership.PG_SHARD_COUNT)]
        self.assertTrue(all(shards))
        self.assertEqual(sum(len(shard) for shard in shards), len(whole))
        self.assertEqual(set().union(*shards), whole)
        for shard in (str(membership.PG_SHARD_COUNT), "01", " "):
            with self.subTest(shard=shard):
                env = {**os.environ, "PG_INCLUDE_SHARD": shard}
                result = subprocess.run(
                    ["bash", "-c", script, "bash", str(FILTER)],
                    capture_output=True, text=True, env=env,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertIn("PG_INCLUDE_SHARD must be one of", result.stderr)

    def test_static_pg_restoration_is_what_bash_sources(self) -> None:
        """The integrity gate judges, per shard, the PG arguments Bash really builds."""
        spec = importlib.util.spec_from_file_location(
            "non_pg_filter_integrity", ROOT / "scripts/check_test_target_integrity.py")
        assert spec and spec.loader
        integrity = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = integrity
        spec.loader.exec_module(integrity)
        restored = integrity.PackageTestIds(ROOT).pg_include_selections()
        self.assertIsInstance(restored, dict)
        for shard in ("", *(str(index) for index in range(load_membership_module().PG_SHARD_COUNT))):
            env = {key: value for key, value in os.environ.items() if key != "PG_INCLUDE_SHARD"}
            if shard:
                env["PG_INCLUDE_SHARD"] = shard
            with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8") as listed:
                ids = sorted(name for name in load_membership_module().load_lib_test_inventory(ROOT)
                             if any(value in name for value in restored[""]))
                listed.write("".join(name + ": test\n" for name in ids) + f"\n{len(ids)} tests, 0 benchmarks\n")
                listed.flush()
                env["PG_LIST"] = listed.name
                result = subprocess.run(
                    ["bash", "-c", 'cargo() { cat "$PG_LIST"; }; source "$1" && printf "%s\\n" "${PG_INCLUDE_ARGS[@]}"',
                     "bash", str(FILTER)], check=True, capture_output=True, text=True, env=env)
            with self.subTest(shard=shard):
                self.assertEqual(restored[shard], tuple(result.stdout.splitlines()))

    def test_pg_contract_rejects_hiding_shared_failures_or_changing_the_list_source(self) -> None:
        membership = load_membership_module()
        text = FILTER.read_text()
        mutations = (
            ('python3 scripts/ci/lib-test-artifact.py run -- "$@"',
             'python3 scripts/ci/lib-test-artifact.py run -- "$@" || "$@"'),
            ('run_prepared_lib_test cargo test --lib -- "${PG_INCLUDE_ARGS[@]}" --list',
             'cargo test --lib -- "${PG_INCLUDE_ARGS[@]}" --list'),
        )
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            target = root / membership.NON_PG_FILTER_REL
            target.parent.mkdir(parents=True)
            for before, after in mutations:
                with self.subTest(mutation=after):
                    self.assertEqual(text.count(before), 1)
                    target.write_text(text.replace(before, after))
                    with self.assertRaisesRegex(ValueError, "PG shard derivation is not canonical"):
                        membership.load_pg_include_shards(root)

    def test_artifact_wrapper_prefixes_preserve_the_proven_cargo_arguments(self) -> None:
        membership = load_membership_module()
        cargo = "cargo test --lib suite:: -- --skip _pg --test-threads=1"
        helper = "python3 scripts/ci/lib-test-artifact.py run -- "
        unset = "env -u AGENTDESK_ROOT_DIR "
        lane = "python3 scripts/run_test_lane.py --lane non-pg-sweep --max-summaries 4 --skip _pg -- "
        for prefix in (helper, unset + helper, lane + unset + helper):
            with self.subTest(prefix=prefix):
                self.assertEqual(membership._proven_cargo_argv(prefix + cargo), shlex.split(cargo))
        for prefix in (helper.replace("run --", "validate --"), helper.replace("run --", "run"),
                       helper.replace("run --", "run --ignored --"),
                       unset.replace("AGENTDESK_ROOT_DIR", "PATH") + helper,
                       lane + helper, lane + unset + helper + helper):
            with self.subTest(prefix=prefix):
                with self.assertRaisesRegex(ValueError, "cargo-command-unparsed"):
                    membership._proven_cargo_argv(prefix + cargo)

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
