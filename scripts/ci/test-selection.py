#!/usr/bin/env python3
"""Choose the PostgreSQL and library-sweep tests one pull request runs.

main always runs both lanes in full. A PR runs the tests whose source sits in a
directory its diff touches, or in the directory of a file that names a changed
module (`crate::<path>`), file or top-level directory literally. A full-run
trigger, an inclusion or module path this cannot resolve, the kill switch, an
error, an empty selection or one of at least 40% of a lane runs that lane in
full. `plan` decides once; `run` replays the decision in each consumer, refuses
a plan it cannot reproduce and has run_test_lane prove every planned id ran,
except main's non-PG filter replay ids, which run outside the ledger as on main.
"""

from __future__ import annotations

import argparse
from collections import Counter
import datetime
import fnmatch
import hashlib
import importlib.util
import json
import os
import posixpath
import re
import shlex
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
LEDGER = "scripts/known_test_failures.txt"
MAX_PLAN_BYTES = 60_000
SWEEP_SHARD_COUNT = 4
# Files holding a `#[cfg(test)]` test_support module re-exported under another path, whose
# consumers never name the file; tests/test_test_selection.py keeps this equal to the source.
SHARED_TEST_HELPERS = (
    "src/services/discord/recovery_engine/rebind_runtime.rs",
    "src/services/discord/recovery_engine/restore_persist_outcome.rs",
    "src/services/discord/router/intake_gate/busy_inject_test_support.rs",
    "src/services/discord/router/message_handler/provider_dispatch/producer_error_test_support.rs",
    "src/services/discord/tmux_output_stream/native_codex.rs",
    "src/services/observability/test_support.rs",
    "src/services/tui_o/writer/actor.rs",
)
# Shared fixtures, build inputs, lane definitions with their authorities, and this code.
FULL_TRIGGERS = (
    "migrations/*", "build.rs", "Cargo.toml", "Cargo.lock", ".cargo/*", "rust-toolchain*",
    "justfile", ".github/workflows/*", ".github/actions/*", "tests/*", "scripts/ci/*",
    "scripts/ci-script-checks.sh", "scripts/ci-timeout.py", "scripts/rust_lex.py",
    "scripts/lib_test_inventory_manifest.txt", "scripts/pg_test_lane_*",
    "scripts/known_test_failures.txt", "scripts/run_test_lane.py",
    "scripts/check_pg_test_lane_membership.py", "scripts/check_test_target_integrity.py",
    "scripts/check_test_lane_coverage.py", "scripts/lane_*",
    "scripts/check-ci-runner-hardening.sh", "scripts/test_lane_*",
    "scripts/test_target_integrity_*",
    "src/db/mod.rs", "src/db/postgres.rs", "src/db/postgres/*", "src/db/fixture_target.rs",
    "src/db/intake_delivery_required_migrations.rs", "src/db/auto_queue/test_support.rs",
    "src/dispatch/test_support.rs", "src/config.rs", "src/config/test_env.rs",
    "src/config/test_env/*", "src/services/turn_orchestrator.rs",
    # Re-exported as `busy_inject_support`; its e2e consumer never names this path.
    "src/services/discord/router/intake_gate/busy_inject.rs", *SHARED_TEST_HELPERS,
)
FULL_PG = ("just", "test-postgres-shard")
CONTINUITY = ("just", "test-postgres-continuity")
PREPARED = "source scripts/ci/non-pg-test-filter.sh && run_prepared_lib_test cargo test --lib --"
GENERIC_NAMES = frozenset({"lib.rs", "main.rs", "mod.rs", "tests.rs"})
FULL_SWEEP = """source scripts/ci/non-pg-test-filter.sh
python3 scripts/run_test_lane.py --lane non-pg-sweep --max-summaries 4 "${NON_PG_SKIP_ARGS[@]}" -- env -u AGENTDESK_ROOT_DIR python3 scripts/ci/lib-test-artifact.py run -- cargo test --lib -- "${NON_PG_SKIP_ARGS[@]}"
run_non_pg_filter_replay
"""
SELECTED = (f"directory selection; full via ADK_CI_SELECT=off now, or label {KILL_LABEL} "
            "from the next push or reopen")


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
        decls: dict = {}
        cov.discover_test_inventory(root, self.aliases, decls)
        self.root = root
        self.src = (root / "src").resolve()
        self.texts: dict[str, str] = {}
        self.files: dict[str, list[str]] = {}
        self.undeclared: set[str] = set()  # No `mod` names them: an include! target or orphan.
        for path in sorted(self.src.rglob("*.rs")):
            rel = path.relative_to(self.src)
            name = f"src/{rel.as_posix()}"
            self.texts[name] = path.read_text("utf-8", errors="replace")
            if rel.name != "main.rs" and rel.parts[0] != "bin":
                self.files.setdefault(module := self.module(name), []).append(name)
                if module and module not in decls:
                    self.undeclared.add(name)

    def module(self, name: str) -> str:
        """The logical module path of a `src/` file, which need not exist any more."""
        cov = tools()[1]
        physical = cov.file_module_path(self.src, self.src / name.removeprefix("src/"))
        return "::".join(cov._normalize_alias_path(physical, self.aliases))

    def unresolved(self, site) -> bool:
        """An include call left unresolved, or a `use … as` alias of the macro; a variable
        that is merely named `include` is neither."""
        if site.target is not None or site.tail:
            return site.target is None
        line = (self.root / site.path).read_text("utf-8", errors="replace").splitlines()
        return bool(re.search(rf"\b{site.macro}\s*(?:!|as\b)", line[site.line - 1]))

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
    unsure = [site for site in sites if inv.unresolved(site)]
    for path in changed:
        if path in owners or any(fnmatch.fnmatchcase(path, glob) for glob in FULL_TRIGGERS):
            return set(), f"full-trigger: {path}"
        # An inclusion or a module path this cannot resolve may reach any test.
        if site := next((site for site in unsure if site.reads(path)), None):
            return set(), f"full-trigger: unresolved-inclusion:{site.path}:{site.line}"
        if path in inv.undeclared:
            return set(), f"full-trigger: unresolved-module:{path}"
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
        readers.update(site.path for site in sites
                       if site.path.startswith("src/") and site.target == path)
    pattern = re.compile("|".join(sorted(needles)))
    readers.update(name for name, text in inv.texts.items() if pattern.search(text))
    # One hop: a reader's own directory files and its child modules, not their readers.
    for name in readers:
        found.update((f"{posixpath.dirname(name)}/*", f"{posixpath.splitext(name)[0]}/**"))
    return found, ""


