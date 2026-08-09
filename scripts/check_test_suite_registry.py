"""Tracked test-suite registry core; policy/enforcement belongs to S3a-2."""
from __future__ import annotations
import ast
import fnmatch
import json
import posixpath
import re
import shlex
import subprocess
from collections import Counter, deque
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path, PurePosixPath
from typing import Collection, Iterable, Mapping, Sequence
class Family(str, Enum):
    TESTS_PY = "tests_py"; E2E_PY = "e2e_py"; SCRIPTS_PY = "scripts_py"
    SHELL = "shell"; GATE = "gate_candidates"
class CandidateKind(str, Enum):
    PYTHON_SUITE = "python_suite"; SHELL_SUITE = "shell_suite"; GATE = "gate"
class ExecStatus(str, Enum):
    FULL = "FULL"; PARTIAL = "PARTIAL"; GLOB = "GLOB"; NONE = "NONE"
class PinStatus(str, Enum):
    PINNED = "PINNED"; REQUIRED_ARRAY = "REQUIRED_ARRAY"
    GLOB_UNPINNED = "GLOB_UNPINNED"; NONE = "NONE"
class DiagnosticKind(str, Enum):
    SOURCE_MISSING = "source_missing"; SOURCE_UNREADABLE = "source_unreadable"
    SOURCE_MALFORMED = "source_malformed"; TRACKED_INPUT = "tracked_input"
    PYTHON_MALFORMED = "python_malformed"; HARNESS_MISMATCH = "harness_mismatch"
    UNSUPPORTED_CALL = "unsupported_call"
@dataclass(frozen=True)
class Diagnostic:
    kind: DiagnosticKind; message: str; source: str | None = None; fatal: bool = False
@dataclass(frozen=True)
class Invocation:
    source: str; target: str; selector: str | None = None; glob: bool = False
    runner: str = "literal"; command: str = ""
@dataclass(frozen=True)
class PinEvidence:
    source: str; target: str; command: str; status: PinStatus
@dataclass(frozen=True)
class Candidate:
    path: str; family: Family; kind: CandidateKind; execution: ExecStatus; pin: PinStatus
    tests: tuple[str, ...] = (); executed_tests: tuple[str, ...] = ()
    invocations: tuple[Invocation, ...] = (); pin_evidence: tuple[PinEvidence, ...] = ()
    @property
    def unexecuted_tests(self) -> tuple[str, ...]:
        return tuple(test for test in self.tests if test not in self.executed_tests)
@dataclass(frozen=True)
class FamilyViolation:
    family: Family; expected_minimum: int; actual: int
@dataclass(frozen=True)
class PartialViolation:
    path: str; targets: tuple[str, ...]; unexecuted_tests: tuple[str, ...]
DEFAULT_PATTERNS: Mapping[Family, tuple[str, ...]] = {
    Family.TESTS_PY: ("tests/**/test_*.py",), Family.E2E_PY: ("scripts/e2e/**/test_*.py",),
    Family.SCRIPTS_PY: ("scripts/**/test_*.py",), Family.SHELL: ("tests/**/test_*.sh",),
    Family.GATE: ("scripts/check_*.py", "scripts/audit_*.py", "scripts/check-*.py", "scripts/check-*.sh"),
}
DEFAULT_SOURCES = ("scripts/ci-script-checks.sh", "justfile", "package.json", "Makefile")
@dataclass(frozen=True)
class RegistryConfig:
    patterns: Mapping[Family, tuple[str, ...]] = field(default_factory=lambda: dict(DEFAULT_PATTERNS)); surface_sources: tuple[str, ...] | None = None
