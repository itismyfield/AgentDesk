"""Shared Rust lexical string/comment stripper for guard scripts.

This module extracts string literals, char literals, and comments from Rust
source code while preserving column positions. It maintains cross-line state
for multi-line constructs (strings, raw strings, block comments).

Used by multiple guard scripts to blank prose and syntax that would otherwise
cause false matches. Blanked output keeps character and brace counts honest
for downstream analysis.

Cross-line state: an unbalanced `{` inside a multi-line raw string poisons
downstream brace-depth tracking without this stripper, hiding later code
under test-only cfg conditions.

Patterns handled:
  * Normal strings with escapes: "…"
  * Byte strings: b"…"
  * Raw strings: r"…", r#"…"#, br"…", b"…" (handled separately)
  * Char literals: '…' (lifetimes like 'a do not match)
  * Line comments: // (rest of line stripped)
  * Block comments: /* … */ (nested depth tracked)
"""

from __future__ import annotations

import re

# Char literal (so `'"'` / `'{'` cannot desync the scanners). Lifetimes (`'a`)
# do not match and fall through harmlessly.
_CHAR_LITERAL = re.compile(r"'(\\.|[^'\\])'")

# Raw / byte string openers: r"…", r#"…"#, br"…"; b"…" is handled separately.
_RAW_STRING_OPEN = re.compile(r'(?:r|br)(#*)"')


class StripState:
    """Cross-line lexer state: strings and block comments span lines."""

    __slots__ = ("in_string", "raw_hashes", "block_depth")

    def __init__(self) -> None:
        self.in_string = False  # inside a normal "…" / b"…" string
        self.raw_hashes: int | None = None  # inside r"…" / r#"…"# (hash count)
        self.block_depth = 0  # nested /* … */ depth


def strip_line(line: str, state: StripState) -> str:
    """Blank out string-literal/comment content, preserving column positions."""
    out: list[str] = []
    i = 0
    n = len(line)
    while i < n:
        if state.block_depth > 0:
            if line.startswith("/*", i):
                state.block_depth += 1
                out.append("  ")
                i += 2
            elif line.startswith("*/", i):
                state.block_depth -= 1
                out.append("  ")
                i += 2
            else:
                out.append(" ")
                i += 1
            continue
        if state.raw_hashes is not None:
            closer = '"' + "#" * state.raw_hashes
            if line.startswith(closer, i):
                state.raw_hashes = None
                out.append(" " * len(closer))
                i += len(closer)
            else:
                out.append(" ")
                i += 1
            continue
        if state.in_string:
            if line[i] == "\\" and i + 1 < n:
                out.append("  ")
                i += 2
            else:
                if line[i] == '"':
                    state.in_string = False
                out.append(" ")
                i += 1
            continue
        # --- normal code ---
        if line.startswith("//", i):
            break  # line comment: drop the rest of the line
        if line.startswith("/*", i):
            state.block_depth = 1
            out.append("  ")
            i += 2
            continue
        raw = _RAW_STRING_OPEN.match(line, i)
        if raw:
            state.raw_hashes = len(raw.group(1))
            out.append(" " * (raw.end() - i))
            i = raw.end()
            continue
        if line[i] == '"' or line.startswith('b"', i):
            skip = 2 if line[i] == "b" else 1
            state.in_string = True
            out.append(" " * skip)
            i += skip
            continue
        if line[i] == "'":
            m = _CHAR_LITERAL.match(line, i)
            if m:
                out.append(" " * (m.end() - i))
                i = m.end()
                continue
        out.append(line[i])
        i += 1
    return "".join(out)


# Structural segmentation. `strip_line` blanks strings AND comments, so it
# cannot answer "did the code change?" -- a literal edit would read as blank on
# both sides. `lex_segments` keeps literal bytes and labels each run instead.

