#!/usr/bin/env python3
"""Deterministic tracked test-suite facts; policy belongs to #5003 S3a-2."""
from __future__ import annotations
import ast, json, posixpath, re, shlex, subprocess, sys
from collections import Counter, deque
from dataclasses import dataclass, field
from enum import Enum
from functools import lru_cache
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
    source: str; command: str; target: str; selector: str | None = None
    runner: str = "literal"; glob: bool = False
@dataclass(frozen=True)
class PinEvidence:
    source: str; command: str; target: str; status: PinStatus
@dataclass(frozen=True)
class Candidate:
    path: str; family: Family; kind: CandidateKind; execution: ExecStatus; pin: PinStatus
    tests: tuple[str, ...] = (); executed_tests: tuple[str, ...] = ()
    invocations: tuple[Invocation, ...] = (); pin_evidence: tuple[PinEvidence, ...] = ()
    @property
    def unexecuted_tests(self) -> tuple[str, ...]:
        return tuple(item for item in self.tests if item not in self.executed_tests)
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
    reachability_contract: str = "declared roots plus tracked literal shell reachability"
    @property
    def input_valid(self) -> bool:
        return not any(item.fatal for item in self.diagnostics)
    def _counts(self, name: str, values: Iterable[Enum]) -> Mapping[Enum, int]:
        found = Counter(getattr(item, name) for item in self.candidates if item.kind is not CandidateKind.GATE)
        return {value: found.get(value, 0) for value in values}
    @property
    def suite_status_counts(self) -> Mapping[ExecStatus, int]:
        return self._counts("execution", ExecStatus)
    @property
    def suite_pin_counts(self) -> Mapping[PinStatus, int]:
        return self._counts("pin", PinStatus)
    def family_violations(self, floors: Mapping[Family, int]) -> tuple[FamilyViolation, ...]:
        return tuple(FamilyViolation(family, floor, self.family_counts.get(family, 0))
                     for family, floor in sorted(floors.items(), key=lambda row: row[0].value)
                     if self.family_counts.get(family, 0) < floor)
    def partial_violations(self, allowed_exact_targets: Collection[tuple[str, str]] = ()) -> tuple[PartialViolation, ...]:
        allowed, result = set(allowed_exact_targets), []
        for item in self.candidates:
            if item.execution is not ExecStatus.PARTIAL:
                continue
            targets = tuple(sorted(call.target for call in item.invocations if not call.glob))
            if not targets or any((item.path, target) not in allowed for target in targets):
                result.append(PartialViolation(item.path, targets, item.unexecuted_tests))
        return tuple(result)
@dataclass(frozen=True)
class _PythonFacts:
    tests: tuple[str, ...]; unittest_tests: tuple[str, ...]; live_main: bool
@lru_cache(maxsize=None)
def _compile_glob(pattern: str) -> re.Pattern[str]:
    """Anchor the declared glob grammar; only an explicit ** crosses slash."""
    out, index = [], 0
    while index < len(pattern):
        if pattern.startswith("**/", index):
            out.append(r"(?:[^/]+/)*"); index += 3
        elif pattern.startswith("**", index):
            out.append(r".*"); index += 2
        elif pattern[index] == "*":
            out.append(r"[^/]*"); index += 1
        elif pattern[index] == "?":
            out.append(r"[^/]"); index += 1
        else:
            out.append(re.escape(pattern[index])); index += 1
    return re.compile("".join(out) + r"\Z")
def _matches(path: str, patterns: Sequence[str]) -> bool:
    return any(_compile_glob(pattern).fullmatch(path) for pattern in patterns)
def _family(path: str, config: RegistryConfig) -> tuple[Family, CandidateKind] | None:
    if re.match(r"scripts/e2e[^/]*/", path) and not path.startswith("scripts/e2e/"):
        return None
    ordered = ((Family.TESTS_PY, CandidateKind.PYTHON_SUITE), (Family.E2E_PY, CandidateKind.PYTHON_SUITE),
               (Family.SCRIPTS_PY, CandidateKind.PYTHON_SUITE),
               (Family.SHELL, CandidateKind.SHELL_SUITE),
               (Family.GATE, CandidateKind.GATE))
    return next(((family, kind) for family, kind in ordered
                 if _matches(path, config.patterns.get(family, ()))), None)
