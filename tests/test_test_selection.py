"""Contracts for the PR test selector (scripts/ci/test-selection.py).

A selection that is too narrow is a false green, so each rule here is pinned by the
case it exists for: the changed file, its siblings and its literal readers are
selected; a full-run authority, an unresolved inclusion or module, the kill switch,
an empty, erroring or 40%-wide selection runs the lane in full; a plan that does not
reproduce, a planned id that did not run and an empty expectation are red; shard 0
always runs the continuity recipe; main never consumes a plan.
"""

from __future__ import annotations

import importlib.util
import json
import os
import re
import shutil
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
PGM = ts.tools()[2]
Site = ts.tools()[3].Site

ALPHA = ["alpha::changed::tests::own", "alpha::changed::tests::own_pg",
         "alpha::changed::tests::pg_named_but_free", "alpha::sibling::tests::near"]
BETA = ["beta::far::tests::far", "beta::reader::tests::reads", "beta::reader::tests::reads_pg"]
GAMMA = [f"gamma::tests::g{index:02}" for index in range(40)] + [
    f"gamma::tests::p{index:02}_pg" for index in range(12)]
HOOK = ["services::discord::router::intake_gate::busy_inject::tests::hook",
        "services::discord::router::intake_gate::busy_inject::tests::hook_pg",
        "services::discord::tui_prompt_relay::relay_e2e::busy_inject_e2e_tests::e2e"]
SOURCES = {
    "src/lib.rs": "pub mod alpha;\npub mod beta;\npub mod delta;\npub mod gamma;\npub mod services;\n",
    "src/alpha/mod.rs": 'pub mod changed;\npub mod sibling;\n#[path = "aliased/real.rs"]\npub mod renamed;\n',
    "src/alpha/changed.rs": "pub fn thing() {}\n#[cfg(test)]\nmod tests {}\n",
    "src/alpha/sibling.rs": "#[cfg(test)]\nmod tests {}\n",
    "src/alpha/aliased/real.rs": "pub fn real() {}\n",
    "src/alpha/orphan.rs": "pub fn nobody_declares_this() {}\n",
    "src/beta/mod.rs": "pub mod far;\npub mod reader;\n",
    "src/beta/reader.rs": "use crate::alpha::changed::thing;\n#[cfg(test)]\nmod tests {}\n",
    "src/beta/far/mod.rs": "#[cfg(test)]\nmod tests {}\n",
    "src/gamma/mod.rs": "// crate::alpha::changed_elsewhere is another module\n#[cfg(test)]\nmod tests {}\n",
    "src/delta/mod.rs": "pub mod lonely;\n",
    "src/delta/lonely.rs": "pub fn alone() {}\n",
    "src/services/mod.rs": "pub mod discord;\n",
    "src/services/discord/mod.rs": "pub mod router;\npub mod tui_prompt_relay;\n",
    "src/services/discord/router/mod.rs": "pub mod intake_gate;\n",
    "src/services/discord/router/intake_gate/mod.rs": "pub mod busy_inject;\n",
    "src/services/discord/router/intake_gate/busy_inject.rs": "#[cfg(test)]\nmod tests {}\n",
    "src/services/discord/tui_prompt_relay/mod.rs": "pub mod relay_e2e;\n",
    "src/services/discord/tui_prompt_relay/relay_e2e/mod.rs": "mod busy_inject_e2e_tests;\n",
    "src/services/discord/tui_prompt_relay/relay_e2e/busy_inject_e2e_tests.rs":
        "use super::super::busy_inject_support;\n",
}
INCLUDER = "\n".join((
    'const DATA: &str = include_str!("../alpha/data.json");',
    'const GEN: &str = include!(concat!(env!("OUT_DIR"), "/gen.rs"));',
    "use std::include as splice;",
    "fn f(include: bool) -> bool { include }",
)) + "\n"
FILTER = """NON_PG_SKIP_ARGS=(
  --skip _pg
  --skip pg_
  --skip postgres
)
NON_PG_FILTER_REPLAY=(
  alpha::changed::tests::pg_named_but_free
)
"""
# The section 4 authorities: each must run both lanes in full, and every FULL_TRIGGERS
# entry must be the one that makes some row full.
AUTHORITIES = (
    "migrations/0001.sql", "build.rs", "Cargo.toml", "Cargo.lock", ".cargo/config.toml",
    "rust-toolchain.toml", "justfile", ".github/workflows/ci-pr.yml",
    ".github/actions/ensure-swap/action.yml", "tests/test_x.py", "tests/run_x.sh",
    "tests/fixtures/pg_lane/x.json", "scripts/ci/non-pg-test-filter.sh",
    "scripts/ci-script-checks.sh", "scripts/ci-timeout.py", "scripts/rust_lex.py",
    "scripts/lib_test_inventory_manifest.txt", "scripts/pg_test_lane_manifest.txt",
    "scripts/known_test_failures.txt", "scripts/run_test_lane.py",
    "scripts/check_pg_test_lane_membership.py", "scripts/check_test_target_integrity.py",
    "scripts/check_test_lane_coverage.py", "scripts/lane_source_parity.py",
    "scripts/lane_new_id_registration.py", "scripts/check-ci-runner-hardening.sh",
    "scripts/test_lane_required_id_exceptions.txt", "scripts/test_target_integrity_allowlist.txt",
    "src/db/mod.rs", "src/db/postgres.rs", "src/db/postgres/pool.rs", "src/db/fixture_target.rs",
    "src/db/intake_delivery_required_migrations.rs", "src/db/auto_queue/test_support.rs",
    "src/dispatch/test_support.rs", "src/config.rs", "src/config/test_env.rs",
    "src/config/test_env/teardown_probe.rs", "src/services/turn_orchestrator.rs",
    "src/services/cluster/intake_worker/s3_real_tests/harness_tests.rs",
    "src/services/discord/router/intake_gate/busy_inject.rs", *ts.SHARED_TEST_HELPERS,
)


