"""Contracts for the PR test selector (scripts/ci/test-selection.py).

A selection that is too narrow is a false green, so each rule here is pinned by the
case it exists for: the changed file, its siblings and its literal readers are
selected; an empty, erroring or 40%-wide selection runs the lane in full; a plan
that does not reproduce, an active shard with nothing to run and an empty
expectation are red; main never consumes a plan.
"""

from __future__ import annotations

import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import yaml

REPO_ROOT = Path(__file__).resolve().parents[1]
SELECTOR = REPO_ROOT / "scripts/ci/test-selection.py"
RUN_TEST_LANE = REPO_ROOT / "scripts/run_test_lane.py"
PR_WORKFLOW = REPO_ROOT / ".github/workflows/ci-pr.yml"
MAIN_WORKFLOW = REPO_ROOT / ".github/workflows/ci-main.yml"


def load_selector():
    spec = importlib.util.spec_from_file_location("test_selection_under_test", SELECTOR)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ts = load_selector()
ts.tools()
INTEGRITY = sys.modules["check_test_target_integrity"]

ALPHA = ["alpha::changed::tests::own", "alpha::changed::tests::own_pg",
         "alpha::changed::tests::pg_named_but_free", "alpha::sibling::tests::near"]
BETA = ["beta::far::tests::far", "beta::reader::tests::reads", "beta::reader::tests::reads_pg"]
GAMMA = [f"gamma::tests::g{index:02}" for index in range(40)] + [
    f"gamma::tests::p{index:02}_pg" for index in range(12)]
HOOK = ["services::discord::router::intake_gate::busy_inject::tests::hook",
        "services::discord::router::intake_gate::busy_inject::tests::hook_pg",
        "services::discord::tui_prompt_relay::relay_e2e::busy_inject_e2e_tests::e2e"]
SOURCES = {
    "src/lib.rs": "pub mod alpha;\npub mod beta;\npub mod gamma;\npub mod services;\n",
    "src/alpha/mod.rs": "pub mod changed;\npub mod sibling;\n",
    "src/alpha/changed.rs": "pub fn thing() {}\n#[cfg(test)]\nmod tests {}\n",
    "src/alpha/sibling.rs": "#[cfg(test)]\nmod tests {}\n",
    "src/beta/mod.rs": "pub mod far;\npub mod reader;\n",
    "src/beta/reader.rs": "use crate::alpha::changed::thing;\n#[cfg(test)]\nmod tests {}\n",
    "src/beta/far/mod.rs": "#[cfg(test)]\nmod tests {}\n",
    "src/gamma/mod.rs": "// crate::alpha::changed_elsewhere is another module\n#[cfg(test)]\nmod tests {}\n",
    "src/delta/lonely.rs": "pub fn alone() {}\n",
    "src/services/discord/router/intake_gate/busy_inject.rs": "#[cfg(test)]\nmod tests {}\n",
    "src/services/discord/tui_prompt_relay/relay_e2e/busy_inject_e2e_tests.rs":
        "use super::super::busy_inject_support;\n",
}
FILTER = """NON_PG_SKIP_ARGS=(
  --skip _pg
  --skip pg_
  --skip postgres
)
NON_PG_FILTER_REPLAY=(
  alpha::changed::tests::pg_named_but_free
)
"""


def git(root: Path, *args: str) -> str:
    return subprocess.run(["git", "-C", str(root), "-c", "user.name=t", "-c", "user.email=t@e",
                           *args], check=True, capture_output=True, text=True).stdout.strip()


def manifest(ids) -> str:
    rows = [INTEGRITY.LIB_INVENTORY_MANIFEST_HEADER, *INTEGRITY.LIB_INVENTORY_MANIFEST_RULES,
            "[tests]", *sorted(set(ids), key=lambda value: value.encode())]
    return "\n".join(rows) + "\n"