@dataclass(frozen=True)
class RegistryResult:
    candidates: tuple[Candidate, ...]; diagnostics: tuple[Diagnostic, ...]
    family_counts: Mapping[Family, int]; reachable_sources: tuple[str, ...]
    reachability_contract: str = "configured roots + recursive literal shell calls"
    @property
    def input_valid(self) -> bool:
        return not any(item.fatal for item in self.diagnostics)
    def _counts(self, attribute: str, values: Iterable[Enum]) -> Mapping[Enum, int]:
        counts = Counter(getattr(c, attribute) for c in self.candidates if c.kind is not CandidateKind.GATE)
        return {value: counts.get(value, 0) for value in values}
    @property
    def suite_status_counts(self) -> Mapping[ExecStatus, int]:
        return self._counts("execution", ExecStatus)
    @property
    def suite_pin_counts(self) -> Mapping[PinStatus, int]:
        return self._counts("pin", PinStatus)
    def family_violations(self, floors: Mapping[Family, int]) -> tuple[FamilyViolation, ...]:
        return tuple(FamilyViolation(f, minimum, self.family_counts.get(f, 0))
                     for f, minimum in sorted(floors.items(), key=lambda item: item[0].value)
                     if self.family_counts.get(f, 0) < minimum)
    def partial_violations(self, allowed_exact_targets: Collection[tuple[str, str]] = ()) -> tuple[PartialViolation, ...]:
        allowed, violations = set(allowed_exact_targets), []
        for candidate in self.candidates:
            if candidate.execution is not ExecStatus.PARTIAL:
                continue
            targets = tuple(sorted(call.target + (f".{call.selector}" if call.selector else "")
                                   for call in candidate.invocations if not call.glob))
            if not targets or not all((candidate.path, target) in allowed for target in targets):
                violations.append(PartialViolation(candidate.path, targets, candidate.unexecuted_tests))
        return tuple(violations)
@dataclass(frozen=True)
class _PythonTests:
    all: tuple[str, ...]; unittest: tuple[str, ...]; has_unittest_main: bool
def _matches(path: str, patterns: Sequence[str]) -> bool:
    """Match only the registry's anchored POSIX family grammar; **/ is zero-or-more directories."""
    def expression(pattern: str) -> str:
        escaped = re.escape(pattern).replace(r"\*\*/", r"(?:[^/]+/)*")
        return escaped.replace(r"\*", r"[^/]*").replace(r"\?", r"[^/]")
    return any(re.fullmatch(expression(pattern), path) is not None for pattern in patterns)
def _family(path: str, config: RegistryConfig) -> tuple[Family, CandidateKind] | None:
    ordered = ((Family.TESTS_PY, CandidateKind.PYTHON_SUITE), (Family.E2E_PY, CandidateKind.PYTHON_SUITE),
               (Family.SCRIPTS_PY, CandidateKind.PYTHON_SUITE),
               (Family.SHELL, CandidateKind.SHELL_SUITE), (Family.GATE, CandidateKind.GATE))
    return next(((family, kind) for family, kind in ordered
                 if _matches(path, config.patterns.get(family, ()))), None)
