#!/usr/bin/env python3
"""Run the explicitly declared relay-authority contract lanes and enforce floors."""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Sequence

import yaml

DEFAULT_MANIFEST = Path("scripts/relay_authority_contract_targets.json")
PR_WORKFLOW = Path(".github/workflows/ci-pr.yml")
RELAY_AUTHORITY_JOB = "relay-authority-contract"
RELAY_AUTHORITY_TARGETS_JOB = "relay_authority_targets"
RELAY_AUTHORITY_MUTATIONS_JOB = "relay_authority_mutations"
CONDITION3_MUTATION_SCRIPT = Path("scripts/run_relay_authority_mutations.sh")
CONDITION3_MUTATION_COMMAND = f"bash {CONDITION3_MUTATION_SCRIPT}"
RELAY_TARGET_STEP = "Run named relay-authority contract targets"
# An absent filter or wiring output must run mutations; only two explicit falses skip them.
CONDITION3_MUTATION_IF = (
    "steps.mutation_paths.outputs.mutation_sources != 'false'"
    " || steps.mutation_wiring.outputs.wiring_changed != 'false'"
)
TEST_ID_SUFFIX = ": test"
LIB_TEST_INVENTORY = Path("scripts/lib_test_inventory_manifest.txt")
MUTATION_FILTER_STEP_ID = "mutation_paths"
SURFACE_GUARD_KINDS = ("mutation_row", "named_target", "entry_test", "known_gap")


class ManifestError(ValueError):
    """The checked-in relay-authority lane manifest is invalid."""


@dataclass(frozen=True)
class Lane:
    name: str
    boundary: str
    module: str
    command: tuple[str, ...]
    minimum: int


@dataclass(frozen=True)
class LaneResult:
    lane: Lane
    selected: int
    returncode: int
    command: tuple[str, ...]
    output: str


def load_relay_authority_job(
    repo_root: Path,
    job_id: str = RELAY_AUTHORITY_TARGETS_JOB,
) -> dict[str, object]:
    workflow = repo_root / PR_WORKFLOW
    try:
        payload = yaml.safe_load(workflow.read_text(encoding="utf-8"))
    except (OSError, yaml.YAMLError) as error:
        raise ManifestError(f"cannot read workflow {PR_WORKFLOW}: {error}") from error
    jobs = payload.get("jobs") if isinstance(payload, dict) else None
    job = jobs.get(job_id) if isinstance(jobs, dict) else None
    if not isinstance(job, dict):
        raise ManifestError(
            f"workflow {PR_WORKFLOW} must contain jobs.{job_id}"
        )
    return job


def expected_workflow_command(lane: Lane) -> str:
    return f"env -u AGENTDESK_ROOT_DIR {shlex.join(lane.command)} -- --test-threads=1"


