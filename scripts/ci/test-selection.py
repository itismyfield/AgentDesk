#!/usr/bin/env python3
"""Choose the PostgreSQL and library-sweep tests one pull request runs.

main always runs both lanes in full. A PR runs the tests whose source sits in a
directory its diff touches, or in the directory of a file that names a changed
module (`crate::<path>`), file or top-level directory literally. A full-run
trigger, the kill switch, an error, an empty selection or one of at least 40% of
a lane runs that lane in full. `plan` decides once; `run` replays the decision
in each consumer and refuses a plan it cannot reproduce.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import importlib.util
import json
import os
import posixpath
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = ROOT / "scripts"
LANES = ("pg", "sweep")
THRESHOLD = 0.40
KILL_LABEL = "ci-full"
PLAN_ENV = "ADK_TEST_SELECTION_PLAN"
MAX_PLAN_BYTES = 60_000
# Shared fixtures, build inputs, lane definitions and the selector's own code.
FULL_TRIGGERS = (
    "migrations/*", "build.rs", "Cargo.toml", "Cargo.lock", ".cargo/*", "rust-toolchain*",
    "justfile", ".github/workflows/*", ".github/actions/*", "tests/*", "scripts/ci/*",
    "scripts/ci-script-checks.sh", "scripts/ci-timeout.py", "scripts/rust_lex.py",
    "scripts/pg_test_lane_*", "scripts/known_test_failures.txt", "scripts/run_test_lane.py",
    "scripts/check_pg_test_lane_membership.py", "scripts/check_test_target_integrity.py",
    "scripts/check_test_lane_coverage.py", "scripts/lane_source_parity.py",
    "scripts/lane_new_id_registration.py", "scripts/check-ci-runner-hardening.sh",
    "scripts/test_lane_*", "scripts/test_target_integrity_*",
    "src/db/mod.rs", "src/db/postgres.rs", "src/db/postgres/*", "src/db/fixture_target.rs",
    "src/db/intake_delivery_required_migrations.rs", "src/db/auto_queue/test_support.rs",
    "src/dispatch/test_support.rs", "src/config.rs", "src/config/test_env.rs",
    "src/config/test_env/*", "src/services/turn_orchestrator.rs",
    "src/services/cluster/intake_worker/s3_real_tests/harness_tests.rs",
    # Re-exported as `busy_inject_support`; its e2e consumer never names this path.
    "src/services/discord/router/intake_gate/busy_inject.rs",
    "src/services/discord/router/intake_gate/busy_inject_test_support.rs",
)
# Changed by most PRs yet read by neither lane runner nor selector rule: new-ID exception
# rows, and the CI script unit tests at the top of tests/ (a Rust reader still names them).
EXEMPT = re.compile(r"scripts/test_lane_required_id_exceptions\.txt|tests/[^/]+\.(?:py|sh)")
FULL_PG = ("just", "test-postgres-shard")
GENERIC_NAMES = frozenset({"lib.rs", "main.rs", "mod.rs", "tests.rs"})
FULL_SWEEP = """source scripts/ci/non-pg-test-filter.sh
python3 scripts/run_test_lane.py --lane non-pg-sweep --max-summaries 4 "${NON_PG_SKIP_ARGS[@]}" -- env -u AGENTDESK_ROOT_DIR python3 scripts/ci/lib-test-artifact.py run -- cargo test --lib -- "${NON_PG_SKIP_ARGS[@]}"
run_non_pg_filter_replay
"""


def _module(name: str, path: Path):
    if name not in sys.modules:
        spec = importlib.util.spec_from_file_location(name, path)
        module = importlib.util.module_from_spec(spec)
        sys.modules[name] = module
        spec.loader.exec_module(module)
    return sys.modules[name]


def tools():
    """The runner, coverage, membership and include scanner this selector reuses."""
    for path in (SCRIPTS / "ci", SCRIPTS):
        if str(path) not in sys.path:
            sys.path.insert(0, str(path))
    return (_module("run_test_lane", SCRIPTS / "run_test_lane.py"),
            _module("check_test_lane_coverage", SCRIPTS / "check_test_lane_coverage.py"),
            _module("check_pg_test_lane_membership",
                    SCRIPTS / "check_pg_test_lane_membership.py"),
            _module("rust_include_reads", SCRIPTS / "ci/rust_include_reads.py"))


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()[:16]


def git(root: Path, *args: str) -> str:
    result = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)
    if result.returncode:
        raise ValueError(f"git {' '.join(args)}: {result.stderr.strip()}")
    return result.stdout


class Inventory:
    """Lane populations from the checked-in manifest and each test's owning file."""

    def __init__(self, root: Path):
        lane, cov, _, _ = tools()
        manifest = root / "scripts/lib_test_inventory_manifest.txt"
        self.digest = digest(manifest.read_bytes())
        runtime, errors = lane.derive_expected_selection(manifest, "linux", [])
        if errors:
            raise ValueError("; ".join(errors))
        values = cov.load_non_pg_skip_args(root)[1::2]
        replay = set(cov.load_non_pg_filter_replay(root)) & runtime
        pg = frozenset(t for t in runtime if any(value in t for value in values))
        self.population = {"pg": pg, "sweep": (runtime - pg) | replay}
        self.aliases: dict = {}
        cov.discover_test_inventory(root, self.aliases)
        self.src = (root / "src").resolve()
        self.texts: dict[str, str] = {}
        self.files: dict[str, list[str]] = {}
        for path in sorted(self.src.rglob("*.rs")):
            rel = path.relative_to(self.src)
            name = f"src/{rel.as_posix()}"
            self.texts[name] = path.read_text("utf-8", errors="replace")
            if rel.name != "main.rs" and rel.parts[0] != "bin":
                self.files.setdefault(self.module(name), []).append(name)

    def module(self, name: str) -> str:
        """The logical module path of a `src/` file, which need not exist any more."""
        cov = tools()[1]
        physical = cov.file_module_path(self.src, self.src / name.removeprefix("src/"))
        return "::".join(cov._normalize_alias_path(physical, self.aliases))

    def owners(self, test_id: str) -> list[str]:
        parts = test_id.split("::")[:-1]
        for size in range(len(parts), -1, -1):
            if found := self.files.get("::".join(parts[:size])):
                return found
        return []

    def selected(self, scopes, lane: str) -> list[str]:
        """`dir/**` takes a directory and everything below it, `dir/*` only its files."""
        trees = tuple(scope[:-2] for scope in scopes if scope.endswith("/**"))
        flats = {scope[:-2] for scope in scopes if not scope.endswith("/**")}
        return sorted(t for t in self.population[lane] if any(
            owner.startswith(trees) or posixpath.dirname(owner) in flats
            for owner in self.owners(t)))


