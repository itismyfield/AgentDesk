#!/usr/bin/env python3
"""Per-file exact-count allowlist for both intake-outbox `done` writers (#5071 T2).

WHY THIS EXISTS. #5071 T2 moves `done` authority from the intake worker toward
journal-proven reconciliation. The legacy spawned writer and the proof writer
must both remain reviewable during that transition.

WHAT IS PINNED. The scope is deliberately the legacy
`crate::db::intake_outbox::mark_done` writer and the proof-only
`crate::db::intake_outbox_delivery_proof::mark_done_from_delivery_proof` writer.
EPIC #5071 says the worker's `Ok` stamp becomes `dispatched` and that only the
terminal-receipt holder drives `done`; it does not move `claimed`, `accepted`,
or `spawned`. Pinning those other lifecycle transitions here would block T2-
unrelated work. `EXPECTED_CALL_SITES` names the writer symbol and the owning
function's file, never a line number. Every Rust file below `src/` is scanned:
within the bounds below, a call added, deleted, moved, or found in an unlisted
file fails closed.

WHAT THIS GATE DOES NOT GUARANTEE. This is a lexical scan, not Rust parsing or
name resolution. It sees a bare `mark_done(...)` only in a file that directly
imports it from `crate::db::intake_outbox`, plus the literal fully-qualified
path. Glob imports, nested-brace imports, `super::intake_outbox::mark_done`
imports, `use ...::mark_done as finish; finish(...)`, renamed re-exports,
name-constructing macros, same-file helper indirection, function-value
indirection, trait dispatch, a line break between `mark_done` and `(`, and a
new direct SQL `UPDATE intake_outbox ... status = 'done'` are not seen. The
line-break form is rejected by the repository's enforced `cargo fmt --check`.
It also does not prove a call is reachable, successful, or the right lifecycle
action. Conversely, a same-spelled free function in a file importing this
writer could be over-counted. Comments, strings, `*_tests.rs`, and
`#[cfg(test)]` regions are excluded, but other cfgs are counted without target
evaluation. When run independently, the wiring test detects removal of the
gate command; it cannot protect deletion of its own unittest invocation from
`ci-script-checks.sh`. These are declared bounds, not silent skips: the gate
only claims exact textual call-site counts for this writer spelling within
those bounds.
"""

from __future__ import annotations

import re
import sys
from collections import defaultdict
from pathlib import Path

SCAN_ROOT = Path("src")
EXPECTED_CALL_SITES: dict[str, dict[str, int]] = {
    "mark_done": {"src/services/cluster/intake_worker.rs": 1},
    "mark_done_from_delivery_proof": {
        "src/services/discord/intake_delivery_reconciler.rs": 1
    },
}
SYMBOL_MODULES = {
    "mark_done": "intake_outbox",
    "mark_done_from_delivery_proof": "intake_outbox_delivery_proof",
}
CFG_TEST_RE = re.compile(r"#\[\s*cfg\s*\(\s*(?:all|any)?\s*\(?\s*test\b")

# Char literal (so `'"'` / `'{'` cannot desync the scanner). Lifetimes (`'a`)
# do not match and fall through harmlessly.
_CHAR_LITERAL = re.compile(r"'(\\.|[^'\\])'")


def is_test_file(name: str) -> bool:
    return name == "tests.rs" or name.endswith("_tests.rs")


def strip_source(text: str) -> str:
    """Blank comments and strings while preserving braces and newlines."""
    out: list[str] = []
    i = 0
    block_depth = 0
    quote: str | None = None
    raw_hashes: int | None = None
    while i < len(text):
        char = text[i]
        next_char = text[i + 1] if i + 1 < len(text) else ""
        if block_depth:
            if char == "/" and next_char == "*":
                block_depth += 1
                out.extend("  ")
                i += 2
            elif char == "*" and next_char == "/":
                block_depth -= 1
                out.extend("  ")
                i += 2
            else:
                out.append("\n" if char == "\n" else " ")
                i += 1
            continue
        if raw_hashes is not None:
            close = '"' + ('#' * raw_hashes)
            if text.startswith(close, i):
                out.extend(" " * len(close))
                i += len(close)
                raw_hashes = None
            else:
                out.append("\n" if char == "\n" else " ")
                i += 1
            continue
        if quote is not None:
            if char == "\\":
                out.append(" ")
                if i + 1 < len(text):
                    out.append("\n" if text[i + 1] == "\n" else " ")
                    i += 2
                else:
                    i += 1
            elif char == quote:
                out.append(" ")
                quote = None
                i += 1
            else:
                out.append("\n" if char == "\n" else " ")
                i += 1
            continue
        if char == "/" and next_char == "/":
            end = text.find("\n", i)
            if end == -1:
                out.extend(" " * (len(text) - i))
                break
            out.extend(" " * (end - i))
            i = end
        elif char == "/" and next_char == "*":
            block_depth = 1
            out.extend("  ")
            i += 2
        elif char == "'":
            literal = _CHAR_LITERAL.match(text, i)
            if literal:
                out.extend(" " * (literal.end() - i))
                i = literal.end()
            else:
                out.append(char)
                i += 1
        elif char == '"':
            quote = char
            out.append(" ")
            i += 1
        elif char in ("r", "b"):
            opener = re.match(r"(?:br|r)(#*)\"", text[i:])
            if opener:
                token = opener.group(0)
                raw_hashes = len(opener.group(1))
                out.extend(" " * len(token))
                i += len(token)
            else:
                out.append(char)
                i += 1
        else:
            out.append(char)
            i += 1
    return "".join(out)