def validate_workflow_contract(
    repo_root: Path,
    lanes: Sequence[Lane],
    mutations_present: bool,
) -> None:
    publisher = load_relay_authority_job(repo_root, RELAY_AUTHORITY_JOB)
    if (
        publisher.get("if") != "${{ !cancelled() }}"
        or publisher.get("needs") != [RELAY_AUTHORITY_TARGETS_JOB, RELAY_AUTHORITY_MUTATIONS_JOB]
        or "continue-on-error" in publisher
    ):
        raise ManifestError(
            f"workflow jobs.{RELAY_AUTHORITY_JOB} must always publish both execution results"
        )
    for job_id in (RELAY_AUTHORITY_TARGETS_JOB, RELAY_AUTHORITY_MUTATIONS_JOB):
        execution = load_relay_authority_job(repo_root, job_id)
        if any(key in execution for key in ("if", "needs", "continue-on-error")):
            raise ManifestError(f"workflow jobs.{job_id} must execute unconditionally without needs")

    job = load_relay_authority_job(repo_root)
    steps = job.get("steps")
    if not isinstance(steps, list):
        raise ManifestError(
            f"workflow jobs.{RELAY_AUTHORITY_TARGETS_JOB}.steps must be an array"
        )

    target_steps = [
        step for step in steps
        if isinstance(step, dict) and step.get("name") == RELAY_TARGET_STEP
    ]
    if len(target_steps) != 1:
        raise ManifestError(
            f"workflow jobs.{RELAY_AUTHORITY_TARGETS_JOB} must contain exactly one "
            f"{RELAY_TARGET_STEP!r} step"
        )
    if any(key in target_steps[0] for key in ("if", "continue-on-error")):
        raise ManifestError(f"workflow {RELAY_TARGET_STEP!r} step must be unconditional and fail closed")
    run = target_steps[0].get("run")
    actual_commands = (
        [line.strip() for line in run.splitlines() if line.strip()]
        if isinstance(run, str)
        else []
    )
    expected_commands = [expected_workflow_command(lane) for lane in lanes]
    if actual_commands != expected_commands:
        raise ManifestError(
            f"workflow jobs.{RELAY_AUTHORITY_TARGETS_JOB} target commands must exactly match "
            "the manifest argv with AGENTDESK_ROOT_DIR unset and "
            "-- --test-threads=1 appended"
        )

    if not mutations_present:
        return
    mutation_job = load_relay_authority_job(repo_root, RELAY_AUTHORITY_MUTATIONS_JOB)
    strategy = mutation_job.get("strategy")
    if strategy != {"fail-fast": False, "matrix": {"shard": [0, 1, 2]}}:
        raise ManifestError(
            f"workflow jobs.{RELAY_AUTHORITY_MUTATIONS_JOB} must execute all three mutation shards"
        )
    steps = mutation_job.get("steps")
    if not isinstance(steps, list):
        raise ManifestError(f"workflow jobs.{RELAY_AUTHORITY_MUTATIONS_JOB}.steps must be an array")
    mutation_steps = [
        step for step in steps
        if isinstance(step, dict)
        and step.get("run") == CONDITION3_MUTATION_COMMAND
        and step.get("if", CONDITION3_MUTATION_IF) == CONDITION3_MUTATION_IF
        and not step.get("continue-on-error")
    ]
    if len(mutation_steps) != 1:
        raise ManifestError(
            f"condition3_mutations_present is true but workflow "
            f"jobs.{RELAY_AUTHORITY_MUTATIONS_JOB} must contain exactly one unconditional "
            f"run step invoking {CONDITION3_MUTATION_COMMAND}, or one guarded by "
            f"exactly {CONDITION3_MUTATION_IF!r}"
        )
    env = mutation_job.get("env")
    if (
        not isinstance(env, dict)
        or env.get("RELAY_AUTHORITY_MUTATION_SHARD_INDEX") != "${{ matrix.shard }}"
        or str(env.get("RELAY_AUTHORITY_MUTATION_SHARD_TOTAL")) != "3"
    ):
        raise ManifestError("workflow mutation job must pass its shard index and total")


