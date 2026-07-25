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
    target: str
    authority: str
    changed_paths: tuple[str, ...]

    def is_applicable_to(self, changed_paths: Iterable[str]) -> bool: ...

    def selects(self, test_name: str) -> bool: ...


class Inventory(Protocol):
    tests_by_module: dict[str, set[str]]
    test_fingerprints: dict[str, str]
    unsupported_tests: tuple[str, ...]
    test_modules: dict[str, str]
    test_sources: dict[str, str]


@dataclass(frozen=True)
class ChangedTest:
    name: str
    module: str
    change: str


@dataclass(frozen=True)
class PrLaneAuthority:
    name: str
    workflow: str
    job: str
    runner: str
    path_filter: str
    job_if: str
    required_job: str
    required_context: str


@dataclass(frozen=True)
class PrLaneWitness:
    authority: str
    source: str
    text: str
    expected_count: int


@dataclass(frozen=True)
class PrLaneManifest:
    lanes: tuple[Lane, ...]
    test_paths: tuple[str, ...]
    authorities: tuple[PrLaneAuthority, ...]
    witnesses: tuple[PrLaneWitness, ...]


@dataclass(frozen=True)
class CheckResult:
    changed_tests: tuple[ChangedTest, ...]
    failures: tuple[str, ...]
    coverage: dict[str, tuple[str, ...]]


def _normalize_repo_path(value: str) -> str:
    return value.replace("\\", "/")