def git(root: Path, *args: str) -> str:
    return subprocess.run(["git", "-C", str(root), "-c", "user.name=t", "-c", "user.email=t@e",
                           *args], check=True, capture_output=True, text=True).stdout.strip()


def manifest(ids) -> str:
    rows = [INTEGRITY.LIB_INVENTORY_MANIFEST_HEADER, *INTEGRITY.LIB_INVENTORY_MANIFEST_RULES,
            "[tests]", *sorted(set(ids), key=lambda value: value.encode())]
    return "\n".join(rows) + "\n"


def transcript(passed=(), ignored=(), summary=None) -> str:
    """A libtest run reporting `passed` and `ignored`; `summary` overrides its counts."""
    lines = [f"test {test_id} ... ok" for test_id in passed]
    lines += [f"test {test_id} ... ignored" for test_id in ignored]
    ok, skipped = summary or (len(passed), len(ignored))
    return (f"running {len(lines)} tests\n" + "".join(line + "\n" for line in lines)
            + f"\ntest result: ok. {ok} passed; 0 failed; {skipped} ignored; "
            "0 measured; 9 filtered out; finished in 0.00s\n")


def ledger_rows(lane: str, ids) -> str:
    return "".join(f"ignored | {test_id} | lane={lane} | #1 | a dormant fixture that this lane "
                   "keeps ignored until its backing service exists in CI\n" for test_id in ids)


def on_shard(shard: int, count: int = 2) -> list[str]:
    return [test_id for test_id in (f"x::case_{index}_pg" for index in range(200))
            if PGM.pg_test_shard(test_id) == shard][:count]