def validate_condition3_script(mutation_script: Path) -> None:
    # This proves only that the checked path is a non-symlink, non-empty,
    # executable regular file. It does not prove that the script mutates the
    # intended contract or that its assertions are effective.
    if mutation_script.is_symlink():
        raise ManifestError(
            f"{CONDITION3_MUTATION_SCRIPT} must not be a symlink"
        )
    if not mutation_script.is_file():
        raise ManifestError(
            f"condition3_mutations_present is true but {CONDITION3_MUTATION_SCRIPT} is missing"
        )
    if mutation_script.stat().st_size == 0:
        raise ManifestError(f"{CONDITION3_MUTATION_SCRIPT} must not be empty")
    if not os.access(mutation_script, os.X_OK):
        raise ManifestError(f"{CONDITION3_MUTATION_SCRIPT} must be executable")
    commands = [
        line.strip()
        for line in mutation_script.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if commands == ["exit 0"]:
        raise ManifestError(
            f"{CONDITION3_MUTATION_SCRIPT} must not be an exit-0-only placeholder"
        )


def load_active_lanes(
    path: Path,
    repo_root: Path | None = None,
) -> tuple[list[Lane], list[dict[str, object]]]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ManifestError(f"cannot read manifest {path}: {error}") from error

    if payload.get("schema_version") != 1 or not isinstance(payload.get("lanes"), list):
        raise ManifestError("manifest must contain schema_version=1 and a lanes array")

    mutations_present = payload.get("condition3_mutations_present")
    if not isinstance(mutations_present, bool):
        raise ManifestError("manifest condition3_mutations_present must be boolean")
    if repo_root is not None:
        mutation_script = repo_root / CONDITION3_MUTATION_SCRIPT
        script_path_present = mutation_script.exists() or mutation_script.is_symlink()
        if mutations_present:
            validate_condition3_script(mutation_script)
        elif script_path_present:
            raise ManifestError(
                f"condition3_mutations_present is false but {CONDITION3_MUTATION_SCRIPT} exists"
            )

    active: list[Lane] = []
    gaps: list[dict[str, object]] = []
    names: set[str] = set()
    for index, raw in enumerate(payload["lanes"]):
        if not isinstance(raw, dict):
            raise ManifestError(f"lane {index} must be an object")
        name = raw.get("name")
        status = raw.get("status")
        if not isinstance(name, str) or not name:
            raise ManifestError(f"lane {index} has no non-empty name")
        if name in names:
            raise ManifestError(f"duplicate lane name: {name}")
        names.add(name)
        if status == "gap":
            if not isinstance(raw.get("reason"), str) or not raw["reason"]:
                raise ManifestError(f"gap lane {name} must state a reason")
            gaps.append(raw)
            continue
        if status != "active":
            raise ManifestError(f"lane {name} has unsupported status {status!r}")

        command = raw.get("command")
        minimum = raw.get("minimum")
        module = raw.get("module")
        boundary = raw.get("boundary")
        if (
            not isinstance(command, list)
            or not command
            or not all(isinstance(item, str) and item for item in command)
        ):
            raise ManifestError(f"active lane {name} must have a non-empty command array")
        if command[:2] != ["cargo", "test"]:
            raise ManifestError(f"active lane {name} command must begin with cargo test")
        if "--lib" not in command or "--all-targets" in command:
            raise ManifestError(f"active lane {name} must select --lib and may not use --all-targets")
        if "--" in command:
            raise ManifestError(f"active lane {name} command must omit the libtest separator")
        filters = [item for item in command[2:] if not item.startswith("-")]
        if not filters:
            raise ManifestError(f"active lane {name} must contain an explicit test filter")
        if not isinstance(minimum, int) or isinstance(minimum, bool) or minimum < 1:
            raise ManifestError(f"active lane {name} minimum must be an integer >= 1")
        if not isinstance(module, str) or not module:
            raise ManifestError(f"active lane {name} must name its source module")
        if not isinstance(boundary, str) or not boundary:
            raise ManifestError(f"active lane {name} must name its boundary")
        active.append(Lane(name, boundary, module, tuple(command), minimum))

    if not active:
        raise ManifestError("manifest must declare at least one active lane")
    if repo_root is not None:
        validate_workflow_contract(repo_root, active, mutations_present)
    return active, gaps


def script_mutation_files(script: str) -> set[str]:
    """The MUTATION_FILES array as the mutation script declares it."""
    constants = dict(re.findall(r'^readonly ([A-Z0-9_]+)="([^"]+)"$', script, re.M))
    body = re.search(r"^readonly -a MUTATION_FILES=\(\n(.*?)^\)$", script, re.M | re.S)
    if body is None:
        raise ManifestError(f"{CONDITION3_MUTATION_SCRIPT} must declare a MUTATION_FILES array")
    names = re.findall(r'"\$([A-Z0-9_]+)"', body.group(1))
    if not names or any(name not in constants for name in names):
        raise ManifestError(f"{CONDITION3_MUTATION_SCRIPT} MUTATION_FILES must name readonly path constants")
    return {constants[name] for name in names}


def mutation_filter_patterns(repo_root: Path) -> set[str]:
    job = load_relay_authority_job(repo_root, RELAY_AUTHORITY_MUTATIONS_JOB)
    for step in job.get("steps") or []:
        if isinstance(step, dict) and step.get("id") == MUTATION_FILTER_STEP_ID:
            filters = yaml.safe_load(str((step.get("with") or {}).get("filters", "")))
            if isinstance(filters, dict) and isinstance(filters.get("mutation_sources"), list):
                return set(filters["mutation_sources"])
    raise ManifestError(f"workflow jobs.{RELAY_AUTHORITY_MUTATIONS_JOB} must keep its {MUTATION_FILTER_STEP_ID} filter")


def lib_test_ids(repo_root: Path) -> set[str]:
    try:
        lines = (repo_root / LIB_TEST_INVENTORY).read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise ManifestError(f"cannot read {LIB_TEST_INVENTORY}: {error}") from error
    return {line for line in lines if line and not line.startswith(("#", "["))}


def validate_authority_surface(payload: dict[str, object], repo_root: Path) -> int:
    """Each declared authority path must exist and name guards that resolve, and every mutation row
    and active lane must be declared. Authority cannot be read off a path, so new files enter by review."""
    surface = payload.get("authority_surface")
    if surface is None and payload.get("condition3_mutations_present") is not True:
        return 0
    if not isinstance(surface, list) or not surface:
        raise ManifestError("manifest must declare a non-empty authority_surface")
    rows = {row.get("name"): row for row in payload.get("condition3_mutations") or [] if isinstance(row, dict)}
    lanes = {lane.get("name"): lane for lane in payload.get("lanes") or [] if isinstance(lane, dict)}
    test_ids: set[str] | None = None
    declared_rows: list[str] = []
    named: set[str] = set()
    paths: set[str] = set()
    for index, entry in enumerate(surface):
        path = entry.get("path") if isinstance(entry, dict) else None
        guards = entry.get("guards") if isinstance(entry, dict) else None
        if not isinstance(path, str) or not path or not isinstance(entry.get("decides"), str) or not entry["decides"]:
            raise ManifestError(f"authority_surface entry {index} needs a path and what it decides")
        if path in paths:
            raise ManifestError(f"authority_surface declares {path} twice")
        paths.add(path)
        source = repo_root / path
        if source.is_symlink() or not source.is_file():
            raise ManifestError(f"authority_surface path {path} is not a regular file; reclassify it")
        if not isinstance(guards, list) or not guards:
            raise ManifestError(f"authority_surface {path} must name at least one guard")
        for guard in guards:
            kind, _, value = str(guard).partition(":")
            if kind not in SURFACE_GUARD_KINDS or not value:
                raise ManifestError(f"authority_surface {path} has unknown guard {guard!r}")
            if kind == "mutation_row":
                if rows.get(value, {}).get("file") != path:
                    raise ManifestError(f"authority_surface {path}: mutation row {value} does not mutate it")
                declared_rows.append(value)
            elif kind == "named_target":
                if lanes.get(value, {}).get("status") != "active":
                    raise ManifestError(f"authority_surface {path}: {value} is not an active lane")
                named.add(value)
            elif kind == "entry_test":
                test_ids = lib_test_ids(repo_root) if test_ids is None else test_ids
                if value not in test_ids:
                    raise ManifestError(f"authority_surface {path}: entry test {value} is not in {LIB_TEST_INVENTORY}")
            elif not re.fullmatch(r"#[1-9][0-9]*", value):
                raise ManifestError(f"authority_surface {path}: known_gap must cite an issue as #<number>")

    if sorted(declared_rows) != sorted(name for name in rows if isinstance(name, str)):
        raise ManifestError("authority_surface must declare every condition3 mutation row exactly once")
    undeclared_lanes = sorted(name for name, lane in lanes.items() if lane.get("status") == "active" and name not in named)
    if undeclared_lanes:
        raise ManifestError(f"authority_surface does not declare active lanes {undeclared_lanes}")
    mutated = {row["file"] for row in rows.values()}
    script = (repo_root / CONDITION3_MUTATION_SCRIPT).read_text(encoding="utf-8")
    if script_mutation_files(script) != mutated:
        raise ManifestError(f"{CONDITION3_MUTATION_SCRIPT} MUTATION_FILES must equal the condition3 row files")
    judges = {row.get("judge") for row in rows.values()}
    if not all(isinstance(judge, str) and (repo_root / judge).is_file() for judge in judges):
        raise ManifestError("every condition3 mutation row must name an existing judge file")
    missing = sorted((mutated | judges) - mutation_filter_patterns(repo_root))
    if missing:
        raise ManifestError(f"mutation path filter must select every mutated and judging file; missing {missing}")
    return len(surface)


def count_test_ids(output: str) -> int:
    return sum(1 for line in output.splitlines() if line.strip().endswith(TEST_ID_SUFFIX))


def list_command(lane: Lane) -> tuple[str, ...]:
    return lane.command + ("--", "--list")


def run_lane(
    lane: Lane,
    repo_root: Path,
    runner: Callable[..., subprocess.CompletedProcess[str]] = subprocess.run,
) -> LaneResult:
    command = list_command(lane)
    env = os.environ.copy()
    env.pop("AGENTDESK_ROOT_DIR", None)
    proc = runner(
        command,
        cwd=repo_root,
        env=env,
        capture_output=True,
        text=True,
    )
    output = f"{proc.stdout}{proc.stderr}"
    return LaneResult(lane, count_test_ids(proc.stdout), proc.returncode, command, output)


def failures_for(result: LaneResult) -> list[str]:
    failures: list[str] = []
    if result.returncode != 0:
        failures.append(f"cargo list command exited {result.returncode}")
    if result.selected == 0:
        failures.append("selected 0 tests")
    if result.selected < result.lane.minimum:
        failures.append(
            f"selected {result.selected} below declared minimum {result.lane.minimum}"
        )
    return failures


def shell_join(command: Sequence[str]) -> str:
    return shlex.join(command)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parents[1],
    )
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument(
        "--check-manifest",
        action="store_true",
        help="validate declarations without invoking cargo",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    repo_root = args.repo_root.resolve()
    manifest = args.manifest
    if not manifest.is_absolute():
        manifest = repo_root / manifest

    try:
        lanes, gaps = load_active_lanes(manifest, repo_root)
        surface = validate_authority_surface(json.loads(manifest.read_text(encoding="utf-8")), repo_root)
    except ManifestError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2

    print(
        f"relay-authority manifest: active={len(lanes)} gaps={len(gaps)} surface={surface} "
        f"path={manifest.relative_to(repo_root) if manifest.is_relative_to(repo_root) else manifest}"
    )
    for gap in gaps:
        print(
            f"GAP boundary={gap['boundary']} lane={gap['name']} "
            f"module={gap['module']}: {gap['reason']}"
        )
    if args.check_manifest:
        return 0

    failed = False
    for lane in lanes:
        result = run_lane(lane, repo_root)
        failures = failures_for(result)
        print(
            f"selection boundary={lane.boundary} lane={lane.name} "
            f"selected={result.selected} minimum={lane.minimum} "
            f"rc={result.returncode} command={shell_join(result.command)}"
        )
        if failures:
            failed = True
            print(
                f"ERROR: lane {lane.name}: {'; '.join(failures)}",
                file=sys.stderr,
            )
            if result.output:
                print(result.output[-4000:], file=sys.stderr)
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
