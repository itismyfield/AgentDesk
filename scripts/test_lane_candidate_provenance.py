"""Candidate/base and PR workflow provenance helpers for test-lane coverage."""

from __future__ import annotations

import io
import re
import subprocess
import tarfile
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable, Protocol

PR_LANE_MANIFEST_REL = Path("scripts/pr_test_lane_manifest.txt")
PR_PROVENANCE_REL = "pr-ci"


class Lane(Protocol):
    command: str
    positives: tuple[str, ...]
    skips: tuple[str, ...]
    exact: bool
    provenance: str

    def is_applicable_to(self, changed_paths: Iterable[str]) -> bool: ...

    def selects(self, test_name: str) -> bool: ...


class Inventory(Protocol):
    tests_by_module: dict[str, set[str]]
    test_fingerprints: dict[str, str]
    unsupported_tests: tuple[str, ...]
    test_modules: dict[str, str]


@dataclass(frozen=True)
class ChangedTest:
    name: str
    module: str
    change: str


@dataclass(frozen=True)
class PrLaneManifest:
    lanes: tuple[Lane, ...]
    test_paths: tuple[str, ...]
    witnesses: tuple[tuple[str, str, int], ...]


@dataclass(frozen=True)
class CheckResult:
    changed_tests: tuple[ChangedTest, ...]
    failures: tuple[str, ...]
    coverage: dict[str, tuple[str, ...]]


def load_pr_lane_manifest(
    path: Path,
    cargo_test_filter: Callable[..., Lane | None],
) -> PrLaneManifest:
    """Load the reviewed PR cargo-test surface without parsing workflow YAML."""
    lanes: list[Lane] = []
    witnesses: list[tuple[str, str, int]] = []
    all_paths: set[str] = set()
    for line_number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split(" :: ")
        if line.startswith("witness ") and len(parts) == 3:
            raw_file, raw_count, command = parts
            workflow_file = raw_file.removeprefix("witness ").strip()
            try:
                expected_count = int(raw_count)
            except ValueError as exc:
                raise ValueError(
                    f"invalid PR lane witness count at {path}:{line_number}"
                ) from exc
            if not workflow_file or expected_count < 1 or not command.strip():
                raise ValueError(f"invalid PR lane witness at {path}:{line_number}")
            witnesses.append((workflow_file, command.strip(), expected_count))
            continue
        if not line.startswith("lane ") or len(parts) != 3:
            raise ValueError(f"invalid PR lane manifest entry at {path}:{line_number}")
        label, raw_paths, command = line.removeprefix("lane ").split(" :: ", 2)
        patterns = tuple(
            pattern.strip() for pattern in raw_paths.split(",") if pattern.strip()
        )
        if not label.strip() or not patterns or not command.strip():
            raise ValueError(f"invalid PR lane manifest entry at {path}:{line_number}")
        all_paths.update(patterns)
        lane = cargo_test_filter(
            command.strip(),
            provenance=f"{PR_PROVENANCE_REL}:{label.strip()}",
            changed_paths=patterns,
        )
        if lane is None:
            raise ValueError(
                "manifest lane is not a supported library/all-target cargo test: "
                f"{path}:{line_number}"
            )
        lanes.append(lane)
    if not lanes or not witnesses:
        raise ValueError(
            f"PR lane manifest must define lanes and workflow witnesses: {path}"
        )
    return PrLaneManifest(
        tuple(lanes), tuple(sorted(all_paths)), tuple(witnesses)
    )


def _git(
    repo_root: Path, args: list[str], *, binary: bool = False
) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", *args],
        cwd=repo_root,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=not binary,
        check=False,
    )


def resolve_commit(repo_root: Path, revision: str) -> str:
    result = _git(repo_root, ["rev-parse", "--verify", f"{revision}^{{commit}}"])
    if result.returncode != 0:
        detail = result.stderr.strip() or "unknown revision"
        raise ValueError(f"base commit is inaccessible: {revision}: {detail}")
    return result.stdout.strip()


def changed_paths(repo_root: Path, base_sha: str) -> tuple[str, ...]:
    result = _git(repo_root, ["diff", "--name-only", base_sha, "HEAD", "--"])
    if result.returncode != 0:
        raise ValueError(f"cannot diff checked-out candidate from {base_sha}")
    return tuple(path for path in result.stdout.splitlines() if path)


def source_tree_at_commit(
    repo_root: Path, commit: str
) -> tempfile.TemporaryDirectory:
    archive = _git(repo_root, ["archive", "--format=tar", commit, "src"], binary=True)
    if archive.returncode != 0:
        detail = archive.stderr.decode("utf-8", errors="replace").strip()
        raise ValueError(f"cannot read source tree at {commit}: {detail}")
    temp = tempfile.TemporaryDirectory()
    with tarfile.open(fileobj=io.BytesIO(archive.stdout), mode="r:") as bundle:
        try:
            bundle.extractall(temp.name, filter="data")
        except TypeError:  # Python <3.12 compatibility for local tooling.
            bundle.extractall(temp.name)
    return temp


