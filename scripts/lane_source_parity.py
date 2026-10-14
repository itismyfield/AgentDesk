#!/usr/bin/env python3
"""Label cargo-test commands with the CI context that runs them, and require every explicit
justfile filter to be selected by required-PR or main lane filters, or an exception."""

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
INERT_MODES = ("--list", "--ignored")
SWEEP_COVERED = "sweep-covered"
MAIN_ONLY_TARGET = "main-only-target"
LIBRARY_TARGETS = frozenset({"lib", "all-targets", "default"})
_UPSTREAM_NAME = re.compile(r"\bUPSTREAM_JOB_NAME\s*:\s*['\"]?([A-Za-z0-9_-]+)")
_UPSTREAM_RESULT = re.compile(
    r"\bUPSTREAM_RESULT\s*:\s*['\"]?\$\{\{\s*needs\.([A-Za-z0-9_-]+)\.result\s*\}\}"
)
_KEY = re.compile(r"^(?P<indent>[ \t]*)(?P<key>[A-Za-z0-9_-]+)\s*:(?P<value>.*)$")
_STEP_GUARD = re.compile(r"^[ \t]*(?:-[ \t]+)?(?P<key>if|continue-on-error)\s*:(?P<value>.*)$")
# `if:` values that always run the step; `continue-on-error` is safe only as false.
RUNNING_IF = frozenset({"true", "always()", "!cancelled()", "success()"})


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
    # False for libtest `--list`/`--ignored`: the command names ids without running them.
    runs: bool = True
    # Why its step may skip or swallow a failure (`if:`, `continue-on-error`), else "".
    guard: str = ""


@dataclass(frozen=True)
class Finding:
    kind: str
    location: str
    detail: str

    def render(self) -> str:
        return f"{self.kind}: {self.location}: {self.detail}"


@dataclass(frozen=True)
class Mirror:
    """One required-check-mirror step: the context it publishes and the job it reports."""

    job: str
    context: str
    upstream: str
    proven: bool
    conditional: bool


@dataclass(frozen=True)
class Sources:
    sources: tuple[LaneSource, ...]
    findings: tuple[Finding, ...]
    justfile_findings: tuple[Finding, ...]
    mirrors: tuple[Mirror, ...] = ()
    pr_jobs: tuple = ()

    def of(self, *origins: str) -> tuple[LaneSource, ...]:
        """The running sources of these origins; `--list`/`--ignored` commands and steps that
        may skip or swallow a failure prove no run."""
        return tuple(s for s in self.sources if s.origin in origins and s.runs and not s.guard)

    @property
    def required(self) -> tuple[LaneSource, ...]:
        """Required PR runs plus main's lanes: a PR runs only the plan's PostgreSQL and sweep
        tests, so main's literal full lanes, which a deploy needs green, prove coverage."""
        return self.of("pr-required") + tuple(s for s in self.of("main") if s.lane)

    @property
    def required_test_jobs(self) -> str:
        return ", ".join(sorted({f"{'main:' * (s.origin == 'main')}{s.owner}"
                                 for s in self.required})) or "none proven"


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
    inert = "--list" in words[split + 1:] or spec.ignored_only
    if inert:
        words = words[:split + 1] + [w for w in words[split + 1:] if w not in INERT_MODES]
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
    # An empty positive matches every id, so it selects like the unfiltered sweep.
    broad = lane is not None and (not lane.positives or "" in lane.positives)
    selection = "sweep" if broad else "named"
    return LaneSource(origin, owner, location, target, filters, lane, selection, not inert), None


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


def _line_owners(text: str, jobs) -> tuple[dict[int, str], list[str], dict[str, int]]:
    """Map each line to its job, name the jobs whose text is no later slice of the file
    (parse_jobs slices the jobs block contiguously in order) and give each job's header line."""
    starts = [0]
    for line in text.splitlines(keepends=True):
        starts.append(starts[-1] + len(line))
    owners: dict[int, str] = {}
    unmatched: list[str] = []
    headers: dict[str, int] = {}
    cursor = 0
    for job in jobs:
        offset = text.find(job.text, cursor) if job.text else -1
        if offset < 0:
            unmatched.append(job.name)
            continue
        cursor = offset + len(job.text)
        first = bisect.bisect_right(starts, offset) + 1
        last = bisect.bisect_right(starts, cursor - 1)
        owners.update(dict.fromkeys(range(first, last + 1), job.name))
        headers[job.name] = first - 1
    return owners, unmatched, headers


def _read_workflow(cti, pgm, repo_root: Path, rel: Path):
    """Jobs, each run-scalar line with its owning job (None when no job owns it),
    the extracted cargo argv and the jobs whose text the file does not contain."""
    path = repo_root / rel
    if not path.is_file():
        return None
    jobs = pgm.parse_jobs(path, repo_root)
    shells: list[list[tuple[int, str]]] = []
    commands = {line: words for line, words, _ in cti.extract_commands(path, None, shells)}
    owners, unmatched, headers = _line_owners(path.read_text("utf-8"), jobs)
    lines = [(owners.get(line), line, snippet) for shell in shells for line, snippet in shell]
    return jobs, lines, commands, unmatched, headers