def kill_switch() -> str:
    """The repository variable, else the label on the PR event that started this run; adding
    the label starts no run, so it applies from the next push or reopen."""
    value = os.environ.get("ADK_CI_SELECT", "").strip().lower()
    if value not in ("", "on"):
        return f"kill-switch: ADK_CI_SELECT={value}"
    try:
        pull = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text("utf-8"))
        labels = {label["name"] for label in pull["pull_request"]["labels"]}
    except (KeyError, OSError, TypeError, ValueError) as error:
        return f"kill-switch: no pull_request payload ({type(error).__name__})"
    return (f"kill-switch: label {KILL_LABEL}, read at this push or reopen"
            if KILL_LABEL in labels else "")


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
    plan = {"version": 1, "sha": head, "base": "", "inventory": "", "force_full": False,
            "scopes": [], "lanes": {}}
    try:
        reason = kill_switch()
        plan["force_full"] = bool(reason)  # Opens lanes whose path gate is closed, too.
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
                "mode": "full" if why else "selected", "reason": why or SELECTED,
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


def shards(ids: list[str], full: bool) -> list[int | str]:
    """PG id shards and one independent continuity leg, including narrow selections."""
    pgm = tools()[2]
    if full:
        return pgm.pg_shard_legs()
    return [*sorted({pgm.pg_test_shard(test_id) for test_id in ids}), "continuity"]


