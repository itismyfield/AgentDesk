"""Static registry core for tracked test suites and script gates.

This module deliberately contains no allowlist or enforcement CLI.  S3a-2 owns
that policy.  Reachability here is limited to configured CI roots plus literal
shell-script calls recursively reached from those roots; dynamic command
construction is reported as unsupported instead of being treated as unwired.
"""

from __future__ import annotations

import ast
import fnmatch
import json
import re
import subprocess
from collections import Counter, deque
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path, PurePosixPath
from typing import Collection, Iterable, Mapping, Sequence


class Family(str, Enum):
    TESTS_PY = "tests_py"
    E2E_PY = "e2e_py"
    SCRIPTS_PY = "scripts_py"
    SHELL = "shell"
    GATE = "gate_candidates"


class CandidateKind(str, Enum):
    PYTHON_SUITE = "python_suite"
    SHELL_SUITE = "shell_suite"
    GATE = "gate"


class ExecStatus(str, Enum):
    FULL = "FULL"
    PARTIAL = "PARTIAL"
    GLOB = "GLOB"
    NONE = "NONE"


class PinStatus(str, Enum):
    PINNED = "PINNED"
    REQUIRED_ARRAY = "REQUIRED_ARRAY"
    GLOB_UNPINNED = "GLOB_UNPINNED"
    NONE = "NONE"


class DiagnosticKind(str, Enum):
    SOURCE_MISSING = "source_missing"
    SOURCE_UNREADABLE = "source_unreadable"
    SOURCE_MALFORMED = "source_malformed"
    TRACKED_INPUT = "tracked_input"
    PYTHON_MALFORMED = "python_malformed"
    HARNESS_MISMATCH = "harness_mismatch"
    UNSUPPORTED_CALL = "unsupported_call"


@dataclass(frozen=True)
class Diagnostic:
    kind: DiagnosticKind
    message: str
    source: str | None = None
    fatal: bool = False


@dataclass(frozen=True)
class Invocation:
    source: str
    target: str
    selector: str | None = None
    glob: bool = False
    runner: str = "literal"


@dataclass(frozen=True)
class Candidate:
    path: str
    family: Family
    kind: CandidateKind
    execution: ExecStatus
    pin: PinStatus
    tests: tuple[str, ...] = ()
    executed_tests: tuple[str, ...] = ()
    invocations: tuple[Invocation, ...] = ()

    @property
    def unexecuted_tests(self) -> tuple[str, ...]:
        return tuple(test for test in self.tests if test not in self.executed_tests)


@dataclass(frozen=True)
class FamilyViolation:
    family: Family
    expected_minimum: int
    actual: int


@dataclass(frozen=True)
class PartialViolation:
    path: str
    targets: tuple[str, ...]
    unexecuted_tests: tuple[str, ...]


DEFAULT_PATTERNS: Mapping[Family, tuple[str, ...]] = {
    Family.TESTS_PY: ("tests/test_*.py", "tests/**/test_*.py"),
    Family.E2E_PY: ("scripts/e2e/test_*.py", "scripts/e2e/**/test_*.py"),
    Family.SCRIPTS_PY: ("scripts/test_*.py", "scripts/**/test_*.py"),
    Family.SHELL: ("tests/test_*.sh", "tests/**/test_*.sh"),
    Family.GATE: (
        "scripts/check_*.py", "scripts/audit_*.py",
        "scripts/check-*.py", "scripts/check-*.sh",
    ),
}
DEFAULT_SOURCES = (
    "scripts/ci-script-checks.sh", "justfile", "package.json", "Makefile",
)


@dataclass(frozen=True)
class RegistryConfig:
    patterns: Mapping[Family, tuple[str, ...]] = field(
        default_factory=lambda: dict(DEFAULT_PATTERNS)
    )
    surface_sources: tuple[str, ...] | None = None


