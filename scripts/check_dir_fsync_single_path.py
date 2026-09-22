#!/usr/bin/env python3
"""Keep `runtime_store::fsync_parent_dir` the only path-based directory fsync.

Windows cannot open a directory with `File::open`, so an inline copy fails on
every call there. Lexical scan over `;`-terminated statements: a read-only
`File::open(..)` / `.open(..)` whose handle is synced in the same or the next
statement. Write-mode opens are files (a directory cannot be opened for write)
and `.join(..)` arguments name an entry inside a directory, so both are skipped.
Only the `//` suffix of each line is ignored.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

OPEN = re.compile(r"(?:File::open|\.open)\(")
WRITE_MODE = re.compile(r"\.(?:write|append|create|create_new|truncate)\(\s*true\s*\)")
SYNC = re.compile(r"\w*sync\w*\(")
CANONICAL = {Path("src/services/discord/runtime_store.rs"): 1}


def open_argument(text: str, start: int) -> str:
    depth = 1
    for index in range(start, len(text)):
        depth += {"(": 1, ")": -1}.get(text[index], 0)
        if depth == 0:
            return text[start:index]
    return text[start:]


def directory_opens(statement: str) -> list[int]:
    if WRITE_MODE.search(statement):
        return []
    return [
        match.start()
        for match in OPEN.finditer(statement)
        if ".join(" not in open_argument(statement, match.end())
    ]


def directory_syncs(text: str) -> list[int]:
    code = "\n".join(line.split("//", 1)[0] for line in text.splitlines())
    statements, offset = [], 0
    for statement in code.split(";"):
        statements.append((offset, statement))
        offset += len(statement) + 1
    lines = []
    for index, (start, statement) in enumerate(statements):
        following = statements[index + 1][1] if index + 1 < len(statements) else ""
        for opened in directory_opens(statement):
            if SYNC.search(statement, opened) or SYNC.search(following):
                lines.append(code.count("\n", 0, start + opened) + 1)
    return lines


def audit(root: Path) -> list[str]:
    findings: list[str] = []
    for path in sorted((root / "src").rglob("*.rs")):
        relative = path.relative_to(root)
        hits = directory_syncs(path.read_text(encoding="utf-8"))
        expected = CANONICAL.get(relative, 0)
        if len(hits) != expected:
            where = ", ".join(f"{relative}:{number}" for number in hits) or str(relative)
            findings.append(
                f"{where}: expected {expected} directory fsync opens, found {len(hits)}; "
                "call runtime_store::fsync_parent_dir instead"
            )
    for relative in CANONICAL:
        if not (root / relative).is_file():
            findings.append(f"{relative}: canonical fsync_parent_dir owner is missing")
    return findings


def main() -> int:
    findings = audit(Path.cwd())
    if findings:
        print("directory fsync single-path audit failed:", file=sys.stderr)
        for finding in findings:
            print(f"  {finding}", file=sys.stderr)
        return 1
    print("directory fsync single-path audit passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
