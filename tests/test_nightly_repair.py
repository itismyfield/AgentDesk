"""Nightly #6006: job profiles, tmux and unchanged executable selection."""
from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[1]
NIGHTLY = ROOT / ".github/workflows/ci-nightly.yml"
DEBUG_JOBS = ("full_macos", "full_windows", "rust_build", "postgres_full",
              "relay_authority_mutations_full")
DEBUG_KEYS = ("CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG")
PG_STEP = "cargo test (e2e smoke against PostgreSQL)"
PG_CARGO = "cargo test --test e2e -- --test-threads=1"
NON_PG_CARGO = "cargo test --bins --examples --benches --test '*' -- --skip _pg --skip pg_ --skip postgres"
NATIVE_LIB_STEPS = (("Build and plan non-PG lib tests", "prepare"),
                    ("Run and audit non-PG lib tests", "run"))


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


FILTERS = load_module("cargo_test_filters", ROOT / "scripts/ci/cargo_test_filters.py")


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

    tmux = statements((step_of(jobs, "full_macos", "Install tmux on macOS") or {}).get("run", ""))
    need(any(line.startswith("brew install ") and "tmux" in line.split() for line in tmux),
         "full_macos does not actually brew install tmux")
    need("tmux -V" in tmux, "full_macos prints no tmux provisioning evidence")

    for name, platform in (("full_macos", "macos"), ("full_windows", "windows")):
        run = (step_of(jobs, name, "cargo test (non-PG, non-lib targets)") or {}).get("run", "")
        need(run.strip() == NON_PG_CARGO,
             f"{name} changed its canonical non-PG non-lib sweep")
        for step_name, phase in NATIVE_LIB_STEPS:
            lib = step_of(jobs, name, step_name) or {}
            need(lib.get("run", "").strip() == f"bash scripts/ci/nextest-native.sh {platform} {platform}-lib pg-classifier {phase}",
                 f"{name} stopped running its lib {phase} through the audited native nextest path")
            need("if" not in lib and not lib.get("continue-on-error"), f"{name} made its native lib {phase} conditional")

    pg = step_of(jobs, "postgres_full", PG_STEP) or {}
    lines = [line.strip() for line in pg.get("run", "").splitlines()]
    job_budget = jobs.get("postgres_full", {}).get("timeout-minutes", 0)
    step_budgets = sum(step.get("timeout-minutes", 0) for step in jobs.get("postgres_full", {}).get("steps", []))
    need(job_budget > step_budgets, "the PG job cannot fit its bounded steps plus setup and cleanup")
    need(lines.count(PG_CARGO) == 1, "the e2e cargo statement is not one direct line")
    return problems


def filter_rule_commands(text: str) -> list[str]:
    """The `cargo test` commands the no-name-filter rule judges in a nightly workflow text."""
    steps = [step for job in yaml.safe_load(text)["jobs"].values() for step in job.get("steps", [])]
    return [" ".join(words) for step in steps if isinstance(step.get("run"), str)
            for words in FILTERS.commands(step["run"])]


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
     lambda t: t.replace("brew install tmux",
                         "brew install pkg-config\n          # tmux")),
    ("removes the tmux version evidence",
     lambda t: t.replace("          tmux -V\n", "")),
    ("leaves no PostgreSQL job budget for cleanup",
     lambda t: t.replace("  postgres_full:\n    name: PostgreSQL full tests\n    runs-on: ubuntu-latest\n    timeout-minutes: 90",
                         "  postgres_full:\n    name: PostgreSQL full tests\n    runs-on: ubuntu-latest\n    timeout-minutes: 30", 1)),
    ("replaces the native audited run",
     lambda t: t.replace("bash scripts/ci/nextest-native.sh windows windows-lib pg-classifier run",
                         "cargo nextest run --lib", 1)),
    ("narrows the native run to a filter",
     lambda t: t.replace("macos macos-lib pg-classifier run", "macos macos-lib relay_recovery run", 1)),
    ("skips the native run",
     lambda t: t.replace("        run: bash scripts/ci/nextest-native.sh macos macos-lib pg-classifier run",
                         "        if: false\n        run: bash scripts/ci/nextest-native.sh macos macos-lib pg-classifier run", 1)),
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

    def test_filter_rule_sees_the_direct_cargo_command(self) -> None:
        self.assertIn(PG_CARGO, filter_rule_commands(self.text))

    def test_wrapping_cargo_does_not_hide_it_from_the_filter_rule(self) -> None:
        for name, mutate in WRAPPER_MUTANTS:
            with self.subTest(mutant=name):
                mutated = mutate(self.text)
                self.assertNotEqual(mutated, self.text)
                self.assertIn(PG_CARGO, filter_rule_commands(mutated))

    def test_each_job_profile_entry_is_required(self) -> None:
        for job in DEBUG_JOBS:
            for key in DEBUG_KEYS:
                with self.subTest(job=job, key=key):
                    document = yaml.safe_load(self.text)
                    del document["jobs"][job]["env"][key]
                    self.assertNotEqual(
                        nightly_contract_problems(yaml.safe_dump(document)), [])


if __name__ == "__main__":
    unittest.main()