@dataclass(frozen=True)
class RegistryResult:
    candidates: tuple[Candidate, ...]
    diagnostics: tuple[Diagnostic, ...]
    family_counts: Mapping[Family, int]
    reachable_sources: tuple[str, ...]
    reachability_contract: str = "configured roots + recursive literal shell calls"

    @property
    def input_valid(self) -> bool:
        return not any(diagnostic.fatal for diagnostic in self.diagnostics)

    @property
    def suite_status_counts(self) -> Mapping[ExecStatus, int]:
        counts = Counter(
            candidate.execution for candidate in self.candidates
            if candidate.kind is not CandidateKind.GATE
        )
        return {status: counts.get(status, 0) for status in ExecStatus}

    @property
    def suite_pin_counts(self) -> Mapping[PinStatus, int]:
        counts = Counter(
            candidate.pin for candidate in self.candidates
            if candidate.kind is not CandidateKind.GATE
        )
        return {status: counts.get(status, 0) for status in PinStatus}

    def family_violations(
        self, floors: Mapping[Family, int]
    ) -> tuple[FamilyViolation, ...]:
        return tuple(
            FamilyViolation(family, minimum, self.family_counts.get(family, 0))
            for family, minimum in sorted(floors.items(), key=lambda item: item[0].value)
            if self.family_counts.get(family, 0) < minimum
        )

    def partial_violations(
        self, allowed_exact_targets: Collection[tuple[str, str]] = ()
    ) -> tuple[PartialViolation, ...]:
        allowed = set(allowed_exact_targets)
        violations = []
        for candidate in self.candidates:
            if candidate.execution is not ExecStatus.PARTIAL:
                continue
            targets = tuple(sorted(
                invocation.target + (f".{invocation.selector}" if invocation.selector else "")
                for invocation in candidate.invocations if not invocation.glob
            ))
            if targets and all((candidate.path, target) in allowed for target in targets):
                continue
            violations.append(PartialViolation(candidate.path, targets, candidate.unexecuted_tests))
        return tuple(violations)


@dataclass(frozen=True)
class _PythonTests:
    all: tuple[str, ...]
    unittest: tuple[str, ...]
    has_unittest_main: bool


def _matches(path: str, patterns: Sequence[str]) -> bool:
    return any(PurePosixPath(path).match(pattern) for pattern in patterns)


def _family(path: str, config: RegistryConfig) -> tuple[Family, CandidateKind] | None:
    ordered = (
        (Family.TESTS_PY, CandidateKind.PYTHON_SUITE),
        (Family.E2E_PY, CandidateKind.PYTHON_SUITE),
        (Family.SCRIPTS_PY, CandidateKind.PYTHON_SUITE),
        (Family.SHELL, CandidateKind.SHELL_SUITE),
        (Family.GATE, CandidateKind.GATE),
    )
    for family, kind in ordered:
        if _matches(path, config.patterns.get(family, ())):
            return family, kind
    return None


