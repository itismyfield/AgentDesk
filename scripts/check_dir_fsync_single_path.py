#!/usr/bin/env python3
"""Keep `runtime_store::fsync_parent_dir` the only path-based directory fsync.

Windows cannot open a directory with `File::open`, so an inline copy fails on
every call there. Lexical scan over `;`-terminated statements: a read-only
`File::open(..)` / `.open(..)` whose handle is synced in the same or the next
statement. Write-mode opens are files (a directory cannot be opened for write)
and `.join(..)` arguments name an entry inside a directory, so both are skipped;
that includes `options.open(..)` when the same function set `options.write(true)`
in an earlier statement. Neither the open nor its sync is followed past a `fn`
header. Only the `//` suffix of each line is ignored.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

OPEN = re.compile(r"(?:File::open|\.open)\(")
WRITE_MODE = re.compile(r"\.(?:write|append|create|create_new|truncate)\(\s*true\s*\)")
SYNC = re.compile(r"\w*sync\w*\(")
WRITE_BUILDER = re.compile(
    r"\b(\w+)\s*(?:\.\s*\w+\([^()]*\)\s*)*"
    r"\.\s*(?:write|append|create|create_new|truncate)\(\s*true\s*\)"
)
RECEIVER = re.compile(r"(\w+)\s*$")
FN_HEADER = re.compile(r"\bfn\s+\w+")
CANONICAL = {Path("src/services/discord/runtime_store.rs"): 1}


def open_argument(text: str, start: int) -> str:
    depth = 1
    for index in range(start, len(text)):
        depth += {"(": 1, ")": -1}.get(text[index], 0)
        if depth == 0:
            return text[start:index]
    return text[start:]


def directory_opens(statement: str, write_builders: set[str]) -> list[int]:
    if WRITE_MODE.search(statement):
        return []
    opens = []
    for match in OPEN.finditer(statement):
        if ".join(" in open_argument(statement, match.end()):
            continue
        receiver = RECEIVER.search(statement, 0, match.start())
        if match.group().startswith(".") and receiver and receiver.group(1) in write_builders:
            continue
        opens.append(match.start())
    return opens


def within_fn(text: str) -> str:
    """Cut `text` at the next `fn` header: a later function is not this handle's sync."""
    header = FN_HEADER.search(text)
    return text[: header.start()] if header else text


def directory_syncs(text: str) -> list[int]:
    code = "\n".join(line.split("//", 1)[0] for line in text.splitlines())
    statements, offset = [], 0
    for statement in code.split(";"):
        statements.append((offset, statement))
        offset += len(statement) + 1
    lines: list[int] = []
    write_builders: set[str] = set()
    for index, (start, statement) in enumerate(statements):
        header = None
        for header in FN_HEADER.finditer(statement):
            pass
        # Builders are per function: opens before the last header keep the old set.
        split = header.start() if header else len(statement)
        opens = [o for o in directory_opens(statement, write_builders) if o < split]
        if header:
            write_builders.clear()
            opens += [o for o in directory_opens(statement, set()) if o >= split]
        body = statement[split:] if header else statement
        following = statements[index + 1][1] if index + 1 < len(statements) else ""
        for opened in opens:
            rest = statement[opened:]
            same = within_fn(rest)
            if SYNC.search(same) or (same == rest and SYNC.search(within_fn(following))):
                lines.append(code.count("\n", 0, start + opened) + 1)
        write_builders.update(match.group(1) for match in WRITE_BUILDER.finditer(body))
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
