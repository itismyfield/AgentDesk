"""Check: giant Rust files missing from the change-surfaces map."""

from __future__ import annotations

import re
from typing import Iterable

from .. import common
from ..common import (
    Finding,
    is_allowlisted,
    line_of,
    production_rust_files,
    read_text,
    rel_posix,
)
from . import CheckSpec
from .namespace_size_caps import (
    count_lines,
    load_namespace_size_caps,
    matching_namespace_cap,
)

THRESHOLD = 1000
CHANGE_SURFACES_DOC = "docs/agent-maintenance/change-surfaces.md"

_BACKTICK_SPAN = re.compile(r"`([^`]+)`", re.DOTALL)
_EXPLICIT_RS_PATH = re.compile(r"src/[A-Za-z0-9_./-]+\.rs")


def _clean_doc_path(path: str) -> str:
    path = " ".join(path.split())
    path = path.split("::", 1)[0]
    path = path.split(":", 1)[0]
    path = re.sub(r"\s*\([^)]*\)", "", path).strip()
    return path


def documented_change_surface_paths() -> set[str]:
    """Rust file paths named in change-surfaces.md.

    The maintenance page is intentionally prose-heavy, so this accepts both
    explicit backtick paths and compact brace groups such as
    ``src/dispatch/{mod,dispatch_context}.rs``.
    """

    path = common.REPO_ROOT / CHANGE_SURFACES_DOC
    text = read_text(path)
    documented: set[str] = set()
    for span_match in _BACKTICK_SPAN.finditer(text):
        span = span_match.group(1)
        compact = " ".join(span.split())
        if not compact.startswith("src/"):
            continue
        if "{" in compact and "}" in compact:
            prefix, rest = compact.split("{", 1)
            body, suffix = rest.split("}", 1)
            suffix = suffix.strip()
            for item in body.split(","):
                item = re.sub(r"\s*\([^)]*\)", "", item).strip()
                if not item:
                    continue
                candidate = _clean_doc_path(f"{prefix}{item}{suffix}")
                if _EXPLICIT_RS_PATH.fullmatch(candidate):
                    documented.add(candidate)
            continue
        for match in _EXPLICIT_RS_PATH.finditer(compact):
            documented.add(_clean_doc_path(match.group(0)))
    return documented


def _run(allowlist: set[str]) -> Iterable[Finding]:
    findings: list[Finding] = []
    documented = documented_change_surface_paths()
    namespace_caps = load_namespace_size_caps()
    for path in production_rust_files():
        text = read_text(path)
        loc = count_lines(text)
        if loc < THRESHOLD:
            continue
        rel = rel_posix(path)
        if matching_namespace_cap(rel, namespace_caps) is not None:
            continue
        if rel in documented or is_allowlisted(allowlist, rel):
            continue
        findings.append(
            Finding(
                rule="giant_files",
                severity="warn",
                file=rel,
                line=None,
                message=(
                    f"{loc} LoC >= {THRESHOLD} threshold and is missing from "
                    f"{CHANGE_SURFACES_DOC}"
                ),
                extra={"loc": str(loc), "source_of_truth": CHANGE_SURFACES_DOC},
            )
        )
    findings.sort(key=lambda f: -(int(f.extra.get("loc", "0"))))
    return findings


# Re-export for harness discovery.
_ = line_of  # quiet unused-import warnings if line_of becomes used later

CHECK = CheckSpec(
    key="giant_files",
    title="Giant files",
    description=(
        f"Production Rust files in src/ with >= {THRESHOLD} lines that are not "
        f"listed in {CHANGE_SURFACES_DOC}."
    ),
    hard_gate=True,
    runner=_run,
)
