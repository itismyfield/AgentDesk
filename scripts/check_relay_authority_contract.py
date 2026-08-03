#!/usr/bin/env python3
"""Run the explicitly declared relay-authority contract lanes and enforce floors."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Sequence

DEFAULT_MANIFEST = Path("scripts/relay_authority_contract_targets.json")
CONDITION3_MUTATION_SCRIPT = Path("scripts/run_relay_authority_mutations.sh")
TEST_ID_SUFFIX = ": test"


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
        script_exists = mutation_script.is_file()
        if mutations_present and not script_exists:
            raise ManifestError(
                f"condition3_mutations_present is true but {CONDITION3_MUTATION_SCRIPT} is missing"
            )
        if not mutations_present and script_exists:
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
    return active, gaps


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
    import shlex

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
    except ManifestError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2

    print(
        f"relay-authority manifest: active={len(lanes)} gaps={len(gaps)} "
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
