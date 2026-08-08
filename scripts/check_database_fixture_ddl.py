#!/usr/bin/env python3
"""Pin direct CREATE/DROP DATABASE Rust string emissions to the shared helper.

The lexical scan covers runtime files below src/ and tests/. It recognizes
ordinary, byte, raw, and raw-byte Rust strings after removing nested comments.
It intentionally does not see non-Rust/generated/runtime-assembled SQL, strings
outside those directories, prefixed SQL, split literals, or escaped source
whitespace. Those limitations are review responsibilities, not claimed safety.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path
from typing import Iterator


ROOT = Path(__file__).resolve().parents[1]
CONTRACTS = {
    "CREATE": ["src/db/postgres.rs"],
    "DROP": ["src/db/postgres.rs"],
}


def rust_string_literals(source: str) -> Iterator[str]:
    index = 0
    while index < len(source):
        if source.startswith("//", index):
            newline = source.find("\n", index + 2)
            index = len(source) if newline < 0 else newline + 1
            continue
        if source.startswith("/*", index):
            index += 2
            depth = 1
            while index < len(source) and depth:
                if source.startswith("/*", index):
                    depth += 1
                    index += 2
                elif source.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
            continue

        raw_prefix = 2 if source.startswith("br", index) else 1 if source.startswith("r", index) else 0
        quote = index + raw_prefix
        while raw_prefix and source[quote : quote + 1] == "#":
            quote += 1
        if raw_prefix and source[quote : quote + 1] == '"':
            start = index
            hashes = source[index + raw_prefix : quote]
            closer = '"' + hashes
            index = quote + 1
            end = source.find(closer, index)
            index = len(source) if end < 0 else end + len(closer)
            yield source[start:index]
            continue

        prefix = 1 if source.startswith('b"', index) else 0
        if source[index + prefix : index + prefix + 1] == '"':
            start = index
            index += prefix + 1
            while index < len(source):
                if source[index] == "\\":
                    index = min(index + 2, len(source))
                elif source[index] == '"':
                    index += 1
                    break
                else:
                    index += 1
            yield source[start:index]
            continue

        char_prefix = 1 if source.startswith("b'", index) else 0
        if source[index + char_prefix : index + char_prefix + 1] == "'":
            quote = index + char_prefix
            closing = quote + (3 if source[quote + 1 : quote + 2] == "\\" else 2)
            if source[closing : closing + 1] == "'":
                index = closing + 1
                continue
        index += 1


def ddl_emissions(root: Path, keyword: str) -> list[str]:
    pattern = re.compile(
        rf'(?is)^(?:br#*|r#*|b)?"\s*{re.escape(keyword)}\s+DATABASE'
    )
    emissions: list[str] = []
    for directory in (root / "src", root / "tests"):
        for path in sorted(directory.rglob("*.rs")):
            source = path.read_text(encoding="utf-8")
            if "DATABASE" not in source.upper():
                continue
            relative = path.relative_to(root).as_posix()
            emissions.extend(
                relative for literal in rust_string_literals(source) if pattern.search(literal)
            )
    return emissions


def main() -> int:
    failed = False
    for keyword, expected in CONTRACTS.items():
        actual = ddl_emissions(ROOT, keyword)
        if actual != expected:
            print(
                f"ERROR: {keyword} DATABASE emission contract drifted: "
                f"expected={expected!r} actual={actual!r}",
                file=sys.stderr,
            )
            failed = True
    if failed:
        return 1
    print("database fixture DDL chokepoint check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