class Fixture:
    """A git repository with a tiny crate, a lib manifest and the shared non-PG filter."""

    def __init__(self, directory: str):
        self.root = Path(directory)
        self.write({**SOURCES, "scripts/ci/non-pg-test-filter.sh": FILTER,
                    "scripts/lib_test_inventory_manifest.txt": manifest(ALPHA + BETA + GAMMA + HOOK)})
        git(self.root, "init", "-q")
        self.base = self.commit("base")

    def write(self, files: dict[str, str]) -> None:
        for name, text in files.items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, "utf-8")

    def commit(self, message: str, files: dict[str, str] | None = None) -> str:
        self.write(files or {})
        git(self.root, "add", "-A")
        git(self.root, "commit", "-q", "--allow-empty", "-m", message)
        return git(self.root, "rev-parse", "HEAD")

    def plan(self, files: dict[str, str], ids=None) -> dict:
        if ids is not None:
            files = {**files, "scripts/lib_test_inventory_manifest.txt": manifest(ids)}
        head = self.commit("change", files)
        with mock.patch.dict(os.environ, {"ADK_CI_SELECT": "", "GITHUB_EVENT_PATH": ""}):
            return ts.make_plan(self.root, self.base, head)

    def selected(self, plan: dict, lane: str) -> list[str]:
        return ts.Inventory(self.root).selected(plan["scopes"], lane)


class SelectionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.repo = Fixture(self.tmp.name)

    def test_changed_file_siblings_and_literal_readers_are_selected(self) -> None:
        plan = self.repo.plan({"src/alpha/changed.rs": SOURCES["src/alpha/changed.rs"] + "// edit\n"})
        self.assertEqual([plan["lanes"][lane]["mode"] for lane in ts.LANES], ["selected"] * 2)
        self.assertEqual(self.repo.selected(plan, "pg"), [
            "alpha::changed::tests::own_pg", "alpha::changed::tests::pg_named_but_free",
            "beta::reader::tests::reads_pg"])
        # The replayed false positive also joins the sweep; a reader's subtree is one hop only.
        self.assertEqual(self.repo.selected(plan, "sweep"), [
            "alpha::changed::tests::own", "alpha::changed::tests::pg_named_but_free",
            "alpha::sibling::tests::near", "beta::reader::tests::reads"])
        self.assertNotIn("src/gamma/*", plan["scopes"])

    def test_new_pg_and_non_pg_tests_in_a_changed_directory_are_selected(self) -> None:
        added = ["alpha::fresh::tests::brand_new", "alpha::fresh::tests::brand_new_pg"]
        plan = self.repo.plan({"src/alpha/fresh.rs": "#[cfg(test)]\nmod tests {}\n"},
                              ids=ALPHA + BETA + GAMMA + HOOK + added)
        self.assertIn(added[1], self.repo.selected(plan, "pg"))
        self.assertIn(added[0], self.repo.selected(plan, "sweep"))

    def test_an_empty_selection_runs_both_lanes_in_full(self) -> None:
        plan = self.repo.plan({"src/delta/lonely.rs": "pub fn alone() { let _ = 1; }\n"})
        for lane in ts.LANES:
            self.assertEqual((plan["lanes"][lane]["mode"], plan["lanes"][lane]["reason"]),
                             ("full", "empty-selection"))
        self.assertEqual(plan["lanes"]["pg"]["shards"], list(range(ts.tools()[2].PG_SHARD_COUNT)))

    def test_forty_percent_is_full_and_just_below_is_selected(self) -> None:
        population = [f"t{index}" for index in range(10)]
        self.assertEqual(ts.verdict(population[:4], population), "ratio 0.40 >= 0.4")
        self.assertEqual(ts.verdict(population[:3], population), "")
        self.assertEqual(ts.verdict([], population), "empty-selection")
        plan = self.repo.plan({"src/gamma/mod.rs": SOURCES["src/gamma/mod.rs"] + "// edit\n"})
        self.assertTrue(plan["lanes"]["sweep"]["reason"].startswith("ratio "), plan["lanes"])
        self.assertEqual(plan["lanes"]["sweep"]["mode"], "full")

    def test_a_selector_error_runs_every_lane_in_full_with_its_reason(self) -> None:
        plan = self.repo.plan({"scripts/lib_test_inventory_manifest.txt": "not a manifest\n",
                               "src/alpha/changed.rs": "// edit\n"})
        for lane in ts.LANES:
            self.assertEqual(plan["lanes"][lane]["mode"], "full")
            self.assertTrue(plan["lanes"][lane]["reason"].startswith("selector-error: "))

    def test_full_triggers_and_include_owners_run_full(self) -> None:
        inv = ts.Inventory(self.repo.root)
        for path in (".cargo/config.toml", ".github/actions/ensure-swap/action.yml",
                     "scripts/ci/non-pg-test-filter.sh", "tests/fixtures/pg_lane/x.json",
                     "scripts/pg_test_lane_manifest.txt", "scripts/known_test_failures.txt",
                     "migrations/0001.sql", "src/config/test_env/teardown_probe.rs", "justfile"):
            with self.subTest(path=path):
                self.assertEqual(ts.scopes(inv, [path], []), (set(), f"full-trigger: {path}"))
        site = ts.tools()[3].Site("src/alpha/changed.rs", 3, "include", "src/alpha/body.rs", "")
        self.assertEqual(ts.scopes(inv, ["src/alpha/body.rs"], [site])[1],
                         "full-trigger: src/alpha/body.rs")
        # High-churn inputs the lanes never read stay selectable.
        for path in ("scripts/lib_test_inventory_manifest.txt", "tests/test_x.py",
                     "scripts/test_lane_required_id_exceptions.txt"):
            with self.subTest(path=path):
                self.assertEqual(ts.scopes(inv, [path], [])[1], "")

    def test_the_reexported_busy_inject_hook_runs_full_although_its_selection_is_narrow(self) -> None:
        hook = "src/services/discord/router/intake_gate/busy_inject.rs"
        inv = ts.Inventory(self.repo.root)
        self.assertEqual(ts.scopes(inv, [hook], [])[1], f"full-trigger: {hook}")
        with mock.patch.object(ts, "FULL_TRIGGERS", tuple(g for g in ts.FULL_TRIGGERS if g != hook)):
            found, reason = ts.scopes(inv, [hook], [])
        ids = inv.selected(found, "sweep")
        # Without the entry the selection is narrow and misses the e2e consumer.
        self.assertEqual((reason, ts.verdict(ids, inv.population["sweep"])), ("", ""))
        self.assertNotIn(HOOK[2], ids)

    def test_kill_switch_variable_and_label_run_both_lanes_in_full(self) -> None:
        head = self.repo.commit("change", {"src/alpha/changed.rs": "// edit\n"})
        event = Path(self.tmp.name) / "event.json"
        event.write_text(json.dumps({"pull_request": {"labels": [{"name": "ci-full"}]}}))
        for env, reason in (({"ADK_CI_SELECT": "OFF", "GITHUB_EVENT_PATH": ""},
                             "kill-switch: ADK_CI_SELECT=off"),
                            ({"ADK_CI_SELECT": "", "GITHUB_EVENT_PATH": str(event)},
                             "kill-switch: label ci-full")):
            with self.subTest(reason=reason), mock.patch.dict(os.environ, env):
                plan = ts.make_plan(self.repo.root, self.repo.base, head)
                self.assertEqual({(e["mode"], e["reason"]) for e in plan["lanes"].values()},
                                 {("full", reason)})

    def test_a_checkout_without_a_merge_parent_runs_full(self) -> None:
        head = self.repo.commit("change", {"src/alpha/changed.rs": "// edit\n"})
        with mock.patch.dict(os.environ, {"ADK_CI_SELECT": "", "GITHUB_EVENT_PATH": ""}):
            plan = ts.make_plan(self.repo.root, None, head)
        self.assertEqual(plan["lanes"]["pg"]["reason"], "no-merge-parent")

    def test_only_active_shards_enter_the_matrix(self) -> None:
        pgm = ts.tools()[2]
        ids = ["alpha::changed::tests::own_pg"]
        self.assertEqual(ts.shards(ids, False), [pgm.pg_test_shard(ids[0])])
        self.assertEqual(ts.shards(ids, True), list(range(pgm.PG_SHARD_COUNT)))