def sweep_population(root: Path) -> list[str]:
    """The existing full sweep: canonical skips plus their source-verified replay ids."""
    runner, coverage = tools()[:2]
    runtime, errors = runner.derive_expected_selection(
        root / "scripts/lib_test_inventory_manifest.txt", "linux", [])
    if errors:
        raise ValueError("; ".join(errors))
    skips = coverage.load_non_pg_skip_args(root)[1::2]
    replay = set(coverage.load_non_pg_filter_replay(root))
    return sorted(test_id for test_id in runtime
                  if test_id in replay or not any(skip in test_id for skip in skips))


def nested_pairs(root: Path) -> tuple[tuple[str, str], ...]:
    return _module("nested_libtest", SCRIPTS / "ci/nested_libtest.py").inherited_pairs(root)


def sweep_runnable_groups(ids, ignored, nested):
    """Parents sharing inherited child output must use the same primary harness."""
    runnable = sorted(set(ids) - ignored)
    owner = {test_id: test_id for test_id in runnable}

    def find(test_id):
        while owner[test_id] != test_id:
            test_id = owner[test_id]
        return test_id

    children = {}
    for parent, child in nested:
        if parent not in ids:
            continue
        if child not in ids or child not in ignored:
            raise ValueError(f"nested child {child} must be selected and declared ignored with {parent}")
        if parent in owner:
            children.setdefault(child, []).append(parent)
    for parents in children.values():
        for parent in parents[1:]:
            owner[find(parent)] = find(parents[0])
    groups = {}
    for test_id in runnable:
        groups.setdefault(find(test_id), []).append(test_id)
    return sorted(groups.values())


def sweep_partition(ids: list[str], ignored: set[str] | frozenset[str] = frozenset(),
                    replay: set[str] | frozenset[str] = frozenset(),
                    nested: tuple[tuple[str, str], ...] = ()) -> list[list[str]]:
    """Keep each shard runnable and attach held judged ids to runnable judged ids."""
    runnable = sorted(test_id for test_id in ids if test_id not in ignored)
    held = sorted(test_id for test_id in ids if test_id in ignored)
    groups = sweep_runnable_groups(ids, ignored, nested)
    count = min(SWEEP_SHARD_COUNT, len(groups) or len(ids))
    if not runnable:
        return [held[index::count] for index in range(count)]
    parts = [[test_id for group in groups[index::count] for test_id in group]
             for index in range(count)]
    judged_shards = [index for index, part in enumerate(parts) if set(part) - replay]
    for test_id in held:
        eligible = judged_shards if test_id not in replay and judged_shards else range(count)
        required = {index for index, part in enumerate(parts)
                    if any(parent in part and child == test_id for parent, child in nested)}
        if len(required) > 1:
            raise ValueError(f"nested parents for {test_id} span multiple shards")
        target = next(iter(required)) if required else min(eligible, key=lambda index: (len(parts[index]), index))
        parts[target].append(test_id)
    return [sorted(part) for part in parts]


def checked_sweep_partition(ids: list[str], ignored: set[str] | frozenset[str] = frozenset(),
                            replay: set[str] | frozenset[str] = frozenset(),
                            nested: tuple[tuple[str, str], ...] = ()) -> list[list[str]]:
    """Independently prove exact union, multiplicity and runnable subrun membership."""
    parts = sweep_partition(ids, ignored, replay, nested)
    combined = [test_id for part in parts for test_id in part]
    runnable = set(ids) - ignored
    judged = runnable - replay
    if not ids or len(set(ids)) != len(ids) or not parts or not all(parts) \
            or len(parts) != min(SWEEP_SHARD_COUNT, len(sweep_runnable_groups(ids, ignored, nested)) or len(ids)) \
            or Counter(combined) != Counter(ids) \
            or (runnable and any(not (set(part) - ignored) for part in parts)) \
            or (judged and any((set(part) & (ignored - replay)) and not (set(part) & judged)
                               for part in parts)):
        raise ValueError("sweep partition has missing, duplicate, extra, empty or ignored-only test sets")
    owners = {test_id: index for index, part in enumerate(parts) for test_id in part}
    for parent, child in nested:
        if parent in owners and owners.get(child) != owners[parent]:
            raise ValueError(f"nested child {child} must share the shard of {parent}")
    print(f"SWEEP_PARTITION_OK expected={len(ids)} shards={len(parts)} "
          f"sizes={','.join(str(len(part)) for part in parts)} "
          f"ids={digest(chr(10).join(sorted(ids)).encode())}", file=sys.stderr)
    return parts


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


