#!/usr/bin/env python3
"""Reject message_outbox producers that bypass the validated service boundary."""

from __future__ import annotations

import re
import sys
from pathlib import Path

INSERT = re.compile(r"INSERT\s+INTO\s+message_outbox\b", re.IGNORECASE)
CANONICAL = {
    Path("src/services/message_outbox.rs"),
    Path("src/services/message_outbox_circuit_authority.rs"),
    Path("src/services/message_outbox_recovery.rs"),
}
TEST_FIXTURE_PATH = Path("src/server/mod.rs")
TEST_ONLY_PATHS = {
    Path("src/services/message_outbox_circuit_authority_tests.rs"),
    Path("src/services/message_outbox_recovery_tests.rs"),
}
TEST_FIXTURE_COLUMNS = (
    "target, content, bot, source, status, retry_count, claimed_at, claim_owner"
)


def audit(root: Path) -> list[str]:
    findings: list[str] = []
    for path in sorted((root / "src").rglob("*.rs")):
        relative = path.relative_to(root)
        if relative in CANONICAL or relative in TEST_ONLY_PATHS:
            continue
        text = path.read_text(encoding="utf-8")
        matches = list(INSERT.finditer(text))
        if relative == TEST_FIXTURE_PATH:
            fixture_matches = [
                match
                for match in matches
                if TEST_FIXTURE_COLUMNS in text[match.start() : match.start() + 300]
                and "old_owner_completion_after_stale_reclaim_is_noop_pg"
                in text[max(0, match.start() - 2500) : match.start()]
            ]
            if len(fixture_matches) == 1:
                matches.remove(fixture_matches[0])
        for match in matches:
            line = text.count("\n", 0, match.start()) + 1
            findings.append(
                f"{relative}:{line}: raw INSERT INTO message_outbox must use "
                "services::message_outbox validated helpers"
            )
    return findings


def main() -> int:
    findings = audit(Path.cwd())
    if findings:
        print("message_outbox raw-insert audit failed:", file=sys.stderr)
        for finding in findings:
            print(f"  {finding}", file=sys.stderr)
        return 1
    print("message_outbox raw-insert audit passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
