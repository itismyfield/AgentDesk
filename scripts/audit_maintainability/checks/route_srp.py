"""Check: route files mixing SQL + json! + domain calls (SRP violation).

A route handler should delegate persistence to ``db::*`` helpers and domain
logic to a service. When a single file has all three of:

* raw SQL strings (``sqlx::query`` / ``"SELECT "``-ish literals)
* ``json!(`` response shaping
* domain function calls (heuristic: ``crate::services::``)

it is flagged as a probable SRP violation worth refactoring.
"""

from __future__ import annotations

import re
from typing import Iterable

from ..common import Finding, read_text, rel_posix, strip_rust_comments
from . import CheckSpec

ROUTE_DIR_PARTS = ("server", "routes")

SQL_HINT = re.compile(
    r"sqlx::query|sqlx::query_as|\bquery!\(|\bquery_as!\(|"
    r"\"\s*(?:SELECT|INSERT|UPDATE|DELETE)\b",
    re.IGNORECASE,
)
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
        if not _is_route_file(rel) or rel in allowlist:
            continue
        text = strip_rust_comments(read_text(path))
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


CHECK = CheckSpec(
    key="route_srp_violations",
    title="Route SRP violations",
    description=(
        "Files under src/server/routes/ that mix raw SQL, json!() shaping, "
        "and crate::services::* calls in the same module."
    ),
    hard_gate=False,
    runner=_run,
)
