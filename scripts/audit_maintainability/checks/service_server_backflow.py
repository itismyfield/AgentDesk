"""Check: modules outside the assembly layer referencing server-layer modules.

Only the assembly modules (``server``, ``cli``, ``launch``, ``bootstrap``) may
depend on ``crate::server``. Services, engine, kanban, dispatch, and the crate
root call the canonical service paths instead of the route re-exports. Existing
debt is tracked by a committed baseline; new or increased references fail
``--check``. Inline ``#[cfg(test)]`` items and test files are not production.
The removed ``agentdesk_runtime_root`` alias is a zero rule over every Rust file.
"""

from __future__ import annotations

import json
import re
from collections import Counter
from pathlib import Path
from typing import Iterable

try:
    from rust_lex import StripState, strip_line
    from rust_production import _production_text
except ModuleNotFoundError:
    from scripts.rust_lex import StripState, strip_line
    from scripts.rust_production import _production_text

from .. import common
from ..common import Finding, line_of, rel_posix
from . import CheckSpec

BASELINE_REL_PATH = "scripts/audit_maintainability/baselines/service_server_backflow.json"
ASSEMBLY_MODULES = frozenset({"server", "cli", "launch", "bootstrap"})
_ROOT = r"(?<![\w$])(?P<root>\$crate|crate|super|self)\s*::\s*(?:super\s*::\s*)*"
# `crate::server`, `super::super::server`, `$crate::server` in macro bodies.
QUALIFIED = re.compile(_ROOT + r"(?P<server>server)\b")
# A bare `server::` path reaches the module through a crate-root glob import or
# a crate-root-relative path; no module outside `src/server` is named `server`.
UNQUALIFIED = re.compile(r"(?<![\w:$])(?P<server>server)\s*::")
# `use crate::{server, ..}` / `use super::{server as alias}` import the module.
GROUP_OPEN = re.compile(_ROOT + r"\{")
GROUP_MEMBER = re.compile(r"(?<![\w:$])(?P<server>server)\b")
PREFILTER = re.compile(r"\bserver\b")
# Callers use `crate::config::runtime_root()`; the CLI alias must not return.
REMOVED_ALIAS = re.compile(r"\bagentdesk_runtime_root\b")
BACKFLOW_RULE = "service_server_backflow"
ALIAS_RULE = "removed_runtime_root_alias"


def _is_assembly_file(rel: str) -> bool:
    parts = rel.split("/")
    if len(parts) < 2 or parts[0] != "src":
        return False
    top = parts[1][:-3] if parts[1].endswith(".rs") else parts[1]
    return top in ASSEMBLY_MODULES


def _group_member_offsets(text: str, open_end: int) -> Iterable[int]:
    """Offsets of `server` tokens at the first nesting level of a use group."""
    depth = 1
    top_level = []
    index = open_end
    while index < len(text) and depth:
        char = text[index]
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
        top_level.append(char if depth == 1 and char not in "{}" else " ")
        index += 1
    for match in GROUP_MEMBER.finditer("".join(top_level)):
        yield open_end + match.start("server")


def _server_reference_offsets(text: str) -> list[tuple[int, str]]:
    found: dict[int, str] = {}
    for match in QUALIFIED.finditer(text):
        found.setdefault(match.start("server"), f"{match.group('root')}::server")
    for match in UNQUALIFIED.finditer(text):
        found.setdefault(match.start("server"), "server")
    for match in GROUP_OPEN.finditer(text):
        for offset in _group_member_offsets(text, match.end()):
            found.setdefault(offset, f"{match.group('root')}::{{server}}")
    return sorted(found.items())


def _removed_alias_findings() -> list[Finding]:
    from ..common import all_rust_files

    findings: list[Finding] = []
    for path in all_rust_files():
        raw = path.read_text(encoding="utf-8")
        if not REMOVED_ALIAS.search(raw):
            continue
        state = StripState()
        for lineno, line in enumerate(raw.splitlines(), start=1):
            if REMOVED_ALIAS.search(strip_line(line, state)):
                findings.append(
                    Finding(
                        rule=ALIAS_RULE,
                        severity="error",
                        file=rel_posix(path),
                        line=lineno,
                        message=(
                            "removed alias `agentdesk_runtime_root` is referenced; "
                            "call `crate::config::runtime_root()` directly"
                        ),
                        extra={"reference": "agentdesk_runtime_root"},
                    )
                )
    return findings