class ReplayTests(unittest.TestCase):
    """`run` replays the plan from the same checkout and refuses one it cannot reproduce."""

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.repo = Fixture(self.tmp.name)
        self.plan = self.repo.plan({"src/alpha/changed.rs": "// edit\n"})

    def load(self, plan, lane="pg"):
        with mock.patch.dict(os.environ, {ts.PLAN_ENV: json.dumps(plan) if plan else ""}), \
                mock.patch.object(ts, "log"):
            return ts.load_plan(self.repo.root, lane)

    def test_the_plan_reproduces_and_a_tampered_or_missing_one_is_red(self) -> None:
        entry, ids = self.load(self.plan)
        self.assertEqual(ids, self.repo.selected(self.plan, "pg"))
        tampered = json.loads(json.dumps(self.plan))
        tampered["scopes"] = ["src/gamma/**"]
        stale = {**self.plan, "sha": "0" * 40}
        for plan in (tampered, stale, None):
            with self.subTest(plan=bool(plan)), self.assertRaises(ValueError):
                self.load(plan)

    def test_an_active_shard_with_no_planned_test_or_an_uncompiled_id_is_red(self) -> None:
        pgm = ts.tools()[2]
        _, ids = self.load(self.plan)
        shard = pgm.pg_test_shard(ids[0])
        mine = [test_id for test_id in ids if pgm.pg_test_shard(test_id) == shard]
        env = {"PG_INCLUDE_SHARD": str(shard), "POSTGRES_TEST_DATABASE_URL_BASE": "postgres://x"}
        listed = subprocess.CompletedProcess([], 0, "--exact\n" + "\n".join(mine) + "\n", "")
        ran = subprocess.CompletedProcess([], 0, "", "")
        with mock.patch.dict(os.environ, env), mock.patch.object(
                ts.subprocess, "run", side_effect=[listed, ran]) as run:
            self.assertEqual(ts.run_pg(self.repo.root, ids), 0)
        self.assertEqual(run.call_args.args[0][-len(mine):], mine)
        self.assertNotIn("PG_INCLUDE_SHARD", run.call_args.kwargs["env"])
        missing = subprocess.CompletedProcess([], 0, "--exact\n", "")
        with mock.patch.dict(os.environ, env), mock.patch.object(
                ts.subprocess, "run", return_value=missing), self.assertRaises(ValueError):
            ts.run_pg(self.repo.root, ids)
        other = {**env, "PG_INCLUDE_SHARD": str(1 - shard)}
        empty = [test_id for test_id in ids if pgm.pg_test_shard(test_id) == shard]
        with mock.patch.dict(os.environ, other), self.assertRaises(ValueError):
            ts.run_pg(self.repo.root, empty)


    def test_full_mode_runs_mains_commands_and_plan_writes_both_outputs(self) -> None:
        full = json.loads(json.dumps(self.plan))
        for entry in full["lanes"].values():
            entry["mode"] = "full"
        commands = {"pg": [*ts.FULL_PG], "sweep": ["bash", "-eo", "pipefail", "-c", ts.FULL_SWEEP]}
        real, ran = subprocess.run, []

        def fake(command, *args, **kwargs):  # git still answers; the lane command is recorded
            if command[0] == "git":
                return real(command, *args, **kwargs)
            ran.append(command)
            return subprocess.CompletedProcess(command, 3)

        for lane, command in commands.items():
            with self.subTest(lane=lane), mock.patch.object(ts, "ROOT", self.repo.root), \
                    mock.patch.dict(os.environ, {ts.PLAN_ENV: json.dumps(full)}), \
                    mock.patch.object(ts, "log"), mock.patch.object(ts.subprocess, "run", fake):
                self.assertEqual(ts.main(["run", "--lane", lane]), 3)
                self.assertEqual(ran.pop(), command)
        output = Path(self.tmp.name) / "github-output"
        head = git(self.repo.root, "rev-parse", "HEAD")
        with mock.patch.dict(os.environ, {"GITHUB_OUTPUT": str(output), "ADK_CI_SELECT": "",
                                          "GITHUB_EVENT_PATH": ""}), mock.patch.object(ts, "log"):
            self.assertEqual(ts.main(["plan", "--root", self.tmp.name, "--base", self.repo.base,
                                      "--head", head]), 0)
        lines = dict(line.split("=", 1) for line in output.read_text().splitlines())
        self.assertEqual(json.loads(lines["matrix"]), {"shard": self.plan["lanes"]["pg"]["shards"]})
        self.assertEqual(json.loads(lines["plan"])["lanes"], self.plan["lanes"])


