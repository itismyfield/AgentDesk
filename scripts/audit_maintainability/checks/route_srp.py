"""Check: route files mixing SQL + json! + domain calls (SRP violation).

A route handler should delegate persistence to ``db::*`` helpers and domain
logic to a service. When a single file has all three of:

* raw SQL strings (``sqlx::query`` / ``"SELECT "``-ish literals)
* ``json!(`` response shaping
* domain function calls (heuristic: ``crate::services::``)

it is flagged as a probable SRP violation worth refactoring.
"""

from __future__ import annotations

import json
import re
from collections import Counter
from pathlib import Path
from typing import Iterable

from .. import common
from ..common import Finding, read_text, rel_posix, strip_rust_comments
from . import CheckSpec

ROUTE_DIR_PARTS = ("server", "routes")
AUTO_QUEUE_ROUTE_FILE = "src/server/routes/auto_queue.rs"
BASELINE_REL_PATH = "scripts/audit_maintainability/baselines/route_srp.json"

SQL_HINT = re.compile(
    r"sqlx::query|sqlx::query_as|\bquery!\(|\bquery_as!\(|"
    r"\"\s*(?:SELECT|INSERT|UPDATE|DELETE)\b",
    re.IGNORECASE,
)
MUTATION_SQL_HINT = re.compile(r"\b(?:INSERT|UPDATE|DELETE)\b", re.IGNORECASE)
JSON_HINT = re.compile(r"\bjson!\s*\(")
DOMAIN_HINT = re.compile(r"crate::services::[A-Za-z0-9_]+")


def _is_route_file(rel: str) -> bool:
    parts = rel.split("/")
    return all(p in parts for p in ROUTE_DIR_PARTS)


def _run(allowlist: set[str]) -> Iterable[Finding]:
    from ..common import production_rust_files

    findings: list[Finding] = []
    for path in production_rust_files():
        rel = rel_posix(path)
        if not _is_route_file(rel):
            continue
        text = strip_rust_comments(read_text(path))
        if rel == AUTO_QUEUE_ROUTE_FILE:
            mutations = len(MUTATION_SQL_HINT.findall(text))
            if mutations > 0:
                findings.append(
                    Finding(
                        rule="route_srp_violations",
                        severity="error",
                        file=rel,
                        line=None,
                        message=(
                            "auto_queue route must stay HTTP-only; move direct "
                            "INSERT/UPDATE/DELETE SQL into services::auto_queue"
                        ),
                        extra={"mutation_sql": str(mutations)},
                    )
                )
        if rel in allowlist:
            continue
        sql = len(SQL_HINT.findall(text))
        js = len(JSON_HINT.findall(text))
        dom = len(DOMAIN_HINT.findall(text))
        if sql > 0 and js > 0 and dom > 0:
            findings.append(
                Finding(
                    rule="route_srp_violations",
                    severity="warn",
                    file=rel,
                    line=None,
                    message=(
                        f"route file mixes SQL ({sql}), json!() ({js}), "
                        f"and crate::services calls ({dom})"
                    ),
                    extra={"sql": str(sql), "json": str(js), "domain": str(dom)},
                )
            )
    findings.sort(
        key=lambda f: -(
            int(f.extra.get("sql", "0"))
            + int(f.extra.get("json", "0"))
            + int(f.extra.get("domain", "0"))
        )
    )
    return findings


def _baseline_path() -> Path:
    return common.REPO_ROOT / BASELINE_REL_PATH


def _baseline_file_counts(path: Path | None = None) -> tuple[int, dict[str, int]]:
    path = path or _baseline_path()
    payload = json.loads(path.read_text(encoding="utf-8"))
    files_raw = payload.get("files", {})
    if not isinstance(files_raw, dict):
        raise ValueError("route SRP baseline must contain a files object")

    file_counts: dict[str, int] = {}
    for file, raw_value in files_raw.items():
        if isinstance(raw_value, dict):
            raw_count = raw_value.get("count", 0)
        else:
            raw_count = raw_value
        count = int(raw_count)
        if count < 0:
            raise ValueError(f"route SRP baseline count cannot be negative: {file}")
        file_counts[str(file)] = count

    total_count = int(payload.get("total_count", sum(file_counts.values())))
    if total_count < 0:
        raise ValueError("route SRP baseline total_count cannot be negative")
    file_total = sum(file_counts.values())
    if total_count != file_total:
        raise ValueError(
            "route SRP baseline total_count must equal the sum of per-file counts"
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
                rule="route_srp_violations",
                severity="error",
                file=BASELINE_REL_PATH,
                line=None,
                message=f"route SRP baseline is missing or invalid: {error}",
            )
        ]

    current_counts = Counter(finding.file for finding in findings)
    current_total = sum(current_counts.values())
    regressions: list[Finding] = []

    if current_total > baseline_total:
        regressions.append(
            Finding(
                rule="route_srp_violations",
                severity="error",
                file=BASELINE_REL_PATH,
                line=None,
                message=(
                    f"route SRP finding count increased from baseline "
                    f"{baseline_total} to {current_total}"
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
                rule="route_srp_violations",
                severity="error",
                file=file,
                line=None,
                message=(
                    "route SRP finding count for file increased from "
                    f"baseline {baseline} to {current}"
                ),
                extra={"baseline": str(baseline), "current": str(current)},
            )
        )

    return regressions


CHECK = CheckSpec(
    key="route_srp_violations",
    title="Route SRP violations",
    description=(
        "Files under src/server/routes/ that mix raw SQL, json!() shaping, "
        "and crate::services::* calls in the same module."
    ),
    hard_gate=False,
    runner=_run,
    baseline_gate=_baseline_gate,
)
