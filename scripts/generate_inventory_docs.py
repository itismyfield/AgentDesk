#!/usr/bin/env python3

from __future__ import annotations

import argparse
import difflib
import re
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SRC_ROOT = REPO_ROOT / "src"
GENERATED_DOCS_DIR = REPO_ROOT / "docs" / "generated"
GIANT_FILE_THRESHOLD = 1000
HTTP_METHODS = ("delete", "get", "head", "options", "patch", "post", "put")
TEST_FILE_NAMES = {"integration_tests.rs", "tests.rs"}

FN_RE = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(",
    re.MULTILINE,
)


@dataclass(frozen=True)
class ModuleEntry:
    module_path: str
    file_path: str
    line_count: int
    flags: tuple[str, ...]


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


class ParseError(RuntimeError):
    pass


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def line_count(text: str) -> int:
    return len(text.splitlines())


def rel_posix(path: Path) -> str:
    return path.relative_to(REPO_ROOT).as_posix()


def offset_to_line(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def is_test_file(path: Path) -> bool:
    return path.name.endswith("_tests.rs") or path.name in TEST_FILE_NAMES


def production_rust_files() -> list[Path]:
    return sorted(
        path for path in SRC_ROOT.rglob("*.rs") if path.is_file() and not is_test_file(path)
    )


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


def scan_balanced(text: str, open_index: int, open_char: str = "(", close_char: str = ")") -> tuple[str, int]:
    if text[open_index] != open_char:
        raise ParseError(f"expected {open_char!r} at offset {open_index}")
    depth = 0
    index = open_index
    in_string = False
    in_char = False
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

        if in_char:
            if escape:
                escape = False
                index += 1
                continue
            if ch == "\\":
                escape = True
                index += 1
                continue
            if ch == "'":
                in_char = False
            index += 1
            continue

        raw_start = split_raw_string_start(text, index)
        if raw_start is not None:
            index, raw_hashes = raw_start
            in_string = True
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
            in_char = True
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
    in_char = False
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

        if in_char:
            if escape:
                escape = False
                index += 1
                continue
            if ch == "\\":
                escape = True
                index += 1
                continue
            if ch == "'":
                in_char = False
            index += 1
            continue

        raw_start = split_raw_string_start(text, index)
        if raw_start is not None:
            index, raw_hashes = raw_start
            in_string = True
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
            in_char = True
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


def collect_modules() -> list[ModuleEntry]:
    modules: list[ModuleEntry] = []
    for path in production_rust_files():
        text = read_text(path)
        flags: list[str] = []
        if line_count(text) >= GIANT_FILE_THRESHOLD:
            flags.append("giant-file")
        modules.append(
            ModuleEntry(
                module_path=module_path_for_file(path),
                file_path=rel_posix(path),
                line_count=line_count(text),
                flags=tuple(flags),
            )
        )
    modules.sort(key=lambda item: item.module_path)
    return modules


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
    match = re.search(rf"pub\s+async\s+fn\s+{re.escape(fn_name)}\s*\(", text)
    if match is None:
        raise ParseError(f"could not find function {fn_name}")
    open_brace = text.find("{", match.end())
    if open_brace == -1:
        raise ParseError(f"could not find body for function {fn_name}")
    body, _ = scan_balanced(text, open_brace, open_char="{", close_char="}")
    return body, open_brace + 1


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
    server_mod_path = REPO_ROOT / "src" / "server" / "mod.rs"
    text = read_text(server_mod_path)
    run_body, body_offset = find_function_body(text, "run")
    workers: list[WorkerEntry] = []

    spawn_batch_re = re.compile(r"([A-Za-z_][A-Za-z0-9_:]*spawn_[A-Za-z0-9_]+)\s*\(")
    explicit_worker_calls: set[tuple[int, str]] = set()

    for match in re.finditer(r"tokio::spawn\s*\(", run_body):
        args, _ = extract_call_args(run_body, match)
        target = find_worker_target(args)
        full_offset = body_offset + match.start()
        line = offset_to_line(text, full_offset)
        comment = preceding_comment_block(text, full_offset)
        explicit_worker_calls.add((full_offset, "tokio::spawn"))
        workers.append(
            WorkerEntry(
                worker=comment or target.split("::")[-1],
                kind="tokio::spawn",
                target=f"`{target}`",
                source=format_path_with_line(server_mod_path, line),
                notes="inline loop" if "loop {" in args else "",
            )
        )

    for match in re.finditer(r"\.spawn\s*\(\s*move\s*\|\|", run_body):
        open_index = run_body.find("(", match.start(), match.end())
        if open_index == -1:
            raise ParseError("could not locate '(' for std::thread::spawn call")
        args, _ = scan_balanced(run_body, open_index)
        target = find_worker_target(args)
        full_offset = body_offset + match.start()
        line = offset_to_line(text, full_offset)
        comment = preceding_comment_block(text, full_offset)
        prefix_start = max(0, match.start() - 200)
        thread_name = find_thread_name(run_body[prefix_start : match.start()])
        worker_name = thread_name or comment or target.split("::")[-1]
        workers.append(
            WorkerEntry(
                worker=worker_name,
                kind="std::thread::spawn",
                target=f"`{target}`",
                source=format_path_with_line(server_mod_path, line),
                notes=comment,
            )
        )

    for match in spawn_batch_re.finditer(run_body):
        target = match.group(1)
        if target == "tokio::spawn":
            continue
        full_offset = body_offset + match.start()
        if any(existing_offset == full_offset for existing_offset, _ in explicit_worker_calls):
            continue
        line = offset_to_line(text, full_offset)
        comment = preceding_comment_block(text, full_offset)
        workers.append(
            WorkerEntry(
                worker=comment or target.split("::")[-1],
                kind="spawn helper",
                target=f"`{target}`",
                source=format_path_with_line(server_mod_path, line),
                notes="direct bootstrap helper call",
            )
        )

    workers.sort(key=lambda item: int(item.source.rsplit(":", 1)[-1].rstrip("`")))
    return workers


def render_module_inventory(entries: list[ModuleEntry]) -> str:
    namespace_counts = Counter(entry.module_path.split("::", 1)[0] for entry in entries)
    giant_count = sum(1 for entry in entries if "giant-file" in entry.flags)
    lines = [
        "# Module Inventory",
        "",
        "> Generated by `python3 scripts/generate_inventory_docs.py`. Do not edit manually.",
        "",
        f"- Production Rust modules: `{len(entries)}`",
        f"- Giant-file threshold: `>= {GIANT_FILE_THRESHOLD}` lines",
        f"- Giant files: `{giant_count}`",
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
            "| Module | Path | Lines | Flags |",
            "| --- | --- | ---: | --- |",
        ]
    )
    for entry in entries:
        flags = ", ".join(entry.flags) if entry.flags else ""
        lines.append(
            f"| `{entry.module_path}` | `{entry.file_path}` | {entry.line_count} | {flags} |"
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
        "- Scope: background task/thread bootstrap started directly from `server::run`.",
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
    route_entries = parse_route_file(
        REPO_ROOT / "src" / "server" / "routes" / "mod.rs",
        "/api",
        by_file,
        by_name,
    )
    route_entries.extend(
        parse_route_file(REPO_ROOT / "src" / "server" / "mod.rs", "", by_file, by_name)
    )
    route_entries.sort(key=lambda entry: (entry.path, entry.method, entry.handler))
    worker_entries = collect_workers()
    return {
        GENERATED_DOCS_DIR / "module-inventory.md": render_module_inventory(module_entries),
        GENERATED_DOCS_DIR / "route-inventory.md": render_route_inventory(route_entries),
        GENERATED_DOCS_DIR / "worker-inventory.md": render_worker_inventory(worker_entries),
    }


def write_documents(documents: dict[Path, str], check: bool) -> int:
    stale_paths: list[Path] = []
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
        else:
            print(f"unchanged {rel_posix(path)}")

    if stale_paths:
        print("")
        print("generated docs are stale; rerun `python3 scripts/generate_inventory_docs.py`")
        return 1
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Generate deterministic code inventories under docs/generated/.")
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