def production_text(path: Path) -> str:
    """Return stripped source with balanced `#[cfg(test)]` items blanked."""
    code = strip_source(path.read_text(encoding="utf-8"))
    chars = list(code)
    for match in list(CFG_TEST_RE.finditer(code)):
        start = code.find("{", match.end())
        semicolon = code.find(";", match.end())
        if start == -1 or (semicolon != -1 and semicolon < start):
            continue
        depth = 0
        for index in range(start, len(chars)):
            if chars[index] == "{":
                depth += 1
            elif chars[index] == "}":
                depth -= 1
                if depth == 0:
                    for blank in range(match.start(), index + 1):
                        if chars[blank] != "\n":
                            chars[blank] = " "
                    break
    return "".join(chars)


def production_call_sites(root: Path) -> tuple[dict[str, dict[str, int]], int, int]:
    """Return ``(counts, scanned_files, skipped_test_files)`` over ``src/``."""
    found: defaultdict[str, defaultdict[str, int]] = defaultdict(lambda: defaultdict(int))
    scanned = 0
    skipped = 0
    for path in sorted((root / SCAN_ROOT).rglob("*.rs")):
        if not path.is_file():
            continue
        if is_test_file(path.name):
            skipped += 1
            continue
        scanned += 1
        code = production_text(path)
        for symbol, module in SYMBOL_MODULES.items():
            call = re.compile(rf"(?<![.\w])\b{symbol}\s*\(")
            definition = re.compile(rf"\bfn\s+{symbol}\s*\(")
            imported = re.compile(rf"\buse\s+crate\s*::\s*db\s*::\s*{module}\s*::\s*(?:{symbol}\b|\{{[^}}]*\b{symbol}\b)", re.DOTALL)
            qualified = re.compile(rf"\b(?:(?:crate\s*::\s*)?db\s*::\s*)?{module}\s*::\s*{symbol}\s*\(")
            if not (imported.search(code) or qualified.search(code)):
                continue
            hits = sum(len(call.findall(line)) for line in code.splitlines() if not definition.search(line))
            if hits:
                found[symbol][path.relative_to(root).as_posix()] += hits
    return found, scanned, skipped


LIMITS = (
    "lexical scan, not Rust parsing or reachability proof; glob/nested-brace/super imports, "
    "aliases, renamed re-exports, name-constructing macros, same-file helper/value indirection, "
    "trait dispatch, mark_done followed by a line break before `(`, and direct SQL writers are "
    "NOT seen (cargo fmt --check rejects the line-break call form); same-spelled free functions "
    "may be over-counted; cfg other than cfg(test) is not evaluated; the wiring test cannot "
    "protect deletion of its own unittest invocation from ci-script-checks.sh"
)


def check(root: Path) -> tuple[bool, str]:
    found, scanned, skipped = production_call_sites(root)
    problems: list[str] = []
    for symbol in sorted(set(EXPECTED_CALL_SITES) | set(found)):
        expected = EXPECTED_CALL_SITES.get(symbol, {})
        actual = found[symbol]
        for rel in sorted(set(expected) | set(actual)):
            want = expected.get(rel, 0)
            have = actual.get(rel, 0)
            if want != have:
                if want == 0:
                    problems.append(f"{symbol}: UNLISTED call site in {rel} ({have}x)")
                elif have == 0:
                    problems.append(f"{symbol}: call site GONE from {rel} (expected {want}x)")
                else:
                    problems.append(f"{symbol}: {rel} has {have}x, expected {want}x")
    total_expected = sum(sum(files.values()) for files in EXPECTED_CALL_SITES.values())
    total_actual = sum(sum(files.values()) for files in found.values())
    header = (
        f"intake-outbox done writer call sites: {total_actual} production sites across "
        f"{len(EXPECTED_CALL_SITES)} symbols; scanned {scanned} Rust files under "
        f"{SCAN_ROOT.as_posix()}/, skipped {skipped} test files; ({LIMITS})"
    )
    if problems:
        return False, (
            f"FAIL: intake-outbox done writer call sites moved (expected {total_expected}, "
            f"found {total_actual}).\n  "
            + "\n  ".join(problems)
            + "\nUpdate EXPECTED_CALL_SITES in scripts/check_intake_outbox_done_writer_call_sites.py "
            "in the same commit, and say in the commit message which site moved and why.\n"
            f"({LIMITS})"
        )
    return True, f"OK: {header}"


def main() -> int:
    ok, message = check(Path(__file__).resolve().parent.parent)
    print(message, file=sys.stdout if ok else sys.stderr)
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
