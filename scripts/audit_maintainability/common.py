"""Shared helpers for maintainability audit checks.

Stdlib-only. No third-party dependencies.
"""

from __future__ import annotations

import re
from hashlib import sha256
from dataclasses import dataclass, field
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
SRC_ROOT = REPO_ROOT / "src"
SCRIPTS_ROOT = REPO_ROOT / "scripts"

TEST_FILE_NAMES = {"integration_tests.rs", "tests.rs"}


@dataclass(frozen=True)
class Finding:
    """A single audit hit.

    ``severity`` is one of: ``info``, ``warn``, ``error``. Hard-gated checks
    fail ``scripts/audit_maintainability.py --check`` when a finding is not
    covered by the current baseline.
    """

    rule: str
    severity: str
    file: str
    line: int | None
    message: str
    extra: dict[str, str] = field(default_factory=dict)


def is_test_rs(path: Path) -> bool:
    name = path.name
    return name.endswith("_tests.rs") or name in TEST_FILE_NAMES


def production_rust_files() -> list[Path]:
    """All production Rust files under ``src/`` (tests excluded)."""

    if not SRC_ROOT.is_dir():
        return []
    return sorted(p for p in SRC_ROOT.rglob("*.rs") if p.is_file() and not is_test_rs(p))


def operational_shell_scripts() -> list[Path]:
    """Shell scripts under ``scripts/`` that may be run against live runtime data."""

    if not SCRIPTS_ROOT.is_dir():
        return []
    return sorted(p for p in SCRIPTS_ROOT.glob("*.sh") if p.is_file())


def all_rust_files() -> list[Path]:
    if not SRC_ROOT.is_dir():
        return []
    return sorted(p for p in SRC_ROOT.rglob("*.rs") if p.is_file())


def rel_posix(path: Path) -> str:
    try:
        return path.relative_to(REPO_ROOT).as_posix()
    except ValueError:
        return path.as_posix()


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return ""


def line_of(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def stable_finding_key(rule: str, file: str, context: str) -> str:
    """Return a line-independent allowlist key for a finding context."""

    normalized = " ".join(context.split())
    digest = sha256(normalized.encode("utf-8")).hexdigest()[:16]
    return f"{file}#{rule}:{digest}"


def is_allowlisted(
    allowlist: set[str],
    file: str,
    line: int | None = None,
    stable_key: str | None = None,
) -> bool:
    """Return true when a finding is covered by a path, line, or stable key."""

    if file in allowlist:
        return True
    if stable_key is not None and stable_key in allowlist:
        return True
    return line is not None and f"{file}:{line}" in allowlist


# Strip ``// ...`` line comments and ``/* ... */`` block comments. Cheap
# heuristic — does not understand strings/raw-strings — but adequate for the
# pattern checks used here.
_LINE_COMMENT = re.compile(r"//[^\n]*")
_BLOCK_COMMENT = re.compile(r"/\*.*?\*/", re.DOTALL)


def strip_rust_comments(text: str) -> str:
    text = _BLOCK_COMMENT.sub(lambda m: "\n" * m.group(0).count("\n"), text)
    text = _LINE_COMMENT.sub("", text)
    return text