TRANSCRIPT = """running {count} tests
{lines}
test result: ok. {count} passed; 0 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.00s
"""


class ExpectedIdsTests(unittest.TestCase):
    """run_test_lane --expected-ids: the plan's set replaces the manifest's, still exactly."""

    def run_lane(self, expected: list[str], reported: list[str], ledger: str = "") -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as directory:
            ids = Path(directory) / "ids"
            ids.write_text("\n".join(expected) + "\n")
            ledger_path = Path(directory) / "ledger"
            ledger_path.write_text(ledger)
            text = TRANSCRIPT.format(count=len(reported), lines="\n".join(
                f"test {test_id} ... ok" for test_id in reported))
            result = subprocess.run(
                [sys.executable, str(RUN_TEST_LANE), "--lane", "non-pg-sweep", "--expected-ids",
                 str(ids), "--ledger", str(ledger_path), "--", sys.executable, "-c",
                 f"import sys; sys.stdout.write({text!r})"], capture_output=True, text=True)
            return result.returncode, result.stderr

    def test_an_empty_plan_or_a_missing_id_is_red_and_the_exact_set_is_green(self) -> None:
        self.assertEqual(self.run_lane(["a::b::c"], ["a::b::c"])[0], 0)
        rc, stderr = self.run_lane([], [])
        self.assertEqual(rc, 1)
        self.assertIn("derived expectation is empty", stderr)
        rc, stderr = self.run_lane(["a::b::c", "a::b::d"], ["a::b::c"])
        self.assertEqual(rc, 1)
        self.assertIn("a::b::d", stderr)

    def test_declared_ignored_ids_outside_the_plan_are_not_stale(self) -> None:
        ledger = ("ignored | x::not::planned | lane=non-pg-sweep | #1 | a dormant fixture that "
                  "this lane keeps ignored until its backing service exists in CI\n")
        rc, stderr = self.run_lane(["a::b::c"], ["a::b::c"], ledger)
        self.assertEqual(rc, 0, stderr)