def scopes(inv: Inventory, changed: list[str], sites) -> tuple[set[str], str]:
    """Source scopes whose tests a change can reach, or a full-run reason."""
    owners = {path for site in sites if site.macro == "include" and site.target
              for path in (site.path, site.target)}
    for path in changed:
        if path in owners or not EXEMPT.fullmatch(path) and any(
                fnmatch.fnmatchcase(path, glob) for glob in FULL_TRIGGERS):
            return set(), f"full-trigger: {path}"
    found: set[str] = set()
    readers: set[str] = set()
    needles: set[str] = set()
    for path in changed:
        name = posixpath.basename(path)
        if name in GENERIC_NAMES:
            name = posixpath.join(posixpath.basename(posixpath.dirname(path)), name)
        needles.add(re.escape(name))
        if path.startswith("src/"):
            found.add(f"{posixpath.dirname(path)}/**")
            if path.endswith(".rs"):
                needles.add(re.escape(f"crate::{inv.module(path)}") + r"(?![A-Za-z0-9_])")
        elif "/" in path:
            needles.add(re.escape(f'"{path.split("/")[0]}"'))
        # Resolved include_str!/include_bytes! readers, and unresolved ones with a known tail.
        readers.update(site.path for site in sites if site.path.startswith("src/") and (
            site.target == path or site.target is None and len(site.tail) >= 3
            and site.reads(path)))
    pattern = re.compile("|".join(sorted(needles)))
    readers.update(name for name, text in inv.texts.items() if pattern.search(text))
    # One hop: a reader's own directory files and its child modules, not their readers.
    for name in readers:
        found.update((f"{posixpath.dirname(name)}/*", f"{posixpath.splitext(name)[0]}/**"))
    return found, ""


