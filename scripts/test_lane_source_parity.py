#!/usr/bin/env python3
"""Label cargo-test commands with the CI context that runs them, and require every
explicit justfile filter to be selected by explicit required-PR filters or an exception."""

from __future__ import annotations

import bisect
import importlib.util
import re
import shlex
import sys
from dataclasses import dataclass, replace
from pathlib import Path

PR_WORKFLOW_REL = Path(".github/workflows/ci-pr.yml")
OTHER_WORKFLOWS = (
    ("main", Path(".github/workflows/ci-main.yml")),
    ("nightly", Path(".github/workflows/ci-nightly.yml")),
    ("trusted", Path(".github/workflows/ci-macos-trusted.yml")),
)
EXCEPTIONS_REL = Path("scripts/test_lane_pr_parity_exceptions.txt")
MIRROR_SCRIPT = "required-check-mirror.sh"
SWEEP_COVERED = "sweep-covered"
MAIN_ONLY_TARGET = "main-only-target"
LIBRARY_TARGETS = frozenset({"lib", "all-targets", "default"})
_UPSTREAM_NAME = re.compile(r"\bUPSTREAM_JOB_NAME\s*:\s*['\"]?([A-Za-z0-9_-]+)")
_UPSTREAM_RESULT = re.compile(
    r"\bUPSTREAM_RESULT\s*:\s*['\"]?\$\{\{\s*needs\.([A-Za-z0-9_-]+)\.result\s*\}\}"
)
_NEEDS = re.compile(r"^(?P<indent>[ \t]*)needs\s*:(?P<value>.*)$")


def _sibling(filename: str):
    """Load a sibling checker from this script's directory, once."""
    name = f"lane_parity_{Path(filename).stem}"
    if name not in sys.modules:
        path = Path(__file__).with_name(filename)
        spec = importlib.util.spec_from_file_location(name, path)
        if spec is None or spec.loader is None:
            raise ImportError(f"cannot import {path}")
        module = importlib.util.module_from_spec(spec)
        sys.modules[name] = module
        spec.loader.exec_module(module)
    return sys.modules[name]


@dataclass(frozen=True)
class LaneSource:
    """One cargo-test command, what it selects, and the CI context that runs it."""

    origin: str
    owner: str
    location: str
    target: str
    filters: tuple[str, ...]
    lane: object | None
    selection: str


@dataclass(frozen=True)
class Finding:
    kind: str
    location: str
    detail: str

    def render(self) -> str:
        return f"{self.kind}: {self.location}: {self.detail}"


@dataclass(frozen=True)
class Sources:
    sources: tuple[LaneSource, ...]
    findings: tuple[Finding, ...]
    justfile_findings: tuple[Finding, ...]

    def of(self, *origins: str) -> tuple[LaneSource, ...]:
        return tuple(source for source in self.sources if source.origin in origins)

    @property
    def required_test_jobs(self) -> str:
        return ", ".join(sorted({s.owner for s in self.of("pr-required")})) or "none proven"


@dataclass(frozen=True)
class ParityException:
    line: int
    recipe: str
    target: str
    filter: str
    mode: str
    reason: str

    @property
    def key(self) -> tuple[str, str, str]:
        return (self.recipe, self.target, self.filter)


@dataclass(frozen=True)
class ParityResult:
    findings: tuple[Finding, ...]
    explicit: int
    matched: int
    excepted: int


def _words(line: str) -> list[str]:
    try:
        return shlex.split(line, comments=True)
    except ValueError:
        return []


def _runs_mirror(words: list[str]) -> bool:
    command = words[1:2] if words[:1] in (["bash"], ["sh"]) else words[:1]
    return bool(command) and "=" not in command[0] and Path(command[0]).name == MIRROR_SCRIPT


def _unreadable(location: str, command: str) -> Finding:
    return Finding(
        "source-command-unresolved", location,
        f"`{command}` selects through a shape or expansion this gate cannot read; "
        "spell the cargo test filter literally or use a generated array",
    )


def _source(cti, coverage, expansions, origin, owner, location, words):
    """Classify one extracted cargo argv into (source, finding); either may be None."""
    split = words.index("--") if "--" in words else len(words)
    cargo_args = words[2:split]
    if "--no-run" in cargo_args:
        return None, None
    spec = cti.parse_command(list(words))
    if spec.opaque_skip or spec.target_inconclusive:
        return None, _unreadable(location, shlex.join(words))
    lane = coverage.cargo_test_filter(
        coverage.expand_lane_command(shlex.join(words), *expansions)
    )
    if lane is None:
        target = "doc" if "--doc" in cargo_args else "other"
    elif "--all-targets" in cargo_args:
        target = "all-targets"
    else:
        target = "lib" if "--lib" in cargo_args else "default"
    filters = tuple(lane.positives) if lane is not None else tuple(spec.filters)
    selection = "sweep" if lane is not None and not lane.positives else "named"
    return LaneSource(origin, owner, location, target, filters, lane, selection), None