def _tracked_files(root: Path) -> tuple[list[str], Diagnostic | None]:
    try:
        run = subprocess.run(["git", "ls-files", "-z"], cwd=root, check=False,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    except OSError as error:
        return [], Diagnostic(DiagnosticKind.TRACKED_INPUT, str(error), fatal=True)
    if run.returncode:
        return [], Diagnostic(DiagnosticKind.TRACKED_INPUT,
                              run.stderr.decode("utf-8", "replace").strip(), fatal=True)
    try:
        return sorted(part.decode() for part in run.stdout.split(b"\0") if part), None
    except UnicodeDecodeError as error:
        return [], Diagnostic(DiagnosticKind.TRACKED_INPUT, str(error), fatal=True)
def _read_source(root: Path, relative: str) -> tuple[str | None, Diagnostic | None]:
    path = root / relative
    if not path.exists():
        return None, Diagnostic(DiagnosticKind.SOURCE_MISSING, f"required source missing: {relative}", relative, True)
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        return None, Diagnostic(DiagnosticKind.SOURCE_UNREADABLE, f"cannot read {relative}: {error}", relative, True)
    if "\0" in text:
        return None, Diagnostic(DiagnosticKind.SOURCE_MALFORMED, f"NUL byte in {relative}", relative, True)
    return text, None
def _parse_python(text: str, path: str) -> tuple[ast.Module | None, Diagnostic | None]:
    try:
        return ast.parse(text, filename=path), None
    except SyntaxError as error:
        return None, Diagnostic(DiagnosticKind.PYTHON_MALFORMED,
                                f"cannot parse {path}: {error}", path, True)
def _logical_lines(text: str, source: str) -> tuple[list[str], Diagnostic | None]:
    result, pending = [], ""
    for raw in text.splitlines():
        line = raw.rstrip()
        if line.endswith("\\"):
            pending += line[:-1].rstrip() + " "
        else:
            result.append(pending + line.lstrip() if pending else line)
            pending = ""
    error = (Diagnostic(DiagnosticKind.SOURCE_MALFORMED,
                        f"unterminated backslash continuation in {source}", source, True)
             if pending else None)
    return result, error
def _strip_comment(line: str) -> str:
    single = double = False
    for index, char in enumerate(line):
        if char == "'" and not double:
            single = not single
        elif char == '"' and not single and (not index or line[index - 1] != "\\"):
            double = not double
        elif char == "#" and not single and not double:
            return line[:index]
    return line
def _workflow_payloads(text: str) -> list[str]:
    lines, payloads, index = text.splitlines(), [], 0
    while index < len(lines):
        match = re.match(r"^(\s*)(?:-\s+)?run:\s*(.*?)\s*$", lines[index])
        if not match:
            index += 1
            continue
        indent, value = len(match.group(1)), match.group(2)
        if re.fullmatch(r"[|>][+-]?", value):
            block, index = [], index + 1
            while index < len(lines):
                raw = lines[index]
                width = len(raw) - len(raw.lstrip())
                if raw.strip() and width <= indent:
                    break
                block.append(raw)
                index += 1
            widths = [len(line) - len(line.lstrip()) for line in block if line.strip()]
            margin = min(widths) if widths else 0
            payloads.append("\n".join(line[margin:] for line in block))
            continue
        if value and not value.startswith("#"):
            if len(value) > 1 and value[0] == value[-1] and value[0] in "'\"":
                try:
                    value = ast.literal_eval(value)
                except (SyntaxError, ValueError):
                    pass
            payloads.append(value)
        index += 1
    return payloads
def _segments(command: str) -> list[list[str]]:
    lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|")
    lexer.whitespace_split, lexer.commenters = True, ""
    segments, current = [], []
    for token in lexer:
        if token and set(token) <= set(";&|"):
            if current:
                segments.append(current)
            current = []
        else:
            current.append(token)
    if current:
        segments.append(current)
    return segments
def _source_commands(text: str, source: str) -> tuple[list[str], Diagnostic | None]:
    if source == "package.json":
        try:
            payload = json.loads(text)
        except json.JSONDecodeError as error:
            return [], Diagnostic(DiagnosticKind.SOURCE_MALFORMED,
                                  f"malformed package.json: {error}", source, True)
        scripts = payload.get("scripts") if isinstance(payload, dict) else None
        if scripts is not None and not isinstance(scripts, dict):
            return [], Diagnostic(DiagnosticKind.SOURCE_MALFORMED,
                                  "package.json scripts must be an object", source, True)
        payloads = [value for value in (scripts or {}).values() if isinstance(value, str)]
    elif source.startswith(".github/workflows/") and source.endswith((".yml", ".yaml")):
        payloads = _workflow_payloads(text)
    else:
        payloads = [text]
    commands, delimiter = [], None
    for payload in payloads:
        lines, error = _logical_lines(payload, source)
        if error:
            return commands, error
        for line in lines:
            if delimiter:
                delimiter = None if line.strip() == delimiter else delimiter
                continue
            command = _strip_comment(line).strip()
            if not command or re.match(r"(?:self\.)?assert\w*\s*\(", command):
                continue
            commands.append(command)
            if match := re.search(r"(?<!<)<<-?(?!<)\s*['\"]?([A-Za-z_]\w*)", command):
                delimiter = match.group(1)
    if delimiter:
        return commands, Diagnostic(DiagnosticKind.SOURCE_MALFORMED,
                                    f"unterminated heredoc in {source}", source, True)
    validated, pending = [], ""
    for command in commands:
        pending = f"{pending}\n{command}".strip()
        try:
            _segments(pending)
        except ValueError as error:
            if "No closing quotation" in str(error):
                continue
            return validated, Diagnostic(DiagnosticKind.SOURCE_MALFORMED,
                                         f"malformed shell command in {source}: {error}", source, True)
        validated.append(pending); pending = ""
    error = (Diagnostic(DiagnosticKind.SOURCE_MALFORMED,
                        f"malformed shell command in {source}: unterminated quote", source, True) if pending else None)
    return validated, error
def _live_unittest_main(node: ast.AST) -> bool:
    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef, ast.Lambda)):
        return False
    if (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
            and isinstance(node.func.value, ast.Name)
            and node.func.value.id == "unittest" and node.func.attr == "main"):
        return True
    return any(_live_unittest_main(child) for child in ast.iter_child_nodes(node))
