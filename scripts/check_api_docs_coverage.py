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
UNDOCUMENTED_API_ROUTES: dict[tuple[str, str], str] = {
    (
        "POST",
        "/api/hook/reset-status",
    ): "Loopback-only hook control-plane mutation for status reconciliation; not an operator-facing API.",
    (
        "POST",
        "/api/hook/skill-usage",
    ): "Loopback-only hook ingestion endpoint for skill usage telemetry; callers use docs-visible analytics routes instead.",
    (
        "DELETE",
        "/api/hook/session/{sessionKey}",
    ): "Loopback-only hook session disconnect endpoint with hook-client path casing; not part of the public API docs contract.",
    (
        "POST",
        "/api/internal/escalation/emit",
    ): "Loopback-only internal escalation emitter under /internal; not discoverable as a public operator route.",
}


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


def parse_docs_endpoints(docs_path: Path = DOCS_RS) -> list[EndpointPair]:
    text = docs_path.read_text(encoding="utf-8")
    body = _rust_function_body(text, "all_endpoints")
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
