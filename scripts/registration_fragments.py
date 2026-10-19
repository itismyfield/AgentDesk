#!/usr/bin/env python3
"""Shared reader for registration fragments: `<list>.d/<node>-<issue>-<slice>.txt`.

A registration list keeps its legacy file and gains one fragment file per lane slice, so lanes
stop inserting into the same lines. This module only enumerates a fragment directory, gives
every payload line its `file:line` provenance and rejects a key that two rows register. Each
consumer parses and validates every payload first, then indexes the rows here, and only then
turns them into its set or dict.
"""

from __future__ import annotations

import re
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path

GUIDE = "README.md"
# Lowercase only, so two names can never collide on a case-insensitive checkout.
NAME = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*-[0-9]+-[a-z0-9]+(?:_[a-z0-9]+)*\.txt")
NAME_HINT = "`<node>-<issue>-<slice>.txt` (lowercase; the slice takes `_`, not `-`)"


@dataclass(frozen=True)
class Line:
    """One payload line of a fragment: repo-relative `file:line` and the text without LF."""

    location: str
    text: str


@dataclass(frozen=True)
class Row:
    """A row its consumer already validated: the global key, the rest, and where it was read."""

    key: object
    payload: object
    location: str


def read_lines(directory: Path, label: str) -> tuple[list[Line], list[tuple[str, str]]]:
    """Every non-blank, non-`#` line of the fragments directly in `directory`, by name then line.

    Problems are `(location, detail)` pairs. Only the guide and regular files with a fragment
    name may sit there; any other entry is a problem, so no row can hide in a skipped file.
    A missing directory reads as empty."""
    if directory.is_symlink() or (directory.exists() and not directory.is_dir()):
        return [], [(label, "must be a real directory holding only fragment files")]
    if not directory.exists():
        return [], []
    lines: list[Line] = []
    problems: list[tuple[str, str]] = []
    for entry in sorted(directory.iterdir(), key=lambda path: path.name):
        rel = f"{label}/{entry.name}"
        if entry.is_symlink() or not entry.is_file():
            problems.append((rel, f"only regular files may sit in {label}"))
            continue
        if entry.name == GUIDE:
            continue
        if not NAME.fullmatch(entry.name):
            problems.append((rel, f"a fragment name must read {NAME_HINT}, or be {GUIDE}"))
            continue
        try:
            text = entry.read_bytes().decode("utf-8")
        except UnicodeDecodeError:
            problems.append((rel, "a fragment must be UTF-8"))
            continue
        if "\r" in text:
            problems.append((rel, "a fragment must use LF line endings"))
            continue
        if text and not text.endswith("\n"):
            problems.append((rel, "a fragment must end with a newline"))
        for number, raw in enumerate(text.split("\n"), 1):
            if raw.strip() and not raw.lstrip().startswith("#"):
                lines.append(Line(f"{rel}:{number}", raw))
    return lines, problems


def index(rows: Iterable[Row]) -> tuple[dict[object, Row], list[tuple[str, str]]]:
    """Key each row once. A second identical row is a duplicate and a different payload under
    the same key a conflict; both name each `file:line`, and nothing is summed or overridden."""
    first: dict[object, Row] = {}
    problems: list[tuple[str, str]] = []
    for row in rows:
        seen = first.get(row.key)
        if seen is None:
            first[row.key] = row
        elif seen.payload == row.payload:
            problems.append((row.location, f"duplicate of {seen.location} for `{row.key}`"))
        else:
            problems.append((row.location, (
                f"conflicts with {seen.location} for `{row.key}` "
                f"({seen.payload!r} there, {row.payload!r} here)"
            )))
    return first, problems
