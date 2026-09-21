"""Nightly #6006: job profiles, tmux and unchanged executable selection."""
from __future__ import annotations

import importlib.util
import re
import sys
import tempfile
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[1]
NIGHTLY = ROOT / ".github/workflows/ci-nightly.yml"
DEBUG_JOBS = ("full_macos", "full_windows", "postgres_full")
DEBUG_KEYS = ("CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG")
PG_STEP = "cargo test (PostgreSQL bootstrap and routes)"
WINDOWS_STEP = "Discord thread-create cross-process lock"
WINDOWS_COMMAND = "cargo test --lib discord_thread_create -- --test-threads=1"
PG_CARGO = ('cargo test --all-targets -- "${PG_INCLUDE_ARGS[@]}" '
            "--nocapture --test-threads=1")
NON_PG_CARGO = 'cargo test --all-targets -- "${NON_PG_SKIP_ARGS[@]}"'
FILTER_SOURCE = "source scripts/ci/non-pg-test-filter.sh"


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


MEMBERSHIP = load_module("check_pg_test_lane_membership",
                         ROOT / "scripts/check_pg_test_lane_membership.py")
INTEGRITY = load_module("check_test_target_integrity",
                        ROOT / "scripts/check_test_target_integrity.py")


def workflow_triggers(document: dict) -> dict:
    """YAML 1.1 resolves a bare `on` key to True; accept that deliberately."""
    found = [value for key, value in document.items() if key is True or key == "on"]
    if len(found) != 1:
        raise AssertionError(f"expected one trigger mapping, found {len(found)}")
    return found[0]


def step_of(jobs: dict, job: str, name: str) -> dict | None:
    for step in jobs.get(job, {}).get("steps", []):
        if step.get("name") == name:
            return step
    return None


def statements(run: str) -> list[str]:
    return [line.strip() for line in run.splitlines()
            if line.strip() and not line.strip().startswith("#")]


def nightly_contract_problems(text: str) -> list[str]:
    """Every structural rule N1 owns, as data a mutant test can falsify."""
    problems: list[str] = []

    def need(condition: object, message: str) -> None:
        if not condition:
            problems.append(message)

    document = yaml.safe_load(text)
    jobs = document["jobs"]
    carrying = sorted(name for name, job in jobs.items()
                      if all((job.get("env") or {}).get(key) == "0" for key in DEBUG_KEYS))
    need(carrying == sorted(DEBUG_JOBS), f"job-level debug profiles cover {carrying}")
    for name in DEBUG_JOBS:
        for step in jobs.get(name, {}).get("steps", []):
            for key in DEBUG_KEYS:
                value = (step.get("env") or {}).get(key)
                need(value in (None, "0"), f"{name}/{step.get('name')!r} overrides {key}")

    windows = step_of(jobs, "full_windows", WINDOWS_STEP) or {}
    env = windows.get("env") or {}
    need(windows, "full_windows lost its focused Discord step")
    need(env.get("BASH_ENV") == "/dev/null", "the Discord step lost its BASH_ENV")
    need(all(env.get(key) == "0" for key in DEBUG_KEYS),
         "the Discord step lost its explicit debug env")
    need(windows.get("run", "").strip() == WINDOWS_COMMAND,
         "the Discord step changed its exact command")
    need(not windows.get("continue-on-error"), "the Discord step became advisory")

    opus = statements((step_of(jobs, "full_macos", "Install Opus on macOS") or {}).get("run", ""))
    need(any(line.startswith("brew install ") and "tmux" in line.split() for line in opus),
         "full_macos does not actually brew install tmux")
    need("tmux -V" in opus, "full_macos prints no tmux provisioning evidence")

    for name in ("full_macos", "full_windows"):
        run = (step_of(jobs, name, "cargo test (non-PG)") or {}).get("run", "")
        need(FILTER_SOURCE in run and NON_PG_CARGO in run,
             f"{name} changed its canonical non-PG sweep")
        need("run_non_pg_filter_false_positives" in run,
             f"{name} stopped replaying the non-PG false positives")

    pg = step_of(jobs, "postgres_full", PG_STEP) or {}
    lines = [line.strip() for line in pg.get("run", "").splitlines()]
    env = pg.get("env") or {}
    need(pg.get("timeout-minutes") == 30, "the PG cargo step lost its 30-minute budget")
    need(lines.count(PG_CARGO) == 1, "the PG cargo statement is not one direct line")
    need(FILTER_SOURCE in lines, "the PG step stopped sourcing the canonical filter")
    return problems


def integrity_commands(text: str) -> list[str]:
    with tempfile.NamedTemporaryFile("w", suffix=".yml", delete=False) as handle:
        handle.write(text)
        path = Path(handle.name)
    try:
        return [rendered for _, _, rendered in INTEGRITY.extract_commands(path)]
    finally:
        path.unlink()


