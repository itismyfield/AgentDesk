#!/usr/bin/env python3

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT_DIR = REPO_ROOT / "scripts"
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import generate_inventory_docs as inventory  # noqa: E402

DOCS_RS = REPO_ROOT / "src" / "server" / "routes" / "docs.rs"
PARAM_RE = re.compile(r"\{[^}/]+\}")
GLOB_CHARS_RE = re.compile(r"[*?\[\]]")

# Exact mounted routes that intentionally stay out of /api/docs. Keep this
# dictionary narrow: no globbing, no path-prefix entries, and every reason must
# explain why callers should not discover the route from the public docs shape.
UNDOCUMENTED_API_ROUTES: dict[tuple[str, str], str] = {}


@dataclass(frozen=True, order=True)
class EndpointPair:
    method: str
    path: str

    @property
    def normalized(self) -> "EndpointPair":
        return EndpointPair(self.method.upper(), normalize_path(self.path))


@dataclass(frozen=True)
class CoverageReport:
    missing: tuple[EndpointPair, ...]
    stale: tuple[EndpointPair, ...]
    unused_allowlist: tuple[EndpointPair, ...]
    allowlist_errors: tuple[str, ...]

    def is_clean(self) -> bool:
        return not (
            self.missing
            or self.stale
            or self.unused_allowlist
            or self.allowlist_errors
        )


def normalize_path(path: str) -> str:
    normalized = PARAM_RE.sub("{}", path.strip())
    if len(normalized) > 1:
        normalized = normalized.rstrip("/")
    return normalized


def _parse_simple_rust_string(expr: str) -> str | None:
    expr = expr.strip()
    if len(expr) < 2 or not expr.startswith('"') or not expr.endswith('"'):
        return None
    return bytes(expr[1:-1], "utf-8").decode("unicode_escape")


def _rust_function_body(text: str, fn_name: str) -> str:
    match = re.search(rf"\bfn\s+{re.escape(fn_name)}\s*\(", text)
    if match is None:
        raise RuntimeError(f"could not find function {fn_name}")
    open_brace = text.find("{", match.end())
    if open_brace == -1:
        raise RuntimeError(f"could not find body for function {fn_name}")
    body, _end = inventory.scan_balanced(
        text, open_brace, open_char="{", close_char="}"
    )
    return body


def _child_module_path(parent_path: Path, module_name: str) -> Path:
    return parent_path.parent / parent_path.stem / f"{module_name}.rs"


def _docs_inventory_source(docs_path: Path) -> tuple[Path, str]:
    text = docs_path.read_text(encoding="utf-8")
    try:
        body = _rust_function_body(text, "all_endpoints")
    except RuntimeError as root_error:
        if re.search(r"(?m)^\s*mod\s+inventory\s*;", text):
            inventory_path = _child_module_path(docs_path, "inventory")
            if inventory_path.exists():
                return inventory_path, inventory_path.read_text(encoding="utf-8")
        raise root_error

    if re.search(r"\binventory::all_endpoints\s*\(\s*\)", body):
        inventory_path = _child_module_path(docs_path, "inventory")
        if inventory_path.exists():
            return inventory_path, inventory_path.read_text(encoding="utf-8")

    return docs_path, text


def _parse_ep_entries(text: str, fn_name: str) -> list[EndpointPair]:
    body = _rust_function_body(text, fn_name)
    entries: list[EndpointPair] = []
    for match in re.finditer(r"\bep\s*\(", body):
        args, _end = inventory.extract_call_args(body, match)
        pieces = inventory.split_top_level(args, maxsplit=3)
        if len(pieces) < 2:
            continue
        method = _parse_simple_rust_string(pieces[0])
        path = _parse_simple_rust_string(pieces[1])
        if method is None or path is None:
            continue
        entries.append(EndpointPair(method.upper(), path))
    return entries


def _extended_endpoint_part_modules(module_text: str) -> list[str]:
    body = _rust_function_body(module_text, "all")
    return re.findall(
        r"\b[A-Za-z_][A-Za-z0-9_]*\.extend\s*\(\s*"
        r"([A-Za-z0-9_]+)::endpoints\s*\(\s*\)\s*\)",
        body,
    )