def load_pr_lane_manifest(
    path: Path,
    cargo_test_filter: Callable[..., Lane | None],
) -> PrLaneManifest:
    """Load the reviewed PR cargo-test surface without parsing workflow YAML."""
    lanes: list[Lane] = []
    authorities: list[PrLaneAuthority] = []
    witnesses: list[PrLaneWitness] = []
    all_paths: set[str] = set()
    for line_number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split(" :: ")
        if line.startswith("authority ") and len(parts) == 8:
            (
                raw_name,
                workflow,
                job,
                runner,
                path_filter,
                job_if,
                required_job,
                context,
            ) = parts
            name = raw_name.removeprefix("authority ").strip()
            values = (
                name,
                workflow,
                job,
                runner,
                path_filter,
                job_if,
                required_job,
                context,
            )
            if not all(value.strip() for value in values):
                raise ValueError(f"invalid PR lane authority at {path}:{line_number}")
            authorities.append(
                PrLaneAuthority(
                    name,
                    _normalize_repo_path(workflow.strip()),
                    job.strip(),
                    runner.strip(),
                    path_filter.strip(),
                    job_if.strip(),
                    required_job.strip(),
                    context.strip(),
                )
            )
            continue
        if line.startswith("witness ") and len(parts) == 4:
            raw_authority, source, raw_count, text = parts
            authority = raw_authority.removeprefix("witness ").strip()
            try:
                expected_count = int(raw_count)
            except ValueError as exc:
                raise ValueError(
                    f"invalid PR lane witness count at {path}:{line_number}"
                ) from exc
            if not authority or not source.strip() or expected_count < 1 or not text.strip():
                raise ValueError(f"invalid PR lane witness at {path}:{line_number}")
            witnesses.append(
                PrLaneWitness(
                    authority,
                    _normalize_repo_path(source.strip()),
                    text.strip(),
                    expected_count,
                )
            )
            continue
        if not line.startswith("lane ") or len(parts) != 4:
            raise ValueError(f"invalid PR lane manifest entry at {path}:{line_number}")
        label, authority, raw_paths, command = line.removeprefix("lane ").split(
            " :: ", 3
        )
        patterns = tuple(
            _normalize_repo_path(pattern.strip())
            for pattern in raw_paths.split(",")
            if pattern.strip()
        )
        if not label.strip() or not authority.strip() or not patterns or not command.strip():
            raise ValueError(f"invalid PR lane manifest entry at {path}:{line_number}")
        all_paths.update(patterns)
        lane = cargo_test_filter(
            command.strip(),
            provenance=f"{PR_PROVENANCE_REL}:{label.strip()}",
            changed_paths=patterns,
            authority=authority.strip(),
        )
        if lane is None:
            raise ValueError(
                "manifest lane is not a supported library/all-target cargo test: "
                f"{path}:{line_number}"
            )
        lanes.append(lane)
    authority_names = [authority.name for authority in authorities]
    if len(authority_names) != len(set(authority_names)):
        raise ValueError(f"PR lane manifest authority names must be unique: {path}")
    if not lanes or not authorities or not witnesses:
        raise ValueError(
            f"PR lane manifest must define lanes, authorities, and witnesses: {path}"
        )
    known = set(authority_names)
    referenced = {lane.authority for lane in lanes} | {
        witness.authority for witness in witnesses
    }
    unknown = sorted(referenced - known)
    if unknown:
        raise ValueError(f"PR lane manifest references unknown authorities: {unknown}")
    return PrLaneManifest(
        tuple(lanes),
        tuple(sorted(all_paths)),
        tuple(authorities),
        tuple(witnesses),
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
        if previous == fingerprint:
            continue
        module = candidate.test_modules.get(name)
        if module is None:
            continue
        changed.append(
            ChangedTest(name, module, "added" if previous is None else "modified")
        )
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
        if lane.target.startswith("bin:"):
            failures.append(
                f"{lane.provenance}: target {lane.target!r} exposes no lib.rs tests"
            )
            continue
        for positive in lane.positives:
            selected = [
                name
                for name in all_tests
                if (name == positive if lane.exact else positive in name)
                and not any(
                    name == skip if lane.exact else skip in name
                    for skip in lane.skips
                )
            ]
            if not selected:
                mode = "exact" if lane.exact else "positive"
                failures.append(
                    f"{lane.provenance}: {mode} filter {positive!r} selects zero tests"
                )
    return tuple(failures)


def _yaml_scalar(value: str) -> str:
    stripped = value.strip()
    if len(stripped) >= 2 and stripped[0] == stripped[-1] and stripped[0] in "\"'":
        return stripped[1:-1]
    return stripped


def _mapping_block(source: str, key: str, indent: int) -> str:
    marker = re.compile(rf"(?m)^{' ' * indent}{re.escape(key)}:\s*$")
    match = marker.search(source)
    if match is None:
        raise ValueError(f"missing workflow mapping: {key}")
    sibling = re.compile(rf"(?m)^{' ' * indent}[A-Za-z0-9_-]+:\s*$").search(
        source, match.end()
    )
    return source[match.start() : sibling.start() if sibling else len(source)]


def _workflow_events(source: str) -> tuple[str, ...]:
    trigger = re.search(
        r"(?ms)^on:\s*\n(?P<body>.*?)(?=^[A-Za-z_][^\n]*:\s*$)", source
    )
    if trigger is None:
        return ()
    return tuple(
        re.findall(
            r"(?m)^  ([A-Za-z_][A-Za-z0-9_-]*):\s*$", trigger.group("body")
        )
    )


def _path_filter_patterns(source: str, filter_name: str) -> tuple[str, ...]:
    filter_match = re.search(
        rf"(?m)^            {re.escape(filter_name)}:\s*$", source
    )
    if filter_match is None:
        raise ValueError(f"missing workflow path filter: {filter_name}")
    sibling = re.compile(r"(?m)^            [A-Za-z0-9_-]+:\s*$").search(
        source, filter_match.end()
    )
    block = source[filter_match.end() : sibling.start() if sibling else len(source)]
    return tuple(
        _normalize_repo_path(match.group(1))
        for match in re.finditer(r"(?m)^              - ['\"]([^'\"]+)['\"]\s*$", block)
    )


def _require_authority(
    repo_root: Path, authority: PrLaneAuthority
) -> tuple[str, tuple[str, ...]]:
    if authority.workflow != ".github/workflows/ci-pr.yml":
        raise ValueError(
            f"PR lane authority must use protected ci-pr workflow: {authority.workflow}"
        )
    source_path = repo_root / Path(*authority.workflow.split("/"))
    source = source_path.read_text(encoding="utf-8")
    if _workflow_events(source) != ("pull_request",):
        raise ValueError(
            f"PR lane authority workflow must be pull_request-only: {authority.workflow}"
        )
    job = _mapping_block(source, authority.job, 2)
    if not re.search(r"(?m)^    needs:\s*changes\s*$", job):
        raise ValueError(f"PR lane authority job needs drift: {authority.name}")
    job_if = re.search(r"(?m)^    if:\s*(.+?)\s*$", job)
    if job_if is None or _yaml_scalar(job_if.group(1)) != authority.job_if:
        raise ValueError(f"PR lane authority job if drift: {authority.name}")
    runs_on = re.search(r"(?m)^    runs-on:\s*(.+?)\s*$", job)
    if runs_on is None or _yaml_scalar(runs_on.group(1)) != authority.runner:
        raise ValueError(f"PR lane authority runner drift: {authority.name}")
    if authority.runner not in {"ubuntu-latest", "${{ matrix.os }}"}:
        raise ValueError(f"PR lane authority job must use a hosted runner: {authority.name}")
    if authority.runner == "${{ matrix.os }}" and not re.search(
        r"(?m)^        os:\s*\[ubuntu-latest\]\s*$", job
    ):
        raise ValueError(f"PR lane authority matrix must be hosted Ubuntu: {authority.name}")

    required = _mapping_block(source, authority.required_job, 2)
    context = re.search(r"(?m)^    name:\s*(.+?)\s*$", required)
    if context is None or _yaml_scalar(context.group(1)) != authority.required_context:
        raise ValueError(f"PR lane required context drift: {authority.name}")
    needs = re.search(
        rf"(?m)^      - {re.escape(authority.job)}\s*$", required
    )
    if needs is None or not re.search(r"(?m)^    if:\s*always\(\)\s*$", required):
        raise ValueError(f"PR lane required mirror is not fail-closed: {authority.name}")
    if f"UPSTREAM_JOB_NAME: {authority.job}" not in required:
        raise ValueError(f"PR lane required mirror upstream drift: {authority.name}")
    filter_output = (
        f"FILTER_OUTPUT: ${{{{ needs.changes.outputs.{authority.path_filter} }}}}"
    )
    if filter_output not in required:
        raise ValueError(f"PR lane required mirror filter drift: {authority.name}")
    return job, _path_filter_patterns(source, authority.path_filter)


def _patterns_have_authority(
    lane_patterns: Iterable[str], authority_patterns: Iterable[str]
) -> bool:
    authority = tuple(authority_patterns)
    return all(pattern in authority for pattern in lane_patterns)


def verify_pr_lane_manifest(
    repo_root: Path,
    manifest_path: Path,
    cargo_test_filter: Callable[..., Lane | None],
    just_recipe_commands: Callable[[str, str], tuple[str, ...]],
) -> PrLaneManifest:
    manifest = load_pr_lane_manifest(manifest_path, cargo_test_filter)
    authorities = {authority.name: authority for authority in manifest.authorities}
    authority_sources: dict[str, str] = {}
    authority_paths: dict[str, tuple[str, ...]] = {}
    for authority in manifest.authorities:
        source, path_patterns = _require_authority(repo_root, authority)
        authority_sources[authority.name] = source
        authority_paths[authority.name] = path_patterns

    direct_witnesses: set[tuple[str, str]] = set()
    indirect_commands: dict[str, set[str]] = {}
    workflow_recipe_witnesses: dict[str, set[str]] = {}
    for witness in manifest.witnesses:
        source_path = repo_root / Path(*witness.source.split("/"))
        source = source_path.read_text(encoding="utf-8")
        actual_count = source.count(witness.text)
        if actual_count != witness.expected_count:
            raise ValueError(
                f"PR lane witness drift: {witness.source} expected "
                f"{witness.expected_count} occurrence(s) of {witness.text!r}, "
                f"found {actual_count}"
            )
        if witness.source.startswith(".github/workflows/"):
            authority = authorities[witness.authority]
            if witness.source != authority.workflow:
                raise ValueError(
                    f"PR lane witness is outside authority workflow: {witness.source}"
                )
            job = authority_sources[witness.authority]
            if job.count(witness.text) != witness.expected_count:
                raise ValueError(
                    f"PR lane witness is outside authority job: {witness.authority}: "
                    f"{witness.text!r}"
                )
            if "cargo test" in witness.text:
                command = witness.text[witness.text.find("cargo test") :]
                direct_witnesses.add((witness.authority, command))
            if "just " in witness.text:
                workflow_recipe_witnesses.setdefault(witness.authority, set()).update(
                    re.findall(r"\bjust\s+([A-Za-z0-9_-]+)", witness.text)
                )
        elif "cargo test" in witness.text:
            command = witness.text[witness.text.find("cargo test") :]
            indirect_commands.setdefault(witness.authority, set()).add(command)

    just_text = (repo_root / "justfile").read_text(encoding="utf-8")
    indirect_witnesses = {
        (authority, command)
        for authority, recipes in workflow_recipe_witnesses.items()
        for recipe in recipes
        for command in just_recipe_commands(just_text, recipe)
        if command in indirect_commands.get(authority, set())
    }
    for lane in manifest.lanes:
        if not _patterns_have_authority(
            lane.changed_paths, authority_paths[lane.authority]
        ):
            raise ValueError(
                f"PR lane paths exceed workflow authority: {lane.provenance}: "
                f"{lane.authority}"
            )
        if (lane.authority, lane.command) not in direct_witnesses | indirect_witnesses:
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
        for name, source_path in candidate_inventory.test_sources.items()
        if name not in candidate_inventory.test_modules
        and source_path in changed_rust
        and (
            name not in base_inventory.test_fingerprints
            or base_inventory.test_fingerprints[name]
            != candidate_inventory.test_fingerprints[name]
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