def membership_commands(text: str) -> list[str]:
    job = re.search(r"(?ms)^  postgres_full:\n.*?(?=^  [A-Za-z0-9_-]+:\n)", text)
    return MEMBERSHIP._cargo_commands(
        job.group(0), MEMBERSHIP.load_non_pg_skip_args(ROOT))


def override_pg_profile(text):
    document = yaml.safe_load(text)
    step = step_of(document["jobs"], "postgres_full", PG_STEP)
    step.setdefault("env", {})["CARGO_PROFILE_TEST_DEBUG"] = "1"
    return yaml.safe_dump(document)


STRUCTURAL_MUTANTS = (
    ("drops one job-level debug entry",
     lambda t: t.replace('    env:\n      CARGO_PROFILE_DEV_DEBUG: "0"\n'
                         '      CARGO_PROFILE_TEST_DEBUG: "0"\n', "", 1)),
    ("overrides a debug profile on the cargo step", override_pg_profile),
    ("moves the profiles to an unrelated job",
     lambda t: t.replace("  cli_smoke:\n    name: CLI smoke\n",
                         "  cli_smoke:\n    name: CLI smoke\n    env:\n"
                         '      CARGO_PROFILE_DEV_DEBUG: "0"\n'
                         '      CARGO_PROFILE_TEST_DEBUG: "0"\n')),
    ("strips tmux from the brew install",
     lambda t: t.replace("brew install opus pkg-config tmux",
                         "brew install opus pkg-config\n          # tmux")),
    ("removes the tmux version evidence",
     lambda t: t.replace("          tmux -V\n", "")),
    ("drops the focused Discord debug env",
     lambda t: t.replace('          CARGO_PROFILE_DEV_DEBUG: "0"\n'
                         '          CARGO_PROFILE_TEST_DEBUG: "0"\n'
                         "        run: cargo test --lib discord_thread_create",
                         "        run: cargo test --lib discord_thread_create")),
    ("narrows the PostgreSQL target selection",
     lambda t: t.replace(PG_CARGO, PG_CARGO.replace("--all-targets", "--lib"))),
    ("shortens the PostgreSQL step budget",
     lambda t: t.replace("        timeout-minutes: 30", "        timeout-minutes: 20")),
    ("drops the non-PG false-positive replay",
     lambda t: t.replace("          run_non_pg_filter_false_positives\n", "", 1)),
)

WRAPPER_MUTANTS = (
    ("python wrapper", lambda t: t.replace(
        PG_CARGO, f"python3 scripts/ci/pg-resource-diagnostics.py wrap -- {PG_CARGO}")),
    ("array wrapper", lambda t: t.replace(
        PG_CARGO, f'PG_ARGV=({PG_CARGO})\n          "${{PG_ARGV[@]}}"')),
)


class NightlyRepairWiringTests(unittest.TestCase):
    def setUp(self) -> None:
        self.text = NIGHTLY.read_text(encoding="utf-8")

    def test_workflow_satisfies_the_repair_contract(self) -> None:
        self.assertEqual(nightly_contract_problems(self.text), [])

    def test_structural_mutants_are_rejected(self) -> None:
        for name, mutate in STRUCTURAL_MUTANTS:
            with self.subTest(mutant=name):
                mutated = mutate(self.text)
                self.assertNotEqual(mutated, self.text, "mutation did not apply")
                self.assertIsInstance(yaml.safe_load(mutated), dict)
                self.assertNotEqual(nightly_contract_problems(mutated), [])

    def test_both_extractors_still_see_the_direct_cargo_command(self) -> None:
        rendered = "cargo test --all-targets -- ${PG_INCLUDE_ARGS[@]} --nocapture --test-threads=1"
        self.assertIn(rendered, integrity_commands(self.text))
        self.assertIn(
            "cargo test --all-targets -- _pg pg_ postgres --nocapture --test-threads=1",
            membership_commands(self.text))

    def test_wrapping_cargo_silently_loses_target_integrity_coverage(self) -> None:
        rendered = "cargo test --all-targets -- ${PG_INCLUDE_ARGS[@]} --nocapture --test-threads=1"
        for name, mutate in WRAPPER_MUTANTS:
            with self.subTest(mutant=name):
                mutated = mutate(self.text)
                self.assertNotEqual(mutated, self.text)
                # The target-integrity inventory loses the command entirely,
                # while membership still finds a `cargo test` substring: a green
                # exit status on either gate would hide the lost coverage.
                self.assertNotIn(rendered, integrity_commands(mutated))
                self.assertTrue(membership_commands(mutated))


    def test_each_of_the_six_profile_entries_is_required(self) -> None:
        for job in DEBUG_JOBS:
            for key in DEBUG_KEYS:
                with self.subTest(job=job, key=key):
                    document = yaml.safe_load(self.text)
                    del document["jobs"][job]["env"][key]
                    self.assertNotEqual(
                        nightly_contract_problems(yaml.safe_dump(document)), [])


if __name__ == "__main__":
    unittest.main()