def _tracked(root: Path) -> tuple[tuple[str, ...], Diagnostic | None]:
    try:
        run = subprocess.run(("git", "ls-files", "-z"), cwd=root, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    except OSError as error:
        return (), Diagnostic(DiagnosticKind.TRACKED_INPUT, str(error), fatal=True)
    if run.returncode:
        return (), Diagnostic(DiagnosticKind.TRACKED_INPUT, run.stderr.decode("utf-8", "replace").strip(), fatal=True)
    try:
        paths = (part.decode("utf-8") for part in run.stdout.split(b"\0") if part)
        return tuple(sorted(paths, key=lambda value: value.encode("utf-8"))), None
    except UnicodeDecodeError as error:
        return (), Diagnostic(DiagnosticKind.TRACKED_INPUT, str(error), fatal=True)
def _read(root: Path, relative: str) -> tuple[str | None, Diagnostic | None]:
    path = root / relative
    if not path.exists():
        return None, Diagnostic(DiagnosticKind.SOURCE_MISSING, f"required input missing: {relative}", relative, True)
    try:
        data = path.read_bytes()
        text = data.decode("utf-8")
    except (OSError, UnicodeError) as error:
        return None, Diagnostic(DiagnosticKind.SOURCE_UNREADABLE, f"cannot read {relative}: {error}", relative, True)
    if "\0" in text:
        return None, Diagnostic(DiagnosticKind.SOURCE_MALFORMED, f"NUL byte in {relative}", relative, True)
    return text, None
def _python_tree(text: str, source: str) -> tuple[ast.Module | None, Diagnostic | None]:
    try:
        return ast.parse(text, filename=source), None
    except SyntaxError as error:
        return None, Diagnostic(DiagnosticKind.PYTHON_MALFORMED, f"cannot parse {source}: {error}", source, True)
def _shell_comment(raw: str) -> tuple[str, bool]:
    single = double = escaped = False
    for index, char in enumerate(raw):
        if escaped:
            escaped = False
        elif char == "\\" and not single:
            escaped = True
        elif char == "'" and not double:
            single = not single
        elif char == '"' and not single:
            double = not double
        elif char == "#" and not single and not double:
            return raw[:index], single
    return raw, single
def _logical_lines(text: str, source: str) -> tuple[list[str], Diagnostic | None]:
    lines, pending = [], ""
    for raw in text.splitlines():
        line, in_single = _shell_comment(raw)
        line = line.rstrip()
        slashes = len(line) - len(line.rstrip("\\"))
        if slashes % 2 == 1 and not in_single:
            pending += line[:-1].rstrip() + " "
        else:
            lines.append(pending + line.lstrip() if pending else line)
            pending = ""
    if pending:
        return lines, Diagnostic(DiagnosticKind.SOURCE_MALFORMED, f"unterminated backslash continuation in {source}", source, True)
    return lines, None
def _segments(command: str) -> list[list[str]]:
    lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|")
    lexer.whitespace_split, lexer.commenters = True, ""
    rows, current = [], []
    for token in lexer:
        if token and set(token) <= set(";&|"):
            if current: rows.append(current)
            current = []
        else:
            current.append(token)
    if current: rows.append(current)
    return rows
def _shell_commands(text: str, source: str) -> tuple[list[str], Diagnostic | None]:
    # Heredoc bodies are another language, not command surfaces.
    outer, heredoc = [], None
    for raw in text.splitlines():
        if heredoc:
            if raw.strip() == heredoc:
                heredoc = None
            continue
        outer.append(raw)
        marker = re.search(r"(?<!<)<<-?\s*['\"]?([A-Za-z_]\w*)", _shell_comment(raw)[0])
        heredoc = marker.group(1) if marker else None
    if heredoc:
        return [], Diagnostic(DiagnosticKind.SOURCE_MALFORMED, f"malformed shell in {source}: unterminated heredoc {heredoc}", source, True)
    physical, error = _logical_lines("\n".join(outer), source)
    if error:
        return [], error
    commands = []
    for line in physical:
        command = _shell_comment(line)[0].strip()
        if not command:
            continue
        if '"$(' in command and "<<" in command:
            command = command.split("$(", 1)[1].strip()
        if command in {'")"', ")'"}:
            continue
        try:
            _segments(command)
        except ValueError as problem:
            # Arbitrary compound shell is outside this grammar. A malformed
            # recognized runner still fails closed; unrelated syntax is inert.
            runner = re.match(r"^(?:(?:if|then|do|!)\s+)?(?:[A-Za-z_]\w*=\S+\s+)*(?:(?:bash|sh|python[\d.]*)(?!\s+-c\b)|source|pytest|node|\./\S+\.sh)\b", command)
            if runner:
                return commands, Diagnostic(DiagnosticKind.SOURCE_MALFORMED, f"malformed supported shell call in {source}: {problem}", source, True)
            continue
        commands.append(command)
    return commands, None
def _workflow_payloads(text: str, source: str) -> tuple[list[str], Diagnostic | None]:
    lines, payloads, index = text.splitlines(), [], 0
    while index < len(lines):
        match = re.match(r"^(\s*)(?:-\s+)?([A-Za-z_][\w-]*):\s*(.*?)\s*$", lines[index])
        if not match:
            index += 1
            continue
        lead, key, value = match.groups()
        key_column = len(lead) + (2 if lines[index][len(lead):].startswith("- ") else 0)
        if re.fullmatch(r"[|>][+-]?", value):
            style, block = value, []
            index += 1
            while index < len(lines):
                raw = lines[index]; width = len(raw) - len(raw.lstrip())
                if raw.strip() and width <= key_column:
                    break
                block.append(raw); index += 1
            margins = [len(row) - len(row.lstrip()) for row in block if row.strip()]
            margin = min(margins) if margins else key_column + 1
            body = [row[margin:] if row.strip() else "" for row in block]
            if key == "run":
                payload = ("\n" if style[0] == "|" else " ").join(body)
                if style.endswith("-"):
                    payload = payload.rstrip("\n")
                elif not style.endswith("+"):
                    payload = payload.rstrip("\n") + "\n"
                payloads.append(payload)
            continue
        if key == "run" and value and not value.startswith("#"):
            if value.startswith("'"):
                if len(value) < 2 or not value.endswith("'"):
                    return payloads, Diagnostic(DiagnosticKind.SOURCE_MALFORMED, f"unterminated quoted run scalar in {source}", source, True)
                value = value[1:-1].replace("''", "'")
            elif value.startswith('"'):
                try:
                    value = json.loads(value)
                except json.JSONDecodeError as problem:
                    return payloads, Diagnostic(DiagnosticKind.SOURCE_MALFORMED, f"malformed quoted run scalar in {source}: {problem}", source, True)
            payloads.append(value)
        index += 1
    return payloads, None
def _source_commands(text: str, source: str) -> tuple[list[str], Diagnostic | None]:
    if source == "package.json":
        try:
            document = json.loads(text)
        except json.JSONDecodeError as error:
            return [], Diagnostic(DiagnosticKind.SOURCE_MALFORMED, f"malformed package.json: {error}", source, True)
        if not isinstance(document, dict):
            return [], Diagnostic(DiagnosticKind.SOURCE_MALFORMED, "package.json must be an object", source, True)
        scripts = document.get("scripts")
        if scripts is not None and not isinstance(scripts, dict):
            return [], Diagnostic(DiagnosticKind.SOURCE_MALFORMED, "package.json scripts must be an object", source, True)
        if scripts and any(not isinstance(value, str) for value in scripts.values()):
            return [], Diagnostic(DiagnosticKind.SOURCE_MALFORMED, "package.json script values must be strings", source, True)
        payloads = [value for value in (scripts or {}).values() if isinstance(value, str)]
    elif source.startswith(".github/workflows/") and source.endswith((".yml", ".yaml")):
        payloads, error = _workflow_payloads(text, source)
        if error:
            return [], error
    else:
        payloads = [text]
    result = []
    for payload in payloads:
        commands, error = _shell_commands(payload, source)
        result.extend(commands)
        if error:
            return result, error
    return result, None
def _call_head(tokens: Sequence[str]) -> tuple[str, list[str]]:
    words = list(tokens)
    while words and words[0] in {"do", "then", "else", "elif", "if", "while", "until", "!"}:
        words.pop(0)
    while words and re.fullmatch(r"[A-Za-z_]\w*=.*", words[0]):
        words.pop(0)
    return (words[0].lstrip("@+-"), words[1:]) if words else ("", [])
def _shell_target(tokens: Sequence[str]) -> str | None:
    executable, args = _call_head(tokens)
    if executable in {"bash", "sh", "source", "."}:
        return next((arg for arg in args if not arg.startswith("-")), None)
    if executable.endswith(".sh") and ("/" in executable or executable.startswith(".")):
        return executable
    return None
def _repo_path(value: str, source: str) -> tuple[str | None, bool]:
    if value == "$0" or value == "${0}":
        return source, False
    base = ""
    for prefix in ("$SCRIPT_DIR/", "${SCRIPT_DIR}/"):
        if value.startswith(prefix):
            base, value = str(PurePosixPath(source).parent), value[len(prefix):]
            break
    if "$" in value or "{{" in value or "}}" in value:
        return None, False
    normalized = posixpath.normpath(posixpath.join(base, value)).removeprefix("./")
    escaped = normalized.startswith("/") or normalized == ".." or normalized.startswith("../")
    return (None, True) if escaped else (normalized, False)
def _main_guard(test: ast.AST) -> bool:
    if not isinstance(test, ast.Compare) or len(test.ops) != 1 or not isinstance(test.ops[0], ast.Eq):
        return False
    left, right = test.left, test.comparators[0]
    return ((isinstance(left, ast.Name) and left.id == "__name__" and isinstance(right, ast.Constant) and right.value == "__main__")
            or (isinstance(right, ast.Name) and right.id == "__name__" and isinstance(left, ast.Constant) and left.value == "__main__"))
def _contains_main(node: ast.AST) -> bool:
    if isinstance(node, ast.If):
        return _main_guard(node.test) and any(_contains_main(child) for child in node.body)
    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef, ast.Lambda,
                         ast.For, ast.AsyncFor, ast.While, ast.Try, ast.With, ast.AsyncWith)) or type(node).__name__ == "Match":
        return False
    if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
        owner = node.func.value
        if isinstance(owner, ast.Name) and owner.id == "unittest" and node.func.attr == "main":
            return True
    return any(_contains_main(child) for child in ast.iter_child_nodes(node))
