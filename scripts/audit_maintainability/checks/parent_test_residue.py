"""Check: decomposition parents left as "test graveyards" (#4267).

When a giant file is decomposed its *production* code moves out into new
children, but its inline ``#[cfg(test)]`` blocks frequently stay behind. The
parent then keeps almost all of its raw line count as stranded test code —
opening it still costs the same tokens even though little production logic
remains, so the AI-readability win of the decomposition evaporates.

The canonical offender is ``src/services/discord/inflight.rs``: after its
production logic was carved into the ``inflight/`` children it carries only
590 production LoC but still holds 5451 lines of inline tests (ratio ~9.2x)
that never migrated with the code.

This check flags production files whose **test-to-prod line ratio exceeds
``RATIO_THRESHOLD`` (3x)**. It reuses the *exact* production/test line
accounting that ``giant_files`` already relies on — ``split_prod_test_lines``
from ``generate_inventory_docs`` plus the shared test-only-module filter — so
the prod/raw split agrees across every giant surface (#3036) rather than
introducing a second, divergent parser.

Definitions (all shared with ``giant_files`` / ``giant_file_ratchet``):

* ``prod`` LoC = raw lines minus whole ``#[cfg(test)] mod`` block lines.
* ``test`` LoC = the lines inside those whole test-gated ``mod`` blocks.
* ``raw`` LoC = ``prod + test`` = the file's total line count.
* ratio = ``test / prod``; a file is flagged when ``ratio > 3`` **and** it is
  genuinely heavy (``raw >= MIN_RAW_LOC``). Files with zero production LoC are
  skipped: they are pure test modules, not decomposition parents, and the
  ratio is undefined.

Landing posture — **report-only** (``hard_gate=False``) with a committed
no-regression ``baseline_gate``, the same shape as ``route_srp_violations`` and
``service_server_backflow``. Existing graveyards are frozen in the
``[parent_test_residue]`` table of ``scripts/audit_maintainability_config.toml``
(each value is the file's current stranded test-LoC ceiling) so CI is not red
on introduction. The gate fails ``--check`` only when a **new** graveyard
appears or a frozen file's test residue **grows** past its ceiling. Lower a
ceiling as tests migrate out; raising one admits more residue and is forbidden
(#4269) — migrate the tests with the production code instead.
"""

from __future__ import annotations

import re
from pathlib import Path
from typing import Iterable

from .. import common
from ..common import Finding
from . import CheckSpec
from . import giant_files

CONFIG_REL_PATH = "scripts/audit_maintainability_config.toml"

# A file is a "graveyard" only when its inline tests dwarf its production code.
RATIO_THRESHOLD = 3.0
# ...and only when the file is heavy enough that its opening cost actually
# hurts. Below this raw-LoC floor a high ratio is a small file, not a
# maintainability concern, so we do not flag it (keeps the signal focused on
# genuine decomposition parents). Half the 1000-LoC giant threshold.
MIN_RAW_LOC = 500

_SECTION_RE = re.compile(r"^\[([A-Za-z0-9_.-]+)\]$")
_ENTRY_RE = re.compile(r'^"([^"]+)"\s*=\s*(\d+)$')