def _parse_endpoint_part_modules(source_path: Path) -> list[EndpointPair]:
    endpoints_mod = source_path.parent / source_path.stem / "endpoints" / "mod.rs"
    if not endpoints_mod.exists():
        return []

    mod_text = endpoints_mod.read_text(encoding="utf-8")
    entries: list[EndpointPair] = []
    for module_name in _extended_endpoint_part_modules(mod_text):
        part_path = endpoints_mod.parent / f"{module_name}.rs"
        if part_path.exists():
            entries.extend(_parse_ep_entries(part_path.read_text(encoding="utf-8"), "endpoints"))
    return entries


def parse_docs_endpoints(docs_path: Path = DOCS_RS) -> list[EndpointPair]:
    source_path, text = _docs_inventory_source(docs_path)
    entries = _parse_ep_entries(text, "all_endpoints")
    if entries:
        return entries
    return _parse_endpoint_part_modules(source_path)


def collect_mounted_api_endpoints() -> list[EndpointPair]:
    return [
        EndpointPair(entry.method.upper(), entry.path)
        for entry in inventory.collect_mounted_api_route_entries()
        if entry.path.startswith("/api/")
    ]


def _validate_allowlist(
    allowlist: dict[tuple[str, str], str],
    mounted_raw: set[EndpointPair],
    docs_normalized: set[EndpointPair],
) -> tuple[tuple[EndpointPair, ...], tuple[str, ...]]:
    unused: list[EndpointPair] = []
    errors: list[str] = []

    for (method, path), reason in sorted(allowlist.items()):
        pair = EndpointPair(method.upper(), path)
        if method != method.upper():
            errors.append(f"{method} {path}: method must be uppercase")
        if not path.startswith("/api/"):
            errors.append(f"{pair.method} {path}: allowlist path must start with /api/")
        if GLOB_CHARS_RE.search(method) or GLOB_CHARS_RE.search(path):
            errors.append(f"{pair.method} {path}: allowlist entries must be exact, not globs")
        if not reason.strip():
            errors.append(f"{pair.method} {path}: allowlist reason must be non-empty")
        if pair not in mounted_raw or pair.normalized in docs_normalized:
            unused.append(pair)

    return tuple(sorted(unused)), tuple(errors)


def build_coverage_report(
    mounted: list[EndpointPair] | None = None,
    docs: list[EndpointPair] | None = None,
    allowlist: dict[tuple[str, str], str] | None = None,
) -> CoverageReport:
    if mounted is None:
        mounted = collect_mounted_api_endpoints()
    if docs is None:
        docs = parse_docs_endpoints()
    if allowlist is None:
        allowlist = UNDOCUMENTED_API_ROUTES

    mounted_raw = set(mounted)
    docs_raw = set(docs)
    docs_normalized = {entry.normalized for entry in docs_raw}
    mounted_normalized = {entry.normalized for entry in mounted_raw}
    allowlisted_raw = {EndpointPair(method.upper(), path) for method, path in allowlist}

    missing = tuple(
        sorted(
            entry
            for entry in mounted_raw
            if entry not in allowlisted_raw and entry.normalized not in docs_normalized
        )
    )
    stale = tuple(
        sorted(entry for entry in docs_raw if entry.normalized not in mounted_normalized)
    )
    unused_allowlist, allowlist_errors = _validate_allowlist(
        allowlist, mounted_raw, docs_normalized
    )

    return CoverageReport(
        missing=missing,
        stale=stale,
        unused_allowlist=unused_allowlist,
        allowlist_errors=allowlist_errors,
    )


def _format_pairs(title: str, pairs: tuple[EndpointPair, ...]) -> list[str]:
    if not pairs:
        return []
    lines = [title]
    lines.extend(f"  - {pair.method} {pair.path}" for pair in pairs)
    return lines


def format_report(report: CoverageReport) -> str:
    if report.is_clean():
        return "api docs coverage check passed"

    lines: list[str] = []
    lines.extend(
        _format_pairs("Missing /api/docs entries for mounted routes:", report.missing)
    )
    lines.extend(_format_pairs("Stale /api/docs entries not mounted:", report.stale))
    lines.extend(_format_pairs("Unused allowlist entries:", report.unused_allowlist))
    if report.allowlist_errors:
        lines.append("Allowlist errors:")
        lines.extend(f"  - {error}" for error in report.allowlist_errors)
    return "\n".join(lines)


def main() -> int:
    report = build_coverage_report()
    print(format_report(report))
    return 0 if report.is_clean() else 1


if __name__ == "__main__":
    raise SystemExit(main())
