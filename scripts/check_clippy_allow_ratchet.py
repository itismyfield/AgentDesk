#!/usr/bin/env python3
"""Enforce the checked-in occurrence baseline for four structural Clippy lints.

Body extraction runs in two stages:

1. ``neutralize_source`` performs one complete left-to-right lexical pre-pass
   over the Rust source, replacing the *interior* of every lexical construct
   (line/block comments; the complete Rust 2024 string-literal prefix grammar
   -- plain, ``b``, ``r``, ``br``, ``c``, ``cr`` in quoted and hash-counted raw
   forms; char and byte-char literals) with harmless filler so their contents
   can never be mistaken for attribute structure. Rust block comments nest and
   are handled with a depth counter; lifetimes (``'a``) are distinguished from
   char literals (``'a'``).

2. The bracket/paren balance scan then runs on the *cleaned* text, where no
   quote, comment, or literal delimiter survives to perturb it.

Any lexically unterminated construct, or any unbalanced attribute/argument
region, sets ``ambiguous`` and the affected file FAILS CLOSED: it counts as if
it contained every governed lint (a saturating sentinel), so a bypass attempt
inflates the count and trips the ratchet instead of silently dropping to zero.
"""

from __future__ import annotations

import argparse
import json
import re
from collections import Counter
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SOURCE_ROOT = REPO_ROOT / "src"
BASELINE = REPO_ROOT / "scripts" / "clippy_allow_occurrences.json"
LINTS = (
    "large_enum_variant",
    "result_large_err",
    "too_many_arguments",
    "type_complexity",
)
ATTRIBUTE_START_RE = re.compile(r"#!?\s*\[")
IDENTIFIER_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
LINT_RE = re.compile(r"\bclippy::(?P<lint>[a-zA-Z0-9_]+)\b")
# A lexically ambiguous file cannot be scanned safely, so it fails closed with a
# saturating count that exceeds any realistic baseline for every governed lint.
AMBIGUOUS_SENTINEL = 1_000_000
# Treat every Clippy lint group as suppressing all governed lints. This is
# intentionally conservative: group membership changes across Clippy releases,
# and a broad allow must never bypass this occurrence ratchet.
CLIPPY_GROUPS = frozenset(
    {
        "all",
        "cargo",
        "complexity",
        "correctness",
        "nursery",
        "pedantic",
        "perf",
        "restriction",
        "style",
        "suspicious",
    }
)

_IDENT_START = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_")
_HEX = set("0123456789abcdefABCDEF")


def _char_literal_end(text: str, pos: int) -> int | None:
    """Return the index just past a char literal starting at ``pos`` (a ``'``).

    Returns ``None`` when the quote begins a lifetime/label (``'a``) rather than
    a char literal, so callers leave the lone quote in place (it never perturbs
    bracket state). Handles escapes ``\\n``, ``\\'``, ``\\\\``, ``\\xHH`` and
    ``\\u{...}`` and single-char literals such as ``']'``, ``')'`` and ``'"'``.
    """
    n = len(text)
    j = pos + 1
    if j >= n:
        return None
    if text[j] == "\\":
        j += 1
        if j >= n:
            return None
        kind = text[j]
        if kind == "x":
            j += 3  # 'x' plus two hex digits
        elif kind == "u":
            j += 1
            if j < n and text[j] == "{":
                while j < n and text[j] != "}":
                    j += 1
                j += 1  # consume closing '}'
        else:
            j += 1  # single-char escape (\n, \t, \', \\, \0, ...)
        if j < n and text[j] == "'":
            return j + 1
        return None
    # Non-escaped: a char literal is exactly one char followed by a closing quote.
    if pos + 2 < n and text[pos + 2] == "'":
        return pos + 3
    return None


def _raw_string_end(text: str, pos: int) -> tuple[int | None, bool]:
    """Match a raw string ``[bc]?r#*"..."#*`` starting at ``pos``.

    Covers every raw string prefix in the complete Rust 2024 grammar: ``r``
    (raw), ``br`` (raw byte), and ``cr`` (raw C string). Returns
    ``(end_index, ambiguous)``. ``end_index`` is ``None`` when no raw string
    begins here. ``ambiguous`` is True when a raw string opens but never
    terminates before EOF.
    """
    n = len(text)
    i = pos
    if i < n and text[i] in ("b", "c"):
        i += 1
    if i >= n or text[i] != "r":
        return None, False
    i += 1
    hashes = 0
    while i < n and text[i] == "#":
        hashes += 1
        i += 1
    if i >= n or text[i] != '"':
        return None, False  # not a raw string (e.g. bare identifier `r` / `br`)
    i += 1  # past opening quote
    terminator = '"' + "#" * hashes
    end = text.find(terminator, i)
    if end == -1:
        return n, True  # unterminated raw string -> fail closed
    return end + len(terminator), False