def justfile_sources(cti, coverage, repo_root: Path, expansions):
    """Every recipe's cargo-test commands, owned by the recipe that declares them."""
    path = repo_root / "justfile"
    extracted = {line: words for line, words, _ in cti.extract_justfile_commands(path)}
    sources: list[LaneSource] = []
    findings: list[Finding] = []
    recipe = None
    for lineno, line in enumerate(path.read_text("utf-8").splitlines(), 1):
        if line and not line[0].isspace():
            header = cti.RECIPE_HEADER.match(line)
            recipe = header.group(1) if header else None
            continue
        location = f"justfile:{lineno}"
        if recipe is None or line.strip().startswith("#"):
            continue
        if lineno in extracted:
            source, finding = _source(
                cti, coverage, expansions, "justfile", recipe, location, extracted[lineno]
            )
            sources.extend([source] if source else [])
            findings.extend([finding] if finding else [])
        elif "cargo test" in line:
            findings.append(_unreadable(location, line.strip()))
    return sources, findings


def _line_owners(text: str, jobs) -> dict[int, str]:
    """Map each line to its job; parse_jobs slices the jobs block contiguously in order."""
    starts = [0]
    for line in text.splitlines(keepends=True):
        starts.append(starts[-1] + len(line))
    owners: dict[int, str] = {}
    cursor = 0
    for job in jobs:
        offset = text.find(job.text, cursor)
        if offset < 0 or not job.text:
            continue
        cursor = offset + len(job.text)
        first = bisect.bisect_right(starts, offset) + 1
        last = bisect.bisect_right(starts, cursor - 1)
        owners.update(dict.fromkeys(range(first, last + 1), job.name))
    return owners


def _read_workflow(cti, pgm, repo_root: Path, rel: Path):
    """Jobs, each run-scalar line with its owning job, and the extracted cargo argv."""
    path = repo_root / rel
    if not path.is_file():
        return None
    jobs = pgm.parse_jobs(path, repo_root)
    shells: list[list[tuple[int, str]]] = []
    commands = {line: words for line, words, _ in cti.extract_commands(path, None, shells)}
    owners = _line_owners(path.read_text("utf-8"), jobs)
    lines = [
        (owners[line], line, snippet)
        for shell in shells
        for line, snippet in shell
        if line in owners
    ]
    return jobs, lines, commands


def _needs(pgm, code: str) -> set[str]:
    """The job's own `needs:` names in scalar, flow or block-sequence form."""
    indent = pgm._job_body_indent(code)
    lines = code.splitlines()
    for index, line in enumerate(lines):
        match = _NEEDS.match(line)
        if not match or pgm._indent_width(match.group("indent")) != indent:
            continue
        value = match.group("value").strip()
        if value:
            return {item.strip(" '\"") for item in value.strip("[]").split(",")} - {""}
        names: set[str] = set()
        for item in lines[index + 1:]:
            stripped = item.strip()
            if not stripped:
                continue
            width = pgm._indent_width(item[: len(item) - len(item.lstrip())])
            if not stripped.startswith("-") or width < indent:
                break
            names.add(stripped[1:].strip(" '\""))
        return names
    return set()


def _mirror_upstreams(pgm, rel: Path, jobs, mirror_jobs: set[str]):
    """Required PR jobs are the upstreams a required-check mirror both needs and reports."""
    names = {job.name for job in jobs}
    required: set[str] = set()
    findings: list[Finding] = []
    for job in jobs:
        if job.name not in mirror_jobs:
            continue
        upstreams = _UPSTREAM_NAME.findall(job.code)
        results = _UPSTREAM_RESULT.findall(job.code)
        needs = _needs(pgm, job.code)
        problem = None
        if not upstreams or len(upstreams) != len(results):
            problem = "UPSTREAM_JOB_NAME and UPSTREAM_RESULT do not pair one to one"
        for upstream, result in zip(upstreams, results):
            if problem:
                break
            if upstream != result:
                problem = f"UPSTREAM_JOB_NAME {upstream} reports needs.{result}.result"
            elif upstream not in needs:
                problem = f"upstream {upstream} is not in needs"
            elif upstream not in names:
                problem = f"upstream {upstream} is not a job in this workflow"
        if problem:
            findings.append(Finding(
                "pr-context-unproven", f"{rel.as_posix()}:{job.name}",
                f"{problem}; no job counts as required through this mirror until its "
                "needs, UPSTREAM_JOB_NAME and UPSTREAM_RESULT name the same job",
            ))
        else:
            required.update(upstreams)
    return required, findings