def _unowned(rel: Path, lines, commands, unmatched) -> list[Finding]:
    """A PR job or selecting line tied to no job hides its origin, so it fails closed."""
    findings = [
        Finding(
            "source-command-unresolved", f"{rel.as_posix()}:{name}",
            f"job `{name}` text from parse_jobs is not a slice of the workflow, "
            "so no command can be proven to run in it",
        )
        for name in unmatched
    ]
    for job, line, snippet in lines:
        words = _words(snippet)
        selects = line in commands or "cargo test" in snippet or _runs_mirror(words) or (
            words[:1] in (["just"], ["run_non_pg_filter_replay"])
        )
        if job is None and selects:
            findings.append(Finding(
                "source-command-unresolved", f"{rel.as_posix()}:{line}",
                f"`{snippet.strip()}` sits outside every job parse_jobs returned, "
                "so its CI origin cannot be proven",
            ))
    return findings


def job_keys(pgm, code: str) -> dict[str, tuple[str, int]]:
    """A job's own mapping keys: name -> (inline value, line index in `code`)."""
    indent = pgm._job_body_indent(code)
    keys: dict[str, tuple[str, int]] = {}
    for index, line in enumerate(code.splitlines()):
        match = _KEY.match(line)
        if match and pgm._indent_width(match.group("indent")) == indent:
            keys.setdefault(match.group("key"), (match.group("value").strip(), index))
    return keys


def _needs(pgm, code: str) -> set[str]:
    """The job's own `needs:` names in scalar, flow or block-sequence form."""
    value, index = job_keys(pgm, code).get("needs", ("", -1))
    if index < 0:
        return set()
    if value:
        return {item.strip(" '\"") for item in value.strip("[]").split(",")} - {""}
    indent = pgm._job_body_indent(code)
    names: set[str] = set()
    for item in code.splitlines()[index + 1:]:
        stripped = item.strip()
        if not stripped:
            continue
        width = pgm._indent_width(item[: len(item) - len(item.lstrip())])
        if not stripped.startswith("-") or width < indent:
            break
        names.add(stripped[1:].strip(" '\""))
    return names


def _steps(pgm, code: str) -> list[tuple[int, int]]:
    """(first, last) line index in `code` of each item under the job's `steps:` key."""
    lines = code.splitlines()
    indent = pgm._job_body_indent(code)
    start = job_keys(pgm, code).get("steps", ("", -1))[1]
    spans: list[list[int]] = []
    item = None
    for index in range(start + 1, len(lines)) if start >= 0 else ():
        line, stripped = lines[index], lines[index].strip()
        width = pgm._indent_width(line[: len(line) - len(line.lstrip())])
        if stripped and width <= indent and not stripped.startswith("-"):
            break
        if stripped.startswith("-") and item in (None, width):
            item = width
            spans.append([index, index])
        elif spans and stripped:
            spans[-1][1] = index
    return [(first, last) for first, last in spans]


def unconditional(key: str, value: str) -> bool:
    """Whether an `if:` value always runs, or a `continue-on-error:` value keeps failures."""
    value = value.strip().strip("'\"")
    if value.startswith("${{") and value.endswith("}}"):
        value = value[3:-2].strip()
    return value == "false" if key == "continue-on-error" else value in RUNNING_IF


def _step_guard(step_lines: list[str]) -> str:
    """The first `if:` or `continue-on-error:` of one step that may skip it or hide a failure."""
    for line in step_lines:
        match = _STEP_GUARD.match(line)
        if match and not unconditional(match.group("key"), match.group("value")):
            return f"step {match.group('key')}: {match.group('value').strip()}"
    return ""


def _mirror_upstreams(pgm, rel: Path, jobs, mirror_lines: dict[str, set[int]], headers):
    """Required PR jobs are the upstreams an unconditional required-check-mirror step both
    reports in its own env and its job needs; env on any other step proves nothing."""
    names = {job.name for job in jobs}
    required: set[str] = set()
    findings: list[Finding] = []
    mirrors: list[Mirror] = []
    for job in jobs:
        if job.name not in mirror_lines:
            continue
        needs = _needs(pgm, job.code)
        keys = job_keys(pgm, job.code)
        context = keys.get("name", (job.name, 0))[0].strip(" '\"") or job.name
        lines = job.code.splitlines()
        for first, last in _steps(pgm, job.code):
            span = range(headers[job.name] + first, headers[job.name] + last + 1)
            if not mirror_lines[job.name] & set(span):
                continue
            step = "\n".join(lines[first:last + 1])
            upstreams, results = _UPSTREAM_NAME.findall(step), _UPSTREAM_RESULT.findall(step)
            problem = None
            if len(upstreams) != 1 or len(results) != 1:
                problem = "the mirror step lacks one UPSTREAM_JOB_NAME and UPSTREAM_RESULT"
            elif upstreams != results:
                problem = f"UPSTREAM_JOB_NAME {upstreams[0]} reports needs.{results[0]}.result"
            elif upstreams[0] not in needs:
                problem = f"upstream {upstreams[0]} is not in needs"
            elif upstreams[0] not in names:
                problem = f"upstream {upstreams[0]} is not a job in this workflow"
            conditional = bool(_step_guard(lines[first:last + 1])) or any(
                key in keys and not unconditional(key, keys[key][0])
                for key in ("if", "continue-on-error")
            )
            for upstream in upstreams or [""]:
                mirrors.append(Mirror(job.name, context, upstream, not problem, conditional))
            if problem:
                findings.append(Finding(
                    "pr-context-unproven", f"{rel.as_posix()}:{job.name}",
                    f"{problem}; no job counts as required through this mirror until the "
                    "step that runs it names the same job in needs, UPSTREAM_JOB_NAME and "
                    "UPSTREAM_RESULT",
                ))
            elif not conditional:
                required.update(upstreams)
    return required, findings, mirrors


