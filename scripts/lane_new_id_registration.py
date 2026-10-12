#!/usr/bin/env python3
"""Judge every library test ID a change adds: some required PR job must run it, proven
through its mirror step, its path gate, a running libtest mode and its runner platform.

The added IDs are the lib inventory manifest's growth over the immutable reference commit;
git diff only finds the touched files that may hide a test the manifest cannot list.
"""

from __future__ import annotations

import datetime
import re
import subprocess
from collections import Counter
from dataclasses import dataclass
from pathlib import Path

EXCEPTIONS_REL = Path("scripts/test_lane_required_id_exceptions.txt")
UNREQUIRED = "new-id-unrequired"
CONTEXT = "new-id-context-unproven"
PATH = "new-id-path-unproven"
RUNMODE = "new-id-runmode-unproven"
PLATFORM = "new-id-platform-unproven"
STALE_MANIFEST = "new-id-manifest-stale"
EMPTY_MODULE = "new-test-module-empty"
TARGET = "new-id-target-unresolved"
INVALID = "new-id-exception-invalid"
STALE = "stale-new-id-exception"
MODE_KINDS = {
    "intentional-nonrequired": frozenset({UNREQUIRED}),
    "model-unresolved": frozenset({CONTEXT, PATH, RUNMODE}),
    "platform-unavailable": frozenset({PLATFORM}),
}
RUNNERS = (("ubuntu-", "linux"), ("macos-", "darwin"))
TEST_MODULE = re.compile(r"(?:^|_)tests?$")
_OUTPUT_IF = re.compile(r"^needs\.changes\.outputs\.([A-Za-z0-9_-]+)\s*==\s*'true'$")
_FILTER_REF = re.compile(r"steps\.filter\.outputs\.([A-Za-z0-9_-]+)")
_FILTERS = re.compile(r"^[ \t]*filters\s*:\s*\|[-+]?\s*$")
_FILTER_NAME = re.compile(r"^[ \t]*([A-Za-z0-9_-]+)\s*:\s*$")
_FILTER_ITEM = re.compile(r"^[ \t]*-\s+(['\"]?)(.+?)\1\s*$")
_ISSUE = re.compile(r"^#\d+$")
_EXPIRES = re.compile(r"^expires=(\d{4}-\d{2}-\d{2})$")


@dataclass(frozen=True)
class Gate:
    """What a PR job's own keys prove: None patterns means no job-level path gate."""

    patterns: tuple[str, ...] | None
    problem: tuple[str, str] | None
    platform: str | None


@dataclass(frozen=True)
class Mapped:
    test_id: str
    job: str
    location: str
    mirror: str
    context: str
    path: str

    def render(self) -> str:
        return (
            f"  {self.test_id} -> {self.job} ({self.location}) / mirror {self.mirror} / "
            f"context \"{self.context}\" / {self.path}"
        )


@dataclass(frozen=True)
class IdException:
    line: int
    test_id: str
    mode: str


@dataclass(frozen=True)
class Verdict:
    base: str
    findings: tuple
    added: int
    mapped: tuple[Mapped, ...]
    excepted: int

    def summary(self) -> str:
        return (
            f"OK: new_required_test_ids: base={self.base} added={self.added} "
            f"mapped={len(self.mapped)} excepted={self.excepted}"
        )