def _run(allowlist: set[str]) -> Iterable[Finding]:
    del allowlist
    from ..common import production_rust_files

    findings: list[Finding] = _removed_alias_findings()
    for path in production_rust_files():
        rel = rel_posix(path)
        if not rel.endswith(".rs") or _is_assembly_file(rel):
            continue
        # Unreadable sources raise instead of reading as zero references.
        if not PREFILTER.search(path.read_text(encoding="utf-8")):
            continue
        text = _production_text(path)
        for offset, reference in _server_reference_offsets(text):
            findings.append(
                Finding(
                    rule=BACKFLOW_RULE,
                    severity="warn",
                    file=rel,
                    line=line_of(text, offset),
                    message=(
                        "module outside server/cli/launch/bootstrap references "
                        f"server-layer path `{reference}`; call the canonical "
                        "service path or pass server dependencies in from the "
                        "assembly layer"
                    ),
                    extra={"reference": reference},
                )
            )
    findings.sort(key=lambda f: (f.file, f.line or 0, f.extra.get("reference", "")))
    return findings


def _baseline_path() -> Path:
    return common.REPO_ROOT / BASELINE_REL_PATH


def _baseline_file_counts(path: Path | None = None) -> tuple[int, dict[str, int]]:
    path = path or _baseline_path()
    payload = json.loads(path.read_text(encoding="utf-8"))
    files_raw = payload.get("files", {})
    if not isinstance(files_raw, dict):
        raise ValueError("service/server backflow baseline must contain a files object")

    file_counts: dict[str, int] = {}
    for file, raw_value in files_raw.items():
        if isinstance(raw_value, dict):
            raw_count = raw_value.get("count", 0)
        else:
            raw_count = raw_value
        count = int(raw_count)
        if count < 0:
            raise ValueError(
                f"service/server backflow baseline count cannot be negative: {file}"
            )
        file_counts[str(file)] = count

    total_count = int(payload.get("total_count", sum(file_counts.values())))
    if total_count < 0:
        raise ValueError("service/server backflow baseline total_count cannot be negative")
    file_total = sum(file_counts.values())
    if total_count != file_total:
        raise ValueError(
            "service/server backflow baseline total_count must equal the sum "
            "of per-file counts"
        )
    return total_count, file_counts


def _baseline_gate(findings: list[Finding]) -> Iterable[Finding]:
    if not findings and not _baseline_path().is_file():
        return []
    try:
        baseline_total, baseline_files = _baseline_file_counts()
    except (OSError, json.JSONDecodeError, TypeError, ValueError) as error:
        return [
            Finding(
                rule="service_server_backflow",
                severity="error",
                file=BASELINE_REL_PATH,
                line=None,
                message=f"service/server backflow baseline is missing or invalid: {error}",
            )
        ]

    current_counts = Counter(
        finding.file for finding in findings if finding.rule == BACKFLOW_RULE
    )
    current_total = sum(current_counts.values())
    regressions: list[Finding] = []

    if current_total > baseline_total:
        regressions.append(
            Finding(
                rule="service_server_backflow",
                severity="error",
                file=BASELINE_REL_PATH,
                line=None,
                message=(
                    "server backflow finding count increased from "
                    f"baseline {baseline_total} to {current_total}"
                ),
                extra={"baseline": str(baseline_total), "current": str(current_total)},
            )
        )

    for file in sorted(current_counts):
        current = current_counts[file]
        baseline = baseline_files.get(file, 0)
        if current <= baseline:
            continue
        regressions.append(
            Finding(
                rule="service_server_backflow",
                severity="error",
                file=file,
                line=None,
                message=(
                    "server backflow finding count for file increased "
                    f"from baseline {baseline} to {current}"
                ),
                extra={"baseline": str(baseline), "current": str(current)},
            )
        )

    return regressions


CHECK = CheckSpec(
    key="service_server_backflow",
    title="Server-layer backflow",
    description=(
        "Production modules outside server/cli/launch/bootstrap that reference "
        "crate::server server-layer modules, and any use of the removed "
        "agentdesk_runtime_root alias."
    ),
    hard_gate=False,
    runner=_run,
    baseline_gate=_baseline_gate,
)