def run_lane(root: Path, lane: str, ids: list[str], command: list[str], env=None) -> int:
    """run_test_lane over exactly `ids`: a missing, extra, undeclared-ignored id, a summary
    that disagrees, an empty expectation or a run that executes none is red."""
    with tempfile.NamedTemporaryFile("w", suffix=".ids", delete=False) as handle:
        handle.write("\n".join(ids) + "\n")
    summaries = 1 + len({child for parent, child in nested_pairs(root) if parent in ids})
    return subprocess.run(
        [sys.executable, "scripts/run_test_lane.py", "--lane", lane, "--max-summaries", str(summaries),
         "--nested-declarations", str(root / "scripts/ci/nested_libtest_declarations.json"),
         "--expected-ids", handle.name, "--", *command], cwd=root, env=env,
        check=False).returncode


def run_pg(root: Path, ids: list[str]) -> int:
    """Run only this numeric shard's planned ids, or the independent continuity recipe."""
    pgm = tools()[2]
    raw_shard = os.environ.get("PG_INCLUDE_SHARD", "")
    if not os.environ.get("POSTGRES_TEST_DATABASE_URL_BASE"):
        raise ValueError("no PostgreSQL test server is named")
    env = {key: value for key, value in os.environ.items() if key != "PG_INCLUDE_SHARD"}
    if raw_shard == "continuity":
        return subprocess.run([*CONTINUITY], cwd=root, env=env, check=False).returncode
    shard = pgm.validate_pg_shard(raw_shard)
    mine = [test_id for test_id in ids if pgm.pg_test_shard(test_id) == shard]
    if not mine:
        raise ValueError(f"shard {shard} selects no test")
    # A shard whose every planned id is #[ignore] must not become a zero-execution pass.
    listed = subprocess.run(
        ["bash", "-c", PREPARED + ' --exact "$@" --list --ignored', "bash", *mine],
        cwd=root, env=env, check=True, stdout=subprocess.PIPE, text=True).stdout
    ignored = set(pgm.partition_pg_test_list(listed, str(shard), allow_empty=True))
    if ignored >= set(mine):
        raise ValueError(f"shard {shard}: every planned id is #[ignore]")
    command = ["bash", "-c", PREPARED + ' --exact "$@" --test-threads=1', "bash", *mine]
    return run_lane(root, "pg", mine, command, env)


def run_sweep(root: Path, ids: list[str]) -> int:
    """Planned ids under run_test_lane, except NON_PG_FILTER_REPLAY ids: main replays those
    outside the failure ledger, so they run that way here; each must still report a verdict
    and may report `ignored` only if the ledger declares that id ignored."""
    runner, coverage = tools()[:2]
    replay = set(coverage.load_non_pg_filter_replay(root))
    judged = [test_id for test_id in ids if test_id not in replay]
    replayed = [test_id for test_id in ids if test_id in replay]
    if judged and (rc := run_lane(root, "non-pg-sweep", judged, [
            "env", "-u", "AGENTDESK_ROOT_DIR", "python3", "scripts/ci/lib-test-artifact.py", "run",
            "--", "cargo", "test", "--lib", "--", "--exact", *judged])):
        return rc
    if not replayed:
        return 0
    rc, outcome = runner.run_and_capture(
        ["bash", "-c", f'cd {shlex.quote(str(root))} && {PREPARED} --exact "$@"', "bash",
         *replayed], None)
    missing = sorted(set(replayed) - outcome.executed - outcome.ignored)
    held = outcome.ignored & set(replayed)
    undeclared = sorted(held - declared_ignored(root)) if held else []
    for what, bad in (("without a verdict", missing), ("ignored without a ledger row", undeclared)):
        if bad:
            print(f"ERROR test-selection sweep: replayed ids {what}: {' '.join(bad)}",
                  file=sys.stderr)
    empty = not judged and not (outcome.executed & set(replayed))
    if empty:
        print("ERROR test-selection sweep: no planned test executed", file=sys.stderr)
    return rc or int(bool(missing or undeclared or empty))