def _git(repo_root: Path, *args: str) -> str:
    try:
        return subprocess.run(
            ["git", "-c", "core.quotepath=off", *args], cwd=repo_root, check=True,
            capture_output=True, text=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as exc:
        detail = getattr(exc, "stderr", "") or str(exc)
        raise ValueError(f"git {' '.join(args)} failed: {detail.strip()}") from exc


def changed_paths(repo_root: Path, base: str) -> dict[str, str]:
    """Reference-to-checkout changes, path -> A/M/D; untracked files count as added."""
    fields = _git(repo_root, "diff", "--name-status", "--no-renames", "-z", base).split("\0")
    changed = {fields[i + 1]: fields[i][:1] for i in range(0, len(fields) - 1, 2)}
    untracked = _git(repo_root, "ls-files", "--others", "--exclude-standard", "-z")
    changed.update(dict.fromkeys(filter(None, untracked.split("\0")), "A"))
    return changed


def load_exceptions(parity, path: Path, today: datetime.date):
    """Parse `exact-test-id | mode | #issue | expires=YYYY-MM-DD | reason` rows sorted by id."""
    rows: list[IdException] = []
    findings = []
    keys: list[str] = []
    text = path.read_text("utf-8") if path.is_file() else ""
    for lineno, raw in enumerate(text.splitlines(), 1):
        row = raw.strip()
        if not row or row.startswith("#"):
            continue
        fields = [field.strip() for field in row.split("|")]
        location = f"{EXCEPTIONS_REL.as_posix()}:{lineno}"
        expires = _EXPIRES.match(fields[3]) if len(fields) == 5 else None
        problem = None
        if len(fields) != 5 or not all(fields):
            problem = "expected `exact-test-id | mode | #issue | expires=YYYY-MM-DD | reason`"
        elif "*" in fields[0] or fields[0].endswith("::") or len(fields[0].split()) != 1:
            problem = "the key must be one exact test id, never a module prefix or pattern"
        elif fields[1] not in MODE_KINDS:
            problem = f"mode must be one of {', '.join(sorted(MODE_KINDS))}"
        elif not _ISSUE.match(fields[2]):
            problem = "issue must be a tracking `#<number>`"
        elif not expires:
            problem = "expiry must read `expires=YYYY-MM-DD`"
        elif len(fields[4].split()) < 3:
            problem = "the reason must say why in words, not a placeholder"
        keys.extend(fields[:1])
        if problem is None:
            try:
                date = datetime.date.fromisoformat(expires.group(1))
            except ValueError:
                problem = f"{expires.group(1)} is not a calendar date"
        if problem:
            findings.append(parity.Finding(INVALID, location, problem))
        elif date < today:
            findings.append(parity.Finding(
                STALE, location, f"`{fields[0]}` expired on {date}; register it or renew the row",
            ))
        else:
            rows.append(IdException(lineno, fields[0], fields[1]))
    if keys != sorted(set(keys)):
        findings.append(parity.Finding(
            INVALID, EXCEPTIONS_REL.as_posix(), "rows must be unique and sorted by test id"
        ))
    return rows, findings


def _glob(pattern: str):
    """paths-filter's picomatch subset this gate proves: `**`, `*`, `?` and literals."""
    if pattern.startswith("!") or any(char in pattern for char in "{}[]()+@"):
        return None
    parts, index = [], 0
    while index < len(pattern):
        for token, regex in (("**/", "(?:.*/)?"), ("**", ".*"), ("*", "[^/]*"), ("?", "[^/]")):
            if pattern.startswith(token, index):
                parts.append(regex)
                index += len(token)
                break
        else:
            parts.append(re.escape(pattern[index]))
            index += 1
    return re.compile("".join(parts) + r"\Z")


def _activates(patterns: tuple[str, ...], path: str) -> bool:
    return any(regex and regex.match(path) for regex in map(_glob, patterns))


def _block(pgm, lines: list[str], start: int) -> list[str]:
    """The non-blank lines nested deeper than line `start`."""
    width = pgm._indent_width(lines[start][: len(lines[start]) - len(lines[start].lstrip())])
    body: list[str] = []
    for line in lines[start + 1:]:
        if line.strip() and pgm._indent_width(line[: len(line) - len(line.lstrip())]) <= width:
            break
        body.extend([line] if line.strip() else [])
    return body


def path_filters(parity, pgm, changes) -> tuple[dict, dict]:
    """The `changes` job's paths-filter lists (None when unreadable) and, for each of its
    outputs, the one filter it forwards (None when it mixes or names none)."""
    if changes is None:
        return {}, {}
    lines = changes.code.splitlines()
    filters: dict[str, list[str] | None] = {}
    start = next((i for i, line in enumerate(lines) if _FILTERS.match(line)), None)
    current = None
    for line in _block(pgm, lines, start) if start is not None else ():
        name, item = _FILTER_NAME.match(line), _FILTER_ITEM.match(line)
        if name:
            current = name.group(1)
            filters[current] = []
        elif current and item and filters[current] is not None:
            filters[current].append(item.group(2))
        elif current:
            filters[current] = None
    outputs: dict[str, str | None] = {}
    index = parity.job_keys(pgm, changes.code).get("outputs", ("", -1))[1]
    for line in _block(pgm, lines, index) if index >= 0 else ():
        match = parity._KEY.match(line)
        if match:
            refs = set(_FILTER_REF.findall(match.group("value")))
            outputs[match.group("key")] = refs.pop() if len(refs) == 1 else None
    return {k: None if v is None else tuple(v) for k, v in filters.items()}, outputs


def _activation(condition: str, needs: set[str], filters: dict, outputs: dict):
    """Patterns whose match runs the job under its `if:`, or why that is unprovable."""
    expr = condition.strip()
    if expr.startswith("${{") and expr.endswith("}}"):
        expr = expr[3:-2].strip()
    patterns: list[str] = []
    for term in expr.split("||"):
        match = _OUTPUT_IF.match(term.strip())
        if match is None or "changes" not in needs:
            return f"job-level `if: {condition}` is not an OR of changes outputs this gate reads"
        listed = filters.get(outputs.get(match.group(1)) or "")
        if listed is None:
            return f"changes output {match.group(1)} forwards no single readable paths-filter list"
        patterns.extend(listed)
    return tuple(patterns)


def job_gates(parity, pgm, jobs) -> dict[str, Gate]:
    changes = next((job for job in jobs if job.name == "changes"), None)
    filters, outputs = path_filters(parity, pgm, changes)
    gates: dict[str, Gate] = {}
    for job in jobs:
        keys = parity.job_keys(pgm, job.code)
        runner = keys.get("runs-on", ("", 0))[0].strip(" '\"")
        platform = next((name for prefix, name in RUNNERS if runner.startswith(prefix)), None)
        problem = patterns = None
        if "strategy" in keys:
            problem = (RUNMODE, "a matrix job runs shards this gate does not model")
        elif keys.get("continue-on-error", ("false", 0))[0] != "false":
            problem = (RUNMODE, "continue-on-error lets the job pass with its tests failing")
        elif "if" in keys:
            found = _activation(keys["if"][0], parity._needs(pgm, job.code), filters, outputs)
            patterns, problem = (found, None) if isinstance(found, tuple) else ((), (PATH, found))
        gates[job.name] = Gate(patterns, problem, platform)
    return gates


def _test_names(coverage, text: str) -> Counter:
    clean = coverage.strip_rust(text)
    return Counter(
        match.group("name") for match in coverage.ATTRIBUTED_FN_RE.finditer(clean)
        if coverage.TEST_ATTR_RE.search(text[match.start("attrs"):match.end("attrs")])
    )


def touched_sources(coverage, parity, repo_root, base, changed, head_ids, sites, aliases):
    """New test fns the lib inventory cannot see, and new test-named cfg(test) modules
    without a single inventoried id, in the changed Rust files."""
    findings = []
    src = (repo_root / "src").resolve()
    for path, status in sorted(changed.items()):
        if status == "D" or not path.endswith(".rs"):
            continue
        head = (repo_root / path).read_text("utf-8")
        old = _git(repo_root, "show", f"{base}:{path}") if status != "A" else ""
        lib = path.startswith("src/") and path != "src/main.rs" and not path.startswith("src/bin/")
        seen = {test_id.rsplit("::", 1)[-1] for test_id in sites.get(path, ())}
        for name in sorted((_test_names(coverage, head) - _test_names(coverage, old)).keys()):
            if not lib or name not in seen:
                findings.append(parity.Finding(TARGET, path, (
                    f"new test fn `{name}` is in no lib inventory id"
                    + ("" if lib else " (not the lib target)")
                    + "; only lib ids are statically proven, so move it into the lib or wait "
                    "for per-target inventories"
                )))
        if not lib:
            continue
        physical = coverage.file_module_path(src, (repo_root / path).resolve())
        added = (coverage.test_modules_in_source(head, physical)
                 - coverage.test_modules_in_source(old, physical))
        for module in sorted(m for m in added if TEST_MODULE.search(m.rsplit("::", 1)[-1])):
            logical = "::".join(coverage._normalize_alias_path(tuple(module.split("::")), aliases))
            if not any(test_id.startswith(logical + "::") for test_id in head_ids):
                findings.append(parity.Finding(EMPTY_MODULE, path, (
                    f"new test module `{logical}` adds no id to the lib inventory manifest; "
                    "its tests are invisible to every lane gate"
                )))
    return findings


def judge_id(test_id, site, sources, pg_tests, pg_only, gates, static_only):
    """A Mapped proof through some required PR job, else [(kind, reason)] per selecting job."""
    selecting: dict[str, list] = {}
    for source in sources.sources:
        lane = source.lane
        if source.origin in ("pr-required", "pr-advisory") and lane is not None \
                and lane.selects_test(test_id) and (lane not in pg_only or test_id in pg_tests):
            selecting.setdefault(source.owner, []).append(source)
    if not selecting:
        return [(UNREQUIRED, "no PR job selects it")]
    reasons: list[tuple[str, str]] = []
    for job, group in sorted(selecting.items(), key=lambda kv: kv[1][0].origin != "pr-required"):
        mirrors = [m for m in sources.mirrors if m.upstream == job]
        running = [source for source in group if source.runs]
        gate = gates.get(job, Gate((), (PATH, "parse_jobs did not return it"), None))
        live = [m for m in mirrors if m.proven and not m.conditional]
        excluded = static_only(gate.platform)
        if group[0].origin != "pr-required":
            reasons.append((CONTEXT, f"{job}'s mirror {mirrors[0].job} is unproven") if mirrors
                           else (UNREQUIRED, f"only the non-required PR job {job} selects it"))
        elif not running:
            reasons.append((RUNMODE, f"{job} only lists or runs ignored ids, {group[0].location}"))
        elif gate.problem:
            reasons.append((gate.problem[0], f"{job}: {gate.problem[1]}"))
        elif not live:
            reasons.append((CONTEXT, f"{job}'s mirror {mirrors[0].job} runs under a step `if:`"))
        elif gate.patterns is not None and not (site and _activates(gate.patterns, site)):
            reasons.append((PATH, f"{job}: {site or 'its unknown source'} wakes no path filter"))
        elif excluded is None or test_id in excluded:
            reasons.append((PLATFORM, f"{job}: {gate.platform or 'its runner'} lacks it"))
        else:
            gate_text = "no path gate" if gate.patterns is None else f"path {site}"
            first = running[0].location
            return Mapped(test_id, job, first, live[0].job, live[0].context, gate_text)
    return reasons


def judge(repo_root: Path, base: str, coverage, parity, sources, pg_tests, skip_args, aliases,
          today: datetime.date | None = None) -> Verdict:
    """Every added id must be mapped or carry a live exception; exceptions must stay needed."""
    cti = parity._sibling("check_test_target_integrity.py")
    pgm = parity._sibling("check_pg_test_lane_membership.py")
    manifest = cti.LIB_INVENTORY_MANIFEST_REL.as_posix()
    base_ids = cti.parse_lib_inventory_manifest(
        _git(repo_root, "show", f"{base}:{manifest}"), f"{base}:{manifest}"
    )
    head_ids = parity.load_test_ids(repo_root)
    added = sorted(head_ids - base_ids)
    changed = changed_paths(repo_root, base)
    today = today or datetime.datetime.now(datetime.timezone.utc).date()
    rows, findings = load_exceptions(parity, repo_root / EXCEPTIONS_REL, today)
    sites: dict[str, set[str]] = {}
    site_of: dict[str, str] = {}
    if rows or manifest in changed or any(p.endswith(".rs") for p in changed):
        inventory = cti.collect_static_tests(cti.discover_targets(repo_root)["lib"], repo_root)
        for test_id, site in [*inventory.tests.items(),
                              *((d[0], d[2]) for d in inventory.duplicate_tests)]:
            site_of.setdefault(test_id, site.rsplit(":", 1)[0])
            sites.setdefault(site.rsplit(":", 1)[0], set()).add(test_id)
        stale = sorted(set(inventory.tests) ^ head_ids)
        problems = [f"{m} unresolved at {s}" for m, s in sorted(inventory.module_errors.items())]
        if stale:
            problems.insert(0, f"{len(stale)} id(s) differ from source: {', '.join(stale[:3])}")
        if problems:
            findings.append(parity.Finding(STALE_MANIFEST, manifest, "; ".join(problems) + (
                f"; regenerate with {cti.lib_inventory_regeneration_command()} and review it"
            )))
        findings.extend(touched_sources(
            coverage, parity, repo_root, base, changed, head_ids, sites, aliases
        ))
    gates = job_gates(parity, pgm, sources.pr_jobs)
    lanes = [s.lane for s in sources.sources if s.origin.startswith("pr-") and s.lane]
    pg_only = coverage.pg_lanes(lanes, skip_args)
    by_id = {row.test_id: row for row in rows}
    mapped: list[Mapped] = []
    excepted = 0
    for test_id in sorted(set(added) | set(by_id)):
        row = by_id.get(test_id)
        location = (f"{EXCEPTIONS_REL.as_posix()}:{row.line}" if row
                    else site_of.get(test_id, manifest))
        if row and test_id not in head_ids:
            findings.append(parity.Finding(
                STALE, location, f"`{test_id}` is no lib inventory id; delete the row"
            ))
            continue
        outcome = judge_id(test_id, site_of.get(test_id), sources, pg_tests, pg_only, gates,
                           cti.expected_lib_static_only)
        if isinstance(outcome, Mapped):
            mapped.extend([outcome] if test_id in added else [])
            if row:
                findings.append(parity.Finding(STALE, location, (
                    f"`{test_id}` is now required through {outcome.job}; delete the row"
                )))
            continue
        kind = outcome[0][0]
        if row and kind in MODE_KINDS[row.mode]:
            excepted += test_id in added
            continue
        if row:
            findings.append(parity.Finding(STALE, location, (
                f"`{test_id}` now fails as {kind}, which mode {row.mode} does not cover"
            )))
        if test_id in added:
            findings.append(parity.Finding(kind, location, (
                f"`{test_id}`: " + "; ".join(reason for _, reason in outcome)
                + f"; run it in a required PR job ({sources.required_test_jobs}) or record "
                f"`{test_id} | <mode> | #<issue> | expires=YYYY-MM-DD | <reason>` in "
                + EXCEPTIONS_REL.as_posix()
            )))
    return Verdict(base, tuple(findings), len(added), tuple(mapped), excepted)
