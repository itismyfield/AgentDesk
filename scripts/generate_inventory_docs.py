#!/usr/bin/env python3

from __future__ import annotations

import argparse
import difflib
import re
import sys
from collections import Counter
from dataclasses import dataclass
from typing import Callable, Iterable
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SRC_ROOT = REPO_ROOT / "src"
GENERATED_DOCS_DIR = REPO_ROOT / "docs" / "generated"
ARCHITECTURE_DOC = REPO_ROOT / "ARCHITECTURE.md"
ARCHITECTURE_SRC_TREE_START = "<!-- BEGIN GENERATED: SRC TREE -->"
ARCHITECTURE_SRC_TREE_END = "<!-- END GENERATED: SRC TREE -->"
ARCHITECTURE_TOP_LEVEL_MAP_START = "<!-- BEGIN GENERATED: TOP LEVEL MODULE MAP -->"
ARCHITECTURE_TOP_LEVEL_MAP_END = "<!-- END GENERATED: TOP LEVEL MODULE MAP -->"
GIANT_FILE_THRESHOLD = 1000
HTTP_METHODS = ("delete", "get", "head", "options", "patch", "post", "put")
TEST_FILE_NAMES = {"integration_tests.rs", "tests.rs"}
GIANT_FILE_REGISTRY = REPO_ROOT / "scripts" / "giant_file_registry.toml"
GIANT_FILE_REGISTRY_DOC = GENERATED_DOCS_DIR / "giant-file-registry.md"

# Only whole test *modules* count as test LoC — inline `#[cfg(test)]` guards on
# production struct fields, conditional logic, or test-only helper fns left in
# the module body remain production code because they shape the compiled-out
# test seams of the production surface.
#
# A module's `#[cfg(...)]` attribute gates it to test-only builds when the
# predicate *requires* the `test` flag. `cfg_requires_test` evaluates this
# structurally (with balanced-paren awareness) so it handles nested forms that
# combine `test`, `all(...)`, `any(...)`, `not(...)`, and feature checks.
# Predicates that do not
# remove the module from production builds — `not(test)` (production-only) and
# `any(test, …)` (compiles when the other option is set) — are rejected.

# `#[cfg(...)]` attribute immediately before a (optionally attributed, optionally
# `pub`) `mod <name> {` declaration. The cfg body is captured for structural
# evaluation; the `mod` open brace anchors the test-module body.
# The predicate stays inside a single attribute (`[^]]`), and only further
# attributes or whitespace may sit between the cfg and the `mod` keyword — so an
# inline `#[cfg(test)]` guarding a function does not spuriously bind to a later
# `mod`.
_CFG_MOD_RE = re.compile(
    r"#\[cfg\((?P<predicate>[^]]*?)\)\]"
    r"\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?"
    r"mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{",
)


def _split_top_level_cfg_args(body: str) -> list[str]:
    args: list[str] = []
    depth = 0
    start = 0
    in_string = False
    for index, ch in enumerate(body):
        if ch == '"':
            in_string = not in_string
            continue
        if in_string:
            continue
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        elif ch == "," and depth == 0:
            args.append(body[start:index])
            start = index + 1
    args.append(body[start:])
    return [arg.strip() for arg in args if arg.strip()]


def cfg_requires_test(predicate: str) -> bool:
    """Return True when a `#[cfg(<predicate>)]` compiles only under `test`.

    * ``test`` -> True.
    * ``all(a, b, …)`` -> True when any argument requires test (conjunction).
    * ``any(a, b, …)`` -> True only when *every* argument requires test (the
      module would otherwise also build without test).
    * ``not(…)`` -> never test-gating for our purposes (``not(test)`` is
      production-only; ``not(other)`` does not require test).
    * anything else (e.g. ``feature = "x"``) -> False.
    """

    predicate = predicate.strip()
    if predicate == "test":
        return True
    for keyword, combine in (("all", any), ("any", all)):
        prefix = keyword + "("
        if predicate.startswith(prefix) and predicate.endswith(")"):
            inner = predicate[len(prefix):-1]
            args = _split_top_level_cfg_args(inner)
            if not args:
                return False
            return combine(cfg_requires_test(arg) for arg in args)
    if predicate.startswith("not(") and predicate.endswith(")"):
        return False
    return False

TOP_LEVEL_MODULE_PURPOSES = {
    "api_caller_observability.rs": "Request-principal classification and uniform log-only API caller attribution records for identity-consuming mutation paths.",
    "bootstrap.rs": "Builds config, database, policy engine, and shared app state before launch.",
    "cli/": "Operator-facing CLI commands, direct API shims, migrations, and Discord send helpers.",
    "compat/": "Centralised home for compatibility/legacy/fallback shims (#1076). Each public item carries a `REMOVE_WHEN` comment so retirement is grep-driven.",
    "app_state.rs": "Shared HTTP route-handler state (`AppState`); lives at crate root below server+services so service-layer handlers reference it without a service→server backflow.",
    "config.rs": "`agentdesk.yaml` parsing, configuration defaults, and shared test env helpers.",
    "config_live_reload.rs": "Hot-reloads `agentdesk.yaml` without a restart: a debounced `notify` watcher pre-validates edits and atomically swaps a process-global config snapshot, keeping the running config on failure and reporting restart-required infra changes.",
    "credential.rs": "Reads runtime credential files such as Discord bot tokens from the AgentDesk root.",
    "db/": "PostgreSQL access layer, migration helpers, and schema authority.",
    "dispatch/": "Dispatch context construction, review metadata, and worktree targeting.",
    "engine/": "QuickJS policy runtime, hook wiring, transition logic, and Rust-JS bridge ops.",
    "error.rs": "Shared HTTP and policy error type with typed codes and JSON response helpers.",
    "eventbus.rs": "In-process broadcast event bus (history/replay/batching) shared by the WS server layer and background services without a service→server backflow.",
    "github/": "GitHub sync, issue triage, and Definition-of-Done mirroring.",
    "high_risk_recovery.rs": "PG-only high-risk recovery tests for boot reconciliation and review refire paths.",
    "kanban/": "High-level kanban orchestration, state machine facade, and shared test support.",
    "launch.rs": "Starts the Tokio runtime and hands off to server boot.",
    "lib.rs": "Library crate boundary that exposes the server/CLI modules for the slim binary entry point and tests.",
    "logging.rs": "Tracing span helpers that stamp dispatch, card, agent, and hook context onto logs.",
    "main.rs": "Binary entry point. Dispatches CLI commands or boots the server runtime.",
    "pipeline.rs": "Pipeline stage loading, resolution, and transition helpers.",
    "receipt.rs": "Receipt parsing and workspace attribution helpers.",
    "reconcile.rs": "Boot-time reconciliation for persisted state and dispatch-runtime drift.",
    "manual_intervention.rs": "Manual intervention parsing and helpers shared by Discord reply/requeue flows.",
    "runtime_layout/": "Managed runtime layout, memory-path migration, shared prompt sync, and skill deployment.",
    "server/": "Axum server boot, routes, workers, background loops, and WebSocket broadcast.",
    "services/": "Core runtime services: provider runners, Discord bot, queueing, memory, and platform helpers.",
    "supervisor/": "Runtime supervisor signals and recovery decisions for orphaned or stalled work.",
    "ui/": "Compatibility shims for persisted UI/session types used by the Discord runtime.",
    "utils/": "Shared formatting and Unicode-safe string utilities.",
    "voice/": "Voice command, STT/TTS, prompt, progress, metrics, receiver, and barge-in helpers.",
}

