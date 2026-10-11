"""Required-PR coverage and justfile/PR filter parity, judged through the coverage CLI check."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
REQUIRED_BASELINE = Path("scripts/test_lane_required_pr_baseline.txt")


def _load(name: str, rel: str):
    spec = importlib.util.spec_from_file_location(name, REPO_ROOT / rel)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


coverage = _load("check_test_lane_coverage_parity", "scripts/check_test_lane_coverage.py")
integrity = _load("check_test_target_integrity_parity", "scripts/check_test_target_integrity.py")

FILTER = (
    "NON_PG_SKIP_ARGS=(\n  --skip _pg\n  --skip pgmod::\n)\n"
    "NON_PG_FILTER_REPLAY=(\n  pgmod::plain\n)\n"
)
SWEEP = (
    "source scripts/ci/non-pg-test-filter.sh",
    'cargo test --lib -- "${NON_PG_SKIP_ARGS[@]}"',
    "run_non_pg_filter_replay",
)
PG_LANE = (
    'source scripts/ci/non-pg-test-filter.sh && '
    'cargo test --lib -- "${PG_INCLUDE_ARGS[@]}" --test-threads=1'
)
# Names carrying `_pg` are skipped by the sweep's name filter although no database is used.
NAME_SKIPPED = ("reads_pg_env",)


def job(name: str, *commands: str) -> str:
    body = "".join(f"          {command}\n" for command in commands)
    return f"  {name}:\n    runs-on: ubuntu-latest\n    steps:\n      - run: |\n{body}"


def mirror(upstream: str, needs: str | None = None) -> str:
    return (
        f"  {upstream}_required_context:\n    needs:\n      - {needs or upstream}\n"
        "    runs-on: ubuntu-latest\n    steps:\n      - env:\n"
        f"          UPSTREAM_JOB_NAME: {upstream}\n"
        f"          UPSTREAM_RESULT: ${{{{ needs.{upstream}.result }}}}\n"
        "        run: ./scripts/required-check-mirror.sh\n"
    )


class Repo:
    """A minimal repository whose required PR jobs are proven through mirrors."""

    def __init__(self) -> None:
        self.modules = {"alpha": ("one",), "gamma": ("local",), "pgmod": ("plain", "uses_pg_db")}
        self.just = ["cargo test --lib alpha -- --skip _pg"]
        self.pr = {"sweep": list(SWEEP), "targeted": ["cargo test --lib alpha", PG_LANE]}
        self.mirrors = {"sweep": None, "targeted": None}
        self.advisory: dict[str, list[str]] = {}
        self.nightly: list[str] = []
        self.exceptions: list[str] = []

    def write(self, root: Path) -> None:
        for rel in ("src", "scripts/ci", ".github/workflows"):
            (root / rel).mkdir(parents=True, exist_ok=True)
        (root / "src/lib.rs").write_text("".join(
            f"#[cfg(test)]\nmod {name} {{\n"
            + "".join(f"    #[test]\n    fn {test}() {{}}\n" for test in tests) + "}\n"
            for name, tests in self.modules.items()
        ), "utf-8")
        ids = {f"{name}::{test}" for name, tests in self.modules.items() for test in tests}
        (root / "scripts/lib_test_inventory_manifest.txt").write_text(
            integrity.render_lib_inventory_manifest(ids), "utf-8"
        )
        (root / coverage.PG_MANIFEST_REL).write_text("[tests]\npgmod::uses_pg_db\n", "utf-8")
        (root / coverage.NON_PG_FILTER_REL).write_text(FILTER, "utf-8")
        (root / "justfile").write_text(
            "test-non-pg:\n" + "".join(f"    {line}\n" for line in self.just), "utf-8"
        )
        jobs = [job(name, *commands) for name, commands in self.pr.items()]
        jobs += [mirror(name, needs) for name, needs in self.mirrors.items()]
        jobs += [job(name, *commands) for name, commands in self.advisory.items()]
        (root / ".github/workflows/ci-pr.yml").write_text(
            "name: pr\non:\n  pull_request:\njobs:\n" + "".join(jobs), "utf-8"
        )
        if self.nightly:
            (root / ".github/workflows/ci-nightly.yml").write_text(
                "name: nightly\non:\n  schedule:\njobs:\n" + job("full", *self.nightly), "utf-8"
            )
        (root / coverage.BASELINE_REL).write_text("", "utf-8")
        (root / REQUIRED_BASELINE).write_text("", "utf-8")
        (root / "scripts/test_lane_pr_parity_exceptions.txt").write_text(
            "".join(f"{row}\n" for row in self.exceptions), "utf-8"
        )

    def check(self) -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.write(root)
            output = io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
                result = coverage.check(
                    root, root / coverage.BASELINE_REL, set(), reference_label="fixture"
                )
        return result, output.getvalue()


def findings(output: str, kind: str) -> list[str]:
    return [line for line in output.splitlines() if kind in line]


class RequiredPrParityTests(unittest.TestCase):
    def assert_clean(self, repo: Repo) -> None:
        result, output = repo.check()
        self.assertEqual(result, 0, output)
        for axis in ("all_coverage", "required_pr_coverage", "pr_filter_parity"):
            self.assertIn(f"OK: {axis}", output)

    def test_fixture_passes_every_axis(self) -> None:
        self.assert_clean(Repo())

    def test_justfile_only_filter_fails_parity_although_the_sweep_covers_it(self) -> None:
        repo = Repo()
        repo.just.append("cargo test --lib gamma -- --skip _pg")
        result, output = repo.check()
        self.assertEqual(result, 1)
        missing = findings(output, "pr-parity-missing")
        self.assertEqual(len(missing), 1, output)
        self.assertIn("justfile:3", missing[0])
        self.assertIn("gamma::local", missing[0])
        self.assertIn("OK: required_pr_coverage", output)
        repo.pr["targeted"].append("cargo test --lib ''")
        self.assertEqual(len(findings(repo.check()[1], "pr-parity-missing")), 1)

    def test_explicit_required_pr_filter_restores_parity(self) -> None:
        repo = Repo()
        repo.just.append("cargo test --lib gamma -- --skip _pg")
        repo.pr["targeted"].append("cargo test --lib gamma")
        self.assert_clean(repo)

    def test_dropping_the_only_pr_selection_fails_required_axis_not_all_axis(self) -> None:
        repo = Repo()
        repo.modules["delta"] = NAME_SKIPPED
        repo.just.append("cargo test --lib delta")
        repo.pr["targeted"].append("cargo test --lib delta")
        self.assert_clean(repo)
        repo.pr["targeted"].remove("cargo test --lib delta")
        result, output = repo.check()
        self.assertEqual(result, 1)
        self.assertIn("OK: all_coverage", output)
        self.assertIn("required-pr-uncovered", output)
        self.assertIn("  + delta (e.g. delta::reads_pg_env)", output)

    def test_exception_for_a_removed_filter_is_stale(self) -> None:
        repo = Repo()
        repo.exceptions.append("test-non-pg | lib | gone | sweep-covered | removed lane")
        result, output = repo.check()
        self.assertEqual(result, 1)
        self.assertEqual(len(findings(output, "stale-pr-parity-exception")), 1, output)

    def test_exception_turns_stale_once_the_filter_is_explicit_in_pr(self) -> None:
        repo = Repo()
        repo.just.append("cargo test --lib gamma -- --skip _pg")
        repo.exceptions.append("test-non-pg | lib | gamma | sweep-covered | local lane")
        self.assert_clean(repo)
        repo.pr["targeted"].append("cargo test --lib gamma")
        result, output = repo.check()
        self.assertEqual(result, 1)
        stale = findings(output, "stale-pr-parity-exception")
        self.assertEqual(len(stale), 1, output)
        self.assertIn("justfile:3", stale[0])
        repo.exceptions.clear()
        self.assert_clean(repo)

    def test_selection_only_in_a_non_required_pr_job_is_not_required_coverage(self) -> None:
        repo = Repo()
        repo.modules["delta"] = NAME_SKIPPED
        repo.advisory["advisory"] = ["cargo test --lib delta"]
        result, output = repo.check()
        self.assertEqual(result, 1)
        self.assertIn("OK: all_coverage", output)
        self.assertIn("required-pr-uncovered", output)
        self.assertIn("  + delta (e.g. delta::reads_pg_env)", output)

    def test_mirror_without_the_upstream_in_needs_is_unproven(self) -> None:
        repo = Repo()
        repo.mirrors["targeted"] = "sweep"
        result, output = repo.check()
        self.assertEqual(result, 1)
        unproven = findings(output, "pr-context-unproven")
        self.assertEqual(len(unproven), 1, output)
        self.assertIn("targeted_required_context", unproven[0])
        self.assertIn("upstream targeted is not in needs", unproven[0])

    def test_pr_skip_of_the_target_id_leaves_it_unselected(self) -> None:
        repo = Repo()
        repo.modules["delta"] = NAME_SKIPPED
        repo.pr["targeted"].append("cargo test --lib delta -- --skip delta::reads_pg_env")
        repo.just.append("cargo test --lib delta")
        result, output = repo.check()
        self.assertEqual(result, 1)
        self.assertIn("  + delta (e.g. delta::reads_pg_env)", output)
        self.assertEqual(len(findings(output, "pr-parity-missing")), 1, output)

    def test_nightly_only_selection_counts_for_all_axis_only(self) -> None:
        repo = Repo()
        repo.modules["epsilon"] = NAME_SKIPPED
        repo.nightly.append("cargo test --lib epsilon")
        result, output = repo.check()
        self.assertEqual(result, 1)
        self.assertIn("OK: all_coverage", output)
        self.assertIn("  + epsilon (e.g. epsilon::reads_pg_env)", output)

    def test_pg_only_lane_is_not_non_pg_coverage_on_either_axis(self) -> None:
        repo = Repo()
        repo.modules["zeta"] = NAME_SKIPPED
        result, output = repo.check()
        self.assertEqual(result, 1)
        self.assertIn("all-uncovered", output)
        self.assertIn("required-pr-uncovered", output)
        self.assertEqual(output.count("  + zeta"), 2, output)

    def test_sweep_exception_without_the_sweep_is_unproven(self) -> None:
        repo = Repo()
        repo.just.append("cargo test --lib gamma -- --skip _pg")
        repo.exceptions.append("test-non-pg | lib | gamma | sweep-covered | local lane")
        del repo.pr["sweep"], repo.mirrors["sweep"]
        result, output = repo.check()
        self.assertEqual(result, 1)
        invalid = findings(output, "pr-parity-exception-invalid")
        self.assertEqual(len(invalid), 1, output)
        self.assertIn("gamma::local", invalid[0])
        self.assertIn("  + gamma (e.g. gamma::local)", output)

    def test_unreadable_required_pr_commands_are_unresolved(self) -> None:
        for command in ('cargo test --lib "$FILTER"', "just test-non-pg || true"):
            with self.subTest(command=command):
                repo = Repo()
                repo.pr["targeted"].append(command)
                result, output = repo.check()
                self.assertEqual(result, 1)
                unresolved = findings(output, "source-command-unresolved")
                self.assertEqual(len(unresolved), 1, output)

    def test_malformed_or_unsorted_exceptions_are_invalid(self) -> None:
        repo = Repo()
        repo.just.append("cargo test --lib gamma -- --skip _pg")
        repo.exceptions += [
            "test-non-pg | lib | gamma | sweep-covered | local lane",
            "test-non-pg | lib | alpha | sweep-covered | listed after gamma",
            "test-non-pg | lib | gamma | always | no proof",
        ]
        result, output = repo.check()
        self.assertEqual(result, 1)
        invalid = findings(output, "pr-parity-exception-invalid")
        self.assertTrue(any("mode must be" in line for line in invalid), output)
        self.assertTrue(any("sorted" in line for line in invalid), output)

    def test_reference_without_required_baseline_reads_as_empty_and_catches_growth(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            Repo().write(root)
            (root / REQUIRED_BASELINE).unlink()
            subprocess.run(["git", "init", "-q", "-b", "main", root], check=True)
            subprocess.run(["git", "-C", root, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", root, "-c", "user.email=t@example.com", "-c", "user.name=T",
                 "-c", "gc.auto=0", "-c", "maintenance.auto=false",
                 "commit", "-q", "-m", "before the required baseline"],
                check=True,
            )
            (root / REQUIRED_BASELINE).write_text("alpha\n", "utf-8")
            result = subprocess.run(
                [sys.executable, str(REPO_ROOT / "scripts/check_test_lane_coverage.py"),
                 "--repo-root", str(root), "--baseline-ref", "HEAD"],
                text=True, capture_output=True, check=False,
            )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("required-pr-baseline-drift: baseline growth forbidden", result.stderr)
        self.assertIn("  + alpha", result.stderr)


if __name__ == "__main__":
    unittest.main()
