"""Check: production giants may not grow beyond their frozen baseline (#3028).

``giant_files`` requires every >= 1000 prod-LoC file to be documented in the
change-surfaces map, but it puts no ceiling on growth — so decomposed modules
silently re-inflated (``tmux_watcher.rs`` regrew 7160 -> 9608 production LoC).

This ratchet freezes each listed giant at a baseline production LoC
(``scripts/audit_maintainability_giant_baseline.toml``) and fails ``--check``
when a file exceeds it. Lower a baseline as a file shrinks; raising one is a
deliberate, reviewable admission that a giant grew (prefer splitting instead).

Production LoC (test code excluded) is shared with ``giant_files`` via
``giant_production_loc()`` so every giant surface agrees on the split (#3036).
"""

from __future__ import annotations

import re
import sys
from pathlib import Path
from typing import Iterable

from .. import common
from ..common import Finding
from . import CheckSpec
from .giant_files import giant_production_loc
from ratchet_admission import audit_repository_admissions

CONFIG_REL_PATH = "scripts/audit_maintainability_giant_baseline.toml"

_SECTION_RE = re.compile(r"^\[([A-Za-z0-9_.-]+)\]$")
_ENTRY_RE = re.compile(r'^"([^"]+)"\s*=\s*(\d+)$')


def load_giant_baseline(path: Path | None = None) -> dict[str, int]:
    """Load the ``[giant_file_ratchet]`` path -> frozen-LoC table.

    Uses the same tiny TOML subset as ``namespace_size_caps``: a single table of
    quoted repo-relative paths mapped to positive integer baselines.
    """

    config_path = path or (common.REPO_ROOT / CONFIG_REL_PATH)
    text = common.read_text(config_path)
    if not text:
        return {}

    section: str | None = None
    baseline: dict[str, int] = {}
    for raw_line in text.splitlines():
        line = raw_line.split("#", 1)[0].strip()
        if not line:
            continue
        section_match = _SECTION_RE.fullmatch(line)
        if section_match:
            section = section_match.group(1)
            continue
        if section != "giant_file_ratchet":
            continue
        entry_match = _ENTRY_RE.fullmatch(line)
        if entry_match:
            value = int(entry_match.group(2))
            if value > 0:
                baseline[entry_match.group(1).strip()] = value
    return baseline


def _run(allowlist: set[str]) -> Iterable[Finding]:
    baseline = load_giant_baseline()
    if not baseline:
        return []

    current = giant_production_loc()
    findings: list[Finding] = []
    admission_audit = audit_repository_admissions(
        repo_root=common.REPO_ROOT,
        ratchet="giant_file_ratchet",
        config_rel_path=CONFIG_REL_PATH,
        table_name="giant_file_ratchet",
    )
    for warning in admission_audit.warnings:
        print(warning, file=sys.stderr)
    for message in admission_audit.errors:
        findings.append(
            Finding(
                rule="giant_file_ratchet",
                severity="error",
                file=CONFIG_REL_PATH,
                line=None,
                message=message,
                extra={"history": "scripts/ratchet_admission_history.toml"},
            )
        )
    for rel, frozen in baseline.items():
        if common.is_allowlisted(allowlist, rel):
            continue
        loc = current.get(rel)
        # `loc is None` means the file dropped below the giant threshold, was
        # split, or was removed — all wins, never a regression.
        if loc is None or loc <= frozen:
            continue
        findings.append(
            Finding(
                rule="giant_file_ratchet",
                severity="warn",
                file=rel,
                line=None,
                message=(
                    f"{loc} production LoC > {frozen} frozen baseline "
                    f"(re-inflation); split the file or lower it back. "
                    f"Baseline: {CONFIG_REL_PATH}"
                ),
                extra={
                    "loc": str(loc),
                    "baseline": str(frozen),
                    "config": CONFIG_REL_PATH,
                },
            )
        )
    findings.sort(key=lambda f: -(int(f.extra.get("loc", "0"))))
    return findings


CHECK = CheckSpec(
    key="giant_file_ratchet",
    title="Giant file re-inflation ratchet",
    description=(
        "Production giants listed in "
        f"{CONFIG_REL_PATH} must not exceed their frozen production-LoC "
        "baseline."
    ),
    hard_gate=True,
    runner=_run,
)