def declared_ignored(root: Path) -> set[str]:
    """Ids the ledger declares `ignored` for the pg or non-pg-sweep lane; malformed is red."""
    runner, today, declared = tools()[0], datetime.date.today(), set()
    for lane in ("pg", "non-pg-sweep"):
        entries, errors = runner.parse_ledger(root / LEDGER, lane, today)
        if errors:
            raise ValueError(f"{LEDGER}: {errors[0]}")
        declared.update(entry.pattern for entry in entries if entry.mode == "ignored")
    return declared


def sweep_partition_inputs(root: Path):
    return declared_ignored(root), set(tools()[1].load_non_pg_filter_replay(root)), nested_pairs(root)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    plan_parser = commands.add_parser("plan")
    plan_parser.add_argument("--root", type=Path, default=ROOT)
    plan_parser.add_argument("--base")
    plan_parser.add_argument("--head", default="HEAD")
    run_parser = commands.add_parser("run")
    run_parser.add_argument("--lane", choices=LANES, required=True)
    run_parser.add_argument("--shard", type=int)
    run_parser.add_argument("--full", action="store_true")
    commands.add_parser("sweep-matrix")
    args = parser.parse_args(argv)
    if args.command == "sweep-matrix":
        try:
            parts = checked_sweep_partition(sweep_population(ROOT), *sweep_partition_inputs(ROOT))
            matrix = json.dumps({"shard": list(range(len(parts)))}, separators=(",", ":"))
            print(matrix)
            return 0
        except (OSError, ValueError) as error:
            print(f"ERROR sweep-matrix: {error}", file=sys.stderr)
            return 1
    if args.command == "plan":
        plan = make_plan(args.root.resolve(), args.base, args.head)
        for lane in LANES:
            log(plan, lane)
        print(f"test-selection: scopes={len(plan['scopes'])} {' '.join(plan['scopes'][:40])}",
              file=sys.stderr)
        text = json.dumps(plan, separators=(",", ":"), sort_keys=True)
        matrix = json.dumps({"shard": plan["lanes"]["pg"]["shards"]}, separators=(",", ":"))
        root = args.root.resolve()
        ids = (sweep_population(root) if plan["lanes"]["sweep"]["mode"] == "full"
               else Inventory(root).selected(plan["scopes"], "sweep"))
        sweep_matrix = json.dumps({"shard": list(range(len(checked_sweep_partition(ids, *sweep_partition_inputs(root)))))},
                                  separators=(",", ":"))
        force = str(plan["force_full"]).lower()
        if output := os.environ.get("GITHUB_OUTPUT"):
            with open(output, "a", encoding="utf-8") as handle:
                handle.write(f"matrix={matrix}\nsweep_matrix={sweep_matrix}\n"
                             f"plan={text}\nforce_full={force}\n")
        else:
            print(text)
        return 0
    try:
        if args.full and (args.lane != "sweep" or args.shard is None):
            raise ValueError("--full requires a sweep shard")
        if args.shard is not None:
            if args.lane != "sweep":
                raise ValueError("--shard is only supported for the sweep lane")
            entry, ids = ({"mode": "full"}, []) if args.full else load_plan(ROOT, "sweep")
            if entry["mode"] == "full":
                ids = sweep_population(ROOT)
            parts = checked_sweep_partition(ids, *sweep_partition_inputs(ROOT))
            if os.environ.get("SWEEP_SHARD_TOTAL") != str(len(parts)):
                raise ValueError(f"SWEEP_SHARD_TOTAL must match planned shard count {len(parts)}")
            if not 0 <= args.shard < len(parts):
                raise ValueError(f"sweep shard {args.shard} is not in this partition")
            return run_sweep(ROOT, parts[args.shard])
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