def _python_facts(tree: ast.Module) -> _PythonFacts:
    tests, methods, bases = [], {}, {}
    for node in tree.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith("test_"):
            tests.append(node.name)
        elif isinstance(node, ast.ClassDef):
            bases[node.name] = tuple(ast.unparse(base) for base in node.bases)
            methods[node.name] = tuple(f"{node.name}.{child.name}" for child in node.body
                                       if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef))
                                       and child.name.startswith("test_"))
            tests.extend(methods[node.name])
    unittest_classes = {name for name, parents in bases.items()
                        if any(parent == "TestCase" or parent.endswith(".TestCase") for parent in parents)}
    while True:
        expanded = unittest_classes | {name for name, parents in bases.items()
                                       if any(parent in unittest_classes for parent in parents)}
        if expanded == unittest_classes:
            break
        unittest_classes = expanded
    unittest_tests = tuple(sorted(test for name in unittest_classes for test in methods.get(name, ())))
    return _PythonFacts(tuple(sorted(tests)), unittest_tests, any(_contains_main(node) for node in tree.body))
def _dedupe_calls(items: Iterable[Invocation]) -> tuple[Invocation, ...]:
    key = lambda item: (item.source, item.command, item.target, item.selector or "", item.runner, item.glob)
    return tuple(sorted({key(item): item for item in items}.values(), key=key))
