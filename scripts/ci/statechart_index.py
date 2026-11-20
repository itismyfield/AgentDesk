#!/usr/bin/env python3
"""Validate or render the index from docs/statecharts/index.d.

Fragments are two LF-terminated lines, a key and a four-column Markdown row.
Local links are relative to docs/statecharts and checked for path existence.
"""

from __future__ import annotations

import argparse
from collections.abc import Callable, Iterable
import html
from pathlib import Path
import re
import sys
from urllib.parse import unquote, urlsplit


MARKER = b"<!-- STATECHART INDEX:"
BEGIN = MARKER + b"BEGIN -->\n"
END = MARKER + b"END -->\n"
RECOVERY = "python3 scripts/ci/statechart_index.py --output docs/statecharts/index.generated.md"
HEADER = b"| Area | Code SHA | Rules that break | Disposition |\n|---|---|---|---|\n"
KEY = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
CODE = re.compile(r"(`+).*?\1")
LINK = re.compile(r"\[[^\[\]\n]+\]\((<[^<>\n]+>|(?:\\.|[^()\\\n]|\([^()\n]*\))+)\)")
CONFLICT = re.compile(rb"^(?:<{7}|={7}|>{7}|\|{7})(?:[ \t].*)?\r?$", re.MULTILINE)


class IndexError(ValueError):
    """Invalid source data or an unresolved conflict."""


class MarkerError(ValueError):
    """The generated region cannot be identified unambiguously."""


def link_target(raw: str):
    if raw.startswith("<"):
        raw = raw[1:-1]
    elif any(char.isspace() for char in raw):
        raise ValueError("use <destination> for links containing spaces")
    decoded = re.sub(r"\\([!\"#$%&'()*+,./:;<=>?@\[\\\]^_`{|}~-])", r"\1", html.unescape(raw))
    url = urlsplit(decoded)
    if url.scheme in {"https", "http", "mailto"} or (url.netloc and not url.scheme):
        return None
    path = unquote(url.path)
    if url.scheme or path.startswith("/") or "\\" in path or "\x00" in path:
        raise ValueError("expected a statechart-relative path or web URL")
    return path, url.query, url.fragment


def validate_row(row: str, source: Path, exists: Callable[[str], bool]) -> None:
    if not row.startswith("|") or not row.endswith("|"):
        raise IndexError(f"{source}: expected a four-column table row")
    # Escaped pipes are cell text even inside code spans; all other pipes delimit cells.
    boundaries = [m.end() - 1 for m in re.finditer(r"(\\*)\|", row) if len(m[1]) % 2 == 0]
    cells = [row[a + 1:b] for a, b in zip(boundaries, boundaries[1:])]
    if len(boundaries) != 5 or any(not cell.strip() for cell in cells):
        raise IndexError(f"{source}: expected four nonempty columns")
    text = CODE.sub("", row)
    for match in LINK.finditer(text):
        try:
            target = link_target(match[1])
            if target is not None and not exists(target[0]):
                raise ValueError("local path does not exist relative to docs/statecharts")
        except ValueError as error:
            raise IndexError(f"{source}: broken/unsupported link {match[1]!r}: {error}") from error
    # Refuse unsupported link syntax instead of silently skipping a local destination.
    if re.search(r"(?<!\\)[\[\]]", LINK.sub("", text)):
        raise IndexError(f"{source}: malformed or reference-style link; use inline [label](destination)")


def read_rows(directory: Path, fragments: Iterable[tuple[Path, bytes]],
              exists: Callable[[str], bool]) -> dict[str, bytes]:
    rows: dict[str, bytes] = {}
    sources: dict[str, Path] = {}
    for path, data in fragments:
        lines = data.split(b"\n")
        if len(lines) != 3 or lines[-1] or b"\r" in data or not lines[1].strip():
            raise IndexError(f"{path}: expected key line and one nonempty row, both LF-terminated")
        try:
            key_line, row = (line.decode("utf-8") for line in lines[:2])
        except UnicodeError as error:
            raise IndexError(f"{path}: invalid UTF-8: {error}") from error
        if not key_line.startswith("key: ") or not KEY.fullmatch(key_line[5:]):
            raise IndexError(f"{path}: expected 'key: <lowercase-hyphenated-slug>'")
        key = key_line[5:]
        if key in rows:
            raise IndexError(f"{path}: duplicate key {key!r}, already in {sources[key]}")
        if path.stem != key:
            raise IndexError(f"{path}: filename key {path.stem!r} differs from body key {key!r}")
        validate_row(row, path, exists)
        rows[key], sources[key] = lines[1] + b"\n", path
    if not rows:
        raise IndexError(f"{directory}: no fragments")
    return rows


def render_table(rows: dict[str, bytes]) -> bytes:
    return HEADER + b"".join(rows[key] for key in sorted(rows))


def validate_readme(original: bytes, source: Path) -> None:
    if CONFLICT.search(original):
        raise IndexError(f"{source}: unresolved merge conflict")
    if MARKER in original:
        raise MarkerError(f"{source}: remove the retired STATECHART INDEX markers and table")


def legacy_table(original: bytes, source: Path, exists: Callable[[str], bool]) -> bytes:
    if CONFLICT.search(original):
        raise IndexError(f"{source}: unresolved merge conflict")
    if (original.count(MARKER) != 2
            or original.count(BEGIN) != 1 or original.count(END) != 1):
        raise MarkerError(f"{source}: missing, duplicate or damaged STATECHART INDEX markers")
    begin, end = original.index(BEGIN), original.index(END)
    left = begin + len(BEGIN)
    if left > end or (begin and original[begin - 1:begin] != b"\n") or original[end - 1:end] != b"\n":
        raise MarkerError(f"{source}: markers must be ordered standalone LF-terminated lines")
    table = original[left:end]
    if not table.startswith(HEADER) or not table[len(HEADER):].strip():
        raise IndexError(f"{source}: invalid legacy statechart table")
    for row in table[len(HEADER):].decode("utf-8").splitlines():
        validate_row(row, source, exists)
    return table



def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2] / "docs/statecharts")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="validate source and README without writing")
    mode.add_argument("--output", type=Path, help="write index.generated.md alongside README.md")
    mode.add_argument("--check-output", type=Path, help="validate already rendered index.generated.md")
    args = parser.parse_args(argv)
    readme, directory = args.root / "README.md", args.root / "index.d"
    try:
        validate_readme(readme.read_bytes(), readme)
        rows = read_rows(directory, ((path, path.read_bytes()) for path in sorted(directory.glob("*.md"))),
                         lambda path: (args.root / path).exists())
        output = args.output or args.check_output
        if output and output.resolve() != args.root.resolve() / "index.generated.md":
            raise IndexError(f"{output}: expected index.generated.md alongside README.md")
        table = render_table(rows)
        if args.output:
            output.write_bytes(table)
        elif args.check_output:
            if not output.is_file() or output.read_bytes() != table:
                raise IndexError(f"{output}: missing or stale index output (drift)")
        elif not args.check:
            sys.stdout.buffer.write(table)
    except (MarkerError, IndexError, OSError) as error:
        print(f"{error}\nReproduce: {RECOVERY}", file=sys.stderr)
        return 2 if isinstance(error, MarkerError) else 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
