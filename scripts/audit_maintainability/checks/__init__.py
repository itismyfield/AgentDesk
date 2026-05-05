"""Per-rule audit check modules.

Each module exposes a ``CHECK`` :class:`CheckSpec` describing the rule and
``run(...)`` that returns a list of :class:`Finding`. The harness in
``scripts/audit_maintainability.py`` imports them lazily.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Callable, Iterable

from ..common import Finding

BaselineGate = Callable[[list[Finding]], Iterable[Finding]]


@dataclass(frozen=True)
class CheckSpec:
    """Static metadata for a maintainability check.

    Attributes:
        key: stable identifier used as YAML/JSON section key.
        title: human-readable title for the markdown report.
        description: 1-line description for the markdown report.
        hard_gate: when ``True`` a finding fails ``--check`` mode unless it is
            covered by the current baseline allowlist.
        baseline_gate: optional no-regression gate that compares this check's
            current findings against a committed baseline.
        runner: callable ``(allowlist) -> Iterable[Finding]``.
    """

    key: str
    title: str
    description: str
    hard_gate: bool
    runner: Callable[[set[str]], Iterable[Finding]]
    baseline_gate: BaselineGate | None = None