def load_residue_baseline(path: Path | None = None) -> dict[str, int]:
    """Load the ``[parent_test_residue]`` path -> frozen test-LoC ceiling table.

    Uses the same tiny TOML subset as ``namespace_size_caps`` and
    ``giant_file_ratchet``: a single table of quoted repo-relative paths mapped
    to positive integer ceilings. Other sections in the shared config file
    (e.g. ``[namespace_size_caps]``) are ignored.
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
        if section != "parent_test_residue":
            continue
        entry_match = _ENTRY_RE.fullmatch(line)
        if entry_match:
            value = int(entry_match.group(2))
            if value > 0:
                baseline[entry_match.group(1).strip()] = value
    return baseline


def parent_test_residue() -> list[tuple[str, int, int, float]]:
    """Return ``(rel, prod, test, ratio)`` for every flagged graveyard file.

    Reuses ``giant_files``' shared accounting: the same test-only-module filter
    and the same ``split_prod_test_lines`` split used to compute production LoC
    for the giant-file threshold (#3036), so every surface agrees.
    """

    test_only = giant_files._test_only_module_files()
    rows: list[tuple[str, int, int, float]] = []
    for path in common.production_rust_files():
        if path in test_only:
            continue
        prod, test = giant_files._INVENTORY.split_prod_test_lines(common.read_text(path))
        raw = prod + test
        if prod <= 0 or raw < MIN_RAW_LOC:
            continue
        ratio = test / prod
        if ratio > RATIO_THRESHOLD:
            rows.append((common.rel_posix(path), prod, test, ratio))
    rows.sort(key=lambda row: -row[3])
    return rows


def _run(allowlist: set[str]) -> Iterable[Finding]:
    findings: list[Finding] = []
    for rel, prod, test, ratio in parent_test_residue():
        if common.is_allowlisted(allowlist, rel):
            continue
        raw = prod + test
        findings.append(
            Finding(
                rule="parent_test_residue",
                severity="warn",
                file=rel,
                line=None,
                message=(
                    f"{test} test LoC vs {prod} prod LoC "
                    f"(ratio {ratio:.2f}x > {RATIO_THRESHOLD:.0f}x, {raw} raw); "
                    f"migrate the stranded tests with the decomposed production code"
                ),
                extra={
                    "prod": str(prod),
                    "test": str(test),
                    "raw": str(raw),
                    "ratio": f"{ratio:.2f}",
                },
            )
        )
    return findings


def _baseline_gate(findings: list[Finding]) -> Iterable[Finding]:
    """No-regression gate: no new graveyards, and no frozen one may grow.

    ``findings`` is the current ``_run`` output. A finding is a regression when
    its file is absent from the ``[parent_test_residue]`` baseline (a brand-new
    graveyard) or when its current test LoC exceeds the frozen ceiling (the
    residue grew). Files at or below their ceiling — and baseline entries that
    dropped off the list because tests migrated out — are wins, never failures.
    """

    baseline = load_residue_baseline()
    regressions: list[Finding] = []
    for finding in findings:
        frozen = baseline.get(finding.file)
        current_test = int(finding.extra.get("test", "0"))
        if frozen is None:
            regressions.append(
                Finding(
                    rule="parent_test_residue",
                    severity="error",
                    file=finding.file,
                    line=None,
                    message=(
                        f"new parent test residue: {finding.extra.get('test')} test LoC "
                        f"vs {finding.extra.get('prod')} prod LoC "
                        f"(ratio {finding.extra.get('ratio')}x). Migrate the tests with "
                        f"the decomposed production code, or — if the residue is truly "
                        f"irreducible — seed a reviewed ceiling in {CONFIG_REL_PATH}"
                    ),
                    extra={"config": CONFIG_REL_PATH, **finding.extra},
                )
            )
            continue
        if current_test > frozen:
            regressions.append(
                Finding(
                    rule="parent_test_residue",
                    severity="error",
                    file=finding.file,
                    line=None,
                    message=(
                        f"{current_test} test LoC > {frozen} frozen residue ceiling "
                        f"(the graveyard grew); migrate tests out or lower the ceiling. "
                        f"Baseline: {CONFIG_REL_PATH}"
                    ),
                    extra={
                        "baseline": str(frozen),
                        "current": str(current_test),
                        "config": CONFIG_REL_PATH,
                    },
                )
            )
    return regressions


CHECK = CheckSpec(
    key="parent_test_residue",
    title="Parent test residue",
    description=(
        "Decomposition parents whose inline test LoC exceeds "
        f"{RATIO_THRESHOLD:.0f}x their production LoC (files >= {MIN_RAW_LOC} raw "
        "lines). Migrate stranded tests with the production code; frozen "
        f"offenders are baselined in {CONFIG_REL_PATH}."
    ),
    hard_gate=False,
    runner=_run,
    baseline_gate=_baseline_gate,
)
