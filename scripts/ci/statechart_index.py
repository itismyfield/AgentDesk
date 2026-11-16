#!/usr/bin/env python3
"""Generate only the marked table in docs/statecharts/README.md.

Each index.d/<key>.md is exactly two LF-terminated lines: `key: <key>` and
one four-column Markdown row. Keys are lowercase hyphenated slugs; existing
numeric prefixes are permanent so migration preserves the original row order.
Links use inline [label](destination) syntax, relative to README.md, never to
index.d. Reference-style links are refused; local checks verify path existence,
not anchors or report contents. No network, TLC, Git or build is needed.
Exit 1 means invalid fragments, unresolved conflicts or drift; exit 2 means
missing, duplicate or damaged README markers.
"""

from __future__ import annotations

import argparse
import html
from pathlib import Path
import re
import sys
from urllib.parse import unquote, urlsplit


BEGIN = b"<!-- STATECHART INDEX:BEGIN -->\n"
END = b"<!-- STATECHART INDEX:END -->\n"
HEADER = b"| Area | Code SHA | Rules that break | Disposition |\n|---|---|---|---|\n"
KEY = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
CODE = re.compile(r"(`+).*?\1")
LINK = re.compile(r"\[[^\[\]\n]+\]\((<[^<>\n]+>|(?:\\.|[^()\\\n]|\([^()\n]*\))+)\)")
CONFLICT = re.compile(rb"^(?:<{7}|={7}|>{7}|\|{7})(?:[ \t].*)?\r?$", re.MULTILINE)


class IndexError(ValueError):
    """Invalid source data or an unresolved conflict."""


class MarkerError(ValueError):
    """The generated region cannot be identified unambiguously."""


def validate_row(row: str, source: Path, readme: Path) -> None:
    if not row.startswith("|") or not row.endswith("|"):
        raise IndexError(f"{source}: expected a four-column table row")
    # Escaped pipes are cell text even inside code spans; all other pipes delimit cells.
    boundaries = [m.end() - 1 for m in re.finditer(r"(\\*)\|", row) if len(m[1]) % 2 == 0]
    cells = [row[a + 1:b] for a, b in zip(boundaries, boundaries[1:])]
    if len(boundaries) != 5 or any(not cell.strip() for cell in cells):
        raise IndexError(f"{source}: expected four nonempty columns")
    text = CODE.sub("", row)
    for match in LINK.finditer(text):
        target = match[1]
        if target.startswith("<"):
            target = target[1:-1]
        elif any(char.isspace() for char in target):
            raise IndexError(f"{source}: use <destination> for links containing spaces")
        try:
            target = re.sub(r"\\([!\"#$%&'()*+,./:;<=>?@\[\\\]^_`{|}~-])", r"\1", html.unescape(target))
            url = urlsplit(target)
            if url.scheme in {"https", "http", "mailto"} or (url.netloc and not url.scheme):
                continue
            path = unquote(url.path)
            if url.scheme or path.startswith("/") or "\\" in path or "\x00" in path:
                raise ValueError("expected a README-relative path or web URL")
            if not (readme.parent / path).exists():
                raise ValueError("local path does not exist relative to README.md")
        except ValueError as error:
            raise IndexError(f"{source}: broken/unsupported link {target!r}: {error}") from error
    # Refuse unsupported link syntax instead of silently skipping a local destination.
    if re.search(r"(?<!\\)[\[\]]", LINK.sub("", text)):
        raise IndexError(f"{source}: malformed or reference-style link; use inline [label](destination)")


def render_table(root: Path) -> bytes:
    readme, directory = root / "README.md", root / "index.d"
    paths = sorted(directory.glob("*.md"))
    if not paths:
        raise IndexError(f"{directory}: no fragments")
    rows: dict[str, bytes] = {}
    sources: dict[str, Path] = {}
    for path in paths:
        data = path.read_bytes()
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
        validate_row(row, path, readme)
        rows[key], sources[key] = lines[1] + b"\n", path
    return HEADER + b"".join(rows[key] for key in sorted(rows))


def replace_index(original: bytes, table: bytes, source: Path) -> bytes:
    if (original.count(b"<!-- STATECHART INDEX:") != 2
            or original.count(BEGIN) != 1 or original.count(END) != 1):
        raise MarkerError(f"{source}: missing, duplicate or damaged STATECHART INDEX markers")
    begin, end = original.index(BEGIN), original.index(END)
    if begin >= end or (begin and original[begin - 1:begin] != b"\n") or original[end - 1:end] != b"\n":
        raise MarkerError(f"{source}: markers must be ordered standalone LF-terminated lines")
    if CONFLICT.search(original):
        raise IndexError(f"{source}: unresolved merge conflict (including manual regions)")
    return original[:begin + len(BEGIN)] + table + original[end:]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2] / "docs/statecharts",
                        help="directory containing README.md and index.d")
    parser.add_argument("--check", action="store_true", help="reject drift without writing any file")
    args = parser.parse_args(argv)
    readme = args.root / "README.md"
    try:
        original = readme.read_bytes()
        # Validate markers before fragments so marker damage always has exit status 2.
        replace_index(original, b"", readme)
        generated = replace_index(original, render_table(args.root), readme)
        if args.check and original != generated:
            raise IndexError(f"{readme}: index drift; run scripts/ci/statechart_index.py")
        if not args.check and original != generated:
            readme.write_bytes(generated)
    except MarkerError as error:
        print(error, file=sys.stderr)
        return 2
    except (IndexError, OSError) as error:
        print(error, file=sys.stderr)
        return 1
    print(f"{readme}: statechart index {'clean' if args.check else 'generated'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