def changed_tests_between(
    base: Inventory, candidate: Inventory
) -> tuple[ChangedTest, ...]:
    changed: list[ChangedTest] = []
    for name, fingerprint in candidate.test_fingerprints.items():
        previous = base.test_fingerprints.get(name)
        module = candidate.test_modules.get(name)
        if module is None:
            continue
        if previous is None:
            changed.append(ChangedTest(name, module, "added"))
        elif previous != fingerprint:
            changed.append(ChangedTest(name, module, "modified"))
    return tuple(sorted(changed, key=lambda item: item.name))


def ensure_non_vacuous_filters(
    lanes: Iterable[Lane], inventory: dict[str, set[str]] | Inventory
) -> tuple[str, ...]:
    if hasattr(inventory, "test_fingerprints"):
        all_tests = set(inventory.test_fingerprints)
    else:
        all_tests = set().union(*inventory.values()) if inventory else set()
    failures: list[str] = []
    for lane in lanes:
        for positive in lane.positives:
            selected = [
                name
                for name in all_tests
                if (name == positive if lane.exact else positive in name)
                and not any(skip in name for skip in lane.skips)
            ]
            if not selected:
                mode = "exact" if lane.exact else "positive"
                failures.append(
                    f"{lane.provenance}: {mode} filter {positive!r} selects zero tests"
                )
    return tuple(failures)


def verify_pr_lane_manifest(
    repo_root: Path,
    manifest_path: Path,
    cargo_test_filter: Callable[..., Lane | None],
    just_recipe_commands: Callable[[str, str], tuple[str, ...]],
) -> PrLaneManifest:
    manifest = load_pr_lane_manifest(manifest_path, cargo_test_filter)
    direct_witnesses: set[str] = set()
    indirect_commands: set[str] = set()
    workflow_recipe_witnesses: set[str] = set()
    for relative_path, text, expected_count in manifest.witnesses:
        source_path = repo_root / relative_path
        actual_count = source_path.read_text(encoding="utf-8").count(text)
        if actual_count != expected_count:
            raise ValueError(
                f"PR lane witness drift: {relative_path} expected {expected_count} "
                f"occurrence(s) of {text!r}, found {actual_count}"
            )
        if "cargo test" in text:
            command = text[text.find("cargo test") :]
            if relative_path.startswith(".github/workflows/"):
                direct_witnesses.add(command)
            else:
                indirect_commands.add(command)
        if relative_path.startswith(".github/workflows/") and "just " in text:
            workflow_recipe_witnesses.update(
                re.findall(r"\bjust\s+([A-Za-z0-9_-]+)", text)
            )
    just_text = (repo_root / "justfile").read_text(encoding="utf-8")
    indirect_witnesses = {
        command
        for recipe in workflow_recipe_witnesses
        for command in just_recipe_commands(just_text, recipe)
        if command in indirect_commands
    }
    for lane in manifest.lanes:
        if lane.command not in direct_witnesses | indirect_witnesses:
            raise ValueError(
                f"PR lane has no exact workflow witness: {lane.provenance}: "
                f"{lane.command!r}"
            )
    return manifest


def check_pr_candidate(
    repo_root: Path,
    base_sha: str,
    *,
    discover_source_inventory: Callable[[Path], Inventory],
    cargo_test_filter: Callable[..., Lane | None],
    just_recipe_commands: Callable[[str, str], tuple[str, ...]],
    manifest_path: Path | None = None,
) -> CheckResult:
    """Verify changed candidate tests against executable PR-time lane provenance."""
    base = resolve_commit(repo_root, base_sha)
    candidate = resolve_commit(repo_root, "HEAD")
    manifest_path = manifest_path or repo_root / PR_LANE_MANIFEST_REL
    manifest = verify_pr_lane_manifest(
        repo_root, manifest_path, cargo_test_filter, just_recipe_commands
    )
    candidate_inventory = discover_source_inventory(repo_root)
    failures = list(ensure_non_vacuous_filters(manifest.lanes, candidate_inventory))
    if base == candidate:
        return CheckResult((), tuple(failures), {})

    paths = changed_paths(repo_root, base)
    changed_rust = tuple(
        path for path in paths if path.startswith("src/") and path.endswith(".rs")
    )
    failures.extend(
        reason
        for reason in candidate_inventory.unsupported_tests
        if reason.split(":", 1)[0] in changed_rust
    )

    with source_tree_at_commit(repo_root, base) as base_root:
        base_inventory = discover_source_inventory(Path(base_root))
    changed = changed_tests_between(base_inventory, candidate_inventory)
    unmounted_changed_tests = sorted(
        name
        for name in candidate_inventory.test_fingerprints
        if name not in candidate_inventory.test_modules
        and name not in base_inventory.test_fingerprints
        and any(
            path.endswith(f"/{name.split('::')[0]}.rs")
            or path == f"src/{name.split('::')[0]}.rs"
            for path in changed_rust
        )
    )
    failures.extend(
        f"unsupported changed test source has no logical cfg(test) module mount: {name}"
        for name in unmounted_changed_tests
    )

    coverage: dict[str, tuple[str, ...]] = {}
    for test in changed:
        owners = tuple(
            lane.provenance
            for lane in manifest.lanes
            if lane.is_applicable_to(paths) and lane.selects(test.name)
        )
        coverage[test.name] = owners
        if not owners:
            failures.append(
                f"{test.change} test {test.name} is not selected by an applicable PR lane"
            )
    return CheckResult(changed, tuple(failures), coverage)
