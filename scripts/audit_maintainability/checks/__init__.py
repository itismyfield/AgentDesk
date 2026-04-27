"""Per-rule audit check modules.

Each module exposes a ``CHECK`` :class:`CheckSpec` describing the rule and
``run(...)`` that returns a list of :class:`Finding`. The harness in
``scripts/audit_maintainability.py`` imports them lazily.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Callable, Iterable

from ..common import Finding


@dataclass(frozen=True)
class CheckSpec:
    """Static metadata for a maintainability check.

    Attributes:
        key: stable identifier used as YAML/JSON section key.
        title: human-readable title for the markdown report.
        description: 1-line description for the markdown report.
        hard_gate: when ``True`` a finding fails ``--check`` mode. All current
            checks default to ``False`` per the issue (#1282) — hard-gating
            is split into follow-up issues.
        runner: callable ``(allowlist) -> Iterable[Finding]``.
    """

    key: str
    title: str
    description: str
    hard_gate: bool
    runner: Callable[[set[str]], Iterable[Finding]]