def neutralize_source(text: str) -> tuple[str, bool]:
    """Replace every Rust lexical construct's interior with filler in one pass.

    Returns ``(cleaned_text, ambiguous)``. ``cleaned_text`` has identical length
    to ``text`` (newlines preserved, other neutralized chars become spaces) so
    that real attribute structure keeps its positions while comment/string/char
    contents can no longer be mistaken for brackets, parens or quotes.
    ``ambiguous`` is True if any construct is left unterminated at EOF.
    """
    n = len(text)
    out: list[str] = []
    ambiguous = False
    i = 0

    def blank(start: int, end: int) -> None:
        for k in range(start, end):
            out.append("\n" if text[k] == "\n" else " ")

    while i < n:
        c = text[i]

        # Line comment: // ... (until newline, newline left intact)
        if c == "/" and i + 1 < n and text[i + 1] == "/":
            j = i + 2
            while j < n and text[j] != "\n":
                j += 1
            blank(i, j)
            i = j
            continue

        # Block comment: /* ... */ with nesting.
        if c == "/" and i + 1 < n and text[i + 1] == "*":
            depth = 1
            j = i + 2
            while j < n and depth:
                if text[j] == "/" and j + 1 < n and text[j + 1] == "*":
                    depth += 1
                    j += 2
                elif text[j] == "*" and j + 1 < n and text[j + 1] == "/":
                    depth -= 1
                    j += 2
                else:
                    j += 1
            if depth:
                ambiguous = True  # unterminated block comment
            blank(i, j)
            i = j
            continue

        # Raw / raw byte string: b?r#*"..."#* (no escapes inside).
        raw_end, raw_ambiguous = _raw_string_end(text, i)
        if raw_end is not None:
            if raw_ambiguous:
                ambiguous = True
            blank(i, raw_end)
            i = raw_end
            continue

        # Byte string: b"..." (with escapes).
        if c == "b" and i + 1 < n and text[i + 1] == '"':
            j = i + 2
            terminated = False
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"':
                    j += 1
                    terminated = True
                    break
                j += 1
            if not terminated:
                ambiguous = True
            blank(i, j)
            i = j
            continue

        # C string: c"..." (with escapes, Rust 2024).
        if c == "c" and i + 1 < n and text[i + 1] == '"':
            j = i + 2
            terminated = False
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"':
                    j += 1
                    terminated = True
                    break
                j += 1
            if not terminated:
                ambiguous = True
            blank(i, j)
            i = j
            continue

        # Plain string: "..." (with escapes).
        if c == '"':
            j = i + 1
            terminated = False
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"':
                    j += 1
                    terminated = True
                    break
                j += 1
            if not terminated:
                ambiguous = True
            blank(i, j)
            i = j
            continue

        # Byte char literal: b'x'.
        if c == "b" and i + 1 < n and text[i + 1] == "'":
            end = _char_literal_end(text, i + 1)
            if end is not None:
                blank(i, end)
                i = end
                continue

        # Char literal vs lifetime: 'x' is a literal, 'a is a lifetime.
        if c == "'":
            end = _char_literal_end(text, i)
            if end is not None:
                blank(i, end)
                i = end
                continue
            # Lifetime/label: emit the quote verbatim (harmless to bracket scan).
            out.append(c)
            i += 1
            continue

        # Plain identifier: consume as a unit so a trailing r/b/quote of the
        # NEXT token is only ever inspected at an identifier boundary.
        if c in _IDENT_START:
            m = IDENTIFIER_RE.match(text, i)
            assert m is not None
            out.append(m.group())
            i = m.end()
            continue

        out.append(c)
        i += 1

    return "".join(out), ambiguous


def rust_attributes(text: str) -> tuple[list[str], bool]:
    """Return balanced Rust attributes from *cleaned* text plus an ambiguity flag.

    ``text`` must already be neutralized. An attribute whose ``[`` never
    balances before EOF sets the ambiguity flag (fail closed) rather than being
    silently dropped.
    """
    attributes: list[str] = []
    ambiguous = False
    cursor = 0
    while match := ATTRIBUTE_START_RE.search(text, cursor):
        depth = 1
        index = match.end()
        while index < len(text) and depth:
            char = text[index]
            if char == "[":
                depth += 1
            elif char == "]":
                depth -= 1
            index += 1
        if depth:
            ambiguous = True  # unbalanced attribute brackets
            break
        attributes.append(text[match.start() : index])
        cursor = index
    return attributes, ambiguous