def _tracked_files(repo_root: Path) -> tuple[list[str], Diagnostic | None]:
    try:
        run = subprocess.run(
            ["git", "ls-files", "-z"], cwd=repo_root, check=False,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
    except OSError as error:
        return [], Diagnostic(DiagnosticKind.TRACKED_INPUT, str(error), fatal=True)
    if run.returncode:
        detail = run.stderr.decode("utf-8", "replace").strip()
        return [], Diagnostic(DiagnosticKind.TRACKED_INPUT, detail, fatal=True)
    try:
        paths = [part.decode("utf-8") for part in run.stdout.split(b"\0") if part]
    except UnicodeDecodeError as error:
        return [], Diagnostic(DiagnosticKind.TRACKED_INPUT, str(error), fatal=True)
    return sorted(paths), None


def _read_source(repo_root: Path, relative: str) -> tuple[str | None, Diagnostic | None]:
    path = repo_root / relative
    if not path.exists():
        return None, Diagnostic(
            DiagnosticKind.SOURCE_MISSING, f"required source missing: {relative}",
            relative, True,
        )
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        return None, Diagnostic(
            DiagnosticKind.SOURCE_UNREADABLE, f"cannot read {relative}: {error}",
            relative, True,
        )
    if "\0" in text:
        return None, Diagnostic(
            DiagnosticKind.SOURCE_MALFORMED, f"NUL byte in {relative}", relative, True,
        )
    return text, None


def _logical_lines(text: str, source: str) -> tuple[list[str], Diagnostic | None]:
    result: list[str] = []
    pending = ""
    for raw in text.splitlines():
        line = raw.rstrip()
        if line.endswith("\\"):
            pending += line[:-1].rstrip() + " "
            continue
        result.append(pending + line.lstrip() if pending else line)
        pending = ""
    if pending:
        return result, Diagnostic(
            DiagnosticKind.SOURCE_MALFORMED,
            f"unterminated backslash continuation in {source}", source, True,
        )
    return result, None


def _strip_comment(line: str) -> str:
    single = double = False
    for index, char in enumerate(line):
        if char == "'" and not double:
            single = not single
        elif char == '"' and not single and (index == 0 or line[index - 1] != "\\"):
            double = not double
        elif char == "#" and not single and not double:
            return line[:index]
    return line


def _source_commands(text: str, source: str) -> tuple[list[str], Diagnostic | None]:
    if source == "package.json":
        try:
            payload = json.loads(text)
        except json.JSONDecodeError as error:
            return [], Diagnostic(
                DiagnosticKind.SOURCE_MALFORMED,
                f"malformed package.json: {error}", source, True,
            )
        scripts = payload.get("scripts") if isinstance(payload, dict) else None
        if scripts is not None and not isinstance(scripts, dict):
            return [], Diagnostic(
                DiagnosticKind.SOURCE_MALFORMED,
                "package.json scripts must be an object", source, True,
            )
        return [value for value in (scripts or {}).values() if isinstance(value, str)], None
    lines, error = _logical_lines(text, source)
    commands = []
    for line in lines:
        command = _strip_comment(line).strip()
        if not command or re.match(r"(?:self\.)?assert\w*\s*\(", command):
            continue
        if (
            not command.startswith("required_shell_suites=")
            and re.match(r"[A-Za-z_]\w*\s*=\s*[('\"\[]", command)
        ):
            continue
        commands.append(command)
    return commands, error


def _python_tests(repo_root: Path, path: str) -> tuple[_PythonTests | None, Diagnostic | None]:
    try:
        tree = ast.parse((repo_root / path).read_text(encoding="utf-8"), filename=path)
    except (OSError, UnicodeError, SyntaxError) as error:
        return None, Diagnostic(
            DiagnosticKind.PYTHON_MALFORMED, f"cannot parse {path}: {error}", path, True,
        )
    all_tests: list[str] = []
    class_tests: dict[str, list[str]] = {}
    class_bases: dict[str, list[str]] = {}
    has_main = False
    for node in tree.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith("test_"):
            all_tests.append(node.name)
        if isinstance(node, ast.ClassDef):
            class_bases[node.name] = [ast.unparse(base) for base in node.bases]
            class_tests[node.name] = []
            for child in node.body:
                if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)) and child.name.startswith("test_"):
                    test_id = f"{node.name}.{child.name}"
                    all_tests.append(test_id)
                    class_tests[node.name].append(test_id)
    unittest_classes = {
        name for name, bases in class_bases.items()
        if any(base == "TestCase" or base.endswith(".TestCase") for base in bases)
    }
    changed = True
    while changed:
        changed = False
        for name, bases in class_bases.items():
            if name not in unittest_classes and any(base in unittest_classes for base in bases):
                unittest_classes.add(name)
                changed = True
    unittest_tests = [
        test for name in unittest_classes for test in class_tests.get(name, ())
    ]
    for node in ast.walk(tree):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
            has_main = has_main or (
                isinstance(node.func.value, ast.Name)
                and node.func.value.id == "unittest" and node.func.attr == "main"
            )
    return _PythonTests(tuple(sorted(all_tests)), tuple(sorted(unittest_tests)), has_main), None


_UNITTEST = re.compile(r"(?:^|\s)-m\s+unittest\s+(.+)")
_PYTEST = re.compile(r"(?:^|\s)(?:python\S*\s+-m\s+)?pytest\s+(.+)")
_PYTHON_PATH = re.compile(r'(?:^|\s)(?:"?\$PYTHON"?|python\d*(?:\.\d+)?)\s+((?:tests|scripts)/[^\s;|&]+\.py)')
_SHELL_PATH = re.compile(r"(?:^|\s)(?:bash|sh|source)\s+((?:tests|scripts)/[^\s;|&]+\.sh)")
_DIRECT_SHELL = re.compile(r"(?:^|\s)\./((?:tests|scripts)/[^\s;|&]+\.sh)")
_GATE_PATH = re.compile(r'(?:^|\s)(?:"?\$PYTHON"?|python\S*)\s+(scripts/(?:check[_-]|audit_)[^\s;|&]+\.py)')


def _words(tail: str) -> list[str]:
    return re.findall(r"[A-Za-z_][\w]*(?:\.[A-Za-z_][\w]*)+|(?:tests|scripts)/[^\s;|&]+", tail)