class Fixture:
    """A git repository with a tiny crate, a lib manifest and the shared non-PG filter, and
    the pull_request event that started its run."""

    def __init__(self, directory: str):
        self.root = Path(directory) / "repo"
        self.event = Path(directory) / "event.json"
        self.label()
        self.write({**SOURCES, "scripts/ci/non-pg-test-filter.sh": FILTER,
                    "scripts/lib_test_inventory_manifest.txt": manifest(ALPHA + BETA + GAMMA + HOOK)})
        git(self.root, "init", "-q")
        self.base = self.commit("base")

    def label(self, *names: str) -> None:
        self.event.write_text(json.dumps({"pull_request": {"labels": [{"name": n} for n in names]}}))

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

    def env(self, **overrides: str) -> dict[str, str]:
        return {"ADK_CI_SELECT": "", "GITHUB_EVENT_PATH": str(self.event), **overrides}

    def plan(self, files: dict[str, str], **env: str) -> dict:
        head = self.commit("change", files)
        with mock.patch.dict(os.environ, self.env(**env)):
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
        self.assertFalse(plan["force_full"])

    def test_new_tests_change_the_manifest_and_run_full_and_their_directory_selects_them(self) -> None:
        added = ["alpha::fresh::tests::brand_new", "alpha::fresh::tests::brand_new_pg"]
        ids = manifest(ALPHA + BETA + GAMMA + HOOK + added)
        plan = self.repo.plan({"scripts/lib_test_inventory_manifest.txt": ids,
                               "src/alpha/mod.rs": SOURCES["src/alpha/mod.rs"] + "pub mod fresh;\n"})
        self.assertEqual(plan["lanes"]["pg"]["reason"],
                         "full-trigger: scripts/lib_test_inventory_manifest.txt")
        # The same ids, already in the base inventory, are selected by their directory.
        self.repo.base = git(self.repo.root, "rev-parse", "HEAD")
        plan = self.repo.plan({"src/alpha/fresh.rs": "#[cfg(test)]\nmod tests {}\n"})
        self.assertIn(added[1], self.repo.selected(plan, "pg"))
        self.assertIn(added[0], self.repo.selected(plan, "sweep"))

    def test_an_empty_selection_runs_both_lanes_in_full(self) -> None:
        plan = self.repo.plan({"src/delta/lonely.rs": "pub fn alone() { let _ = 1; }\n"})
        for lane in ts.LANES:
            self.assertEqual((plan["lanes"][lane]["mode"], plan["lanes"][lane]["reason"]),
                             ("full", "empty-selection"))
        self.assertEqual(plan["lanes"]["pg"]["shards"], list(range(PGM.PG_SHARD_COUNT)))

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

    def test_every_authority_runs_full_and_every_trigger_is_load_bearing(self) -> None:
        inv = ts.Inventory(self.repo.root)

        def reasons(triggers) -> dict[str, str]:
            with mock.patch.object(ts, "FULL_TRIGGERS", triggers):
                return {path: ts.scopes(inv, [path], [])[1] for path in AUTHORITIES}

        self.assertEqual(reasons(ts.FULL_TRIGGERS),
                         {path: f"full-trigger: {path}" for path in AUTHORITIES})
        for glob in ts.FULL_TRIGGERS:
            with self.subTest(glob=glob):
                rest = tuple(other for other in ts.FULL_TRIGGERS if other != glob)
                self.assertIn("", reasons(rest).values())
        site = Site("src/alpha/changed.rs", 3, "include", "src/alpha/body.rs", "")
        self.assertEqual(ts.scopes(inv, ["src/alpha/body.rs"], [site])[1],
                         "full-trigger: src/alpha/body.rs")

    def test_unresolved_inclusions_and_modules_run_full_and_resolved_readers_stay_one_hop(self) -> None:
        self.repo.write({"src/beta/includer.rs": INCLUDER})
        inv = ts.Inventory(self.repo.root)
        host = "src/beta/includer.rs"
        data = Site(host, 1, "include_str", "src/alpha/data.json", "")
        spliced = Site(host, 2, "include", None, "/gen.rs")
        unknown = Site(host, 2, "include", None, "")
        alias = Site(host, 3, "include", None, "")
        variable = Site(host, 4, "include", None, "")
        unresolved = "full-trigger: unresolved-inclusion:src/beta/includer.rs:"
        self.assertEqual(ts.scopes(inv, ["src/alpha/changed.rs"], [spliced])[1], unresolved + "2")
        self.assertEqual(ts.scopes(inv, ["src/alpha/data.json"], [unknown])[1], unresolved + "2")
        self.assertEqual(ts.scopes(inv, ["src/alpha/changed.rs"], [alias])[1], unresolved + "3")
        tail = Site(host, 1, "include_str", None, ".json")
        self.assertEqual(ts.scopes(inv, ["src/alpha/data.json"], [tail])[1], unresolved + "1")
        # Not full: a tail that rules the file out, a variable named include, a resolved reader.
        self.assertEqual(ts.scopes(inv, ["src/alpha/changed.rs"], [tail, variable])[1], "")
        found, reason = ts.scopes(inv, ["src/alpha/data.json"], [data, variable])
        self.assertEqual(reason, "")
        self.assertTrue({"src/beta/*", "src/beta/includer/**", "src/alpha/**"} <= found)
        self.assertEqual(ts.scopes(inv, ["src/alpha/orphan.rs"], [])[1],
                         "full-trigger: unresolved-module:src/alpha/orphan.rs")
        self.assertEqual(ts.scopes(inv, ["src/alpha/aliased/real.rs"], [])[1], "")

    def test_the_scanner_s_unresolved_include_reaches_the_plan(self) -> None:
        plan = self.repo.plan({"src/beta/includer.rs": INCLUDER.splitlines()[1] + "\n",
                               "src/beta/mod.rs": SOURCES["src/beta/mod.rs"] + "pub mod includer;\n"})
        self.assertTrue(plan["lanes"]["pg"]["reason"].startswith(
            "full-trigger: unresolved-inclusion:src/beta/includer.rs:1"), plan["lanes"])

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

    def test_kill_switch_variable_label_and_a_missing_payload_force_both_lanes_full(self) -> None:
        head = self.repo.commit("change", {"src/alpha/changed.rs": "// edit\n"})
        broken = Path(self.tmp.name) / "broken.json"
        broken.write_text("{")
        bare = Path(self.tmp.name) / "push.json"
        bare.write_text(json.dumps({"ref": "refs/heads/x"}))
        cases = (({"ADK_CI_SELECT": "OFF"}, "kill-switch: ADK_CI_SELECT=off"),
                 ({"ADK_CI_SELECT": "false"}, "kill-switch: ADK_CI_SELECT=false"),
                 ({"GITHUB_EVENT_PATH": ""}, "kill-switch: no pull_request payload"),
                 ({"GITHUB_EVENT_PATH": str(broken)}, "kill-switch: no pull_request payload"),
                 ({"GITHUB_EVENT_PATH": str(bare)}, "kill-switch: no pull_request payload"),
                 ({"label": "ci-full"}, "kill-switch: label ci-full, read at this push or reopen"))
        for env, reason in cases:
            with self.subTest(env=env):
                self.repo.label(env.pop("label", "other"))
                with mock.patch.dict(os.environ, self.repo.env(**env)):
                    plan = ts.make_plan(self.repo.root, self.repo.base, head)
                self.assertTrue(plan["force_full"])
                self.assertEqual({e["mode"] for e in plan["lanes"].values()}, {"full"})
                self.assertEqual({e["reason"].split(" (")[0] for e in plan["lanes"].values()},
                                 {reason})
        self.repo.label()
        plan = self.repo.plan({"src/alpha/changed.rs": "// edit again\n"})
        self.assertEqual((plan["force_full"], plan["lanes"]["pg"]["mode"]), (False, "selected"))
        self.assertIn("next push or reopen", plan["lanes"]["pg"]["reason"])

    def test_a_checkout_without_a_merge_parent_runs_full(self) -> None:
        head = self.repo.commit("change", {"src/alpha/changed.rs": "// edit\n"})
        with mock.patch.dict(os.environ, self.repo.env()):
            plan = ts.make_plan(self.repo.root, None, head)
        self.assertEqual(plan["lanes"]["pg"]["reason"], "no-merge-parent")

    def test_shard_zero_always_enters_the_selected_matrix(self) -> None:
        ids = on_shard(1)
        self.assertEqual(ts.shards(ids, False), [0, 1])
        self.assertEqual(ts.shards(on_shard(0), False), [0])
        self.assertEqual(ts.shards(ids, True), list(range(PGM.PG_SHARD_COUNT)))


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

    def run_pg(self, shard: int, ids: list[str], ignored=(), declared=(), reported=None,
               summary=None):
        """run_pg with libtest faked: the ignored listing, then run_test_lane for real over a
        transcript reporting `ignored` as ignored and the rest it was asked to run (or
        `reported`) as passed, against a ledger that declares `declared` for lane pg."""
        calls, real = [], subprocess.run
        ledger = Path(self.tmp.name) / "ledger"
        ledger.write_text(ledger_rows("pg", declared))

        def fake(command, *args, **kwargs):
            calls.append(command)
            self.assertNotIn("PG_INCLUDE_SHARD", kwargs.get("env") or {"PG_INCLUDE_SHARD": ""})
            if command[0] == "git":
                return real(command, *args, **kwargs)
            if command[:2] == ["bash", "-c"] and "--list --ignored" in command[2]:
                listed = "".join(f"{i}: test\n" for i in ignored) + f"\n{len(ignored)} tests, 0 benchmarks\n"
                return subprocess.CompletedProcess(command, 0, listed, "")
            if str(command[1]).endswith("run_test_lane.py"):
                split = command.index("--")
                asked = command[split + 5:]
                passed = [i for i in asked if i not in ignored] if reported is None else reported
                text = transcript(passed, [i for i in asked if i in ignored], summary)
                lane = [sys.executable, str(RUN_TEST_LANE), "--ledger", str(ledger),
                        *command[2:split + 1], sys.executable, "-c",
                        f"import sys; sys.stdout.write({text!r})"]
                return real(lane, capture_output=True, text=True)
            return subprocess.CompletedProcess(command, 0)  # `just`, or an unwrapped libtest run

        env = {"PG_INCLUDE_SHARD": str(shard), "POSTGRES_TEST_DATABASE_URL_BASE": "postgres://x"}
        with mock.patch.dict(os.environ, env), mock.patch.object(ts.subprocess, "run", fake):
            return ts.run_pg(self.repo.root, ids), calls

    def test_the_plan_reproduces_and_a_tampered_or_missing_one_is_red(self) -> None:
        entry, ids = self.load(self.plan)
        self.assertEqual(ids, self.repo.selected(self.plan, "pg"))
        tampered = json.loads(json.dumps(self.plan))
        tampered["scopes"] = ["src/gamma/**"]
        stale = {**self.plan, "sha": "0" * 40}
        for plan in (tampered, stale, None):
            with self.subTest(plan=bool(plan)), self.assertRaises(ValueError):
                self.load(plan)

    def test_selected_pg_ids_run_under_run_test_lane_and_zero_or_ignored_runs_are_red(self) -> None:
        ids = on_shard(1)
        rc, calls = self.run_pg(1, ids)
        self.assertEqual(rc, 0)
        lane = next(call for call in calls if str(call[1]).endswith("run_test_lane.py"))
        self.assertEqual(lane[2:4], ["--lane", "pg"])
        self.assertIn("--expected-ids", lane)
        self.assertEqual(self.run_pg(1, ids, reported=[])[0], 1)
        self.assertEqual(self.run_pg(1, ids, reported=ids[:1])[0], 1)

    def test_a_mixed_shard_judges_every_planned_ignore_against_the_pg_ledger(self) -> None:
        ids = on_shard(1)  # ids[0] runs, ids[1] is #[ignore]
        rc, calls = self.run_pg(1, ids, ignored=ids[1:])
        self.assertEqual(rc, 1)  # An undeclared ignore is red.
        lane = next(call for call in calls if str(call[1]).endswith("run_test_lane.py"))
        self.assertEqual(lane[-2:], ids)
        self.assertEqual(self.run_pg(1, ids, ignored=ids[1:], declared=ids[1:])[0], 0)
        for case in ({"reported": []}, {"summary": (2, 0)}, {"summary": (1, 0)}):
            with self.subTest(case=case):
                self.assertEqual(self.run_pg(1, ids, ignored=ids[1:], declared=ids[1:], **case)[0], 1)
        with self.assertRaises(ValueError):  # Every planned id ignored runs nothing.
            self.run_pg(1, ids, ignored=ids, declared=ids)

    def test_shard_zero_runs_the_continuity_recipe_once_and_other_shards_never(self) -> None:
        for shard, ids, lanes, recipes in ((0, [], 0, 1), (0, on_shard(0), 1, 1),
                                           (1, on_shard(1), 1, 0)):
            with self.subTest(shard=shard, ids=len(ids)):
                rc, calls = self.run_pg(shard, ids)
                self.assertEqual(rc, 0)
                self.assertEqual(sum(str(c[1]).endswith("run_test_lane.py") for c in calls), lanes)
                self.assertEqual(calls.count([*ts.CONTINUITY]), recipes)
        with self.assertRaises(ValueError):
            self.run_pg(1, [])
        shard_recipe = (REPO_ROOT / "justfile").read_text("utf-8").split("\ntest-postgres-shard:", 1)[1]
        self.assertIn("&& just test-postgres-continuity", shard_recipe.split("\n\n", 1)[0])

    def test_full_mode_runs_mains_commands_and_plan_writes_its_outputs(self) -> None:
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
        with mock.patch.dict(os.environ, {"GITHUB_OUTPUT": str(output), **self.repo.env()}), \
                mock.patch.object(ts, "log"):
            self.assertEqual(ts.main(["plan", "--root", str(self.repo.root), "--base",
                                      self.repo.base, "--head", head]), 0)
        lines = dict(line.split("=", 1) for line in output.read_text().splitlines())
        self.assertEqual(json.loads(lines["matrix"]), {"shard": self.plan["lanes"]["pg"]["shards"]})
        self.assertEqual(json.loads(lines["plan"])["lanes"], self.plan["lanes"])
        self.assertEqual(lines["force_full"], "false")