def suppression_bodies(attribute: str) -> tuple[list[str], bool]:
    """Extract balanced allow/expect argument lists from one *cleaned* attribute.

    Returns ``(bodies, ambiguous)``. An ``allow(``/``expect(`` whose parens never
    balance sets the ambiguity flag (fail closed) instead of dropping the body.
    """
    bodies: list[str] = []
    ambiguous = False
    index = 0
    while index < len(attribute):
        identifier = IDENTIFIER_RE.match(attribute, index)
        if identifier is None:
            index += 1
            continue
        name = identifier.group()
        index = identifier.end()
        if name not in {"allow", "expect"}:
            continue
        while index < len(attribute) and attribute[index].isspace():
            index += 1
        if index >= len(attribute) or attribute[index] != "(":
            continue
        body_start = index + 1
        depth = 1
        index += 1
        while index < len(attribute) and depth:
            char = attribute[index]
            if char == "(":
                depth += 1
            elif char == ")":
                depth -= 1
            index += 1
        if depth == 0:
            bodies.append(attribute[body_start : index - 1])
        else:
            ambiguous = True  # unbalanced allow/expect parentheses
    return bodies, ambiguous


def _count_all_governed(occurrences: Counter[tuple[str, str]], relative: str) -> None:
    """Fail closed: record a saturating count for every governed lint."""
    for lint in LINTS:
        occurrences[(relative, lint)] = AMBIGUOUS_SENTINEL


def collect_occurrences(source_root: Path = SOURCE_ROOT) -> Counter[tuple[str, str]]:
    occurrences: Counter[tuple[str, str]] = Counter()
    for path in sorted(source_root.rglob("*.rs")):
        relative = path.relative_to(REPO_ROOT).as_posix()
        text = path.read_text(encoding="utf-8")
        cleaned, ambiguous = neutralize_source(text)
        attributes, attr_ambiguous = rust_attributes(cleaned)
        if ambiguous or attr_ambiguous:
            _count_all_governed(occurrences, relative)
            continue
        body_ambiguous = False
        for attribute in attributes:
            bodies, imbalance = suppression_bodies(attribute)
            if imbalance:
                body_ambiguous = True
                break
            for body in bodies:
                for lint_match in LINT_RE.finditer(body):
                    lint = lint_match.group("lint")
                    governed = LINTS if lint in CLIPPY_GROUPS else (lint,)
                    for governed_lint in governed:
                        if governed_lint in LINTS:
                            occurrences[(relative, governed_lint)] += 1
        if body_ambiguous:
            _count_all_governed(occurrences, relative)
    return occurrences


def load_baseline(path: Path = BASELINE) -> Counter[tuple[str, str]]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("lints") != list(LINTS):
        raise ValueError("baseline lint set/order must exactly match the four governed lints")
    entries = payload.get("occurrences")
    if not isinstance(entries, list):
        raise ValueError("baseline occurrences must be a list")
    result: Counter[tuple[str, str]] = Counter()
    for entry in entries:
        key = (entry.get("path"), entry.get("lint"))
        count = entry.get("count")
        if (
            not isinstance(key[0], str)
            or key[1] not in LINTS
            or not isinstance(count, int)
            or isinstance(count, bool)
            or count <= 0
            or key in result
        ):
            raise ValueError(f"invalid or duplicate baseline occurrence: {entry!r}")
        result[key] = count
    return result


def validate_occurrences(
    actual: Counter[tuple[str, str]], baseline: Counter[tuple[str, str]]
) -> list[str]:
    problems: list[str] = []
    for key, count in sorted(actual.items()):
        allowed = baseline.get(key, 0)
        if count > allowed:
            path, lint = key
            problems.append(
                f"{path}: clippy::{lint} has {count} allow/expect occurrence(s), baseline {allowed}"
            )
    return problems


def write_baseline(actual: Counter[tuple[str, str]], path: Path = BASELINE) -> None:
    payload = {
        "schema_version": 1,
        "lints": list(LINTS),
        "occurrences": [
            {"path": source, "lint": lint, "count": count}
            for (source, lint), count in sorted(actual.items())
        ],
    }
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--write", action="store_true", help="replace baseline with current occurrences")
    args = parser.parse_args()
    actual = collect_occurrences()
    if args.write:
        write_baseline(actual)
        print(f"wrote {BASELINE.relative_to(REPO_ROOT)} ({sum(actual.values())} occurrences)")
        return 0
    try:
        baseline = load_baseline()
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"clippy allow ratchet baseline error: {error}")
        return 1
    problems = validate_occurrences(actual, baseline)
    if problems:
        print("clippy allow occurrence ratchet failed:")
        for problem in problems:
            print(f"  - {problem}")
        return 1
    print(
        "clippy allow occurrence ratchet passed "
        f"({sum(actual.values())}/{sum(baseline.values())} occurrences)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