def _job_sources(cti, coverage, expansions, recipes, rel, lines, commands, origins):
    sources: list[LaneSource] = []
    findings: list[Finding] = []
    for job, line, snippet in lines:
        origin = origins[job]
        location = f"{rel.as_posix()}:{line}"
        words = _words(snippet)
        source = finding = None
        if line in commands:
            source, finding = _source(
                cti, coverage, expansions, origin, job, location, commands[line]
            )
        elif words == ["run_non_pg_filter_replay"]:
            lane = coverage.cargo_test_filter(
                coverage.expand_non_pg_filter_replay(words[0], expansions[1])
            )
            if lane is not None:
                source = LaneSource(origin, job, location, "lib", lane.positives, lane, "replay")
        elif words[:1] == ["just"] and len(words) > 1:
            sources.extend(
                replace(recipe_source, origin=origin, owner=job)
                for recipe_source in recipes.get(words[1], ())
            )
        elif "cargo test" in snippet:
            finding = _unreadable(location, snippet.strip())
        sources.extend([source] if source else [])
        if finding and origin == "pr-required":
            findings.append(finding)
    return sources, findings


def collect_sources(repo_root: Path, coverage) -> Sources:
    """Every justfile and workflow cargo-test command, labelled with its CI origin."""
    cti = _sibling("check_test_target_integrity.py")
    pgm = _sibling("check_pg_test_lane_membership.py")
    filter_present = (repo_root / coverage.NON_PG_FILTER_REL).is_file()
    expansions = (
        coverage.load_non_pg_skip_args(repo_root) if filter_present else (),
        coverage.load_non_pg_filter_replay(repo_root) if filter_present else (),
    )
    sources, justfile_findings = justfile_sources(cti, coverage, repo_root, expansions)
    findings: list[Finding] = []
    recipes: dict[str, list[LaneSource]] = {}
    for source in sources:
        recipes.setdefault(source.owner, []).append(source)
    for name, rel in (("pr", PR_WORKFLOW_REL), *OTHER_WORKFLOWS):
        parsed = _read_workflow(cti, pgm, repo_root, rel)
        if parsed is None:
            continue
        jobs, lines, commands = parsed
        if name == "pr":
            mirrors = {job for job, _, snippet in lines if _runs_mirror(_words(snippet))}
            required, unproven = _mirror_upstreams(pgm, rel, jobs, mirrors)
            findings.extend(unproven)
            origins = {
                job.name: "pr-required" if job.name in required else "pr-advisory"
                for job in jobs
            }
        else:
            origins = dict.fromkeys((job.name for job in jobs), name)
        found, problems = _job_sources(
            cti, coverage, expansions, recipes, rel, lines, commands, origins
        )
        sources.extend(found)
        findings.extend(problems)
    return Sources(tuple(sources), tuple(findings), tuple(justfile_findings))


def load_test_ids(repo_root: Path) -> frozenset[str]:
    """The checked-in static library test-ID manifest that CTI verifies against cargo."""
    cti = _sibling("check_test_target_integrity.py")
    return cti.load_lib_inventory_manifest(repo_root / cti.LIB_INVENTORY_MANIFEST_REL)


def load_exceptions(path: Path) -> tuple[list[ParityException], list[Finding]]:
    """Parse `recipe | target | positive-filter | mode | reason` rows sorted by key."""
    if not path.is_file():
        return [], []
    rows: list[ParityException] = []
    findings: list[Finding] = []
    for lineno, raw in enumerate(path.read_text("utf-8").splitlines(), 1):
        row = raw.strip()
        if not row or row.startswith("#"):
            continue
        fields = [field.strip() for field in row.split("|")]
        problem = None
        if len(fields) != 5 or not all(fields):
            problem = "expected `recipe | target | positive-filter | mode | reason`"
        elif fields[3] not in (SWEEP_COVERED, MAIN_ONLY_TARGET):
            problem = f"mode must be {SWEEP_COVERED} or {MAIN_ONLY_TARGET}"
        elif (fields[3] == SWEEP_COVERED) != (fields[1] in LIBRARY_TARGETS):
            problem = f"mode {fields[3]} does not apply to target {fields[1]}"
        if problem:
            location = f"{EXCEPTIONS_REL.as_posix()}:{lineno}"
            findings.append(Finding("pr-parity-exception-invalid", location, problem))
        else:
            rows.append(ParityException(lineno, *fields))
    keys = [row.key for row in rows]
    if keys != sorted(set(keys)):
        findings.append(Finding(
            "pr-parity-exception-invalid", EXCEPTIONS_REL.as_posix(),
            "rows must be unique and sorted by recipe, target and filter",
        ))
    return rows, findings


