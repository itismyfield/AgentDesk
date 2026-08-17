#!/usr/bin/env python3
"""Per-file exact-count allowlist for intake-outbox `done` writers (#5071 T2).

WHY THIS EXISTS, AND WHY IT LANDS BEFORE T2. #5071 T2 moves the `done`
transition from the intake worker to the terminal-receipt owner. Before that
move, the current writer call location must be reviewable: a behaviour change
with no gate underneath it protects nothing. This script changes no production
behaviour; it records the production call sites that exist before the move.

WHAT IS PINNED. The scope is deliberately the legacy
`crate::db::intake_outbox::mark_done` writer, the delivery-proof
`crate::db::intake_outbox_delivery_proof::mark_done_from_delivery_proof`
writer, and the receipt-backed
`crate::db::intake_outbox_delivery_proof::settle_intake_done_from_receipt`
writer.
EPIC #5071 says the worker's `Ok` stamp becomes `dispatched` and that only the
terminal-receipt holder drives `done`; it does not move `claimed`, `accepted`,
or `spawned`. Pinning those other lifecycle transitions here would block T2-
unrelated work. `EXPECTED_CALL_SITES` names the writer symbol and the owning
function's file, never a line number. The proof writer expects zero sites until
its exact future owner exists, then exactly one there. Every regular `.rs` file
under `src/` is scanned: within the bounds below, a protected direct import or
call added, deleted, moved within the scanned `src/` tree, aliased, or found in
an unlisted file fails closed.

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
writer could be over-counted. Comments, strings, pinned whole-file test modules,
and `#[cfg(test)]` regions are excluded, but other cfgs are counted without
target evaluation. Whole-file skips use the single lexical pin in
`scripts/test_only_module_skip_pin.py`; non-`.rs` regular files are rejected
before classification, `src/` symlinks are rejected, and the skipped census
must equal the pin count. Symlink rejection is lexical rather than atomic
against a post-enumeration replacement; CI assumes a static checkout while
the gate runs.

The unchanged shared resolver does not guarantee at least seven measured forms:
`#[path]`/`mod` separated by a comment, macro-generated `mod`,
`cfg(not(test))+include!`, `cfg(any(test,feature))+include!`, `cfg_attr(path=)`,
raw-string `#[path]`, or ungated `include!`. The pin guarantees membership, not
content: production reachability can change inside a pinned file without a set
delta. Compiler-backed reachability is explicitly follow-up slice work.

When run independently, the wiring test detects removal of the
gate command; it cannot protect deletion of its own unittest invocation from
`ci-script-checks.sh`. These are declared bounds, not silent skips: the gate
only claims exact textual call-site counts for this writer spelling within
those bounds.
"""

from __future__ import annotations

import importlib.util
import re
import sys
from collections.abc import Iterable
from collections import defaultdict
from pathlib import Path

try:
    from rust_lex import StripState, strip_line
except ModuleNotFoundError:  # imported from a repo-root unittest
    from scripts.rust_lex import StripState, strip_line

SCAN_ROOT = Path("src")
PROOF_OWNER = "src/services/discord/runtime_bootstrap/intake_delivery_reconciler.rs"
SYMBOL_MODULES = {
    "mark_done": "intake_outbox",
    "mark_done_from_delivery_proof": "intake_outbox_delivery_proof",
    "settle_intake_done_from_receipt": "intake_outbox_delivery_proof",
}
CFG_EXCLUSIVELY_TEST_RE = re.compile(
    r"#\[\s*cfg\s*\(\s*(?:test\s*\)|all\s*\(\s*test\s*(?:,|\)))"
)


def _load_skip_pin_module():
    name = "test_only_module_skip_pin"
    if name in sys.modules:
        return sys.modules[name]
    spec = importlib.util.spec_from_file_location(
        name, Path(__file__).resolve().parent / "test_only_module_skip_pin.py"
    )
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load scripts/test_only_module_skip_pin.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


_SKIP_PIN = _load_skip_pin_module()
PINNED_TEST_ONLY_MODULE_FILES = _SKIP_PIN.PINNED_TEST_ONLY_MODULE_FILES


def is_test_file(name: str) -> bool:
    return name == "tests.rs" or name.endswith("_tests.rs")


def strip_source(text: str) -> str:
    """Blank comments and strings while preserving braces and newlines.

    Uses the shared cross-line lexer from rust_lex module to maintain
    consistency with other guard scripts. Splits on \\n only (not other
    line-separation control characters like \\u2028) so line-comment
    boundaries remain intact within strip_line's break behavior.
    """
    state = StripState()
    lines = text.split('\n')
    result = []
    for i, line in enumerate(lines):
        stripped = strip_line(line, state)
        result.append(stripped)
        # Preserve \n boundaries except after the last line
        if i < len(lines) - 1:
            result.append('\n')
    return "".join(result)


def production_text(path: Path) -> str:
    """Return stripped source with exclusively-test cfg items blanked."""
    code = strip_source(path.read_text(encoding="utf-8"))
    chars = list(code)
    for match in list(CFG_EXCLUSIVELY_TEST_RE.finditer(code)):
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