def glob_regex(pattern: str) -> re.Pattern[str]:
    return re.compile("".join(".*" if part == "**" else re.escape(part).replace(r"\*", "[^/]*")
                              for part in re.split(r"(\*\*)", pattern)) + "$")


class WorkflowGateTests(unittest.TestCase):
    """P2-1: every full-run trigger reaches the selector; mirrors read the job's own gate."""

    def setUp(self) -> None:
        self.text = PR_WORKFLOW.read_text("utf-8")
        self.jobs = yaml.safe_load(self.text)["jobs"]

    def test_full_triggers_enter_both_lanes_and_mirrors_read_the_same_gate(self) -> None:
        block = self.text.split("            rust_or_policy:\n", 1)[1].split("            # ", 1)[0]
        patterns = re.findall(r"- '([^']+)'", block)
        outputs = self.jobs["changes"]["outputs"]
        self.assertIn("steps.filter.outputs.rust_or_policy == 'true'", outputs["pg_db"])
        for path in (".cargo/config.toml", ".github/actions/ensure-swap/action.yml",
                     "scripts/ci/test-selection.py", "tests/fixtures/pg_lane/x.json",
                     "scripts/pg_test_lane_manifest.txt", "scripts/known_test_failures.txt",
                     "scripts/lib_test_inventory_manifest.txt"):
            with self.subTest(path=path):
                self.assertTrue(any(glob_regex(p).match(path) for p in patterns), path)
        gates = {
            "test_fast": "${{ !cancelled() && needs.changes.outputs.pg_db == 'true' }}",
            "library_sweep": "${{ !cancelled() && needs.changes.outputs.rust_tests == 'true' }}",
            "pg_shards": "needs.changes.outputs.pg_db == 'true' || needs.changes.outputs.rust_tests == 'true'",
        }
        for job, gate in gates.items():
            self.assertEqual(self.jobs[job]["if"], gate)
        for mirror, (upstream, name) in {
                "fast_targeted_tests_required_context": ("test_fast", "pg_db"),
                "library_sweep_required_context": ("library_sweep", "rust_tests")}.items():
            env = next(step["env"] for step in self.jobs[mirror]["steps"]
                       if step.get("env", {}).get("UPSTREAM_JOB_NAME") == upstream)
            self.assertEqual((env["FILTER_NAME"], env["FILTER_OUTPUT"]),
                             (name, f"${{{{ needs.changes.outputs.{name} }}}}"))

    def test_the_kill_switch_is_read_once_and_main_never_consumes_a_plan(self) -> None:
        self.assertEqual(self.text.count("vars.ADK_CI_SELECT"), 1)
        self.assertEqual(self.jobs["pg_shards"]["steps"][1]["env"],
                         {"ADK_CI_SELECT": "${{ vars.ADK_CI_SELECT }}"})
        main = MAIN_WORKFLOW.read_text("utf-8")
        self.assertNotIn("test-selection", main)
        self.assertNotIn(ts.PLAN_ENV, main)


if __name__ == "__main__":
    unittest.main()
