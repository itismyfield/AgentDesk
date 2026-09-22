#!/usr/bin/env python3
"""Keep `runtime_store::fsync_parent_dir` the only path-based directory fsync.

Windows cannot open a directory with `File::open`, so an inline copy fails on
every call there. Lexical scan: a `File::open(` whose argument names a parent or
directory, or that is chained straight into `.sync_all()` / `.sync_data()`.
Only the `//` suffix of each line is ignored.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

DIRECTORY_OPEN = re.compile(
    r"File::open\(\s*&?\s*[^)]*\b\w*(?:parent|dir)\w*\b"
    r"|File::open\([^;]*\)\s*\??\s*\.sync_(?:all|data)\(",
    re.IGNORECASE,
)
CANONICAL = {Path("src/services/discord/runtime_store.rs"): 1}


def audit(root: Path) -> list[str]:
    findings: list[str] = []
    for path in sorted((root / "src").rglob("*.rs")):
        relative = path.relative_to(root)
        hits = [
            number
            for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1)
            if DIRECTORY_OPEN.search(line.split("//", 1)[0])
        ]
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