def _collect_invocations(
    commands: Iterable[tuple[str, str]], candidates: Sequence[str]
) -> tuple[dict[str, list[Invocation]], set[str], set[str], list[Diagnostic]]:
    invocations: dict[str, list[Invocation]] = {path: [] for path in candidates}
    required: set[str] = set()
    reached_scripts: set[str] = set()
    diagnostics: list[Diagnostic] = []
    modules = {path[:-3].replace("/", "."): path for path in candidates if path.endswith(".py")}
    combined = " ".join(command for _, command in commands)
    for array in re.findall(r"required_shell_suites\s*=\(([^)]*)\)", combined):
        required.update(re.findall(r"tests/[^\s)]+\.sh", array))
    for variable, pattern in re.findall(
        r"for\s+(\w+)\s+in\s+(tests/[^\s;]+\.sh)", combined
    ):
        if re.search(
            rf"\b(?:bash|sh)\s+[\"']?\${{{variable}}}|"
            rf"\b(?:bash|sh)\s+[\"']?\${variable}", combined,
        ):
            for path in candidates:
                if fnmatch.fnmatchcase(path, pattern):
                    invocations[path].append(Invocation("shell-glob", pattern, glob=True))
    for source, command in commands:
        if re.search(r"\b(?:xargs|docker\s+run)\b|\bfind\b.*-(?:exec|execdir)\b", command) and re.search(r"test_|unittest|pytest", command):
            diagnostics.append(Diagnostic(
                DiagnosticKind.UNSUPPORTED_CALL,
                f"unsupported dynamic test invocation: {command}", source, True,
            ))
        unit = _UNITTEST.search(command)
        if unit:
            if "$" in unit.group(1):
                diagnostics.append(Diagnostic(
                    DiagnosticKind.UNSUPPORTED_CALL,
                    f"dynamic unittest target: {unit.group(1)}", source, True,
                ))
            for target in _words(unit.group(1)):
                matches = [module for module in modules if target == module or target.startswith(module + ".")]
                if not matches:
                    if "$" in target:
                        diagnostics.append(Diagnostic(
                            DiagnosticKind.UNSUPPORTED_CALL,
                            f"dynamic unittest target: {target}", source, True,
                        ))
                    continue
                module = max(matches, key=len)
                selector = target[len(module) + 1:] if target != module else None
                path = modules[module]
                invocations[path].append(Invocation(source, module, selector, runner="unittest"))
        pytest = _PYTEST.search(command)
        if pytest:
            if "$" in pytest.group(1):
                diagnostics.append(Diagnostic(
                    DiagnosticKind.UNSUPPORTED_CALL,
                    f"dynamic pytest target: {pytest.group(1)}", source, True,
                ))
            for target in _words(pytest.group(1)):
                raw_path, _, selector = target.partition("::")
                for path in candidates:
                    if not path.endswith(".py"):
                        continue
                    is_glob = any(char in raw_path for char in "*?[")
                    if path == raw_path or (is_glob and fnmatch.fnmatchcase(path, raw_path)) or (
                        raw_path.endswith("/") and path.startswith(raw_path)
                    ):
                        invocations[path].append(Invocation(
                            source, raw_path, selector or None,
                            is_glob or path != raw_path, "pytest",
                        ))
        for regex in (_SHELL_PATH, _DIRECT_SHELL):
            for match in regex.finditer(command):
                path = match.group(1)
                if path in invocations:
                    invocations[path].append(Invocation(source, path))
                if path.startswith("scripts/"):
                    reached_scripts.add(path)
        for match in re.finditer(
            r'(?:bash|sh|source)\s+["\']?\$SCRIPT_DIR/([^\s"\']+\.sh)', command
        ):
            path = str(PurePosixPath(source).parent / match.group(1))
            if path in invocations:
                invocations[path].append(Invocation(source, path))
            reached_scripts.add(path)
        for match in _PYTHON_PATH.finditer(command):
            path = match.group(1).strip('"\'')
            if path in invocations:
                invocations[path].append(Invocation(source, path, runner="script"))
        for match in _GATE_PATH.finditer(command):
            path = match.group(1).strip('"\'')
            if path in invocations:
                invocations[path].append(Invocation(source, path))
    return invocations, required, reached_scripts, diagnostics