FN_RE = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(",
    re.MULTILINE,
)


@dataclass(frozen=True)
class ModuleEntry:
    module_path: str
    file_path: str
    line_count: int
    prod_line_count: int
    test_line_count: int
    flags: tuple[str, ...]


@dataclass(frozen=True)
class GiantFileRegistration:
    file_path: str
    owner: str
    deadline: str
    decompose_issue: str
    prod_line_count: int


@dataclass(frozen=True)
class RouteEntry:
    method: str
    path: str
    handler: str
    handler_source: str
    route_decl: str


@dataclass(frozen=True)
class WorkerEntry:
    worker: str
    kind: str
    target: str
    source: str
    notes: str


@dataclass(frozen=True)
class ModuleFileDeclaration:
    parent: Path
    name: str
    child_paths: tuple[Path, ...]
    requires_test: bool


class ParseError(RuntimeError):
    pass


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def line_count(text: str) -> int:
    return len(text.splitlines())


def test_line_count(text: str) -> int:
    """Count lines that live inside ``#[cfg(test)] mod`` blocks.

    Whole ``*_tests.rs`` files are already excluded from the production set by
    :func:`is_test_file`; this splits the remaining files so the giant-file
    signal tracks the *production* review surface rather than inline test
    fixtures (#3036).
    """

    total = line_count(text)
    test_lines: set[int] = set()
    for match in _CFG_MOD_RE.finditer(text):
        if not cfg_requires_test(match.group("predicate")):
            continue
        brace = text.rindex("{", match.start(), match.end())
        try:
            _body, end = scan_balanced(text, brace, "{", "}")
        except ParseError:
            continue
        start_line = offset_to_line(text, match.start())
        end_line = offset_to_line(text, end)
        for line in range(start_line, end_line + 1):
            if 1 <= line <= total:
                test_lines.add(line)
    return len(test_lines)


def split_prod_test_lines(text: str) -> tuple[int, int]:
    total = line_count(text)
    test = test_line_count(text)
    return total - test, test


def rel_posix(path: Path) -> str:
    try:
        return path.relative_to(REPO_ROOT).as_posix()
    except ValueError:
        return path.resolve().relative_to(REPO_ROOT.resolve()).as_posix()


