"""Block production giant growth against the comparison SHA, without slack.

Git exports enforce frozen caps; a Git candidate may retain inherited overage
only while keeping or reducing the comparison tree's measured production LoC.
"""

from __future__ import annotations

import re
import sys
import tempfile
from pathlib import Path
from typing import Iterable

from .. import common
from ..common import Finding
from . import CheckSpec
from .giant_files import giant_production_loc
from ratchet_admission import WIRING_SLACK_LINES, audit_repository_admissions

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
    current = giant_production_loc()
    if not baseline and not current:
        return []
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
    prior: dict[str, int] = {}
    if admission_audit.base_sha is not None:
        import giant_file_progress as progress
        from .giant_files import _INVENTORY

        with tempfile.TemporaryDirectory(prefix="giant-audit-base-") as tmp:
            old_root = progress.ROOT
            try:
                progress.ROOT = common.REPO_ROOT
                progress.archive(admission_audit.base_sha, Path(tmp))
            finally:
                progress.ROOT = old_root
            prior = _INVENTORY.giant_file_snapshot(Path(tmp))["modules"]
    for rel, loc in current.items():
        if common.is_allowlisted(allowlist, rel):
            continue
        frozen = baseline.get(rel)
        ceiling = prior.get(rel) if admission_audit.base_sha is not None else frozen
        if ceiling is not None and ceiling >= 1000 and loc <= ceiling:
            continue
        findings.append(
            Finding(
                rule="giant_file_ratchet",
                severity="error",
                file=rel,
                line=None,
                message=(
                    f"new giant at {loc} production LoC; split below 1000"
                    if ceiling is None or ceiling < 1000 else
                    f"grew to {loc} production LoC > baseline {ceiling} + slack "
                    f"{WIRING_SLACK_LINES} (re-inflation); split or shrink it back"
                ),
                extra={"loc": str(loc), "baseline": str(ceiling),
                       "slack": str(WIRING_SLACK_LINES), "config": CONFIG_REL_PATH},
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
        "baseline SHA measurement; Git exports enforce the frozen cap."
    ),
    hard_gate=True,
    runner=_run,
)
