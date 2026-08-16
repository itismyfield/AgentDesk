#!/usr/bin/env python3
"""Ratchet guard for blind `save_inflight_state(...)` writes (#4259).

`save_inflight_state` was the store-side "blind whole-blob write" half of the
inflight sidecar contract (src/services/discord/inflight/save_store.rs): it
serializes the WHOLE `InflightTurnState` row and clobbers whatever is on disk,
with no compare-and-set on turn identity. A concurrent turn that legitimately
re-owns the channel between a caller's snapshot and its write is silently
overwritten. The identity-guarded variant
`save_inflight_state_if_identity_unchanged(state, caller)`
(save_store/identity_gate.rs) refuses cross-turn re-ownership (returns
`GuardedSaveOutcome`); callers that can race same-turn writers must use a
lock-held narrow patch like the R4 bridge-entry seam so watcher progress is
preserved too.

This guard is a monotonic ceiling on the number of *production* blind
`save_inflight_state(` call sites. It may only ever go DOWN: converting a site
to the guarded variant (or a `_if_absent` / `_create_new` create-shaped variant)
drops the count, and the ceiling is lowered to match. It can never grow back, so
the blind-write debt converges to zero (#4259 PR-2..N do the per-track
conversions) without anyone having to remember to chase it. At baseline zero,
the blind helper and its facade re-export are `#[cfg(test)]`: production code
cannot name the API, so the compiler is the authoritative boundary and this
script remains the source inventory tripwire.

Only `src/services/discord/**/*.rs` is scanned. Test surfaces are excluded:
files named `tests.rs` / `*_tests.rs`, and `#[cfg(test)]` / `#[cfg(all(test, ...))]`
modules/items (balanced-brace tracked). Suffixed variants
(`save_inflight_state_if_identity_unchanged`, `_in_root`, `_if_matches_identity`,
...) are NOT blind writes and are not counted — the regex requires `(` to follow
`save_inflight_state` directly. The `fn save_inflight_state(` definition itself is
skipped.

String/comment handling: a cross-line lexer (`StripState` / `strip_line`,
imported from `scripts/rust_lex.py`) blanks string literals
(normal strings with escapes, `b"…"`, and `r"…"` / `r#"…"#` raw strings — all
of which may span lines), char literals, `//` line comments, and nested
`/* … */` block comments BEFORE the cfg(test) brace tracking and call matching
run. Without the cross-line state, an unbalanced `{` inside a multi-line raw
string in a cfg(test) module would poison the brace depth, keep skip mode alive
past the module, and hide every later production call (codex r1).

To intentionally remove a blind write, convert the site then lower BASELINE to
the new count. Raising BASELINE is a deliberate, reviewable diff edit that should
carry justification — it is not the normal path.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

try:
    from rust_lex import StripState, strip_line
except ModuleNotFoundError:  # imported from a repo-root unittest
    from scripts.rust_lex import StripState, strip_line

# Monotonic ceiling: the number of production blind `save_inflight_state(`
# call sites permitted under src/services/discord. Lower this as sites convert
# to the identity-guarded variant; never raise it casually.
#
# #4259 PR-1 baseline = 29; PR-2a lowered to 26; #4596 lowered to 25;
# post-loop-finalize lowered to 21; external writers lowered to 13; R1 stream
# tick lowered to 8; R2 runtime-handoff atomic stamps lower to 3; R3 stream-loop
# terminal/cancel patches lower to 1. Remaining:
#   turn_bridge/runtime_handoff_loop.rs .. 0  (R2 field-scoped locked stamp)
#   turn_bridge/stream_tick.rs .......... 0  (R1)
#   turn_bridge/stream_loop.rs .......... 0  (R3 narrow locked patches)
#   turn_bridge/post_loop_finalize.rs ... 0
#   turn_bridge/mod.rs (hotfile, solo) .. 0  (R4)
#   external (router/session/tui) ....... 0
#
# R4 converts the final bridge-entry whole-row write to a lock-held narrow RMW,
# then removes the blind helper from production compilation entirely.
BASELINE = 0

SCAN_ROOT = Path("src") / "services" / "discord"

# `save_inflight_state` followed by `(` — a blind write. The left `\b` rejects
# longer identifiers ending in the name; `\s*` tolerates whitespace left behind
# by the stripper blanking an interleaved comment (`save_inflight_state /* x */ (`,
# a form rustfmt preserves — codex r2). Suffixed variants
# (`_if_identity_unchanged`, `_in_root`, ...) are still rejected because `_`
# is not whitespace.
CALL_RE = re.compile(r"\bsave_inflight_state\s*\(")
DEFN_RE = re.compile(r"\bfn\s+save_inflight_state\s*\(")
# `#[cfg(test)]`, `#[cfg(all(test, ...))]`, `#[cfg(any(test, ...))]`.
CFG_TEST_RE = re.compile(r"#\[\s*cfg\s*\(\s*(?:all|any)?\s*\(?\s*test\b")

# Cross-line string/comment stripper imported from scripts/rust_lex.py.
# Blanked output keeps `{` / `}` / `;` counts honest for the cfg(test)
# brace tracking below: without cross-line state, an unbalanced `{` in a
# multi-line raw string poisons the brace depth.


def count_blind_saves(repo_root: Path) -> tuple[int, list[str]]:
    """Count production blind `save_inflight_state(` call sites under
    src/services/discord. Excludes test files, `#[cfg(test)]` modules/items
    (balanced-brace tracked), comments/strings (cross-line stripped), and the
    fn definition."""
    total = 0
    locations: list[str] = []
    scan_root = repo_root / SCAN_ROOT
    for path in sorted(scan_root.rglob("*.rs")):
        rel = path.relative_to(repo_root)
        if "target" in rel.parts:
            continue
        name = path.name
        if name == "tests.rs" or name.endswith("_tests.rs"):
            continue

        strip_state = StripState()
        brace_depth = 0
        mode = "normal"  # normal | armed (saw cfg(test) attr) | skip (in test block)
        skip_start_depth = 0
        for lineno, raw in enumerate(
            path.read_text(encoding="utf-8").splitlines(), start=1
        ):
            code = strip_line(raw, strip_state)
            countable = mode == "normal"

            if mode == "normal" and CFG_TEST_RE.search(code):
                mode = "armed"

            # Walk braces / statement terminators to update the test-region state
            # machine. An armed cfg(test) attribute resolves on the item that
            # follows: a `{` opens a balanced-brace skip region; a `;` at the
            # attribute's depth means a statement item (use/const/type) — disarm.
            for ch in code:
                if ch == "{":
                    if mode == "armed":
                        mode = "skip"
                        skip_start_depth = brace_depth
                    brace_depth += 1
                elif ch == "}":
                    brace_depth -= 1
                    if mode == "skip" and brace_depth <= skip_start_depth:
                        mode = "normal"
                elif ch == ";":
                    if mode == "armed":
                        mode = "normal"

            if not countable:
                continue
            if DEFN_RE.search(code):
                continue
            for _ in CALL_RE.finditer(code):
                total += 1
                locations.append(f"{rel}:{lineno}")

    return total, locations


def main() -> int:
    repo_root = Path(__file__).resolve().parent.parent
    current, locations = count_blind_saves(repo_root)

    if current > BASELINE:
        print(
            f"FAIL: {current} blind `save_inflight_state(` call sites exceed the "
            f"ratchet baseline of {BASELINE}.",
            file=sys.stderr,
        )
        print(
            "      The blind-write count may only decrease. Use a field-scoped "
            "lock-held RMW (or a `_if_absent` / `_create_new` create variant) "
            "instead of adding a blind write.",
            file=sys.stderr,
        )
        for loc in locations:
            print(f"        {loc}", file=sys.stderr)
        return 1

    if current < BASELINE:
        print(
            f"NOTE: {current} blind save sites is below the baseline of {BASELINE}. "
            f"Lower BASELINE to {current} in "
            "scripts/check_inflight_blind_save_ratchet.py to lock in the win."
        )
        return 0

    print(f"OK: {current} blind `save_inflight_state(` call sites (baseline {BASELINE}).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