def _conditionally_mirrored(rel: Path, mirrors, demoted: set[str], found) -> list[Finding]:
    """A test-selecting job that only conditional mirrors report has no required run."""
    selecting = demoted & {source.owner for source in found}
    pairs = dict.fromkeys(
        (m.job, m.upstream) for m in mirrors if m.proven and m.upstream in selecting
    )
    return [
        Finding(
            "pr-context-unproven", f"{rel.as_posix()}:{job}",
            f"every proven mirror of {upstream} may skip or swallow its result (`if:` or "
            "`continue-on-error:`), so its tests prove no required PR run; drop the condition "
            "or add an unconditional mirror",
        )
        for job, upstream in pairs
    ]


def guarded_lines(pgm, jobs, headers) -> dict[int, str]:
    """Workflow lines inside a step whose own `if:`/`continue-on-error:` is conditional, or
    inside a job whose `continue-on-error:` may swallow every step's failure."""
    guarded: dict[int, str] = {}
    for job in jobs:
        lines = job.code.splitlines()
        value = job_keys(pgm, job.code).get("continue-on-error", ("false", -1))[0]
        swallowed = "" if unconditional("continue-on-error", value) else (
            f"job continue-on-error: {value}"
        )
        for first, last in _steps(pgm, job.code) if job.name in headers else ():
            guard = _step_guard(lines[first:last + 1]) or swallowed
            span = range(headers[job.name] + first, headers[job.name] + last + 1)
            guarded.update(dict.fromkeys(span, guard) if guard else {})
    return guarded


def _job_sources(
    cti, coverage, expansions, recipes, rel, lines, commands, origins, guarded, strict=frozenset()
):
    sources: list[LaneSource] = []
    findings: list[Finding] = []
    for job, line, snippet in lines:
        if job is None:
            continue
        before = len(sources)
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
        elif words[:1] == ["just"] and len(words) == 2:
            sources.extend(
                replace(recipe_source, origin=origin, owner=job)
                for recipe_source in recipes.get(words[1], ())
            )
        elif "cargo test" in snippet or words[:1] == ["just"]:
            finding = _unreadable(location, snippet.strip())
        sources.extend([source] if source else [])
        if line in guarded:
            sources[before:] = [replace(s, guard=guarded[line]) for s in sources[before:]]
        if finding and (origin == "pr-required" or job in strict):
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
    mirrors: list[Mirror] = []
    pr_jobs: tuple = ()
    for name, rel in (("pr", PR_WORKFLOW_REL), *OTHER_WORKFLOWS):
        parsed = _read_workflow(cti, pgm, repo_root, rel)
        if parsed is None:
            continue
        jobs, lines, commands, unmatched, headers = parsed
        if name == "pr":
            pr_jobs = tuple(jobs)
            findings.extend(_unowned(rel, lines, commands, unmatched))
            mirror_lines: dict[str, set[int]] = {}
            for job, line, snippet in lines:
                if job and _runs_mirror(_words(snippet)):
                    mirror_lines.setdefault(job, set()).add(line)
            required, unproven, mirrors = _mirror_upstreams(
                pgm, rel, jobs, mirror_lines, headers
            )
            findings.extend(unproven)
            # Jobs only conditional mirrors report still fail closed on unreadable commands.
            demoted = {mirror.upstream for mirror in mirrors if mirror.proven} - required
            origins = {
                job.name: "pr-required" if job.name in required else "pr-advisory"
                for job in jobs
            }
            guarded = guarded_lines(pgm, jobs, headers)
        else:
            origins = dict.fromkeys((job.name for job in jobs), name)
            guarded, demoted = {}, set()
        found, problems = _job_sources(
            cti, coverage, expansions, recipes, rel, lines, commands, origins, guarded, demoted
        )
        sources.extend(found)
        findings.extend(problems)
        findings.extend(_conditionally_mirrored(rel, mirrors, demoted, found))
    return Sources(
        tuple(sources), tuple(findings), tuple(justfile_findings), tuple(mirrors), pr_jobs
    )


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
    """Explicit justfile filters need required-PR or main selection or a proven exception;
    the sweep and its replay never count as explicit, the PG lane only for PG ids."""
    required = sources.required
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
                # An identical required lane (PG-only too) counts only when it selects ids.
                parity = bool(chosen) and (source.lane in named or not missing)
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