def _sample(ids) -> str:
    ordered = sorted(ids)
    more = f" (+{len(ordered) - 3} more)" if len(ordered) > 3 else ""
    return ", ".join(ordered[:3]) + more


def judge_parity(
    sources: Sources, test_ids: frozenset[str], exceptions, pg_tests=frozenset(), pg_only=()
) -> ParityResult:
    """Explicit justfile filters need explicit required-PR selection or a proven exception;
    the required sweep and its replay never count as explicit, the PG lane only for PG ids."""
    required = sources.of("pr-required")
    ordered = sorted(test_ids)

    def selected(lanes) -> set[str]:
        return set().union(*(lane.selected_tests(ordered) for lane in lanes))

    named = {s.lane for s in required if s.selection == "named" and s.lane}
    named_ids = selected(named - set(pg_only)) | (selected(named) & set(pg_tests))
    sweep_ids = selected(s.lane for s in required if s.selection in ("sweep", "replay"))
    named_targets = {(s.target, s.filters) for s in required if s.lane is None}
    other_targets = {
        (s.target, s.filters) for s in sources.of("main", "nightly", "trusted") if s.lane is None
    }
    by_key: dict[tuple[str, str, str], list[LaneSource]] = {}
    for source in sources.of("justfile"):
        if source.filters:
            key = (source.owner, source.target, " ".join(source.filters))
            by_key.setdefault(key, []).append(source)
    by_exception = {exception.key: exception for exception in exceptions}
    findings = list(sources.justfile_findings)
    explicit = matched = excepted = 0
    explicit_keys: set[tuple[str, str, str]] = set()
    for key, group in by_key.items():
        for source in group:
            explicit += 1
            label = f"[{key[0]}] {key[1]} `{key[2]}`"
            if source.lane is None:
                gap = "no required PR job runs this target with this filter"
                parity = (source.target, source.filters) in named_targets
                proven = (source.target, source.filters) in other_targets
            else:
                chosen = source.lane.selected_tests(ordered)
                missing = chosen - named_ids
                gap = (
                    f"selects {len(chosen)} id(s), {len(missing)} not selected by any "
                    f"required PR named filter, e.g. {_sample(missing)}"
                    if chosen else "selects no id in the lib inventory manifest"
                )
                parity = source.lane in named or (bool(chosen) and not missing)
                proven = bool(chosen) and missing <= sweep_ids
            if parity:
                matched += 1
                explicit_keys.add(key)
                continue
            exception = by_exception.get(key)
            if exception is None:
                findings.append(Finding(
                    "pr-parity-missing", source.location,
                    f"{label} {gap}; add the filter to a required PR job "
                    f"({sources.required_test_jobs}) or record "
                    f"`{' | '.join(key)} | <mode> | <reason>` in {EXCEPTIONS_REL.as_posix()}",
                ))
            elif not proven:
                findings.append(Finding(
                    "pr-parity-exception-invalid",
                    f"{EXCEPTIONS_REL.as_posix()}:{exception.line}",
                    f"{exception.mode} for {label} at {source.location} is unproven: {gap}; "
                    + ("the required PR sweep and replay do not select them"
                       if exception.mode == SWEEP_COVERED
                       else "no main, nightly or trusted workflow runs it"),
                ))
            else:
                excepted += 1
    findings.extend(stale_exceptions(exceptions, by_key, explicit_keys))
    return ParityResult(tuple(findings), explicit, matched, excepted)


def stale_exceptions(exceptions, by_key, explicit_keys) -> list[Finding]:
    """An exception must name exactly one current justfile filter that still lacks parity."""
    findings: list[Finding] = []
    for exception in exceptions:
        location = f"{EXCEPTIONS_REL.as_posix()}:{exception.line}"
        matches = by_key.get(exception.key, [])
        if len(matches) > 1:
            findings.append(Finding(
                "pr-parity-exception-invalid", location,
                "matches several justfile commands: "
                + ", ".join(match.location for match in matches),
            ))
        elif not matches or exception.key in explicit_keys:
            detail = (
                f"{matches[0].location} is now selected by required PR named filters"
                if matches else "no current justfile command has this recipe, target and filter"
            )
            findings.append(Finding(
                "stale-pr-parity-exception", location, f"{detail}; delete the exception"
            ))
    return findings