class ExpectedIdsTests(unittest.TestCase):
    """run_test_lane --expected-ids: the plan's set replaces the manifest's, still exactly."""

    def run_lane(self, expected: list[str], reported: list[str], ledger: str = "",
                 lane: str = "non-pg-sweep", ignored=(), summary=None) -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as directory:
            ids = Path(directory) / "ids"
            ids.write_text("\n".join(expected) + "\n")
            ledger_path = Path(directory) / "ledger"
            ledger_path.write_text(ledger)
            text = transcript(reported, ignored, summary)
            result = subprocess.run(
                [sys.executable, str(RUN_TEST_LANE), "--lane", lane, "--expected-ids",
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

    def test_a_pg_run_of_zero_tests_or_of_undeclared_ignores_is_red(self) -> None:
        planned, run, held = ["a::b::c_pg", "a::b::d_pg"], ["a::b::c_pg"], ["a::b::d_pg"]
        declared = ledger_rows("pg", held)
        self.assertEqual(self.run_lane(planned[:1], run, lane="pg")[0], 0)
        self.assertEqual(self.run_lane(planned[:1], [], lane="pg")[0], 1)
        self.assertEqual(self.run_lane(planned, run, lane="pg", ignored=held)[0], 1)
        self.assertEqual(self.run_lane(planned, run, declared, lane="pg", ignored=held)[0], 0)
        self.assertEqual(self.run_lane(planned, run, declared, lane="pg", ignored=held,
                                       summary=(2, 0))[0], 1)
        self.assertEqual(self.run_lane(planned, [], declared, lane="pg", ignored=held)[0], 1)
        # Ignores the ledger declares still leave a plan that executed nothing.
        rc, stderr = self.run_lane(held, [], declared, lane="pg", ignored=held)
        self.assertEqual(rc, 1)
        self.assertIn("no planned test executed", stderr)

    def test_declared_ignored_ids_outside_the_plan_are_not_stale(self) -> None:
        ledger = ("ignored | x::not::planned | lane=non-pg-sweep | #1 | a dormant fixture that "
                  "this lane keeps ignored until its backing service exists in CI\n")
        rc, stderr = self.run_lane(["a::b::c"], ["a::b::c"], ledger)
        self.assertEqual(rc, 0, stderr)


LIBTEST_CRATE = {
    "Cargo.toml": '[package]\nname = "lanecheck"\nversion = "0.1.0"\nedition = "2021"\n',
    "src/lib.rs": "#[cfg(test)]\nmod tests {\n    #[test]\n    fn runs_pg() {}\n\n"
                  "    #[test]\n    #[ignore]\n    fn held_pg() {}\n}\n",
}


@unittest.skipUnless(shutil.which("cargo"), "cargo is not installed")
class LibtestIntegrationTests(unittest.TestCase):
    """Real libtest output, one passing and one #[ignore] test, through run_test_lane."""

    def test_a_real_ignored_id_is_red_until_the_pg_ledger_declares_it(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory) / "crate"
            for name, text in LIBTEST_CRATE.items():
                (crate / name).parent.mkdir(parents=True, exist_ok=True)
                (crate / name).write_text(text)
            ids, ledger = Path(directory) / "ids", Path(directory) / "ledger"
            planned = ["tests::runs_pg", "tests::held_pg"]
            ids.write_text("\n".join(planned) + "\n")
            # No compiler wrapper (empty disables one a cargo config names) and a private target.
            env = {**os.environ, "RUSTC_WRAPPER": "", "CARGO_BUILD_RUSTC_WRAPPER": "",
                   "CARGO_TARGET_DIR": str(Path(directory) / "target")}

            def lane(rows: str) -> subprocess.CompletedProcess:
                ledger.write_text(rows)
                return subprocess.run(
                    [sys.executable, str(RUN_TEST_LANE), "--lane", "pg", "--expected-ids", str(ids),
                     "--ledger", str(ledger), "--", "cargo", "test", "--lib", "--",
                     "--exact", *planned, "--test-threads=1"],
                    cwd=crate, env=env, capture_output=True, text=True)

            undeclared = lane("")
            self.assertEqual(undeclared.returncode, 1, undeclared.stdout + undeclared.stderr)
            self.assertIn("undeclared-ignored: tests::held_pg", undeclared.stdout + undeclared.stderr)
            self.assertIn("1 passed; 0 failed; 1 ignored", undeclared.stdout)
            declared = lane(ledger_rows("pg", planned[1:]))
            self.assertEqual(declared.returncode, 0, declared.stdout + declared.stderr)
            self.assertIn("1 passed; 0 failed; 1 ignored", declared.stdout)
            self.assertIn("executed=1", declared.stdout)


REEXPORT = re.compile(r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*pub(?:\([^)]*\))?\s+use\s+([\w:]+)")


class RepositoryTests(unittest.TestCase):
    """The checked-in tree: shared test helpers that consumers reach by another path."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.inv = ts.Inventory(REPO_ROOT)

    def reexported_helpers(self) -> set[str]:
        """Files of every `#[cfg(test)]` test_support module a `pub … use` re-exports."""
        found: set[str] = set()
        for name, text in self.inv.texts.items():
            for path in REEXPORT.findall(text):
                parts = path.split("::")
                hit = next((index for index, part in enumerate(parts)
                            if part == "test_support" or part.endswith("_test_support")), None)
                if hit is None:
                    continue
                base = self.inv.module(name).split("::") if self.inv.module(name) else []
                segments = parts[:hit + 1]
                while segments and segments[0] in ("self", "super", "crate"):
                    head = segments.pop(0)
                    base = [] if head == "crate" else base[:-1] if head == "super" else base
                module, files = [*base, *segments], None
                while module and not (files := self.inv.files.get("::".join(module))):
                    module = module[:-1]  # An inline module lives in its parent's file.
                found.update(files or [f"unresolved re-export in {name}: {path}"])
        return found

    def test_shared_test_helpers_equal_the_source_s_reexports(self) -> None:
        self.assertEqual(self.reexported_helpers(), set(ts.SHARED_TEST_HELPERS))

    def test_the_observability_helper_runs_full_and_without_it_misses_its_consumers(self) -> None:
        helper = "src/services/observability/test_support.rs"
        self.assertEqual(ts.scopes(self.inv, [helper], [])[1], f"full-trigger: {helper}")
        rest = tuple(glob for glob in ts.FULL_TRIGGERS if glob != helper)
        with mock.patch.object(ts, "FULL_TRIGGERS", rest):
            found, reason = ts.scopes(self.inv, [helper], [])
        population = self.inv.population["pg"] | self.inv.population["sweep"]
        consumers = {t for t in population
                     if "src/high_risk_recovery.rs" in self.inv.owners(t)}
        selected = set(self.inv.selected(found, "pg")) | set(self.inv.selected(found, "sweep"))
        self.assertEqual(reason, "")
        self.assertTrue(consumers and selected and not consumers & selected)
        self.assertEqual(ts.verdict(sorted(selected), population), "")


def runs(condition: str, outputs: dict[str, str]) -> bool:
    """Evaluate a job's `if:` over the given needs outputs, with no run cancelled."""
    expression = condition.strip().removeprefix("${{").removesuffix("}}")
    expression = re.sub(r"needs\.\w+\.outputs\.(\w+)", lambda m: repr(outputs.get(m.group(1), "")),
                        expression)
    expression = expression.replace("!cancelled()", "True").replace("&&", " and ")
    return bool(eval(expression.replace("||", " or "), {}))  # noqa: S307 - test-only input


def glob_regex(pattern: str) -> re.Pattern[str]:
    return re.compile("".join(".*" if part == "**" else re.escape(part).replace(r"\*", "[^/]*")
                              for part in re.split(r"(\*\*)", pattern)) + "$")


class WorkflowGateTests(unittest.TestCase):
    """Every full-run trigger reaches the selector; the kill switch opens closed gates."""

    def setUp(self) -> None:
        self.text = PR_WORKFLOW.read_text("utf-8")
        self.workflow = yaml.safe_load(self.text)
        self.jobs = self.workflow["jobs"]

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
        for mirror, (upstream, name) in {
                "fast_targeted_tests_required_context": ("test_fast", "pg_db"),
                "library_sweep_required_context": ("library_sweep", "rust_tests")}.items():
            env = next(step["env"] for step in self.jobs[mirror]["steps"]
                       if step.get("env", {}).get("UPSTREAM_JOB_NAME") == upstream)
            self.assertEqual((env["FILTER_NAME"], env["FILTER_OUTPUT"]),
                             (name, f"${{{{ needs.changes.outputs.{name} }}}}"))

    def test_force_full_opens_both_lanes_whatever_their_path_gates_say(self) -> None:
        self.assertNotIn("if", self.jobs["pg_shards"])  # Every PR plans.
        self.assertEqual(self.jobs["pg_shards"]["outputs"]["force_full"],
                         "${{ steps.plan.outputs.force_full }}")
        for gates in ({"pg_db": "false", "rust_tests": "false"},   # comment-only, docs
                      {"pg_db": "true", "rust_tests": "false"},    # sqlx-data.json
                      {"pg_db": "false", "rust_tests": "true"}):
            for job, own in (("test_fast", "pg_db"), ("library_sweep", "rust_tests")):
                with self.subTest(job=job, gates=gates):
                    condition = self.jobs[job]["if"]
                    self.assertTrue(runs(condition, {**gates, "force_full": "true"}))
                    self.assertEqual(runs(condition, {**gates, "force_full": "false"}),
                                     gates[own] == "true")

    def test_a_label_starts_no_run_and_the_kill_switch_is_read_once(self) -> None:
        # A `labeled` run would re-post skipped required checks on the same head SHA.
        self.assertIsNone(self.workflow[True]["pull_request"])
        step = self.jobs["pg_shards"]["steps"][1]
        self.assertEqual(step["env"], {"ADK_CI_SELECT": "${{ vars.ADK_CI_SELECT }}"})
        comment = self.text.split("      - id: plan\n", 1)[0].rsplit("\n", 2)[1]
        self.assertIn("ci-full label from the next push or reopen", comment)
        self.assertEqual(self.text.count("vars.ADK_CI_SELECT"), 1)
        main = MAIN_WORKFLOW.read_text("utf-8")
        self.assertNotIn("test-selection", main)
        self.assertNotIn(ts.PLAN_ENV, main)


if __name__ == "__main__":
    unittest.main()
