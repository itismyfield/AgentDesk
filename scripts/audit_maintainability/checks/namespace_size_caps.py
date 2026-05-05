"""Check: namespace-specific Rust file size caps."""

from __future__ import annotations

import fnmatch
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

from .. import common
from ..common import Finding
from . import CheckSpec

CONFIG_REL_PATH = "scripts/audit_maintainability_config.toml"

_SECTION_RE = re.compile(r"^\[([A-Za-z0-9_.-]+)\]$")
_CAP_RE = re.compile(r'^"([^"]+)"\s*=\s*(\d+)$')


@dataclass(frozen=True)
class NamespaceSizeCap:
    pattern: str
    max_lines: int


def default_config_path() -> Path:
    return common.REPO_ROOT / CONFIG_REL_PATH


def load_namespace_size_caps(path: Path | None = None) -> tuple[NamespaceSizeCap, ...]:
    """Load per-namespace line caps from the audit config.

    The parser intentionally supports only the tiny TOML subset this audit
    needs: a ``[namespace_size_caps]`` table with quoted path globs mapped to
    positive integer limits.
    """

    config_path = path or default_config_path()
    text = common.read_text(config_path)
    if not text:
        return ()

    section: str | None = None
    caps: list[NamespaceSizeCap] = []
    for raw_line in text.splitlines():
        line = raw_line.split("#", 1)[0].strip()
        if not line:
            continue
        section_match = _SECTION_RE.fullmatch(line)
        if section_match:
            section = section_match.group(1)
            continue
        if section != "namespace_size_caps":
            continue
        cap_match = _CAP_RE.fullmatch(line)
        if not cap_match:
            continue
        pattern = cap_match.group(1).strip()
        max_lines = int(cap_match.group(2))
        if pattern and max_lines > 0:
            caps.append(NamespaceSizeCap(pattern=pattern, max_lines=max_lines))
    return tuple(caps)


def matching_namespace_cap(
    rel_path: str, caps: Iterable[NamespaceSizeCap]
) -> NamespaceSizeCap | None:
    for cap in caps:
        if fnmatch.fnmatchcase(rel_path, cap.pattern):
            return cap
    return None


def count_lines(text: str) -> int:
    return text.count("\n") + (1 if text and not text.endswith("\n") else 0)


def _run(allowlist: set[str]) -> Iterable[Finding]:
    caps = load_namespace_size_caps()
    if not caps:
        return []

    findings: list[Finding] = []
    for path in common.production_rust_files():
        rel = common.rel_posix(path)
        cap = matching_namespace_cap(rel, caps)
        if cap is None or common.is_allowlisted(allowlist, rel):
            continue
        loc = count_lines(common.read_text(path))
        if loc <= cap.max_lines:
            continue
        findings.append(
            Finding(
                rule="namespace_size_caps",
                severity="warn",
                file=rel,
                line=None,
                message=f"{loc} LoC > {cap.max_lines} namespace cap for {cap.pattern}",
                extra={
                    "loc": str(loc),
                    "max_lines": str(cap.max_lines),
                    "namespace": cap.pattern,
                    "config": CONFIG_REL_PATH,
                },
            )
        )
    findings.sort(key=lambda f: (-int(f.extra.get("loc", "0")), f.file))
    return findings


CHECK = CheckSpec(
    key="namespace_size_caps",
    title="Namespace size caps",
    description=(
        "Production Rust files under configured namespaces must stay within "
        f"their per-namespace caps from {CONFIG_REL_PATH}."
    ),
    hard_gate=True,
    runner=_run,
)