def offset_to_line(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def is_test_file(path: Path) -> bool:
    return path.name.endswith("_tests.rs") or path.name in TEST_FILE_NAMES


def all_rust_files() -> list[Path]:
    if not SRC_ROOT.is_dir():
        return []
    return sorted(path for path in SRC_ROOT.rglob("*.rs") if path.is_file())


def production_rust_files() -> list[Path]:
    return sorted(
        path
        for path in SRC_ROOT.rglob("*.rs")
        if path.is_file() and not is_test_file(path)
    )


def is_inventory_ignored(path: Path) -> bool:
    return path.name == "__pycache__" or path.name.startswith(".")


def top_level_src_entries() -> list[Path]:
    return sorted(
        (path for path in SRC_ROOT.iterdir() if not is_inventory_ignored(path)),
        key=lambda child: (child.is_file(), child.name),
    )


def top_level_src_key(path: Path) -> str:
    return f"{path.name}/" if path.is_dir() else path.name


def module_path_for_file(path: Path) -> str:
    rel = path.relative_to(SRC_ROOT)
    if rel.name == "main.rs":
        return "crate"
    if rel.name == "mod.rs":
        parts = rel.parts[:-1]
    else:
        parts = rel.with_suffix("").parts
    if not parts:
        return "crate"
    return "::".join(parts)


def format_path_with_line(path: Path, line: int) -> str:
    return f"`{rel_posix(path)}:{line}`"


def strip_wrapping_whitespace(text: str) -> str:
    return " ".join(text.strip().split())


def split_raw_string_start(text: str, index: int) -> tuple[int, int] | None:
    if text[index] != "r":
        return None
    cursor = index + 1
    hashes = 0
    while cursor < len(text) and text[cursor] == "#":
        hashes += 1
        cursor += 1
    if cursor < len(text) and text[cursor] == '"':
        return cursor + 1, hashes
    return None


def char_literal_length(text: str, index: int) -> int | None:
    """Return the length of a Rust char literal starting at ``index``.

    ``text[index]`` is assumed to be ``'``. Returns the number of characters
    that make up a genuine char literal (e.g. ``'a'`` -> 3, ``'\\n'`` -> 4,
    ``'\\u{1F600}'`` -> the full escape) or ``None`` when the ``'`` actually
    begins a lifetime (``'a``, ``'static``, ``'_``) rather than a char literal.

    Distinguishing the two matters because a lifetime is *not* terminated by a
    closing quote; treating it as a char literal makes the scanner swallow
    everything up to the next stray ``'`` and corrupts brace balancing (#3028).
    """

    end = len(text)
    if index >= end or text[index] != "'":
        return None
    body = index + 1
    if body >= end:
        return None
    if text[body] == "\\":
        # Escaped char literal (``'\n'``, ``'\\'``, ``'\''``, ``'\x41'``,
        # ``'\u{1F600}'`` ...). First fully consume the escape sequence that
        # follows the backslash, *then* require the closing quote. Consuming
        # the escape is what makes ``'\''`` (escaped quote) and ``'\\'``
        # (escaped backslash) parse correctly: the first char after ``\`` is
        # part of the escape and must never be mistaken for the terminator.
        cursor = body + 1  # first char of the escape sequence
        if cursor >= end:
            return None
        esc = text[cursor]
        if esc == "\n":
            return None
        if esc == "x":
            # ``\xNN`` -- two hex digits.
            cursor += 1 + 2
        elif esc == "u":
            # ``\u{...}`` -- braced hex escape.
            cursor += 1
            if cursor >= end or text[cursor] != "{":
                return None
            close = text.find("}", cursor)
            if close == -1:
                return None
            cursor = close + 1
        else:
            # Simple single-char escape: ``\n \t \r \\ \' \" \0`` etc.
            cursor += 1
        # The closing quote must immediately follow the escape sequence.
        if cursor < end and text[cursor] == "'":
            return cursor - index + 1
        return None
    # Non-escaped: a char literal holds exactly one char then a closing quote.
    if body + 1 < end and text[body + 1] == "'":
        return 3
    # Otherwise this is a lifetime (``'a``, ``'static`` ...), not a char.
    return None


def scan_balanced(text: str, open_index: int, open_char: str = "(", close_char: str = ")") -> tuple[str, int]:
    if text[open_index] != open_char:
        raise ParseError(f"expected {open_char!r} at offset {open_index}")
    depth = 0
    index = open_index
    in_string = False
    escape = False
    raw_hashes: int | None = None
    line_comment = False
    block_comment_depth = 0
    while index < len(text):
        ch = text[index]

        if line_comment:
            if ch == "\n":
                line_comment = False
            index += 1
            continue

        if block_comment_depth:
            if text.startswith("/*", index):
                block_comment_depth += 1
                index += 2
                continue
            if text.startswith("*/", index):
                block_comment_depth -= 1
                index += 2
                continue
            index += 1
            continue

        if in_string:
            if raw_hashes is not None:
                if ch == '"' and text[index + 1 : index + 1 + raw_hashes] == "#" * raw_hashes:
                    closing_hashes = raw_hashes
                    in_string = False
                    raw_hashes = None
                    index += 1 + closing_hashes
                    continue
                index += 1
                continue
            if escape:
                escape = False
                index += 1
                continue
            if ch == "\\":
                escape = True
                index += 1
                continue
            if ch == '"':
                in_string = False
            index += 1
            continue

        raw_start = split_raw_string_start(text, index)
        if raw_start is not None:
            index, raw_hashes = raw_start
            in_string = True
            continue
        # Byte-string / raw-byte-string / byte-char literals: b"..", br#".."#, b'x'.
        if ch == "b" and index + 1 < len(text):
            byte_raw = split_raw_string_start(text, index + 1)
            if byte_raw is not None:
                index, raw_hashes = byte_raw
                in_string = True
                continue
            if text[index + 1] == '"':
                in_string = True
                index += 2
                continue
            if text[index + 1] == "'":
                byte_len = char_literal_length(text, index + 1)
                if byte_len is not None:
                    index += 1 + byte_len
                    continue
        if text.startswith("//", index):
            line_comment = True
            index += 2
            continue
        if text.startswith("/*", index):
            block_comment_depth = 1
            index += 2
            continue
        if ch == '"':
            in_string = True
            index += 1
            continue
        if ch == "'":
            char_len = char_literal_length(text, index)
            if char_len is not None:
                index += char_len
                continue
            # Lifetime (``'a``, ``'static`` ...): skip only the quote.
            index += 1
            continue
        if ch == open_char:
            depth += 1
        elif ch == close_char:
            depth -= 1
            if depth == 0:
                return text[open_index + 1 : index], index + 1
        index += 1
    raise ParseError(f"unterminated balanced segment starting at offset {open_index}")


def split_top_level(text: str, delimiter: str = ",", maxsplit: int = 1) -> list[str]:
    parts: list[str] = []
    last = 0
    depth_paren = 0
    depth_brace = 0
    depth_bracket = 0
    index = 0
    in_string = False
    escape = False
    raw_hashes: int | None = None
    line_comment = False
    block_comment_depth = 0

    while index < len(text):
        ch = text[index]

        if line_comment:
            if ch == "\n":
                line_comment = False
            index += 1
            continue

        if block_comment_depth:
            if text.startswith("/*", index):
                block_comment_depth += 1
                index += 2
                continue
            if text.startswith("*/", index):
                block_comment_depth -= 1
                index += 2
                continue
            index += 1
            continue

        if in_string:
            if raw_hashes is not None:
                if ch == '"' and text[index + 1 : index + 1 + raw_hashes] == "#" * raw_hashes:
                    closing_hashes = raw_hashes
                    in_string = False
                    raw_hashes = None
                    index += 1 + closing_hashes
                    continue
                index += 1
                continue
            if escape:
                escape = False
                index += 1
                continue
            if ch == "\\":
                escape = True
                index += 1
                continue
            if ch == '"':
                in_string = False
            index += 1
            continue

        raw_start = split_raw_string_start(text, index)
        if raw_start is not None:
            index, raw_hashes = raw_start
            in_string = True
            continue
        # Byte-string / raw-byte-string / byte-char literals: b"..", br#".."#, b'x'.
        if ch == "b" and index + 1 < len(text):
            byte_raw = split_raw_string_start(text, index + 1)
            if byte_raw is not None:
                index, raw_hashes = byte_raw
                in_string = True
                continue
            if text[index + 1] == '"':
                in_string = True
                index += 2
                continue
            if text[index + 1] == "'":
                byte_len = char_literal_length(text, index + 1)
                if byte_len is not None:
                    index += 1 + byte_len
                    continue
        if text.startswith("//", index):
            line_comment = True
            index += 2
            continue
        if text.startswith("/*", index):
            block_comment_depth = 1
            index += 2
            continue
        if ch == '"':
            in_string = True
            index += 1
            continue
        if ch == "'":
            char_len = char_literal_length(text, index)
            if char_len is not None:
                index += char_len
                continue
            # Lifetime (``'a``, ``'static`` ...): skip only the quote.
            index += 1
            continue
        if ch == "(":
            depth_paren += 1
        elif ch == ")":
            depth_paren -= 1
        elif ch == "{":
            depth_brace += 1
        elif ch == "}":
            depth_brace -= 1
        elif ch == "[":
            depth_bracket += 1
        elif ch == "]":
            depth_bracket -= 1
        elif (
            ch == delimiter
            and depth_paren == 0
            and depth_brace == 0
            and depth_bracket == 0
        ):
            parts.append(text[last:index])
            last = index + 1
            if len(parts) == maxsplit:
                parts.append(text[last:])
                return parts
        index += 1

    parts.append(text[last:])
    return parts


def build_function_index(paths: list[Path]) -> tuple[dict[Path, dict[str, int]], dict[str, list[tuple[Path, int]]]]:
    by_file: dict[Path, dict[str, int]] = {}
    by_name: dict[str, list[tuple[Path, int]]] = {}
    for path in paths:
        text = read_text(path)
        functions: dict[str, int] = {}
        for match in FN_RE.finditer(text):
            fn_name = match.group(1)
            fn_line = offset_to_line(text, match.start())
            functions[fn_name] = fn_line
            by_name.setdefault(fn_name, []).append((path, fn_line))
        by_file[path] = functions
    return by_file, by_name


def resolve_function_source(
    handler_expr: str,
    by_file: dict[Path, dict[str, int]],
    by_name: dict[str, list[tuple[Path, int]]],
) -> tuple[Path, int] | None:
    handler_expr = handler_expr.strip()
    if "|" in handler_expr:
        return None

    parts = [part for part in handler_expr.split("::") if part not in {"crate", "self", "super"}]
    if not parts:
        return None

    fn_name = parts[-1]
    module_parts = parts[:-1]
    candidates: list[Path] = []
    candidate_dirs: list[Path] = []

    if module_parts:
        if module_parts[0] == "ws":
            rel = Path(*module_parts)
            candidates.extend(
                [
                    REPO_ROOT / "src" / "server" / f"{rel}.rs",
                    REPO_ROOT / "src" / "server" / rel / "mod.rs",
                ]
            )
            candidate_dirs.append(REPO_ROOT / "src" / "server" / rel)
        else:
            rel = Path(*module_parts)
            candidates.extend(
                [
                    REPO_ROOT / "src" / "server" / "routes" / f"{rel}.rs",
                    REPO_ROOT / "src" / "server" / "routes" / rel / "mod.rs",
                ]
            )
            candidate_dirs.append(REPO_ROOT / "src" / "server" / "routes" / rel)

    for candidate in candidates:
        if candidate.exists() and fn_name in by_file.get(candidate, {}):
            return candidate, by_file[candidate][fn_name]

    for candidate_dir in candidate_dirs:
        if candidate_dir.is_dir():
            for candidate in sorted(candidate_dir.rglob("*.rs")):
                if is_test_file(candidate):
                    continue
                if fn_name in by_file.get(candidate, {}):
                    return candidate, by_file[candidate][fn_name]

    matches = by_name.get(fn_name, [])
    if len(matches) == 1:
        return matches[0]
    return None


# File-module and inline-module declarations, capturing immediately preceding
# attributes in any order. The test-only module graph uses this for `#[cfg]`,
# `#[path]`, and inline test-module context; the older `_MOD_DECL_RE` name is
# kept as an alias for callers/tests that intentionally inspect this parser.
_MOD_DECL_RE = re.compile(
    r"(?P<attrs>(?:\s*#\[[^\]]*\]\s*)*)"
    r"(?:pub(?:\([^)]*\))?\s+)?mod\s+"
    r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*"
    r"(?P<kind>[{;])",
    re.MULTILINE,
)
_CFG_ATTR_RE = re.compile(r"#\[\s*cfg\((?P<predicate>[^]]*?)\)\s*\]")
_PATH_ATTR_RE = re.compile(r'#\[\s*path\s*=\s*"(?P<path>[^"]+)"\s*\]')


@dataclass(frozen=True)
class _InlineModuleContext:
    end: int
    module_dir: Path
    requires_test: bool


def _attrs_require_test(attrs: str) -> bool:
    return any(
        cfg_requires_test(match.group("predicate"))
        for match in _CFG_ATTR_RE.finditer(attrs)
    )


def _path_attr(attrs: str) -> str | None:
    match = _PATH_ATTR_RE.search(attrs)
    return match.group("path") if match else None


def _file_module_dir(parent: Path) -> Path:
    if parent.name in {"lib.rs", "main.rs", "mod.rs"}:
        return parent.parent
    return parent.parent / parent.stem


def _declared_child_paths(
    parent: Path,
    name: str,
    *,
    base: Path | None = None,
    path_attr: str | None = None,
) -> tuple[Path, ...]:
    """Resolve the file(s) a `mod <name>;` declaration in ``parent`` points to."""

    if path_attr is not None:
        path_base = base if base is not None else parent.parent
        return ((path_base / path_attr).resolve(),)
    base = base or _file_module_dir(parent)
    return ((base / f"{name}.rs").resolve(), (base / name / "mod.rs").resolve())


def _module_file_declarations(
    parent: Path,
    text: str,
    *,
    parent_requires_test: bool = False,
) -> list[ModuleFileDeclaration]:
    declarations: list[ModuleFileDeclaration] = []
    contexts: list[_InlineModuleContext] = []
    for match in _MOD_DECL_RE.finditer(text):
        start = match.start()
        contexts = [context for context in contexts if start < context.end]
        inherited_test = parent_requires_test or any(
            context.requires_test for context in contexts
        )
        attrs = match.group("attrs") or ""
        requires_test = inherited_test or _attrs_require_test(attrs)
        name = match.group("name")

        if match.group("kind") == "{":
            try:
                _body, end = scan_balanced(text, match.end() - 1, "{", "}")
            except ParseError:
                continue
            base = contexts[-1].module_dir if contexts else _file_module_dir(parent)
            contexts.append(
                _InlineModuleContext(
                    end=end,
                    module_dir=base / name,
                    requires_test=requires_test,
                )
            )
            continue

        base = contexts[-1].module_dir if contexts else None
        declarations.append(
            ModuleFileDeclaration(
                parent=parent,
                name=name,
                child_paths=_declared_child_paths(
                    parent,
                    name,
                    base=base,
                    path_attr=_path_attr(attrs),
                ),
                requires_test=requires_test,
            )
        )
    return declarations


def test_only_module_files(
    production_files: Iterable[Path] | None = None,
    *,
    all_files: Iterable[Path] | None = None,
    read_text_fn: Callable[[Path], str] = read_text,
) -> set[Path]:
    """Files whose module is declared *only* under a test-requiring cfg.

    A file like ``src/server/routes/routes_tests/common.rs`` carries no
    ``#[cfg(test)]`` of its own but is compiled solely because its parent
    ``mod routes_tests;`` is gated behind ``#[cfg(all(test, …))]``. Such whole
    subtrees are test fixtures and must be excluded from the production
    giant-file surface (#3036). A module declared in *both* a production and a
    test context (e.g. ``src/db/schema.rs``) stays production.
    """

    prod_file_items = list(production_files or production_rust_files())
    prod_by_resolved = {path.resolve(): path for path in prod_file_items}
    prod_files = list(prod_by_resolved)
    source_files = [path.resolve() for path in (all_files or all_rust_files())]
    prod_file_set = set(prod_files)
    declarations_by_parent: dict[Path, list[ModuleFileDeclaration]] = {}
    for path in source_files:
        try:
            text = read_text_fn(path)
        except (OSError, UnicodeDecodeError):
            continue
        declarations_by_parent[path] = _module_file_declarations(
            path,
            text,
            parent_requires_test=is_test_file(path),
        )

    result: set[Path] = set()
    while True:
        test_targets: set[Path] = set()
        prod_targets: set[Path] = set()
        for parent, declarations in declarations_by_parent.items():
            parent_is_test = is_test_file(parent) or parent in result
            for declaration in declarations:
                target_set = (
                    test_targets
                    if (parent_is_test or declaration.requires_test)
                    else prod_targets
                )
                target_set.update(declaration.child_paths)

        test_only_dirs = [
            target.parent
            for target in (test_targets - prod_targets)
            if target.name == "mod.rs"
        ]
        next_result: set[Path] = set()
        direct_test_only = test_targets - prod_targets
        for path in prod_files:
            if path in direct_test_only or any(
                directory in path.parents for directory in test_only_dirs
            ):
                next_result.add(path)

        if next_result == result:
            return {
                prod_by_resolved[path] for path in next_result if path in prod_file_set
            }
        result = next_result


def collect_modules() -> list[ModuleEntry]:
    modules: list[ModuleEntry] = []
    test_only = test_only_module_files()
    for path in production_rust_files():
        text = read_text(path)
        total = line_count(text)
        if path in test_only:
            # Whole file is reached only through a test-gated parent `mod`; count
            # every line as test so the production surface is not frozen (#3036).
            prod_lines, test_lines = 0, total
        else:
            prod_lines, test_lines = split_prod_test_lines(text)
        flags: list[str] = []
        # Giant-file freezing keys off *production* LoC so that a module which
        # is only large because of inline test fixtures is not frozen against
        # legitimate prod-side bugfixes or further test growth (#3036).
        if prod_lines >= GIANT_FILE_THRESHOLD:
            flags.append("giant-file")
        modules.append(
            ModuleEntry(
                module_path=module_path_for_file(path),
                file_path=rel_posix(path),
                line_count=total,
                prod_line_count=prod_lines,
                test_line_count=test_lines,
                flags=tuple(flags),
            )
        )
    modules.sort(key=lambda item: item.module_path)
    return modules


_REGISTRY_SECTION_RE = re.compile(r"^\[(?P<name>\[?[A-Za-z0-9_.-]+\]?)\]$")
_REGISTRY_LIST_KV_RE = re.compile(r'^"(?P<value>[^"]+)"\s*,?\s*$')
_REGISTRY_KV_RE = re.compile(r'^(?P<key>[A-Za-z0-9_]+)\s*=\s*"(?P<value>[^"]*)"$')
_REGISTRY_ARRAY_RE = re.compile(r"^(?P<name>[A-Za-z0-9_]+)\s*=\s*\[\s*(?P<rest>.*)$")
_DEADLINE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
_REGISTRY_ENTRY_FIELDS = ("file", "owner", "deadline", "decompose_issue")
_REGISTRY_ARRAY_NAMES = ("grandfathered", "grandfathered_baseline_paths")


def _strip_toml_comment(line: str) -> str:
    in_string = False
    for index, ch in enumerate(line):
        if ch == '"':
            in_string = not in_string
        elif ch == "#" and not in_string:
            return line[:index].rstrip()
    return line.rstrip()


def load_giant_file_registry() -> tuple[list[str], list[dict[str, str]], list[str] | None]:
    """Parse the tiny TOML subset used by the giant-file registry.

    Supports the ``grandfathered`` and ``grandfathered_baseline_paths`` string
    arrays (each possibly spread over multiple lines) and any number of
    ``[[entry]]`` tables with quoted string values. Avoids a hard dependency on
    ``tomllib`` so the check runs on the same interpreters as the existing audit
    scripts. Returns ``(grandfathered, entries, baseline_paths)`` where
    ``baseline_paths`` is ``None`` if the array is absent.
    """

    if not GIANT_FILE_REGISTRY.is_file():
        raise ParseError(f"giant-file registry metadata missing: {rel_posix(GIANT_FILE_REGISTRY)}")

    arrays: dict[str, list[str]] = {}
    seen_arrays: set[str] = set()
    entries: list[dict[str, str]] = []
    current_entry: dict[str, str] | None = None
    active_array: str | None = None

    def consume_array_tokens(target: list[str], text: str) -> bool:
        """Append quoted paths from ``text``; return True when the array closes."""
        closed = False
        if "]" in text:
            text, _after = text.split("]", 1)
            closed = True
        for token in text.split(","):
            token = token.strip()
            if not token:
                continue
            match = _REGISTRY_LIST_KV_RE.fullmatch(token + ",")
            if match is None:
                raise ParseError(f"unparsable registry array entry: {token!r}")
            target.append(match.group("value"))
        return closed

    for raw_line in read_text(GIANT_FILE_REGISTRY).splitlines():
        line = _strip_toml_comment(raw_line).strip()
        if not line:
            continue

        if active_array is not None:
            if consume_array_tokens(arrays[active_array], line):
                active_array = None
            continue

        section = _REGISTRY_SECTION_RE.fullmatch(line)
        if section:
            name = section.group("name")
            if name == "[entry]":
                current_entry = {}
                entries.append(current_entry)
            else:
                current_entry = None
            continue

        array_open = _REGISTRY_ARRAY_RE.fullmatch(line)
        if array_open and array_open.group("name") in _REGISTRY_ARRAY_NAMES:
            name = array_open.group("name")
            if name in seen_arrays:
                raise ParseError(f"duplicate registry array: {name!r}")
            seen_arrays.add(name)
            arrays[name] = []
            current_entry = None
            if not consume_array_tokens(arrays[name], array_open.group("rest")):
                active_array = name
            continue

        if current_entry is not None:
            kv = _REGISTRY_KV_RE.fullmatch(line)
            if kv is None:
                raise ParseError(f"unparsable registry entry line: {line!r}")
            current_entry[kv.group("key")] = kv.group("value")

    baseline_paths = arrays.get("grandfathered_baseline_paths")
    return arrays.get("grandfathered", []), entries, baseline_paths


def build_giant_registrations(modules: list[ModuleEntry]) -> list[GiantFileRegistration]:
    """Validate the registry against measured prod-giants and build rows.

    Raises ``ParseError`` (failing generation, and therefore CI) when the
    registry drifts from reality (#3036):

      * a current prod-giant is registered nowhere (new giant without an
        owner/deadline/decompose issue);
      * a registered path is no longer a prod-giant (ghost registration left
        behind after decomposition);
      * an ``[[entry]]`` is missing a required field or has a malformed
        deadline;
      * a grandfathered path is absent from the frozen
        ``grandfathered_baseline_paths`` baseline, which would let a new giant
        skip the owner/deadline/decompose requirement.
    """

    grandfathered, entries, baseline_paths = load_giant_file_registry()
    prod_giants = {
        entry.file_path: entry.prod_line_count
        for entry in modules
        if "giant-file" in entry.flags
    }

    problems: list[str] = []
    seen: set[str] = set()

    # Closed baseline: `grandfathered` must be a subset of the frozen
    # `grandfathered_baseline_paths` snapshot. Removing a path (decomposed or
    # promoted to an [[entry]]) is fine; introducing a brand-new path is not —
    # this blocks swapping in a new prod-giant to dodge the owner/deadline gate.
    if baseline_paths is None:
        problems.append(
            "missing `grandfathered_baseline_paths` array in "
            f"{rel_posix(GIANT_FILE_REGISTRY)}"
        )
    else:
        baseline_set = set(baseline_paths)
        for path in sorted(set(grandfathered) - baseline_set):
            problems.append(
                f"grandfathered path {path!r} is not in the frozen "
                "`grandfathered_baseline_paths` baseline; new giants must be "
                "registered as an [[entry]] with owner/deadline/decompose_issue "
                "instead of grandfathered"
            )

    registrations: list[GiantFileRegistration] = []
    for entry in entries:
        missing = [field for field in _REGISTRY_ENTRY_FIELDS if not entry.get(field)]
        if missing:
            problems.append(
                f"[[entry]] {entry.get('file', '<no file>')!r} missing required "
                f"field(s): {', '.join(missing)}"
            )
            continue
        path = entry["file"]
        if not _DEADLINE_RE.fullmatch(entry["deadline"]):
            problems.append(
                f"[[entry]] {path!r} deadline {entry['deadline']!r} is not YYYY-MM-DD"
            )
        if path in seen:
            problems.append(f"duplicate registry path: {path!r}")
        seen.add(path)
        prod = prod_giants.get(path)
        if prod is None:
            problems.append(
                f"ghost registration: [[entry]] {path!r} is no longer a prod-giant "
                f"(>= {GIANT_FILE_THRESHOLD} prod lines); remove it once decomposed"
            )
            continue
        registrations.append(
            GiantFileRegistration(
                file_path=path,
                owner=entry["owner"],
                deadline=entry["deadline"],
                decompose_issue=entry["decompose_issue"],
                prod_line_count=prod,
            )
        )

    for path in grandfathered:
        if path in seen:
            problems.append(f"duplicate registry path: {path!r}")
            continue
        seen.add(path)
        prod = prod_giants.get(path)
        if prod is None:
            problems.append(
                f"ghost registration: grandfathered {path!r} is no longer a "
                f"prod-giant (>= {GIANT_FILE_THRESHOLD} prod lines); remove it"
            )
            continue
        registrations.append(
            GiantFileRegistration(
                file_path=path,
                owner="",
                deadline="",
                decompose_issue="",
                prod_line_count=prod,
            )
        )

    unregistered = sorted(set(prod_giants) - seen)
    for path in unregistered:
        problems.append(
            f"unregistered giant: {path!r} has {prod_giants[path]} prod lines "
            f"(>= {GIANT_FILE_THRESHOLD}) but is missing from "
            f"{rel_posix(GIANT_FILE_REGISTRY)}; add an [[entry]] with owner, "
            "deadline, and decompose_issue"
        )

    if problems:
        raise ParseError(
            "giant-file registry drift:\n  - " + "\n  - ".join(sorted(problems))
        )

    registrations.sort(key=lambda item: item.file_path)
    return registrations


def render_giant_file_registry(registrations: list[GiantFileRegistration]) -> str:
    tracked = [reg for reg in registrations if reg.deadline]
    grandfathered = [reg for reg in registrations if not reg.deadline]
    lines = [
        "# Giant-file Registry",
        "",
        "> Generated by `python3 scripts/generate_inventory_docs.py` from "
        "`scripts/giant_file_registry.toml`. Do not edit manually.",
        "",
        f"- Giant-file threshold: `>= {GIANT_FILE_THRESHOLD}` production lines "
        "(excludes `#[cfg(test)] mod` blocks).",
        f"- Registered giant files: `{len(registrations)}`",
        f"- Tracked (owner + deadline + decompose issue): `{len(tracked)}`",
        f"- Grandfathered (awaiting owner/deadline backfill or decomposition): "
        f"`{len(grandfathered)}`",
        "",
        "## Tracked Decompositions",
        "",
        "| Path | Prod | Owner | Deadline | Decompose Issue |",
        "| --- | ---: | --- | --- | --- |",
    ]
    if tracked:
        for reg in tracked:
            lines.append(
                f"| `{reg.file_path}` | {reg.prod_line_count} | {reg.owner} | "
                f"{reg.deadline} | {reg.decompose_issue} |"
            )
    else:
        lines.append("| _none_ | | | | |")
    lines.extend(
        [
            "",
            "## Grandfathered",
            "",
            "> Predate the deadline mandate (#3036). Each must be decomposed "
            "(drops off this list) or promoted to a tracked decomposition with "
            "an owner and deadline.",
            "",
            "| Path | Prod |",
            "| --- | ---: |",
        ]
    )
    for reg in grandfathered:
        lines.append(f"| `{reg.file_path}` | {reg.prod_line_count} |")
    lines.append("")
    return "\n".join(lines)


def extract_call_args(text: str, match: re.Match[str]) -> tuple[str, int]:
    open_index = match.end() - 1
    args, end_index = scan_balanced(text, open_index)
    return args, end_index


def parse_method_chain(method_expr: str) -> list[tuple[str, str]]:
    entries: list[tuple[str, str]] = []
    index = 0
    while index < len(method_expr):
        while index < len(method_expr) and method_expr[index] in " \t\r\n.":
            index += 1
        matched = False
        for method in HTTP_METHODS:
            prefix = method_expr[index : index + len(method)]
            next_index = index + len(method)
            if prefix == method and next_index < len(method_expr) and method_expr[next_index] == "(":
                handler, end_index = scan_balanced(method_expr, next_index)
                entries.append((method.upper(), handler.strip()))
                index = end_index
                matched = True
                break
        if not matched:
            break
    if not entries:
        raise ParseError(f"could not parse route methods from: {strip_wrapping_whitespace(method_expr)!r}")
    return entries


def parse_route_file(
    path: Path,
    path_prefix: str,
    by_file: dict[Path, dict[str, int]],
    by_name: dict[str, list[tuple[Path, int]]],
) -> list[RouteEntry]:
    text = read_text(path)
    entries: list[RouteEntry] = []
    route_re = re.compile(r"\.route\s*\(")
    for match in route_re.finditer(text):
        args, _ = extract_call_args(text, match)
        pieces = split_top_level(args, maxsplit=1)
        if len(pieces) != 2:
            raise ParseError(f"expected route(path, handler) in {path}")
        path_expr = pieces[0].strip()
        methods_expr = pieces[1].strip()
        if not path_expr.startswith('"') or not path_expr.endswith('"'):
            raise ParseError(f"unsupported non-literal path {path_expr!r} in {path}")
        route_path = path_expr[1:-1]
        decl_line = offset_to_line(text, match.start())
        for method, handler in parse_method_chain(methods_expr):
            resolved = resolve_function_source(handler, by_file, by_name)
            if resolved is None and "::" not in handler and handler in by_file.get(path, {}):
                resolved = (path, by_file[path][handler])
            if resolved is None:
                raise ParseError(f"could not resolve handler source for {handler!r}")
            handler_path, handler_line = resolved
            entries.append(
                RouteEntry(
                    method=method,
                    path=f"{path_prefix}{route_path}",
                    handler=f"`{handler}`",
                    handler_source=format_path_with_line(handler_path, handler_line),
                    route_decl=format_path_with_line(path, decl_line),
                )
            )
    return entries


def find_function_body(text: str, fn_name: str) -> tuple[str, int]:
    match = re.search(
        rf"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+{re.escape(fn_name)}\s*\(",
        text,
    )
    if match is None:
        raise ParseError(f"could not find function {fn_name}")
    open_brace = text.find("{", match.end())
    if open_brace == -1:
        raise ParseError(f"could not find body for function {fn_name}")
    body, _ = scan_balanced(text, open_brace, open_char="{", close_char="}")
    return body, open_brace + 1


def _router_source_path_from_merge_expr(expr: str, routes_root: Path) -> Path:
    stripped = expr.strip()
    domain_match = re.match(
        r"^(?:(?:crate::server::routes|self)::)?domains::"
        r"(?P<module>[A-Za-z_][A-Za-z0-9_]*)::router\s*\(",
        stripped,
    )
    if domain_match:
        source_path = routes_root / "domains" / f"{domain_match.group('module')}.rs"
    else:
        top_level_match = re.match(
            r"^(?:(?:crate::server::routes|self)::)?"
            r"(?P<module>[A-Za-z_][A-Za-z0-9_]*)::router\s*\(",
            stripped,
        )
        if top_level_match is None:
            raise ParseError(
                "unsupported compose_api_router merge expression: "
                f"{strip_wrapping_whitespace(stripped)!r}"
            )
        source_path = routes_root / f"{top_level_match.group('module')}.rs"

    if not source_path.exists():
        raise ParseError(
            "compose_api_router merge references missing router source: "
            f"{rel_posix(source_path) if source_path.is_relative_to(REPO_ROOT) else source_path}"
        )
    return source_path


def mounted_api_route_source_paths(
    routes_mod_path: Path | None = None,
    routes_root: Path | None = None,
) -> list[Path]:
    """Return route declaration files merged by ``compose_api_router``.

    ``src/server/mod.rs`` nests the composed API router under ``/api``. The
    composed router itself is centralized in ``src/server/routes/mod.rs``; parse
    that merge chain so coverage follows the actual mounted router graph instead
    of scanning files by convention.
    """

    if routes_mod_path is None:
        routes_mod_path = REPO_ROOT / "src" / "server" / "routes" / "mod.rs"
    if routes_root is None:
        routes_root = routes_mod_path.parent

    text = read_text(routes_mod_path)
    body, _offset = find_function_body(text, "compose_api_router")
    paths: list[Path] = []
    seen: set[Path] = set()
    for match in re.finditer(r"\.merge\s*\(", body):
        args, _end = extract_call_args(body, match)
        pieces = split_top_level(args, maxsplit=1)
        if len(pieces) != 1:
            raise ParseError(
                "expected one router expression in compose_api_router merge: "
                f"{strip_wrapping_whitespace(args)!r}"
            )
        source_path = _router_source_path_from_merge_expr(pieces[0], routes_root)
        if source_path not in seen:
            paths.append(source_path)
            seen.add(source_path)

    if not paths:
        raise ParseError(f"could not find merged API routers in {rel_posix(routes_mod_path)}")
    return paths


def collect_mounted_api_route_entries(
    function_paths: list[Path] | None = None,
    by_file: dict[Path, dict[str, int]] | None = None,
    by_name: dict[str, list[tuple[Path, int]]] | None = None,
) -> list[RouteEntry]:
    if by_file is None or by_name is None:
        if function_paths is None:
            function_paths = sorted(
                path
                for path in (REPO_ROOT / "src" / "server").rglob("*.rs")
                if path.is_file() and not is_test_file(path)
            )
        by_file, by_name = build_function_index(function_paths)

    route_entries: list[RouteEntry] = []
    for route_path in mounted_api_route_source_paths():
        route_entries.extend(parse_route_file(route_path, "/api", by_file, by_name))
    route_entries.sort(key=lambda entry: (entry.path, entry.method, entry.handler))
    return route_entries


def preceding_comment_block(text: str, offset: int) -> str:
    lines = text[:offset].splitlines()
    comments: list[str] = []
    cursor = len(lines) - 1
    while cursor >= 0:
        line = lines[cursor].rstrip()
        if not line:
            if comments:
                break
            cursor -= 1
            continue
        stripped = line.lstrip()
        if stripped.startswith("//"):
            comments.append(stripped[2:].strip())
            cursor -= 1
            continue
        break
    comments.reverse()
    return " ".join(comments)


def find_worker_target(inner: str) -> str:
    awaited_targets = re.findall(r"([A-Za-z_][A-Za-z0-9_:]*)\s*\([^;\n]*?\)\.await", inner, re.DOTALL)
    awaited_targets = [target for target in awaited_targets if not target.endswith("tick")]
    if awaited_targets:
        return awaited_targets[-1]
    block_on_match = re.search(r"block_on\(\s*([A-Za-z_][A-Za-z0-9_:]*)\s*\(", inner)
    if block_on_match is not None:
        return block_on_match.group(1)
    raise ParseError(f"could not infer worker target from block: {strip_wrapping_whitespace(inner)!r}")


def find_thread_name(prefix: str) -> str | None:
    match = re.search(r'\.name\("([^"]+)"', prefix)
    return match.group(1) if match else None


def collect_workers() -> list[WorkerEntry]:
    registry_path = REPO_ROOT / "src" / "server" / "worker_registry.rs"
    text = read_text(registry_path)
    workers: list[WorkerEntry] = []

    array_match = re.search(
        r"pub\(crate\)\s+const\s+WORKER_SPECS\s*:[^=]*=\s*\[(?P<body>.*?)\n\];",
        text,
        re.DOTALL,
    )
    if array_match is None:
        raise ParseError("could not locate WORKER_SPECS definition")

    spec_re = re.compile(r"WorkerSpec\s*\{(?P<body>.*?)\n\s*\}", re.DOTALL)
    kind_labels = {
        "TokioTask": "tokio::spawn",
        "DedicatedThread": "std::thread::spawn",
        "SpawnHelper": "spawn helper",
    }
    stage_labels = {
        "AfterBootReconcile": "after_boot_reconcile",
        "AfterWebsocketBroadcast": "after_websocket_broadcast",
    }
    restart_labels = {
        "SkipWhenDisabled": "skip_when_disabled",
        "LoopOwned": "loop_owned",
        "RestartableWithBudget": "restartable_with_budget",
        "ManualProcessRestart": "manual_process_restart",
    }
    shutdown_labels = {
        "RuntimeShutdown": "runtime_shutdown",
        "ProcessExit": "process_exit",
    }

    def capture(body: str, pattern: str, field: str) -> str:
        match = re.search(pattern, body)
        if match is None:
            raise ParseError(f"missing {field} in WORKER_SPECS entry: {strip_wrapping_whitespace(body)!r}")
        return match.group(1)

    array_body = array_match.group("body")
    for match in spec_re.finditer(array_body):
        body = match.group("body")
        full_offset = array_match.start("body") + match.start()
        line = offset_to_line(text, full_offset)
        worker = capture(body, r'name:\s*"([^"]+)"', "name")
        target = capture(body, r'target:\s*"([^"]+)"', "target")
        kind = kind_labels[capture(body, r"kind:\s*WorkerKind::([A-Za-z0-9_]+)", "kind")]
        stage = stage_labels[capture(body, r"start_stage:\s*WorkerStartStage::([A-Za-z0-9_]+)", "start_stage")]
        start_order = capture(body, r"start_order:\s*([0-9]+)", "start_order")
        restart = restart_labels[
            capture(
                body,
                r"restart_policy:\s*WorkerRestartPolicy::([A-Za-z0-9_]+)",
                "restart_policy",
            )
        ]
        shutdown = shutdown_labels[
            capture(
                body,
                r"shutdown_policy:\s*WorkerShutdownPolicy::([A-Za-z0-9_]+)",
                "shutdown_policy",
            )
        ]
        responsibility = capture(body, r'responsibility:\s*"([^"]+)"', "responsibility")
        owner = capture(body, r'owner:\s*"([^"]+)"', "owner")
        health_owner = capture(body, r'health_owner:\s*"([^"]+)"', "health_owner")
        notes = capture(body, r'notes:\s*"([^"]*)"', "notes")
        workers.append(
            WorkerEntry(
                worker=worker,
                kind=kind,
                target=f"`{target}`",
                source=format_path_with_line(registry_path, line),
                notes=(
                    f"stage={stage}; order={start_order}; restart={restart}; shutdown={shutdown}; "
                    f"owner={owner}; health={health_owner}; responsibility={responsibility}; {notes}"
                ),
            )
        )

    workers.sort(key=lambda item: int(item.source.rsplit(":", 1)[-1].rstrip("`")))
    return workers


def render_ascii_tree(root: Path) -> list[str]:
    lines: list[str] = [f"{root.name}/"]

    def walk(directory: Path, prefix: str) -> None:
        children = sorted(
            (child for child in directory.iterdir() if not is_inventory_ignored(child)),
            key=lambda child: (child.is_file(), child.name),
        )
        for index, child in enumerate(children):
            is_last = index == len(children) - 1
            branch = "└── " if is_last else "├── "
            label = child.name + ("/" if child.is_dir() else "")
            lines.append(f"{prefix}{branch}{label}")
            if child.is_dir():
                walk(child, prefix + ("    " if is_last else "│   "))

    walk(root, "")
    return lines


def replace_generated_block(
    text: str,
    start_marker: str,
    end_marker: str,
    replacement: str,
) -> str:
    start_index = text.find(start_marker)
    end_index = text.find(end_marker)
    if start_index == -1 or end_index == -1 or end_index < start_index:
        raise ParseError(f"could not locate generated block {start_marker!r} … {end_marker!r}")

    block_start = start_index + len(start_marker)
    return (
        text[:block_start]
        + "\n"
        + replacement.rstrip()
        + "\n"
        + text[end_index:]
    )


def render_architecture_doc() -> str:
    current = read_text(ARCHITECTURE_DOC)
    src_tree = "\n".join(["```text", *render_ascii_tree(SRC_ROOT), "```"])
    current = replace_generated_block(
        current,
        ARCHITECTURE_SRC_TREE_START,
        ARCHITECTURE_SRC_TREE_END,
        src_tree,
    )
    return replace_generated_block(
        current,
        ARCHITECTURE_TOP_LEVEL_MAP_START,
        ARCHITECTURE_TOP_LEVEL_MAP_END,
        render_top_level_module_map(),
    )


def render_top_level_module_map() -> str:
    entries = top_level_src_entries()
    entry_keys = [top_level_src_key(path) for path in entries]
    missing = [key for key in entry_keys if key not in TOP_LEVEL_MODULE_PURPOSES]
    extra = sorted(key for key in TOP_LEVEL_MODULE_PURPOSES if key not in entry_keys)
    if missing or extra:
        problems: list[str] = []
        if missing:
            problems.append(f"missing descriptions for {', '.join(missing)}")
        if extra:
            problems.append(f"stale descriptions for {', '.join(extra)}")
        raise ParseError("top-level architecture map drift: " + "; ".join(problems))

    lines = [
        "> Generated by `python3 scripts/generate_inventory_docs.py`. Update `TOP_LEVEL_MODULE_PURPOSES` when `src/` top-level entries change.",
        "",
        "| Path | Purpose |",
        "| --- | --- |",
    ]
    for path in entries:
        key = top_level_src_key(path)
        lines.append(f"| `src/{key}` | {TOP_LEVEL_MODULE_PURPOSES[key]} |")
    lines.append("")
    return "\n".join(lines)


def render_module_inventory(entries: list[ModuleEntry]) -> str:
    namespace_counts = Counter(entry.module_path.split("::", 1)[0] for entry in entries)
    giant_count = sum(1 for entry in entries if "giant-file" in entry.flags)
    lines = [
        "# Module Inventory",
        "",
        "> Generated by `python3 scripts/generate_inventory_docs.py`. Do not edit manually.",
        "> Drift policy: see [docs/generated/README.md](README.md#generated-docs-drift-policy).",
        "",
        f"- Production Rust modules: `{len(entries)}`",
        f"- Giant-file threshold: `>= {GIANT_FILE_THRESHOLD}` production lines",
        f"- Giant files: `{giant_count}`",
        "",
        "> `Prod` excludes lines inside `#[cfg(test)] mod` blocks and whole",
        "> files reached only through test-only module declarations; the",
        "> giant-file flag tracks `Prod` so test fixtures do not freeze a module",
        "> (#3036/#4394). `Lines` is the raw total for reference.",
        "",
        "## Namespace Summary",
        "",
        "| Namespace | Modules |",
        "| --- | ---: |",
    ]
    for namespace, count in sorted(namespace_counts.items()):
        lines.append(f"| `{namespace}` | {count} |")
    lines.extend(
        [
            "",
            "## Detailed Inventory",
            "",
            "| Module | Path | Lines | Prod | Test | Flags |",
            "| --- | --- | ---: | ---: | ---: | --- |",
        ]
    )
    for entry in entries:
        flags = ", ".join(entry.flags) if entry.flags else ""
        lines.append(
            f"| `{entry.module_path}` | `{entry.file_path}` | {entry.line_count} | "
            f"{entry.prod_line_count} | {entry.test_line_count} | {flags} |"
        )
    lines.append("")
    return "\n".join(lines)


def render_route_inventory(entries: list[RouteEntry]) -> str:
    method_counts = Counter(entry.method for entry in entries)
    lines = [
        "# Route Inventory",
        "",
        "> Generated by `python3 scripts/generate_inventory_docs.py`. Do not edit manually.",
        "",
        f"- HTTP routes: `{len(entries)}`",
        f"- Methods: {', '.join(f'`{method}`={method_counts[method]}' for method in sorted(method_counts))}",
        "",
        "| Method | Path | Handler | Handler Source | Route Decl |",
        "| --- | --- | --- | --- | --- |",
    ]
    for entry in entries:
        lines.append(
            f"| `{entry.method}` | `{entry.path}` | {entry.handler} | {entry.handler_source} | {entry.route_decl} |"
        )
    lines.append("")
    return "\n".join(lines)


def render_worker_inventory(entries: list[WorkerEntry]) -> str:
    lines = [
        "# Bootstrap Worker Inventory",
        "",
        "> Generated by `python3 scripts/generate_inventory_docs.py`. Do not edit manually.",
        "",
        "- Scope: supervised worker specs registered in `server::worker_registry::WORKER_SPECS`.",
        f"- Workers: `{len(entries)}`",
        "",
        "| Worker | Kind | Target | Source | Notes |",
        "| --- | --- | --- | --- | --- |",
    ]
    for entry in entries:
        lines.append(
            f"| {entry.worker} | `{entry.kind}` | {entry.target} | {entry.source} | {entry.notes} |"
        )
    lines.append("")
    return "\n".join(lines)


def generated_documents() -> dict[Path, str]:
    function_paths = sorted(
        path
        for path in (REPO_ROOT / "src" / "server").rglob("*.rs")
        if path.is_file() and not is_test_file(path)
    )
    by_file, by_name = build_function_index(function_paths)

    module_entries = collect_modules()
    giant_registrations = build_giant_registrations(module_entries)
    route_entries = collect_mounted_api_route_entries(function_paths, by_file, by_name)
    route_entries.extend(
        parse_route_file(REPO_ROOT / "src" / "server" / "mod.rs", "", by_file, by_name)
    )
    route_entries.sort(key=lambda entry: (entry.path, entry.method, entry.handler))
    worker_entries = collect_workers()
    return {
        ARCHITECTURE_DOC: render_architecture_doc(),
        GENERATED_DOCS_DIR / "module-inventory.md": render_module_inventory(module_entries),
        GIANT_FILE_REGISTRY_DOC: render_giant_file_registry(giant_registrations),
        GENERATED_DOCS_DIR / "route-inventory.md": render_route_inventory(route_entries),
        GENERATED_DOCS_DIR / "worker-inventory.md": render_worker_inventory(worker_entries),
    }


def write_documents(documents: dict[Path, str], check: bool) -> int:
    stale_paths: list[Path] = []
    wrote_files = False
    for path, content in documents.items():
        if check:
            current = path.read_text(encoding="utf-8") if path.exists() else None
            if current != content:
                stale_paths.append(path)
                print(f"stale generated doc: {rel_posix(path)}")
                if current is not None:
                    diff = difflib.unified_diff(
                        current.splitlines(),
                        content.splitlines(),
                        fromfile=f"{rel_posix(path)} (current)",
                        tofile=f"{rel_posix(path)} (expected)",
                        lineterm="",
                    )
                    for line in list(diff)[:80]:
                        print(line)
                continue
            print(f"up to date: {rel_posix(path)}")
            continue

        path.parent.mkdir(parents=True, exist_ok=True)
        current = path.read_text(encoding="utf-8") if path.exists() else None
        if current != content:
            path.write_text(content, encoding="utf-8")
            print(f"wrote {rel_posix(path)}")
            wrote_files = True
        else:
            print(f"unchanged {rel_posix(path)}")

    if stale_paths:
        print("")
        print("generated docs are stale; rerun `python3 scripts/generate_inventory_docs.py`")
        return 1

    if wrote_files:
        print("\nNOTE: Generated inventory changed. Check for existing open PRs to avoid duplicate inventory refreshes.")

    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Generate deterministic code inventories and ARCHITECTURE.md source snapshots."
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail when committed generated docs differ from current source tree",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        documents = generated_documents()
        return write_documents(documents, check=args.check)
    except ParseError as error:
        print(f"inventory generation failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