# Char literal including escapes (`'\''`, `'\u{2F}'`) and the `b'x'` byte form.
# Lifetimes (`'a`) do not match and fall through as ordinary code.
_CHAR_LITERAL_FULL = re.compile(
    r"b?'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]{1,6}\}|.)|[^'\\\n])'"
)

# String openers, raw form first so `r#"` never degrades to `r` + `#"`.
_STRING_OPEN_FULL = re.compile(r'b?r(?P<hashes>#*)"|b?"')

# Only these can begin a literal or comment, so a code run skips the rest.
_SPECIAL_STARTS = frozenset('/"\'rb')

CODE = "code"
LITERAL = "literal"
COMMENT = "comment"
SPACE = "space"


def _scan_block_comment(line: str, start: int, state: StripState) -> int:
    i, n = start, len(line)
    while i < n and state.block_depth > 0:
        if line.startswith("/*", i):
            state.block_depth += 1
            i += 2
        elif line.startswith("*/", i):
            state.block_depth -= 1
            i += 2
        else:
            i += 1
    return i


def _scan_quoted_string(line: str, start: int, state: StripState) -> int:
    i, n = start, len(line)
    while i < n:
        if line[i] == "\\":
            i += 2
            continue
        if line[i] == '"':
            state.in_string = False
            return i + 1
        i += 1
    return n


def _scan_raw_string(line: str, start: int, state: StripState) -> int:
    closer = '"' + "#" * (state.raw_hashes or 0)
    end = line.find(closer, start)
    if end < 0:
        return len(line)
    state.raw_hashes = None
    return end + len(closer)


def lex_segments(line: str, state: StripState) -> list[tuple[str, str]]:
    """Split one line into labelled ``(kind, text)`` runs, literal-aware.

    Kinds are ``code``, ``literal`` (delimiters included), ``comment``
    (delimiters included) and ``space``. `state` carries the cross-line
    string/raw-string/block-comment position, so callers feed lines in order.
    Concatenating every ``text`` reproduces the line exactly.
    """

    segments: list[tuple[str, str]] = []
    i, n = 0, len(line)
    while i < n:
        if state.block_depth > 0:
            end = _scan_block_comment(line, i, state)
            segments.append((COMMENT, line[i:end]))
            i = end
            continue
        if state.raw_hashes is not None:
            end = _scan_raw_string(line, i, state)
            segments.append((LITERAL, line[i:end]))
            i = end
            continue
        if state.in_string:
            end = _scan_quoted_string(line, i, state)
            segments.append((LITERAL, line[i:end]))
            i = end
            continue
        if line.startswith("//", i):
            segments.append((COMMENT, line[i:]))
            i = n
            continue
        if line.startswith("/*", i):
            state.block_depth = 1
            end = _scan_block_comment(line, i + 2, state)
            segments.append((COMMENT, line[i:end]))
            i = end
            continue
        opener = _STRING_OPEN_FULL.match(line, i)
        if opener:
            if opener.group("hashes") is None:
                state.in_string = True
                end = _scan_quoted_string(line, opener.end(), state)
            else:
                state.raw_hashes = len(opener.group("hashes"))
                end = _scan_raw_string(line, opener.end(), state)
            segments.append((LITERAL, line[i:end]))
            i = end
            continue
        char_literal = _CHAR_LITERAL_FULL.match(line, i)
        if char_literal:
            segments.append((LITERAL, char_literal.group(0)))
            i = char_literal.end()
            continue
        if line[i].isspace():
            end = i
            while end < n and line[end].isspace():
                end += 1
            segments.append((SPACE, line[i:end]))
            i = end
            continue
        end = i + 1
        while end < n and not line[end].isspace():
            if line[end] in _SPECIAL_STARTS and (
                line.startswith("//", end)
                or line.startswith("/*", end)
                or _STRING_OPEN_FULL.match(line, end)
                or _CHAR_LITERAL_FULL.match(line, end)
            ):
                break
            end += 1
        segments.append((CODE, line[i:end]))
        i = end
    return segments