def scan_registry(repo_root: Path, config: RegistryConfig | None = None) -> RegistryResult:
    config = config or RegistryConfig()
    tracked, tracked_error = _tracked_files(repo_root)
    diagnostics: list[Diagnostic] = [tracked_error] if tracked_error else []
    classified = [(path, _family(path, config)) for path in tracked]
    classified = [(path, value) for path, value in classified if value is not None]
    candidate_paths = [path for path, _ in classified]
    counts = Counter(family for _, (family, _) in classified)

    sources = list(config.surface_sources or DEFAULT_SOURCES)
    if config.surface_sources is None:
        workflows = sorted(path for path in tracked if fnmatch.fnmatchcase(path, ".github/workflows/*.yml"))
        if not workflows:
            diagnostics.append(Diagnostic(
                DiagnosticKind.SOURCE_MISSING, "no tracked .github/workflows/*.yml sources", fatal=True,
            ))
        sources.extend(workflows)
    reachable: list[str] = []
    queued = deque(sources)
    seen: set[str] = set()
    all_invocations: dict[str, list[Invocation]] = {path: [] for path in candidate_paths}
    required: set[str] = set()
    while queued:
        source = queued.popleft()
        if source in seen:
            continue
        seen.add(source)
        text, error = _read_source(repo_root, source)
        if error:
            diagnostics.append(error)
            continue
        reachable.append(source)
        source_commands, malformed = _source_commands(text or "", source)
        if malformed:
            diagnostics.append(malformed)
        batch = [(source, command) for command in source_commands]
        found, found_required, reached, unsupported = _collect_invocations(batch, candidate_paths)
        for path, values in found.items():
            all_invocations[path].extend(values)
        required.update(found_required)
        diagnostics.extend(unsupported)
        for path in sorted(reached):
            if path in tracked and path not in seen:
                queued.append(path)

    records: list[Candidate] = []
    for path, (family, kind) in classified:
        calls = tuple(all_invocations[path])
        direct = tuple(call for call in calls if not call.glob)
        glob = tuple(call for call in calls if call.glob)
        tests: tuple[str, ...] = ()
        executed: set[str] = set()
        harness_mismatch = False
        if kind is CandidateKind.PYTHON_SUITE:
            parsed, error = _python_tests(repo_root, path)
            if error:
                diagnostics.append(error)
            elif parsed:
                tests = parsed.all
                module = path[:-3].replace("/", ".")
                for call in direct:
                    if call.runner == "pytest":
                        if call.selector:
                            selector = call.selector.replace("::", ".")
                            executed.update(test for test in parsed.all if test == selector or test.startswith(selector + "."))
                        else:
                            executed.update(parsed.all)
                    elif call.target == path:
                        if parsed.has_unittest_main:
                            executed.update(parsed.unittest)
                        else:
                            harness_mismatch = True
                    elif call.target == module:
                        if not parsed.unittest:
                            harness_mismatch = True
                        elif call.selector:
                            executed.update(test for test in parsed.unittest if test == call.selector or test.startswith(call.selector + "."))
                        else:
                            executed.update(parsed.unittest)
                if harness_mismatch:
                    diagnostics.append(Diagnostic(
                        DiagnosticKind.HARNESS_MISMATCH,
                        f"execution form for {path} selects zero tests", path,
                    ))
                if glob:
                    executed.update(parsed.all)
        if direct and kind is not CandidateKind.PYTHON_SUITE:
            execution = ExecStatus.FULL
        elif direct and executed == set(tests) and tests and not any(call.selector for call in direct):
            execution = ExecStatus.FULL
        elif direct and executed:
            execution = ExecStatus.PARTIAL
        elif glob:
            execution = ExecStatus.GLOB
        else:
            execution = ExecStatus.NONE
        if direct and execution is not ExecStatus.NONE:
            pin = PinStatus.PINNED
        elif path in required:
            pin = PinStatus.REQUIRED_ARRAY
        elif glob:
            pin = PinStatus.GLOB_UNPINNED
        else:
            pin = PinStatus.NONE
        records.append(Candidate(
            path, family, kind, execution, pin, tests,
            tuple(sorted(executed)), calls,
        ))
    return RegistryResult(
        tuple(records), tuple(diagnostics),
        {family: counts.get(family, 0) for family in Family},
        tuple(reachable),
    )