def _python_tests(tree: ast.Module) -> _PythonTests:
    all_tests, class_tests, bases = [], {}, {}
    for node in tree.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith("test_"):
            all_tests.append(node.name)
        if isinstance(node, ast.ClassDef):
            bases[node.name], class_tests[node.name] = [ast.unparse(base) for base in node.bases], []
            for child in node.body:
                if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)) and child.name.startswith("test_"):
                    test_id = f"{node.name}.{child.name}"
                    all_tests.append(test_id)
                    class_tests[node.name].append(test_id)
    unittest_classes = {name for name, values in bases.items()
                        if any(value == "TestCase" or value.endswith(".TestCase") for value in values)}
    while True:
        expanded = unittest_classes | {name for name, values in bases.items()
                                       if any(value in unittest_classes for value in values)}
        if expanded == unittest_classes:
            break
        unittest_classes = expanded
    unittest_tests = [test for name in unittest_classes for test in class_tests.get(name, ())]
    return _PythonTests(tuple(sorted(all_tests)), tuple(sorted(unittest_tests)),
                        any(_live_unittest_main(node) for node in tree.body))
def _command_head(tokens: Sequence[str]) -> tuple[str, list[str]]:
    words = list(tokens)
    while words and words[0] in {"do", "then", "else", "elif", "if", "while", "until", "!"}:
        words.pop(0)
    while words and re.match(r"^[A-Za-z_]\w*=", words[0]):
        words.pop(0)
    if not words:
        return "", []
    return words[0].lstrip("@+-"), words[1:]
def _dynamic(value: str) -> bool:
    return "$" in value or "{{" in value or "}}" in value
def _repo_path(value: str, source: str) -> tuple[str | None, bool]:
    script_dir = None
    for prefix in ("$SCRIPT_DIR/", "${SCRIPT_DIR}/"):
        if value.startswith(prefix):
            script_dir, value = str(PurePosixPath(source).parent), value[len(prefix):]
            break
    if _dynamic(value):
        return None, False
    raw = posixpath.join(script_dir, value) if script_dir else value
    normalized = posixpath.normpath(raw)
    if normalized.startswith("/") or normalized == ".." or normalized.startswith("../"):
        return None, True
    return normalized.removeprefix("./"), False
def _unique_invocations(values: Iterable[Invocation]) -> tuple[Invocation, ...]:
    key = lambda item: (item.source, item.command, item.target, item.selector or "", item.glob, item.runner)
    return tuple(sorted({key(item): item for item in values}.values(), key=key))
def _unique_pins(values: Iterable[PinEvidence]) -> tuple[PinEvidence, ...]:
    key = lambda item: (item.source, item.command, item.target, item.status.value)
    return tuple(sorted({key(item): item for item in values}.values(), key=key))
