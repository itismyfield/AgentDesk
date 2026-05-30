"""Check: service-layer modules referencing server-layer modules.

Code under ``src/services/**`` should not depend on ``crate::server`` (or a
relative ``super::server`` sibling). Server routes may call services, but
services calling back into routes, DTOs, websocket emitters, or cluster helpers
keeps the layers coupled. Existing debt is tracked by a committed baseline; new
or increased references fail ``--check``.
"""

from __future__ import annotations

import json
import re
from collections import Counter
from pathlib import Path
from typing import Iterable

from .. import common
from ..common import Finding, line_of, read_text, rel_posix, strip_rust_comments
from . import CheckSpec

BASELINE_REL_PATH = "scripts/audit_maintainability/baselines/service_server_backflow.json"
SERVER_BACKFLOW = re.compile(r"\b(?P<root>crate|super)\s*::\s*server\b")


def _is_service_file(rel: str) -> bool:
    return rel.startswith("src/services/") and rel.endswith(".rs")


def _run(allowlist: set[str]) -> Iterable[Finding]:
    del allowlist
    from ..common import production_rust_files

    findings: list[Finding] = []
    for path in production_rust_files():
        rel = rel_posix(path)
        if not _is_service_file(rel):
            continue
        text = strip_rust_comments(read_text(path))
        for match in SERVER_BACKFLOW.finditer(text):
            root = match.group("root")
            reference = f"{root}::server"
            findings.append(
                Finding(
                    rule="service_server_backflow",
                    severity="warn",
                    file=rel,
                    line=line_of(text, match.start()),
                    message=(
                        "service-layer module references server-layer path "
                        f"`{reference}`; move shared types/helpers below "
                        "services or pass server dependencies in from routes"
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

    current_counts = Counter(finding.file for finding in findings)
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
                    "service/server backflow finding count increased from "
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
                    "service/server backflow finding count for file increased "
                    f"from baseline {baseline} to {current}"
                ),
                extra={"baseline": str(baseline), "current": str(current)},
            )
        )

    return regressions


CHECK = CheckSpec(
    key="service_server_backflow",
    title="Service/server backflow",
    description=(
        "Files under src/services/ that reference crate::server or "
        "super::server server-layer modules."
    ),
    hard_gate=False,
    runner=_run,
    baseline_gate=_baseline_gate,
)