def _dedupe_pins(items: Iterable[PinEvidence]) -> tuple[PinEvidence, ...]:
    key = lambda item: (item.source, item.command, item.target, item.status.value)
    return tuple(sorted({key(item): item for item in items}.values(), key=key))
def _analyze(rows: Sequence[tuple[str, str]], paths: Sequence[str]) -> tuple[dict[str, list[Invocation]], dict[str, list[PinEvidence]], list[Diagnostic]]:
    calls = {path: [] for path in paths}; pins = {path: [] for path in paths}; diagnostics = []
    modules = {path[:-3].replace("/", "."): path for path in paths if path.endswith(".py")}
    bindings: dict[tuple[str, str], list[str]] = {}
    for source, command in rows:
        for segment in _segments(command):
            executable, args = _call_head(segment)
            if executable == "for" and "in" in args:
                split = args.index("in")
                if split == 1:
                    bindings.setdefault((source, args[0]), []).extend(arg for arg in args[2:] if arg != "do")
    for source in sorted({row[0] for row in rows}):
        joined = "\n".join(command for owner, command in rows if owner == source)
        for match in re.finditer(r"^required_shell_suites\s*=\((.*?)\)", joined, re.M | re.S):
            for target in re.findall(r"tests/[^\s)'\"]+\.sh", match.group(1)):
                if target in pins:
                    pins[target].append(PinEvidence(source, match.group(0), target, PinStatus.REQUIRED_ARRAY))
    def fatal(source: str, command: str, detail: str) -> None:
        diagnostics.append(Diagnostic(DiagnosticKind.UNSUPPORTED_CALL, f"{detail}: {command}", source, True))
    def record(path: str, call: Invocation, status: PinStatus) -> None:
        if path in calls:
            calls[path].append(call); pins[path].append(PinEvidence(call.source, call.command, call.target, status))
    for source, command in rows:
        for segment in _segments(command):
            executable, args = _call_head(segment)
            python = bool(re.fullmatch(r"(?:python\d*(?:\.\d+)?|\$\{?PYTHON\}?)", executable))
            runner, targets = "", []
            if executable == "pytest":
                runner, targets = "pytest", args
            elif python and args[:2] == ["-m", "unittest"]:
                runner, targets = "unittest", args[2:]
            elif python and args[:2] == ["-m", "pytest"]:
                runner, targets = "pytest", args[2:]
            elif executable == "node" and args[:1] == ["--test"]:
                runner, targets = "node", args[1:]
            if runner:
                for target in (item for item in targets if not item.startswith("-")):
                    if "$" in target or "{{" in target or "}}" in target:
                        fatal(source, command, f"dynamic {runner} target"); continue
                    if runner == "unittest":
                        matching = [module for module in modules if target == module or target.startswith(module + ".")]
                        if matching:
                            module = max(matching, key=len); selector = target[len(module):].removeprefix(".") or None
                            record(modules[module], Invocation(source, command, target, selector, runner), PinStatus.PINNED)
                    elif runner == "pytest":
                        raw, _, selected = target.partition("::")
                        wildcard = any(mark in raw for mark in "*?")
                        for path in paths:
                            matched = path.endswith(".py") and (path == raw or (wildcard and _matches(path, (raw,)))
                                                                          or (raw.endswith("/") and path.startswith(raw)))
                            if matched:
                                glob = wildcard or path != raw
                                record(path, Invocation(source, command, target, selected.replace("::", ".") or None,
                                                        runner, glob), PinStatus.GLOB_UNPINNED if glob else PinStatus.PINNED)
                continue
            if python and args and args[0] not in {"-", "-c"} and not args[0].startswith("-"):
                target, escaped = _repo_path(args[0], source)
                if escaped:
                    fatal(source, command, "python target escapes repository")
                elif target is None:
                    fatal(source, command, "dynamic python target")
                elif target in calls:
                    record(target, Invocation(source, command, target, runner="python-path"), PinStatus.PINNED)
                continue
            shell = _shell_target(segment)
            if shell is None:
                if executable in {"find", "xargs", "docker"} and any("python" in item or "bash" in item for item in args):
                    fatal(source, command, f"unsupported {executable} runner construction")
                continue
            variable = re.fullmatch(r"\$\{?([A-Za-z_]\w*)\}?", shell)
            patterns = bindings.get((source, variable.group(1)), ()) if variable else ()
            if patterns:
                for pattern in patterns:
                    if "$" in pattern or "{{" in pattern or "}}" in pattern:
                        fatal(source, command, "dynamic loop pattern"); continue
                    for path in paths:
                        if path.endswith(".sh") and _matches(path, (pattern,)):
                            record(path, Invocation(source, command, pattern, runner="shell-loop", glob=True),
                                   PinStatus.GLOB_UNPINNED)
                continue
            target, escaped = _repo_path(shell, source)
            if escaped:
                fatal(source, command, "shell target escapes repository")
            elif target is None:
                fatal(source, command, "dynamic shell target")
            elif target in calls:
                record(target, Invocation(source, command, target, runner="shell"), PinStatus.PINNED)
    return calls, pins, diagnostics