def _collect_invocations(commands: Iterable[tuple[str, str]], candidates: Sequence[str]) -> tuple[
        dict[str, list[Invocation]], dict[str, list[PinEvidence]], set[str], list[Diagnostic]]:
    calls = {path: [] for path in candidates}
    pins = {path: [] for path in candidates}
    reached, diagnostics = set(), []
    modules = {path[:-3].replace("/", "."): path for path in candidates if path.endswith(".py")}
    rows = list(commands)
    by_source: dict[str, list[str]] = {}
    loop_vars: dict[tuple[str, str], str] = {}
    for source, command in rows:
        by_source.setdefault(source, []).append(command)
        for segment in _segments(command):
            executable, args = _command_head(segment)
            if executable == "for" and "in" in args:
                split = args.index("in")
                if split == 1 and split + 1 < len(args):
                    loop_vars[source, args[0]] = args[split + 1]
    for source, source_commands in by_source.items():
        joined = "\n".join(source_commands)
        for match in re.finditer(r"required_shell_suites\s*=\((.*?)\)", joined, re.S):
            declaration = match.group(0)
            for path in re.findall(r"tests/[^\s)]+\.sh", match.group(1)):
                if path in pins:
                    pins[path].append(PinEvidence(source, path, declaration, PinStatus.REQUIRED_ARRAY))
    def record(path: str, call: Invocation, status: PinStatus = PinStatus.PINNED) -> None:
        if path in calls:
            calls[path].append(call)
            pins[path].append(PinEvidence(call.source, call.target, call.command, status))
    def unsupported(source: str, command: str, detail: str) -> None:
        diagnostics.append(Diagnostic(DiagnosticKind.UNSUPPORTED_CALL,
                                      f"{detail}: {command}", source, True))
    for source, command in rows:
        for segment in _segments(command):
            executable, args = _command_head(segment)
            python = bool(re.fullmatch(r"(?:python\d*(?:\.\d+)?|\$\{?PYTHON\}?)", executable))
            runner, targets = "", []
            if executable == "pytest":
                runner, targets = "pytest", args
            elif python and len(args) >= 2 and args[:2] == ["-m", "unittest"]:
                runner, targets = "unittest", args[2:]
            elif python and len(args) >= 2 and args[:2] == ["-m", "pytest"]:
                runner, targets = "pytest", args[2:]
            if runner:
                for target in targets:
                    if target.startswith("-"):
                        continue
                    if _dynamic(target):
                        unsupported(source, command, f"dynamic {runner} target")
                        continue
                    if runner == "unittest":
                        matches = [module for module in modules
                                   if target == module or target.startswith(module + ".")]
                        if matches:
                            module = max(matches, key=len)
                            selector = target[len(module) + 1:] if target != module else None
                            record(modules[module], Invocation(source, module, selector,
                                                               runner="unittest", command=command))
                    else:
                        raw, _, selector = target.partition("::")
                        glob = any(char in raw for char in "*?[")
                        for path in candidates:
                            if path.endswith(".py") and (path == raw or (glob and fnmatch.fnmatchcase(path, raw))
                                                        or (raw.endswith("/") and path.startswith(raw))):
                                status = PinStatus.GLOB_UNPINNED if glob or path != raw else PinStatus.PINNED
                                record(path, Invocation(source, raw, selector or None,
                                                        status is PinStatus.GLOB_UNPINNED,
                                                        "pytest", command), status)
                continue
            if executable in {"bash", "sh", "source"}:
                target = next((arg for arg in args if not arg.startswith("-")), "")
                variable = target.removeprefix("${").removesuffix("}").removeprefix("$")
                pattern = loop_vars.get((source, variable)) if target.startswith("$") else None
                if pattern:
                    for path in candidates:
                        if fnmatch.fnmatchcase(path, pattern):
                            record(path, Invocation(source, pattern, glob=True,
                                                    runner="shell", command=command), PinStatus.GLOB_UNPINNED)
                    continue
                if not target:
                    continue
                if target in {"$0", "${0}"}:
                    continue
                if _dynamic(target) and not target.startswith(("$SCRIPT_DIR/", "${SCRIPT_DIR}/")):
                    unsupported(source, command, "dynamic shell target")
                    continue
                path, escaped = _repo_path(target, source)
                if escaped:
                    unsupported(source, command, "shell target escapes repository")
                    continue
                if path and path.endswith(".sh"):
                    record(path, Invocation(source, path, runner="shell", command=command))
                    if path.startswith("scripts/"):
                        reached.add(path)
                continue
            if executable.startswith("./"):
                path, escaped = _repo_path(executable, source)
                if escaped:
                    unsupported(source, command, "shell target escapes repository")
                elif path and path.endswith(".sh"):
                    record(path, Invocation(source, path, runner="shell", command=command))
                    if path.startswith("scripts/"):
                        reached.add(path)
                continue
            if python and args:
                target = args[0]
                if _dynamic(target):
                    unsupported(source, command, "dynamic python target")
                    continue
                path, escaped = _repo_path(target, source)
                if not escaped and path and path.endswith(".py") and path in calls:
                    record(path, Invocation(source, path, runner="script", command=command))
                continue
            if executable in {"xargs", "find", "docker"} and re.search(r"test_|unittest|pytest|-exec", command):
                unsupported(source, command, "unsupported dynamic test invocation")
    return calls, pins, reached, diagnostics
