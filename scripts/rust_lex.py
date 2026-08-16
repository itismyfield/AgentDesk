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
