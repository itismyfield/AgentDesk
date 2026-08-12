#!/usr/bin/env python3
"""Reject byte/scalar comparisons against Discord's message-limit constant.

Discord does not document which Unicode unit its 2000-character limit uses.
The relay's conservative policy is UTF-16 code units, implemented by
`discord_message_units` and `byte_index_at_discord_message_units`. A raw
`str::len()` measures UTF-8 bytes and `chars().count()` measures Unicode
scalars, so neither may be used to decide a `DISCORD_MSG_LIMIT` boundary.

This intentionally scans production Rust only. Tests may describe byte or
scalar fixtures, but production comparisons must use the shared helpers.
Independently named `*_BYTES` budgets are outside this rule because they are
not Discord message-limit checks.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
SOURCE_ROOT = REPO_ROOT / "src"
LIMIT = "DISCORD_MSG_LIMIT"
RAW_COUNT = re.compile(r"(?:\.len\(\)|\.chars\(\)\.count\(\)|\bchar_count\()")
LIMIT_NAME = re.compile(r"\b(?:super::|discord::)?DISCORD_MSG_LIMIT\b")
LET_LIMIT = re.compile(
    r"\blet\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*=.*\bDISCORD_MSG_LIMIT\b"
)


def production_lines(path: Path) -> list[tuple[int, str]]:
    """Return code lines with comments and `#[cfg(test)]` blocks omitted."""
    kept: list[tuple[int, str]] = []
    skip_depth: int | None = None
    waiting_for_test_body = False
    block_comment = False
    for number, original in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = original
        if "#[cfg(test)]" in line:
            waiting_for_test_body = True
            continue
        if waiting_for_test_body:
            if "{" in line:
                skip_depth = line.count("{") - line.count("}")
                waiting_for_test_body = False
            continue
        if skip_depth is not None:
            skip_depth += line.count("{") - line.count("}")
            if skip_depth <= 0:
                skip_depth = None
            continue

        if block_comment:
            if "*/" not in line:
                continue
            line = line.split("*/", 1)[1]
            block_comment = False
        if "/*" in line:
            before, after = line.split("/*", 1)
            if "*/" in after:
                line = before + after.split("*/", 1)[1]
            else:
                line = before
                block_comment = True
        line = line.split("//", 1)[0]
        kept.append((number, line))
    return kept


def scan(root: Path = REPO_ROOT) -> list[tuple[Path, int, str]]:
    violations: list[tuple[Path, int, str]] = []
    for path in (root / "src").rglob("*.rs"):
        # This repository also keeps Rust test-only modules in sibling files
        # rather than under an inline `#[cfg(test)] mod`; their fixture counts
        # are deliberately out of scope for this production-code gate.
        if path.name == "tests.rs" or path.name.endswith("_tests.rs"):
            continue
        aliases: set[str] = set()
        for number, line in production_lines(path):
            if match := LET_LIMIT.search(line):
                # A short `limit` alias is the common way to hide the direct
                # comparison. Wider names (e.g. an unrelated `max_bytes` in a
                # different function) are intentionally not carried across
                # functions; their declaration itself is still checked.
                if match.group("name") == "limit":
                    aliases.add("limit")
            mentions_limit = bool(LIMIT_NAME.search(line)) or any(
                re.search(rf"\b{re.escape(name)}\b", line) for name in aliases
            )
            if mentions_limit and RAW_COUNT.search(line):
                violations.append((path.relative_to(root), number, line.strip()))
    return violations


def main() -> int:
    violations = scan()
    if not violations:
        print("OK: Discord message-limit comparisons use shared UTF-16 helpers.")
        return 0
    print("FAIL: raw byte/scalar count used with DISCORD_MSG_LIMIT:", file=sys.stderr)
    for path, line, source in violations:
        print(f"  {path}:{line}: {source}", file=sys.stderr)
    print(
        "Use discord_message_units / byte_index_at_discord_message_units instead.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