def kill_switch() -> str:
    """The repository variable, or the label on the event that started this run."""
    if os.environ.get("ADK_CI_SELECT", "").strip().lower() == "off":
        return "kill-switch: ADK_CI_SELECT=off"
    event = os.environ.get("GITHUB_EVENT_PATH")
    pull = json.loads(Path(event).read_text("utf-8")).get("pull_request", {}) if event else {}
    labels = {label.get("name") for label in pull.get("labels", [])}
    return f"kill-switch: label {KILL_LABEL}" if KILL_LABEL in labels else ""


def merge_parent(root: Path, head: str) -> str | None:
    """The PR merge commit's base parent, fetched when the checkout is shallow."""
    parents = [line.split()[1] for line in git(root, "cat-file", "-p", head).splitlines()
               if line.startswith("parent ")]
    if len(parents) != 2:
        return None
    if subprocess.run(["git", "-C", str(root), "cat-file", "-e", f"{parents[0]}^{{commit}}"],
                      capture_output=True, check=False).returncode:
        git(root, "fetch", "--no-tags", "--depth=1", "origin", parents[0])
    return parents[0]


def verdict(ids: list[str], population, reason: str = "") -> str:
    """Why a lane runs in full, or "" when its selection stands; an empty one never does."""
    ratio = len(ids) / max(len(population), 1)
    return reason or ("empty-selection" if not ids else
                      f"ratio {ratio:.2f} >= {THRESHOLD}" if ratio >= THRESHOLD else "")


def make_plan(root: Path, base: str | None, head: str) -> dict:
    head = git(root, "rev-parse", head).strip()
    plan = {"version": 1, "sha": head, "base": "", "inventory": "", "scopes": [], "lanes": {}}
    try:
        reason = kill_switch()
        if not reason and base is None:
            base = merge_parent(root, head)
            reason = "" if base else "no-merge-parent"
        if base:
            plan["base"] = git(root, "rev-parse", base).strip()
        inv = Inventory(root)
        plan["inventory"] = inv.digest
        if not reason:
            changed = sorted(set(git(root, "diff", "--name-only", "--no-renames", "-z",
                                     plan["base"], head).split("\0")) - {""})
            found, reason = scopes(inv, changed, tools()[3].tree_sites(str(root), head))
            plan["scopes"] = sorted(found)
        for lane in LANES:
            ids = [] if reason else inv.selected(plan["scopes"], lane)
            why = verdict(ids, inv.population[lane], reason)
            plan["lanes"][lane] = {
                "mode": "full" if why else "selected", "reason": why or "directory selection",
                "selected": len(ids), "population": len(inv.population[lane]),
                "ids": digest("\n".join(ids).encode()), "shards": shards(ids, bool(why))}
    except Exception as error:  # Any failure runs every lane in full, with its reason.
        plan["scopes"] = []
        plan["lanes"] = {lane: {"mode": "full", "reason": f"selector-error: {error}",
                                "selected": 0, "population": 0, "ids": "",
                                "shards": shards([], True)} for lane in LANES}
    if all(entry["mode"] == "full" for entry in plan["lanes"].values()):
        plan["scopes"] = []  # No consumer replays them; keep the output small.
    if len(json.dumps(plan)) > MAX_PLAN_BYTES:
        plan["scopes"] = []
        for entry in plan["lanes"].values():
            entry.update(mode="full", reason="plan-too-large", shards=shards([], True))
    return plan


def shards(ids: list[str], full: bool) -> list[int]:
    pgm = tools()[2]
    return list(range(pgm.PG_SHARD_COUNT)) if full else sorted(
        {pgm.pg_test_shard(test_id) for test_id in ids})


def log(plan: dict, lane: str) -> None:
    entry = plan["lanes"][lane]
    print(f"test-selection: sha={plan['sha']} base={plan['base'] or '-'} "
          f"inventory={plan['inventory'] or '-'} lane={lane} mode={entry['mode']} "
          f"selected={entry['selected']}/{entry['population']} reason={entry['reason']}",
          file=sys.stderr)


