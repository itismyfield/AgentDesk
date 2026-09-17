#!/usr/bin/env python3
"""Decide mechanically whether a change touches only Rust comments.

Usage: ``check_comment_only_change.py <base-ref> [head-ref]``

For every changed ``.rs`` file the two sides are reduced to a normal form that
drops comments, collapses code whitespace to single spaces, and keeps string,
raw-string and char literal bytes verbatim. The verdict is byte equality of
those normal forms, so the answer does not move with line numbers, indentation
or where a comment sat on its line. Any other change -- one character of code,
one byte inside a literal, an added or deleted file -- fails with the first
diverging source line on each side.

Declared holes: whitespace outside literals is not compared, so a pure blank
line or indentation reflow also reads as comment-only (``cargo fmt --check``
covers that); CR/LF inside a multi-line literal is normalised to LF; and the
lexer is lexical, so it does not expand macros or read ``include!`` targets.

``binary_sha256`` equality is not a usable substitute: ``unwrap``/``expect``/
``panic!`` bake ``file:line`` into ``Location``, so deleting a comment line
always moves the binary.
"""

from __future__ import annotations

import argparse
import bisect
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from rust_lex import COMMENT, SPACE, StripState, lex_segments  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parent.parent
WORKING_TREE = "<working tree>"


class GitError(RuntimeError):
    pass


def git(*args: str, binary: bool = False) -> str | bytes:
    result = subprocess.run(
        ["git", "-C", str(REPO_ROOT), *args],
        check=False,
        capture_output=True,
    )
    if result.returncode != 0:
        raise GitError(
            f"git {' '.join(args)} failed: "
            f"{result.stderr.decode('utf-8', 'replace').strip()}"
        )
    return result.stdout if binary else result.stdout.decode("utf-8")


def normalize(text: str) -> tuple[str, list[int], list[int]]:
    """Return the comment-free normal form plus an offset -> source line map."""

    state = StripState()
    pieces: list[str] = []
    starts: list[int] = []
    lines: list[int] = []
    width = 0
    pending_space = False

    def emit(chunk: str, lineno: int) -> None:
        nonlocal width
        pieces.append(chunk)
        starts.append(width)
        lines.append(lineno)
        width += len(chunk)

    for lineno, line in enumerate(text.splitlines(), start=1):
        for kind, chunk in lex_segments(line, state):
            if kind in (COMMENT, SPACE):
                pending_space = True
                continue
            if pending_space and pieces:
                emit(" ", lineno)
            pending_space = False
            emit(chunk, lineno)
        if state.in_string or state.raw_hashes is not None:
            emit("\n", lineno)
            pending_space = False
        else:
            pending_space = True
    return "".join(pieces), starts, lines


def line_at(offset: int, starts: list[int], lines: list[int]) -> int:
    if not starts:
        return 0
    return lines[max(0, bisect.bisect_right(starts, offset) - 1)]


def source_line(text: str, lineno: int) -> str:
    rows = text.splitlines()
    if 1 <= lineno <= len(rows):
        return rows[lineno - 1].strip()
    return "<end of file>"


def first_divergence(base_text: str, head_text: str) -> tuple[int, int] | None:
    base_norm, base_starts, base_lines = normalize(base_text)
    head_norm, head_starts, head_lines = normalize(head_text)
    if base_norm == head_norm:
        return None
    limit = min(len(base_norm), len(head_norm))
    offset = limit
    for index in range(limit):
        if base_norm[index] != head_norm[index]:
            offset = index
            break
    return (
        line_at(offset, base_starts, base_lines),
        line_at(offset, head_starts, head_lines),
    )


def parse_name_status(payload: str) -> list[tuple[str, str, str]]:
    fields = [field for field in payload.split("\0") if field != ""]
    entries: list[tuple[str, str, str]] = []
    index = 0
    while index < len(fields):
        status = fields[index]
        index += 1
        if status[:1] in ("R", "C"):
            entries.append((status[:1], fields[index], fields[index + 1]))
            index += 2
        else:
            entries.append((status[:1], fields[index], fields[index]))
            index += 1
    return entries


def read_side(rev: str, path: str) -> str | None:
    if rev == WORKING_TREE:
        candidate = REPO_ROOT / path
        if not candidate.is_file():
            return None
        raw = candidate.read_bytes()
    else:
        try:
            raw = git("show", f"{rev}:{path}", binary=True)
        except GitError:
            return None
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError:
        return None


def resolve_base(base_ref: str, head_ref: str) -> str:
    head = "HEAD" if head_ref == WORKING_TREE else head_ref
    return git("merge-base", base_ref, head).strip()


def check(base_ref: str, head_ref: str, allow_non_rust: bool) -> int:
    base = resolve_base(base_ref, head_ref)
    diff_args = ["diff", "--name-status", "--find-renames", "-z", base]
    if head_ref != WORKING_TREE:
        diff_args.append(head_ref)
    entries = parse_name_status(git(*diff_args))

    rust = [entry for entry in entries if entry[2].endswith(".rs")]
    other = [entry for entry in entries if not entry[2].endswith(".rs")]
    failures: list[str] = []

    for status, old_path, new_path in rust:
        if status == "A":
            failures.append(f"{new_path}: added file (no base side to compare)")
            continue
        if status == "D":
            failures.append(f"{old_path}: deleted file (no head side to compare)")
            continue
        if status in ("R", "C"):
            failures.append(f"{old_path} -> {new_path}: renamed or copied, not a comment edit")
            continue
        base_text = read_side(base, old_path)
        head_text = read_side(head_ref, new_path)
        if base_text is None or head_text is None:
            failures.append(f"{new_path}: could not read both sides as UTF-8 text")
            continue
        divergence = first_divergence(base_text, head_text)
        if divergence is None:
            continue
        base_line, head_line = divergence
        failures.append(
            f"{new_path}: code differs after comment removal\n"
            f"    base {base}:{base_line}: {source_line(base_text, base_line)}\n"
            f"    head {head_ref}:{head_line}: {source_line(head_text, head_line)}"
        )

    if other and not allow_non_rust:
        for status, _old, new_path in other:
            failures.append(f"{new_path}: non-Rust change ({status}); pass --allow-non-rust to ignore")

    print(f"base: {base_ref} -> {base}")
    print(f"head: {head_ref}")
    print(f"changed Rust files: {len(rust)}")
    if failures:
        print("")
        print(f"NOT a comment-only change ({len(failures)} finding(s)):")
        for failure in failures:
            print(f"  {failure}")
        return 1
    print("OK: every changed Rust file is byte-identical once comments are removed")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("base_ref", help="base revision (merge-base is computed against it)")
    parser.add_argument("head_ref", nargs="?", default=WORKING_TREE, help="head revision (default: working tree)")
    parser.add_argument("--allow-non-rust", action="store_true", help="do not fail on changed non-Rust files")
    args = parser.parse_args(argv)
    try:
        return check(args.base_ref, args.head_ref, args.allow_non_rust)
    except GitError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
