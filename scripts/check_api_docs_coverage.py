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
from check_prompt_api_routes import API_ENDPOINTS_JSON, load_endpoint_pairs  # noqa: E402
import rust_lex  # noqa: E402

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


def load_docs_endpoints(catalog: Path = REPO_ROOT / API_ENDPOINTS_JSON) -> list[EndpointPair]:
    return [EndpointPair(method.upper(), path) for method, path in load_endpoint_pairs(catalog)]


def collect_mounted_api_endpoints() -> list[EndpointPair]:
    entries = [
        EndpointPair(entry.method.upper(), entry.path)
        for entry in inventory.collect_mounted_api_route_entries()
        if entry.path.startswith("/api/")
    ]
    entries.extend(collect_mounted_root_surface_endpoints())
    return entries


def collect_mounted_root_surface_endpoints() -> list[EndpointPair]:
    """Collect documented control routes mounted outside the nested /api router.

    The root web surface composes the TUI and hook routers separately. Keep the
    route scan tied to those mount points so a docs entry becomes stale when a
    router is no longer mounted, while newly declared routes in those mounted
    modules are discovered automatically.
    """

    repo_root = inventory.REPO_ROOT
    web_surface_path = repo_root / "src" / "server" / "web_surface.rs"
    web_surface = inventory.read_text(web_surface_path)
    state = rust_lex.StripState()
    active_web_surface = "\n".join(
        rust_lex.strip_line(line, state).ljust(len(line))
        for line in web_surface.splitlines()
    )
    source_paths = [web_surface_path]
    if re.search(r"\btui_relay::router\s*\(", active_web_surface):
        source_paths.append(
            repo_root / "src" / "services" / "claude_tui" / "tui_relay.rs"
        )
    if re.search(r"\bhook_receiver_router\s*\(", active_web_surface):
        source_paths.append(
            repo_root / "src" / "services" / "claude_tui" / "hook_server.rs"
        )

    function_paths = sorted(
        path
        for path in (repo_root / "src").rglob("*.rs")
        if path.is_file() and not inventory.is_test_file(path)
    )
    by_file, by_name = inventory.build_function_index(function_paths)

    entries: list[EndpointPair] = []
    for source_path in source_paths:
        for entry in inventory.parse_route_file(source_path, "", by_file, by_name):
            # / and /settings are browser UI pages, not API/control surfaces.
            if entry.path == "/ws" or entry.path.startswith(("/tui/", "/hooks/")):
                entries.append(EndpointPair(entry.method.upper(), entry.path))
    return entries


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
        docs = load_docs_endpoints()
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

    lines: list[str] = ["warning: API documentation catalog drift"]
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
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