def scan_registry(repo_root: Path, config: RegistryConfig | None = None) -> RegistryResult:
    config = config or RegistryConfig()
    tracked, error = _tracked_files(repo_root)
    diagnostics = [error] if error else []
    classified = [(path, value) for path in tracked if (value := _family(path, config))]
    paths = [path for path, _ in classified]
    counts = Counter(family for _, (family, _) in classified)
    parsed_inputs: dict[str, ast.Module | str | None] = {}
    for path, _ in classified:
        text, read_error = _read_source(repo_root, path)
        if read_error:
            diagnostics.append(read_error)
            parsed_inputs[path] = None
        elif path.endswith(".py"):
            tree, parse_error = _parse_python(text or "", path)
            diagnostics.extend([parse_error] if parse_error else [])
            parsed_inputs[path] = tree
        else:
            _, malformed = _source_commands(text or "", path)
            diagnostics.extend([malformed] if malformed else [])
            parsed_inputs[path] = None if malformed else text
    sources = list(config.surface_sources or DEFAULT_SOURCES)
    if config.surface_sources is None:
        workflows = sorted(path for path in tracked
                           if path.startswith(".github/workflows/") and path.endswith((".yml", ".yaml")))
        if not workflows:
            diagnostics.append(Diagnostic(DiagnosticKind.SOURCE_MISSING,
                                          "no tracked .github/workflows/*.yml sources", fatal=True))
        sources.extend(workflows)
    reachable, queue, seen = [], deque(sources), set()
    all_calls, all_pins = {path: [] for path in paths}, {path: [] for path in paths}
    while queue:
        source = queue.popleft()
        if source in seen:
            continue
        seen.add(source)
        text, read_error = _read_source(repo_root, source)
        if read_error:
            diagnostics.append(read_error)
            continue
        reachable.append(source)
        commands, malformed = _source_commands(text or "", source)
        diagnostics.extend([malformed] if malformed else [])
        if malformed:
            continue
        found, pins, reached, unsupported = _collect_invocations(
            ((source, command) for command in commands), paths)
        for path in paths:
            all_calls[path].extend(found[path])
            all_pins[path].extend(pins[path])
        diagnostics.extend(unsupported)
        queue.extend(path for path in sorted(reached) if path in tracked and path not in seen)
    records = []
    for path, (family, kind) in classified:
        calls, pin_evidence = _unique_invocations(all_calls[path]), _unique_pins(all_pins[path])
        direct, globs = tuple(call for call in calls if not call.glob), tuple(call for call in calls if call.glob)
        tests, executed, mismatch = (), set(), False
        data = parsed_inputs[path]
        if kind is CandidateKind.PYTHON_SUITE and isinstance(data, ast.Module):
            parsed, module = _python_tests(data), path[:-3].replace("/", ".")
            tests = parsed.all
            for call in direct:
                if call.runner == "pytest":
                    selector = (call.selector or "").replace("::", ".")
                    executed.update(test for test in parsed.all
                                    if not selector or test == selector or test.startswith(selector + "."))
                elif call.target == path:
                    executed.update(parsed.unittest if parsed.has_unittest_main else ())
                    mismatch |= not parsed.has_unittest_main or not parsed.unittest
                elif call.target == module:
                    selected = (test for test in parsed.unittest
                                if not call.selector or test == call.selector
                                or test.startswith(call.selector + "."))
                    before = len(executed)
                    executed.update(selected)
                    mismatch |= len(executed) == before and not parsed.unittest
            if globs:
                executed.update(parsed.all)
            if mismatch:
                diagnostics.append(Diagnostic(DiagnosticKind.HARNESS_MISMATCH,
                                              f"execution form for {path} selects zero tests", path))
        valid = data is not None
        if not valid:
            execution = ExecStatus.NONE
        elif kind is not CandidateKind.PYTHON_SUITE and direct:
            execution = ExecStatus.FULL
        elif direct and tests and executed == set(tests) and not any(call.selector for call in direct):
            execution = ExecStatus.FULL
        elif direct and executed:
            execution = ExecStatus.PARTIAL
        elif globs:
            execution = ExecStatus.GLOB
        else:
            execution = ExecStatus.NONE
        statuses = {item.status for item in pin_evidence}
        pin = next((status for status in (PinStatus.PINNED, PinStatus.REQUIRED_ARRAY,
                                          PinStatus.GLOB_UNPINNED) if status in statuses), PinStatus.NONE)
        records.append(Candidate(path, family, kind, execution, pin, tests,
                                 tuple(sorted(executed)), calls, pin_evidence))
    diagnostics = list(dict.fromkeys(diagnostics))
    return RegistryResult(tuple(records), tuple(diagnostics),
                          {family: counts.get(family, 0) for family in Family}, tuple(reachable))
