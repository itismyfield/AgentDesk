"""Required-PR coverage and justfile/PR filter parity, judged through the coverage CLI check."""

from __future__ import annotations

import contextlib
import dataclasses
import importlib.util
import io
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

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

FILTER_SKIPS = "NON_PG_SKIP_ARGS=(\n  --skip _pg\n  --skip pgmod::\n)\n"
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


def job(name: str, *commands: str, keys: str = "", step: str = "") -> str:
    body = "".join(f"          {command}\n" for command in commands)
    return f"  {name}:\n    runs-on: ubuntu-latest\n{keys}    steps:\n      - run: |\n{body}{step}"


def changes_job(filters: dict[str, list[str]], options: str = "") -> str:
    """A dorny/paths-filter `changes` job forwarding each filter as an output."""
    outputs = "".join(f"      {n}: ${{{{ steps.filter.outputs.{n} }}}}\n" for n in filters)
    lists = "".join(
        f"            {name}:\n" + "".join(f"              - '{p}'\n" for p in patterns)
        for name, patterns in filters.items()
    )
    return (
        "  changes:\n    runs-on: ubuntu-latest\n    outputs:\n" + outputs
        + "    steps:\n      - id: filter\n        uses: dorny/paths-filter@v3\n"
        "        with:\n" + options + "          filters: |\n" + lists
    )


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
        self.trailer = ""
        self.replay = ["pgmod::plain"]
        self.files: dict[str, str] = {}
        self.lib_tail = ""
        self.manifest: set[str] | None = None
        self.baselines: tuple[list[str], list[str]] = ([], [])
        self.filters: dict[str, list[str]] = {}
        self.gated: dict[str, str] = {}
        self.keys: dict[str, str] = {}
        self.filter_options = ""
        self.step_keys: dict[str, str] = {}
        self.mirror_text: dict[str, str] = {}
        self.id_exceptions: list[str] = []
        self.extra_ids: set[str] = set()
        self.removed: list[str] = []

    def write(self, root: Path) -> None:
        for rel in ("src", "scripts/ci", ".github/workflows"):
            (root / rel).mkdir(parents=True, exist_ok=True)
        (root / "src/lib.rs").write_text("".join(
            f"#[cfg(test)]\nmod {name} {{\n"
            + "".join(f"    #[test]\n    fn {test}() {{}}\n" for test in tests) + "}\n"
            for name, tests in self.modules.items()
        ) + self.lib_tail, "utf-8")
        for rel, text in self.files.items():
            (root / rel).parent.mkdir(parents=True, exist_ok=True)
            (root / rel).write_text(text, "utf-8")
        for rel in self.removed:
            (root / rel).unlink(missing_ok=True)
        (root / "Cargo.toml").write_text('[package]\nname = "fixture"\n', "utf-8")
        ids = {f"{name}::{test}" for name, tests in self.modules.items() for test in tests}
        (root / "scripts/lib_test_inventory_manifest.txt").write_text(
            integrity.render_lib_inventory_manifest(
                ids | self.extra_ids if self.manifest is None else self.manifest
            ), "utf-8"
        )
        (root / coverage.PG_MANIFEST_REL).write_text("[tests]\npgmod::uses_pg_db\n", "utf-8")
        (root / coverage.NON_PG_FILTER_REL).write_text(
            FILTER_SKIPS + "NON_PG_FILTER_REPLAY=(\n"
            + "".join(f"  {entry}\n" for entry in sorted(self.replay)) + ")\n", "utf-8"
        )
        (root / "justfile").write_text(
            "test-non-pg:\n" + "".join(f"    {line}\n" for line in self.just), "utf-8"
        )
        gate = "    needs: changes\n    if: needs.changes.outputs.{} == 'true'\n"
        jobs = [changes_job(self.filters, self.filter_options)] if self.filters else []
        jobs += [
            job(
                name, *commands, step=self.step_keys.get(name, ""),
                keys=(gate.format(self.gated[name]) if name in self.gated else "")
                + self.keys.get(name, ""),
            )
            for name, commands in self.pr.items()
        ]
        jobs += [
            self.mirror_text.get(name) or mirror(name, needs)
            for name, needs in self.mirrors.items()
        ]
        jobs += [job(name, *commands) for name, commands in self.advisory.items()]
        (root / ".github/workflows/ci-pr.yml").write_text(
            "name: pr\non:\n  pull_request:\njobs:\n" + "".join(jobs) + self.trailer, "utf-8"
        )
        if self.nightly:
            (root / ".github/workflows/ci-nightly.yml").write_text(
                "name: nightly\non:\n  schedule:\njobs:\n" + job("full", *self.nightly), "utf-8"
            )
        for rel, rows in zip((coverage.BASELINE_REL, REQUIRED_BASELINE), self.baselines):
            (root / rel).write_text("".join(f"{row}\n" for row in sorted(rows)), "utf-8")
        (root / "scripts/test_lane_pr_parity_exceptions.txt").write_text(
            "".join(f"{row}\n" for row in self.exceptions), "utf-8"
        )
        (root / "scripts/test_lane_required_id_exceptions.txt").write_text(
            "".join(f"{row}\n" for row in self.id_exceptions), "utf-8"
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


    def delta(self, change) -> tuple[int, str]:
        """Commit this repo as the reference, commit `change` on top, and run the coverage
        CLI against the reference the way CI does."""
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.write(root)
            subprocess.run(["git", "init", "-q", "-b", "main", root], check=True)
            subprocess.run(GIT + ["-C", root, "add", "."], check=True)
            subprocess.run(GIT + ["-C", root, "commit", "-q", "-m", "reference"], check=True)
            base = subprocess.check_output(["git", "-C", root, "rev-parse", "HEAD"], text=True)
            change(self)
            self.write(root)
            subprocess.run(GIT + ["-C", root, "add", "-A"], check=True)
            subprocess.run(GIT + ["-C", root, "commit", "-q", "--allow-empty", "-m", "change"],
                           check=True)
            output = io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
                result = coverage.main(
                    ["--repo-root", str(root), "--baseline-ref", base.strip()]
                )
        return result, output.getvalue()


GIT = [
    "git", "-c", "user.email=t@example.com", "-c", "user.name=T", "-c", "gc.auto=0",
    "-c", "maintenance.auto=false", "-c", "commit.gpgsign=false",
]


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

    def test_identical_filters_selecting_nothing_are_not_parity(self) -> None:
        repo = Repo()
        repo.just.append("cargo test --lib no_such_test")
        repo.pr["targeted"].append("cargo test --lib no_such_test")
        result, output = repo.check()
        self.assertEqual(result, 1)
        missing = findings(output, "pr-parity-missing")
        self.assertEqual(len(missing), 1, output)
        self.assertIn("selects no id in the lib inventory manifest", missing[0])

    def test_job_text_the_workflow_does_not_contain_is_unresolved(self) -> None:
        pgm = coverage._parity_module()._sibling("check_pg_test_lane_membership.py")
        parse_jobs = pgm.parse_jobs

        def drifted(path, repo_root, findings=None):
            return [
                dataclasses.replace(job, text=job.text + "# drifted")
                if job.name == "targeted" else job
                for job in parse_jobs(path, repo_root, findings)
            ]

        with mock.patch.object(pgm, "parse_jobs", drifted):
            result, output = Repo().check()
        self.assertEqual(result, 1)
        unresolved = findings(output, "source-command-unresolved")
        self.assertTrue(any("ci-pr.yml:targeted:" in line for line in unresolved), output)

    def test_pr_cargo_run_outside_any_parsed_job_is_unresolved(self) -> None:
        repo = Repo()
        repo.trailer = "x-unparsed:\n  steps:\n    - run: cargo test --lib gamma\n"
        result, output = repo.check()
        self.assertEqual(result, 1)
        unresolved = findings(output, "source-command-unresolved")
        self.assertEqual(len(unresolved), 1, output)
        self.assertIn("cargo test --lib gamma", unresolved[0])

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



NEW = "FAIL: new_required_test_ids: "
NEW_FINDING = re.compile(r"^FAIL: new_required_test_ids: ([a-z-]+): [^`]*`([^`]+)`")
REPLAY_EXPECTED = REPO_ROOT / "tests/fixtures/lane_new_id/debt_module_replay_expected.txt"
REPLAY_MODULE = "services::discord::router::message_handler::provider_isolation"
REPLAY_TESTS = (
    "matrix_preserves_live_turns_and_only_recreates_strong_idle_mismatches",
    "weak_idle_mismatch_falls_back_at_escalation_threshold",
    "stale_inflight_falls_back_but_live_turn_never_recreates",
    "reconcile_defer_does_not_call_recreate_sink",
    "reconcile_recreate_calls_sink_once_for_exact_tmux_session",
)
STEP_DECOUPLED_MIRROR = (
    "  targeted_required_context:\n    needs:\n      - targeted\n"
    "    runs-on: ubuntu-latest\n    steps:\n      - env:\n"
    "          UPSTREAM_JOB_NAME: targeted\n"
    "          UPSTREAM_RESULT: ${{ needs.targeted.result }}\n"
    "        run: echo staged\n"
    "      - run: ./scripts/required-check-mirror.sh\n"
)


def new_findings(output: str, kind: str) -> list[str]:
    return [line for line in output.splitlines() if line.startswith(f"{NEW}{kind}: ")]


def debt_repo() -> Repo:
    """`delta` already sits on both debt baselines, the #5046 shape."""
    repo = Repo()
    repo.modules["delta"] = ("old_case",)
    repo.baselines = (["delta"], ["delta"])
    return repo


def add_unselected(repo: Repo) -> None:
    # `_pg` in the name keeps the non-PG sweep away although no database is used.
    repo.modules["delta"] = (*repo.modules["delta"], "reads_pg_env")


def add_required(repo: Repo, command: str = "cargo test --lib delta") -> None:
    add_unselected(repo)
    repo.pr["targeted"].append(command)
    repo.baselines = ([], [])


class NewRequiredIdTests(unittest.TestCase):
    """Each lib id a change adds must run in a required PR job, judged against the
    committed reference the way the contracts shard runs the coverage CLI."""

    def assert_module_axes_ok(self, output: str) -> None:
        for axis in ("all_coverage", "required_pr_coverage", "pr_filter_parity"):
            self.assertIn(f"OK: {axis}", output)

    def test_new_test_in_a_baselined_debt_module_is_unrequired(self) -> None:
        result, output = debt_repo().delta(add_unselected)
        unrequired = new_findings(output, "new-id-unrequired")
        self.assertEqual(len(unrequired), 1, output)
        self.assertIn("`delta::reads_pg_env`: no PR job selects it", unrequired[0])
        self.assert_module_axes_ok(output)
        self.assertEqual(result, 1)

    def test_replayed_debt_module_additions_fail_each_id_while_module_axes_pass(self) -> None:
        repo = Repo()
        del repo.pr["sweep"], repo.mirrors["sweep"]
        nest = "".join(f"pub mod {name} {{ " for name in REPLAY_MODULE.split("::"))
        close = "}" * len(REPLAY_MODULE.split("::"))

        def module(*tests: str) -> str:
            body = "".join(f"    #[test]\n    fn {test}() {{}}\n" for test in tests)
            inner = f"#[cfg(test)]\nmod thread_role_inheritance_tests {{\n{body}}}\n"
            return f"{nest}\n{inner}{close}\n"

        prefix = f"{REPLAY_MODULE}::thread_role_inheritance_tests"
        repo.lib_tail = module("existing_case")
        repo.extra_ids = {f"{prefix}::existing_case"}
        debt = ["gamma", "pgmod", prefix]
        repo.baselines = (debt, debt)

        def replay(r: Repo) -> None:
            r.lib_tail = module("existing_case", *REPLAY_TESTS)
            r.extra_ids |= {f"{prefix}::{test}" for test in REPLAY_TESTS}

        result, output = repo.delta(replay)
        observed = {
            f"{m.group(1)} {m.group(2)}"
            for m in map(NEW_FINDING.match, output.splitlines()) if m
        }
        expected = {
            row for row in REPLAY_EXPECTED.read_text("utf-8").splitlines()
            if row and not row.startswith("#")
        }
        self.assertEqual(observed, expected, output)
        self.assert_module_axes_ok(output)
        self.assertEqual(result, 1)

    def test_new_test_file_no_pr_filter_selects_is_unrequired(self) -> None:
        def add_file(repo: Repo) -> None:
            repo.files["src/fresh.rs"] = (
                "#[cfg(test)]\nmod tests {\n    #[test]\n    fn reads_pg_env() {}\n}\n"
            )
            repo.lib_tail = "pub mod fresh;\n"
            repo.extra_ids = {"fresh::tests::reads_pg_env"}

        result, output = Repo().delta(add_file)
        unrequired = new_findings(output, "new-id-unrequired")
        self.assertEqual(len(unrequired), 1, output)
        self.assertIn("src/fresh.rs: `fresh::tests::reads_pg_env`", unrequired[0])
        self.assertEqual(result, 1)

    def test_selection_only_in_an_advisory_pr_job_is_unrequired(self) -> None:
        def advisory(repo: Repo) -> None:
            add_unselected(repo)
            repo.advisory["advisory"] = ["cargo test --lib delta"]
            repo.baselines = ([], ["delta"])

        result, output = debt_repo().delta(advisory)
        unrequired = new_findings(output, "new-id-unrequired")
        self.assertEqual(len(unrequired), 1, output)
        self.assertIn("only the non-required PR job advisory selects it", unrequired[0])
        self.assert_module_axes_ok(output)
        self.assertEqual(result, 1)

    def test_required_selection_maps_the_id_to_its_job_and_mirror(self) -> None:
        result, output = debt_repo().delta(add_required)
        self.assertIn("added=1 mapped=1 excepted=0", output)
        self.assertRegex(
            output,
            r"  delta::reads_pg_env -> targeted \(\.github/workflows/ci-pr\.yml:\d+\) / "
            r"mirror targeted_required_context / context \"targeted_required_context\" / "
            r"no path gate",
        )
        self.assertEqual(result, 0, output)

    def test_mirror_needs_mismatch_leaves_the_id_context_unproven(self) -> None:
        def unproven(repo: Repo) -> None:
            add_required(repo)
            repo.mirrors["targeted"] = "sweep"

        result, output = debt_repo().delta(unproven)
        context = new_findings(output, "new-id-context-unproven")
        self.assertEqual(len(context), 1, output)
        self.assertIn("targeted's mirror targeted_required_context is unproven", context[0])
        self.assertEqual(result, 1)

        def conditional(repo: Repo) -> None:
            add_required(repo)
            repo.mirror_text["targeted"] = mirror("targeted").replace(
                "      - env:", "      - if: ${{ github.event_name == 'push' }}\n        env:"
            )

        def job_conditional(repo: Repo) -> None:
            add_required(repo)
            repo.mirror_text["targeted"] = mirror("targeted").replace(
                "    runs-on:", "    if: ${{ github.event_name == 'push' }}\n    runs-on:"
            )

        def false_step(repo: Repo) -> None:
            add_required(repo)
            repo.mirror_text["targeted"] = mirror("targeted").replace(
                "      - env:", "      - if: false\n        env:"
            )

        def false_job(repo: Repo) -> None:
            add_required(repo)
            repo.mirror_text["targeted"] = mirror("targeted").replace(
                "    runs-on:", "    if: false\n    runs-on:"
            )

        for change in (conditional, job_conditional, false_step, false_job):
            with self.subTest(change=change.__name__):
                result, output = debt_repo().delta(change)
                context = new_findings(output, "new-id-context-unproven")
                self.assertEqual(len(context), 1, output)
                self.assertIn("every mirror reporting it runs under an `if:`", context[0])
                self.assertNotIn("delta::reads_pg_env -> ", output)
                self.assertEqual(result, 1)

    def test_path_filter_that_misses_the_test_file_is_path_unproven(self) -> None:
        repo = debt_repo()
        repo.filters = {"rust": ["src/**"]}
        repo.gated = {"targeted": "rust"}
        result, output = repo.delta(add_required)
        self.assertIn("/ path src/lib.rs", output)
        self.assertEqual(result, 0, output)

        repo = debt_repo()
        repo.filters = {"rust": ["src/**"]}
        repo.gated = {"targeted": "rust"}

        def narrowed(r: Repo) -> None:
            add_required(r)
            r.filters = {"rust": ["docs/**"]}

        result, output = repo.delta(narrowed)
        path = new_findings(output, "new-id-path-unproven")
        self.assertEqual(len(path), 1, output)
        self.assertIn("targeted: src/lib.rs wakes no path filter", path[0])
        self.assertEqual(result, 1)

        repo = debt_repo()
        repo.filters = {"rust": ["src/**"]}
        repo.gated = {"targeted": "rust"}
        repo.filter_options = "          predicate-quantifier: every\n"

        result, output = repo.delta(add_required)
        path = new_findings(output, "new-id-path-unproven")
        self.assertEqual(len(path), 1, output)
        self.assertIn("forwards no single readable paths-filter list", path[0])

    def test_job_needing_a_path_gated_job_is_path_unproven(self) -> None:
        repo = debt_repo()
        repo.filters = {"docs": ["docs/**"]}
        repo.pr["docs_only"] = ["echo docs"]
        repo.gated = {"docs_only": "docs"}

        def chained(r: Repo) -> None:
            add_required(r)
            r.keys["targeted"] = "    needs:\n      - docs_only\n"

        result, output = repo.delta(chained)
        path = new_findings(output, "new-id-path-unproven")
        self.assertEqual(len(path), 1, output)
        self.assertIn("targeted: it needs the gated job docs_only", path[0])
        self.assertEqual(result, 1)

    def test_listing_or_ignored_only_commands_prove_no_run(self) -> None:
        for mode in ("--list", "--ignored"):
            with self.subTest(mode=mode):
                result, output = debt_repo().delta(
                    lambda r: add_required(r, f"cargo test --lib delta -- {mode}")
                )
                runmode = new_findings(output, "new-id-runmode-unproven")
                self.assertEqual(len(runmode), 1, output)
                self.assertIn("targeted only lists, runs ignored ids", runmode[0])
                self.assertIn("  + delta (e.g. delta::reads_pg_env)", output)
                self.assertEqual(result, 1)

    def test_selecting_step_that_may_skip_or_swallow_failure_proves_no_run(self) -> None:
        for key in (
            "continue-on-error: true", "if: ${{ github.event_name == 'push' }}", "if: false",
            "continue-on-error: ${{ false }} || true",
        ):
            with self.subTest(key=key):
                def guarded(repo: Repo, key=key) -> None:
                    add_required(repo)
                    repo.step_keys["targeted"] = f"        {key}\n"

                result, output = debt_repo().delta(guarded)
                runmode = new_findings(output, "new-id-runmode-unproven")
                self.assertEqual(len(runmode), 1, output)
                self.assertIn(f"sits under a step {key}", runmode[0])
                self.assertNotIn("delta::reads_pg_env -> ", output)
                self.assertEqual(result, 1)

    def test_mirror_env_on_another_step_is_unproven(self) -> None:
        repo = Repo()
        repo.mirror_text["targeted"] = STEP_DECOUPLED_MIRROR
        result, output = repo.check()
        unproven = findings(output, "pr-context-unproven")
        self.assertEqual(len(unproven), 1, output)
        self.assertIn("targeted_required_context", unproven[0])
        self.assertIn("lacks one UPSTREAM_JOB_NAME and UPSTREAM_RESULT", unproven[0])
        self.assertEqual(result, 1)

    def test_mirror_job_parse_jobs_drops_is_unresolved(self) -> None:
        pgm = coverage._parity_module()._sibling("check_pg_test_lane_membership.py")
        parse_jobs = pgm.parse_jobs

        def dropped(path, repo_root, findings=None):
            return [
                job for job in parse_jobs(path, repo_root, findings)
                if job.name != "targeted_required_context"
            ]

        with mock.patch.object(pgm, "parse_jobs", dropped):
            result, output = Repo().check()
        unresolved = findings(output, "source-command-unresolved")
        self.assertTrue(any("required-check-mirror.sh" in x for x in unresolved), output)
        self.assertEqual(result, 1)

    def test_rust_test_missing_from_the_manifest_is_stale(self) -> None:
        repo = debt_repo()

        def unlisted(r: Repo) -> None:
            r.manifest = {"alpha::one", "delta::old_case", "gamma::local", "pgmod::plain",
                          "pgmod::uses_pg_db"}
            add_unselected(r)

        result, output = repo.delta(unlisted)
        stale = new_findings(output, "new-id-manifest-stale")
        self.assertEqual(len(stale), 1, output)
        self.assertIn("1 id(s) differ from source: delta::reads_pg_env", stale[0])
        self.assertEqual(result, 1)

    def test_new_test_module_without_inventoried_ids_is_empty(self) -> None:
        def empty(repo: Repo) -> None:
            repo.lib_tail = "#[cfg(test)]\nmod epsilon_tests {\n}\n"

        result, output = Repo().delta(empty)
        modules = new_findings(output, "new-test-module-empty")
        self.assertEqual(len(modules), 1, output)
        self.assertIn("new test module `epsilon_tests`", modules[0])
        self.assertEqual(result, 1)

    def test_any_new_empty_test_module_fails_unless_an_exact_helper_row_names_it(self) -> None:
        row = "{} | helper-module | #5082 | expires=2099-12-31 | shared fixtures for sibling tests"
        for name in ("regression", "spy", "test_support"):
            with self.subTest(name=name):
                def empty(repo: Repo, name=name) -> None:
                    repo.lib_tail = f"#[cfg(test)]\nmod {name} {{\n    pub fn helper() {{}}\n}}\n"

                result, output = Repo().delta(empty)
                modules = new_findings(output, "new-test-module-empty")
                self.assertEqual(len(modules), 1, output)
                self.assertIn(f"new test module `{name}`", modules[0])
                self.assertEqual(result, 1)

                def excepted(repo: Repo, name=name) -> None:
                    empty(repo)
                    repo.id_exceptions = [row.format(name)]

                result, output = Repo().delta(excepted)
                self.assertEqual(new_findings(output, "new-test-module-empty"), [], output)
                self.assertEqual(result, 0, output)

        result, output = Repo().delta(lambda r: setattr(r, "id_exceptions", [row.format("gone")]))
        stale = new_findings(output, "stale-new-id-exception")
        self.assertEqual(len(stale), 1, output)
        self.assertIn("`gone` is no cfg(test) module", stale[0])

    def test_moved_file_keeps_its_empty_module_but_a_new_one_still_fails(self) -> None:
        home = (
            "#[cfg(test)]\nmod test_support {\n    pub fn helper() -> u32 {\n        7\n    }\n}\n"
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn helper_is_seven() {\n"
            "        assert_eq!(super::test_support::helper(), 7);\n    }\n}\n"
        )
        repo = Repo()
        repo.files["src/old_home.rs"] = home
        repo.lib_tail = "pub mod old_home;\n"
        repo.extra_ids = {"old_home::tests::helper_is_seven"}

        def moved(r: Repo, extra: str = "") -> None:
            r.removed = ["src/old_home.rs"]
            r.files = {"src/new_home.rs": home + extra}
            r.lib_tail = "pub mod new_home;\n"
            r.extra_ids = {"new_home::tests::helper_is_seven"}

        result, output = repo.delta(moved)
        self.assertEqual(new_findings(output, "new-test-module-empty"), [], output)
        self.assertIn("added=1 mapped=1", output)
        self.assertEqual(result, 0, output)

        repo = Repo()
        repo.files["src/old_home.rs"] = home
        repo.lib_tail = "pub mod old_home;\n"
        repo.extra_ids = {"old_home::tests::helper_is_seven"}
        result, output = repo.delta(lambda r: moved(r, "#[cfg(test)]\nmod regression {\n}\n"))
        modules = new_findings(output, "new-test-module-empty")
        self.assertEqual(len(modules), 1, output)
        self.assertIn("new test module `new_home::regression`", modules[0])
        self.assertEqual(result, 1)

    def test_new_test_outside_the_lib_target_is_unresolved(self) -> None:
        def integration(repo: Repo) -> None:
            repo.files["tests/outside.rs"] = "#[test]\nfn outside_case() {}\n"

        result, output = Repo().delta(integration)
        target = new_findings(output, "new-id-target-unresolved")
        self.assertEqual(len(target), 1, output)
        self.assertIn("`outside_case` is in no lib inventory id (not the lib target)", target[0])
        self.assertEqual(result, 1)

    def test_valid_exception_excepts_an_unrequired_id(self) -> None:
        def excepted(repo: Repo) -> None:
            add_unselected(repo)
            repo.id_exceptions = [
                "delta::reads_pg_env | intentional-nonrequired | #5082 | expires=2099-12-31 | "
                "kept out of PR lanes on purpose"
            ]

        result, output = debt_repo().delta(excepted)
        self.assertIn("added=1 mapped=0 excepted=1", output)
        self.assertEqual(result, 0, output)

    def test_exceptions_that_no_longer_apply_are_stale(self) -> None:
        row = "delta::reads_pg_env | {} | #5082 | expires={} | kept out of PR lanes on purpose"
        cases = {
            "deleted id": (
                add_unselected,
                ["delta::gone | intentional-nonrequired | #5082 | expires=2099-12-31 | "
                 "kept out of PR lanes on purpose",
                 row.format("intentional-nonrequired", "2099-12-31")],
                "`delta::gone` is no lib inventory id",
            ),
            "now required": (
                add_required, [row.format("intentional-nonrequired", "2099-12-31")],
                "is now required through targeted",
            ),
            "mode mismatch": (
                add_unselected, [row.format("platform-unavailable", "2099-12-31")],
                "now fails as new-id-unrequired, which mode platform-unavailable",
            ),
            "expired": (
                add_unselected, [row.format("intentional-nonrequired", "2000-01-01")],
                "expired on 2000-01-01",
            ),
        }
        for name, (change, rows, detail) in cases.items():
            with self.subTest(case=name):
                def apply(r: Repo, change=change, rows=rows) -> None:
                    change(r)
                    r.id_exceptions = rows

                result, output = debt_repo().delta(apply)
                stale = new_findings(output, "stale-new-id-exception")
                self.assertEqual(len(stale), 1, output)
                self.assertIn(detail, stale[0])
                self.assertEqual(result, 1)

    def test_malformed_id_exceptions_are_invalid(self) -> None:
        def malformed(repo: Repo) -> None:
            add_unselected(repo)
            repo.id_exceptions = [
                "delta::reads_pg_env | intentional-nonrequired | 5082 | expires=2099-12-31 | "
                "issue lacks its hash mark",
                "delta:: | model-unresolved | #5082 | expires=2099-12-31 | a module prefix row",
            ]

        result, output = debt_repo().delta(malformed)
        invalid = new_findings(output, "new-id-exception-invalid")
        self.assertTrue(any("tracking `#<number>`" in line for line in invalid), output)
        self.assertTrue(any("never a module prefix" in line for line in invalid), output)
        self.assertTrue(any("sorted by test id" in line for line in invalid), output)
        self.assertEqual(len(new_findings(output, "new-id-unrequired")), 1, output)
        self.assertEqual(result, 1)

    def test_change_without_new_ids_passes_the_new_axis(self) -> None:
        result, output = Repo().delta(lambda r: None)
        self.assertRegex(output, r"OK: new_required_test_ids: base=[0-9a-f]{40} added=0 "
                                 r"mapped=0 excepted=0")
        self.assertEqual(result, 0, output)

    def test_sweep_skipped_id_in_the_generated_replay_is_mapped(self) -> None:
        def replayed(repo: Repo) -> None:
            repo.modules["pgmod"] = (*repo.modules["pgmod"], "fresh_case")
            repo.replay.append("pgmod::fresh_case")

        result, output = Repo().delta(replayed)
        self.assertRegex(output, r"  pgmod::fresh_case -> sweep \(\.github/workflows/ci-pr\.yml:")
        self.assertIn("added=1 mapped=1 excepted=0", output)
        self.assertEqual(result, 0, output)

    def test_sweep_skipped_id_without_the_replay_call_is_unrequired(self) -> None:
        def no_replay(repo: Repo) -> None:
            repo.modules["pgmod"] = (*repo.modules["pgmod"], "fresh_case")
            repo.replay.append("pgmod::fresh_case")
            repo.pr["sweep"].remove("run_non_pg_filter_replay")

        result, output = Repo().delta(no_replay)
        unrequired = new_findings(output, "new-id-unrequired")
        self.assertEqual(len(unrequired), 1, output)
        self.assertIn("`pgmod::fresh_case`: no PR job selects it", unrequired[0])
        self.assertEqual(result, 1)

    def test_new_id_the_runner_cfg_excludes_is_platform_unproven(self) -> None:
        tail = (
            "#[cfg(test)]\nmod plat {\n"
            "    #[cfg(windows)]\n    #[test]\n    fn windows_only() {}\n"
            "    #[cfg(target_os = \"windows\")]\n    #[test]\n    fn windows_os_only() {}\n"
            "    #[cfg(target_arch = \"riscv64\")]\n    #[test]\n    fn unreadable_arch() {}\n"
            "    #[cfg(unix)]\n    #[test]\n    fn unix_runs() {}\n}\n"
            "#[cfg(all(test, windows))]\nmod win_tests {\n"
            "    #[test]\n    fn inherited_gate() {}\n}\n"
        )
        gated = {"plat::windows_only", "plat::windows_os_only", "plat::unreadable_arch",
                 "win_tests::inherited_gate"}

        def platform_tests(repo: Repo) -> None:
            repo.lib_tail = tail
            repo.extra_ids = gated | {"plat::unix_runs"}

        result, output = Repo().delta(platform_tests)
        flagged = {
            m.group(2) for m in map(NEW_FINDING.match, output.splitlines())
            if m and m.group(1) == "new-id-platform-unproven"
        }
        self.assertEqual(flagged, gated, output)
        self.assertEqual(result, 1)

        def portable(repo: Repo) -> None:
            repo.lib_tail = (
                "#[cfg(test)]\nmod plat {\n    #[cfg(unix)]\n    #[test]\n    fn unix_runs() {}\n"
                "    #[cfg(not(windows))]\n    #[test]\n    fn not_windows() {}\n}\n"
                "#[cfg(all(test, target_os = \"linux\"))]\nmod linux_tests {\n"
                "    #[test]\n    fn on_linux() {}\n}\n"
            )
            repo.extra_ids = {"plat::unix_runs", "plat::not_windows", "linux_tests::on_linux"}

        result, output = Repo().delta(portable)
        self.assertIn("added=3 mapped=3", output)
        self.assertIn("  plat::unix_runs -> sweep (", output)
        self.assertEqual(result, 0, output)

        def excepted(repo: Repo) -> None:
            repo.lib_tail = "#[cfg(test)]\nmod plat {\n    #[cfg(windows)]\n    #[test]\n" \
                            "    fn windows_only() {}\n}\n"
            repo.extra_ids = {"plat::windows_only"}
            repo.id_exceptions = [
                "plat::windows_only | platform-unavailable | #5082 | expires=2099-12-31 | "
                "windows only behaviour"
            ]

        result, output = Repo().delta(excepted)
        self.assertIn("added=1 mapped=0 excepted=1", output)
        self.assertEqual(result, 0, output)

    def test_unevaluated_conditional_attribute_leaves_only_its_id_platform_unproven(self) -> None:
        cases = {
            "tests::new_platform_case": ("lib", "cfg() with an unevaluated cfg_attr", (
                "#[cfg(test)]\nmod tests {\n    #[cfg_attr(unix, cfg(windows))]\n"
                "    #[test]\n    fn new_platform_case() {}\n}\n")),
            "inline_case::tests::new_windows_case": ("inline_case", "cfg(windows)", (
                "#[cfg(test)]\nmod tests {\n    #![cfg(windows)] // Windows only\n"
                "    #[test]\n    fn new_windows_case() {}\n}\n")),
            "same_line::tests::new_token_case": ("same_line", "cfg(windows)", (
                "#[cfg(test)]\nmod tests { #![cfg(windows)]\n"
                "    #[test]\n    fn new_token_case() {}\n}\n")),
        }
        # Controls that must stay MAPPED: a plain test, lint-only cfg_attrs, a quoted inner cfg.
        controls = (
            "#[cfg(test)]\nmod plain_tests {\n    #[test]\n    fn runs_anywhere() {}\n}\n"
            "#[cfg(test)]\nmod lint_tests {\n    #![cfg_attr(not(test), allow(dead_code))]\n"
            "    #[cfg_attr(unix, allow(clippy::all, unused))]\n"
            "    #[test]\n    fn lint_only() {}\n}\n"
        )
        quoted = "#![cfg(any(target_os = \"macos\", target_os = \"linux\"))]\n" \
                 "#[test]\nfn on_linux() {}\n"

        def conditional(repo: Repo) -> None:
            repo.lib_tail = controls + cases["tests::new_platform_case"][2] + "pub mod quoted;\n"
            repo.files["src/quoted.rs"] = quoted
            for home, _, text in list(cases.values())[1:]:
                repo.lib_tail += f"pub mod {home};\n"
                repo.files[f"src/{home}.rs"] = text
            repo.extra_ids = set(cases) | {
                "plain_tests::runs_anywhere", "lint_tests::lint_only", "quoted::on_linux"}

        result, output = Repo().delta(conditional)
        found = {
            NEW_FINDING.match(line).group(2): line
            for line in output.splitlines() if line.startswith("FAIL")
        }
        self.assertEqual(set(found), set(cases), output)
        for case, (_, reason, _) in cases.items():
            self.assertIn(f"new-id-platform-unproven: src/", found[case])
            self.assertIn(f"{reason} is not proven to hold on linux", found[case])
        self.assertEqual(result, 1)

if __name__ == "__main__":
    unittest.main()