def production_call_sites(
    root: Path,
    expected: dict[str, dict[str, int]],
    *,
    pinned_test_only_files: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
) -> tuple[dict[str, dict[str, int]], list[str], int, int]:
    """Return call counts, import violations, and scan totals over ``src/``."""
    found: defaultdict[str, defaultdict[str, int]] = defaultdict(lambda: defaultdict(int))
    violations: list[str] = []
    all_files, whole_file_skips = _SKIP_PIN.validated_scan_files(
        root, SCAN_ROOT, is_test_file, pinned_paths=pinned_test_only_files
    )
    scanned = 0
    skipped = 0
    for path in all_files:
        if path in whole_file_skips:
            skipped += 1
            continue
        scanned += 1
        code = production_text(path)
        rel = path.relative_to(root).as_posix()
        for symbol, module in SYMBOL_MODULES.items():
            call = re.compile(rf"(?<![.\w])\b{symbol}\s*\(")
            definition = re.compile(rf"\bfn\s+{symbol}\s*\(")
            import_stmt = re.compile(
                rf"\buse\s+crate\s*::\s*db\s*::\s*{module}\s*::\s*(?P<body>[^;]+);",
                re.DOTALL,
            )
            imports = [
                match.group("body")
                for match in import_stmt.finditer(code)
                if re.search(rf"\b{symbol}\b", match.group("body"))
            ]
            if any(re.search(rf"\b{symbol}\s+as\s+\w+", body) for body in imports):
                violations.append(f"{symbol}: ALIASED protected import in {rel}")
            elif imports and rel not in expected.get(symbol, {}):
                violations.append(f"{symbol}: UNLISTED protected import in {rel}")
            qualified = re.compile(
                rf"\b(?:(?:crate\s*::\s*)?db\s*::\s*)?{module}\s*::\s*{symbol}\s*\("
            )
            if not (imports or qualified.search(code)):
                continue
            hits = sum(
                len(call.findall(line))
                for line in code.splitlines()
                if not definition.search(line)
            )
            if hits:
                found[symbol][rel] += hits
    return found, violations, scanned, skipped


def expected_call_sites(root: Path) -> dict[str, dict[str, int]]:
    """Activate the proof-writer pin only when its exact future module exists."""
    return {
        "mark_done": {"src/services/cluster/intake_worker.rs": 1},
        "mark_done_from_delivery_proof": (
            {PROOF_OWNER: 1} if (root / PROOF_OWNER).is_file() else {}
        ),
        "settle_intake_done_from_receipt": {
            "src/services/discord/turn_bridge/intake_settlement.rs": 1
        },
    }


LIMITS = (
    "lexical scan, not Rust parsing or reachability proof; glob/nested-brace/super imports, "
    "renamed re-exports, name-constructing macros, same-file helper/value indirection, trait "
    "dispatch, protected symbols split before `(`, and direct SQL writers are "
    "NOT seen (cargo fmt --check rejects the line-break call form); same-spelled free functions "
    "may be over-counted; direct protected-symbol aliases are rejected; only braced cfg(test) "
    "and cfg(all(test,...)) items are stripped, while other cfg/cfg_attr forms remain scanned; the "
    "whole-file skips use one lexical pin, reject src symlinks, and check their census; "
    "non-.rs regular files fail closed before classification; symlink rejection is "
    "non-atomic outside a static CI checkout; the scan root is `src/`; call sites in files "
    "reached by `#[path]`/`include!` targets resolving outside `src/` are not seen; "
    "fail-closed handling for that boundary is follow-up work; at least seven resolver forms are not "
    "guaranteed (path/mod comment, macro mod, two cfg/include "
    "forms, cfg_attr path, raw path, ungated include); pin membership cannot detect "
    "production reachability changes inside pinned files and compiler-backed reachability "
    "is follow-up work; wiring tests cannot protect deletion of their own unittest invocation"
)


def check(
    root: Path,
    expected: dict[str, dict[str, int]] | None = None,
    *,
    pinned_test_only_files: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
) -> tuple[bool, str]:
    expected = expected if expected is not None else expected_call_sites(root)
    try:
        found, import_problems, scanned, skipped = production_call_sites(
            root, expected, pinned_test_only_files=pinned_test_only_files
        )
    except RuntimeError as exc:
        return False, str(exc)
    problems = list(import_problems)
    for symbol in sorted(set(expected) | set(found)):
        expected_files = expected.get(symbol, {})
        actual = found[symbol]
        for rel in sorted(set(expected_files) | set(actual)):
            want = expected_files.get(rel, 0)
            have = actual.get(rel, 0)
            if want != have:
                if want == 0:
                    problems.append(f"{symbol}: UNLISTED call site in {rel} ({have}x)")
                elif have == 0:
                    problems.append(f"{symbol}: call site GONE from {rel} (expected {want}x)")
                else:
                    problems.append(f"{symbol}: {rel} has {have}x, expected {want}x")
    total_expected = sum(sum(files.values()) for files in expected.values())
    total_actual = sum(sum(files.values()) for files in found.values())
    header = (
        f"intake-outbox done writer call sites: {total_actual} production sites across "
        f"{len(expected)} symbols; scanned {scanned} Rust files under "
        f"{SCAN_ROOT.as_posix()}/, skipped {skipped} test files; ({LIMITS})"
    )
    if problems:
        return False, (
            f"FAIL: intake-outbox done writer call sites moved (expected {total_expected}, "
            f"found {total_actual}).\n  "
            + "\n  ".join(problems)
            + "\nUpdate expected_call_sites/SYMBOL_MODULES in this checker in the same commit, "
            "and say which protected site moved and why.\n"
            f"({LIMITS})"
        )
    return True, f"OK: {header}"


def main() -> int:
    ok, message = check(Path(__file__).resolve().parent.parent)
    print(message, file=sys.stdout if ok else sys.stderr)
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