def load_plan(root: Path, lane: str):
    """The plan this checkout produced, its lane entry and, if selected, the replayed ids."""
    plan = json.loads(os.environ.get(PLAN_ENV) or "null")
    if not isinstance(plan, dict) or plan.get("version") != 1:
        raise ValueError(f"{PLAN_ENV} carries no selection plan")
    if plan["sha"] != git(root, "rev-parse", "HEAD").strip():
        raise ValueError(f"plan was made for {plan['sha']}, not this checkout")
    entry = plan["lanes"][lane]
    log(plan, lane)
    if entry["mode"] == "full":
        return entry, []
    inv = Inventory(root)
    ids = inv.selected(plan["scopes"], lane)
    if entry["mode"] != "selected" or inv.digest != plan["inventory"] or not ids \
            or digest("\n".join(ids).encode()) != entry["ids"] or len(ids) != entry["selected"]:
        raise ValueError(f"the {lane} plan does not reproduce from this checkout")
    return entry, ids


def run_pg(root: Path, ids: list[str]) -> int:
    """The shard's planned ids; shard 0's continuity commands are sweep-population tests."""
    pgm = tools()[2]
    shard = pgm.validate_pg_shard(os.environ.get("PG_INCLUDE_SHARD", ""))
    mine = [test_id for test_id in ids if pgm.pg_test_shard(test_id) == shard]
    if not mine or not os.environ.get("POSTGRES_TEST_DATABASE_URL_BASE"):
        raise ValueError(f"shard {shard} selects no test, or no PostgreSQL test server is named")
    # The shared partition lists the compiled tests; every planned id must be among them.
    listed = subprocess.run(
        ["bash", "-c", 'source scripts/ci/non-pg-test-filter.sh && printf "%s\\n" '
         '"${PG_INCLUDE_ARGS[@]}"'], cwd=root, check=True, stdout=subprocess.PIPE,
        text=True).stdout.split()
    if missing := sorted(set(mine) - set(listed[1:])):
        raise ValueError(f"{len(missing)} planned id(s) are not compiled tests: {missing[:3]}")
    env = {key: value for key, value in os.environ.items() if key != "PG_INCLUDE_SHARD"}
    return subprocess.run(
        ["bash", "-c", 'source scripts/ci/non-pg-test-filter.sh && run_prepared_lib_test '
         'cargo test --lib -- --exact "$@" --nocapture --test-threads=1', "bash", *mine],
        cwd=root, env=env, check=False).returncode


def run_sweep(root: Path, ids: list[str]) -> int:
    with tempfile.NamedTemporaryFile("w", suffix=".ids", delete=False) as handle:
        handle.write("\n".join(ids) + "\n")
    command = [sys.executable, "scripts/run_test_lane.py", "--lane", "non-pg-sweep",
               "--max-summaries", "4", "--expected-ids", handle.name, "--", "env", "-u",
               "AGENTDESK_ROOT_DIR", "python3", "scripts/ci/lib-test-artifact.py", "run", "--",
               "cargo", "test", "--lib", "--", "--exact", *ids]
    return subprocess.run(command, cwd=root, check=False).returncode


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    plan_parser = commands.add_parser("plan")
    plan_parser.add_argument("--root", type=Path, default=ROOT)
    plan_parser.add_argument("--base")
    plan_parser.add_argument("--head", default="HEAD")
    run_parser = commands.add_parser("run")
    run_parser.add_argument("--lane", choices=LANES, required=True)
    args = parser.parse_args(argv)
    if args.command == "plan":
        plan = make_plan(args.root.resolve(), args.base, args.head)
        for lane in LANES:
            log(plan, lane)
        print(f"test-selection: scopes={len(plan['scopes'])} {' '.join(plan['scopes'][:40])}",
              file=sys.stderr)
        text = json.dumps(plan, separators=(",", ":"), sort_keys=True)
        matrix = json.dumps({"shard": plan["lanes"]["pg"]["shards"]}, separators=(",", ":"))
        if output := os.environ.get("GITHUB_OUTPUT"):
            with open(output, "a", encoding="utf-8") as handle:
                handle.write(f"matrix={matrix}\nplan={text}\n")
        else:
            print(text)
        return 0
    try:
        entry, ids = load_plan(ROOT, args.lane)
        if entry["mode"] == "full":
            command = [*FULL_PG] if args.lane == "pg" else ["bash", "-eo", "pipefail", "-c", FULL_SWEEP]
            return subprocess.run(command, cwd=ROOT, check=False).returncode
        return (run_pg if args.lane == "pg" else run_sweep)(ROOT, ids)
    except (OSError, KeyError, ValueError, subprocess.CalledProcessError) as error:
        print(f"ERROR test-selection {args.lane}: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