def scan_registry(root: Path, config: RegistryConfig | None = None) -> RegistryResult:
    root, config = root.resolve(), config or RegistryConfig()
    tracked, tracked_error = _tracked(root)
    diagnostics = [tracked_error] if tracked_error else []
    selected = [(path, *_family(path, config)) for path in tracked if _family(path, config)]
    valid, facts = {}, {}
    for path, _family_name, kind in selected:
        text, error = _read(root, path)
        if error:
            diagnostics.append(error); valid[path] = False; continue
        if path.endswith(".py"):
            tree, error = _python_tree(text, path)
            if error:
                diagnostics.append(error); valid[path] = False; continue
            facts[path] = _python_facts(tree)
        elif path.endswith(".sh"):
            _, error = _shell_commands(text, path)
            if error:
                diagnostics.append(error); valid[path] = False; continue
        valid[path] = True
    if config.surface_sources is None:
        workflows = tuple(path for path in tracked if re.fullmatch(r"\.github/workflows/[^/]+\.ya?ml", path))
        roots = DEFAULT_SOURCES + workflows
    else:
        roots = config.surface_sources
    queue, seen, rows = deque(roots), set(), []
    while queue:
        source = queue.popleft()
        if source in seen:
            continue
        seen.add(source)
        text, error = _read(root, source)
        if error:
            diagnostics.append(error); continue
        commands, error = _source_commands(text, source)
        rows.extend((source, command) for command in commands)
        if error:
            diagnostics.append(error); continue
        for command in commands:
            for segment in _segments(command):
                target = _shell_target(segment)
                if target is None:
                    continue
                path, escaped = _repo_path(target, source)
                if not escaped and path in tracked and path.endswith(".sh"):
                    queue.append(path)
    paths = [item[0] for item in selected]
    calls, pins, analysis_diagnostics = _analyze(rows, paths)
    diagnostics.extend(analysis_diagnostics)
    candidates = []
    for path, family, kind in selected:
        item_calls, item_pins = _dedupe_calls(calls[path]), _dedupe_pins(pins[path])
        test_facts = facts.get(path, _PythonFacts((), (), False))
        executed = set()
        if valid.get(path):
            for call in item_calls:
                if call.glob or kind is not CandidateKind.PYTHON_SUITE:
                    continue
                if call.runner == "pytest":
                    selected_tests = test_facts.tests
                elif call.runner == "unittest":
                    selected_tests = test_facts.unittest_tests
                elif call.runner == "python-path" and test_facts.live_main:
                    selected_tests = test_facts.unittest_tests
                else:
                    selected_tests = ()
                if call.selector:
                    selected_tests = tuple(test for test in selected_tests
                                           if test == call.selector or test.startswith(call.selector + "."))
                executed.update(selected_tests)
        exact = any(not call.glob for call in item_calls) and valid.get(path, False)
        glob = any(call.glob for call in item_calls) and valid.get(path, False)
        if kind is CandidateKind.PYTHON_SUITE:
            if test_facts.tests and executed == set(test_facts.tests):
                execution = ExecStatus.FULL
            elif executed:
                execution = ExecStatus.PARTIAL
            elif glob:
                execution = ExecStatus.GLOB
            else:
                execution = ExecStatus.NONE
            if exact and test_facts.tests and not executed:
                diagnostics.append(Diagnostic(DiagnosticKind.HARNESS_MISMATCH,
                                              f"supported command collects no static tests in {path}", path))
        else:
            execution = ExecStatus.FULL if exact else ExecStatus.GLOB if glob else ExecStatus.NONE
        statuses = {evidence.status for evidence in item_pins}
        pin = (PinStatus.PINNED if PinStatus.PINNED in statuses else
               PinStatus.REQUIRED_ARRAY if PinStatus.REQUIRED_ARRAY in statuses else
               PinStatus.GLOB_UNPINNED if PinStatus.GLOB_UNPINNED in statuses else PinStatus.NONE)
        candidates.append(Candidate(path, family, kind, execution, pin, test_facts.tests,
                                    tuple(sorted(executed)), item_calls, item_pins))
    counts = Counter(item[1] for item in selected)
    family_counts = {family: counts.get(family, 0) for family in Family}
    diagnostic_key = lambda item: (item.kind.value, item.source or "", item.message, item.fatal)
    diagnostics = list({diagnostic_key(item): item for item in diagnostics}.values())
    return RegistryResult(tuple(candidates), tuple(sorted(diagnostics, key=diagnostic_key)), family_counts,
                          tuple(sorted(seen, key=lambda value: value.encode("utf-8"))))
def main() -> int:
    result = scan_registry(Path(__file__).resolve().parents[1])
    for diagnostic in result.diagnostics:
        if diagnostic.fatal:
            print(f"{diagnostic.kind.value}: {diagnostic.message}", file=sys.stderr)
    return 0 if result.input_valid else 1
if __name__ == "__main__":
    raise SystemExit(main())
