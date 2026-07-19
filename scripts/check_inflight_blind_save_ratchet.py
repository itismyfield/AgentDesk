#!/usr/bin/env python3
"""Ratchet guard for blind `save_inflight_state(...)` writes (#4259).

`save_inflight_state` is the store-side "blind whole-blob write" half of the
inflight sidecar contract (src/services/discord/inflight/save_store.rs): it
serializes the WHOLE `InflightTurnState` row and clobbers whatever is on disk,
with no compare-and-set on turn identity. A concurrent turn that legitimately
re-owns the channel between a caller's snapshot and its write is silently
overwritten. The drop-in guarded variant
`save_inflight_state_if_identity_unchanged(state, caller)`
(save_store/identity_gate.rs) refuses that race (returns `GuardedSaveOutcome`),
and every remaining blind caller holds a snapshot local it can pin an identity
against.

This guard is a monotonic ceiling on the number of *production* blind
`save_inflight_state(` call sites. It may only ever go DOWN: converting a site
to the guarded variant (or a `_if_absent` / `_create_new` create-shaped variant)
drops the count, and the ceiling is lowered to match. It can never grow back, so
the blind-write debt converges to zero (#4259 PR-2..N do the per-track
conversions) without anyone having to remember to chase it.

Only `src/services/discord/**/*.rs` is scanned. Test surfaces are excluded:
files named `tests.rs` / `*_tests.rs`, and `#[cfg(test)]` / `#[cfg(all(test, ...))]`
modules/items (balanced-brace tracked). Suffixed variants
(`save_inflight_state_if_identity_unchanged`, `_in_root`, `_if_matches_identity`,
...) are NOT blind writes and are not counted — the regex requires `(` to follow
`save_inflight_state` directly. The `fn save_inflight_state(` definition itself is
skipped.

String/comment handling: a cross-line lexer (`StripState` / `strip_line`,
ported verbatim from `scripts/check_log_key_drift.py`, #4218 — kept as a copy
so both guards stay dependency-free single-file scripts) blanks string literals
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

# Monotonic ceiling: the number of production blind `save_inflight_state(`
# call sites permitted under src/services/discord. Lower this as sites convert
# to the identity-guarded variant; never raise it casually.
#
# #4259 PR-1 baseline = 29; PR-2a lowered to 26; #4596 lowered to 25;
# the post-loop-finalize slice lowered to 21. Track decomposition
# (convert + lower per PR-2..N):
#   turn_bridge/runtime_handoff_loop.rs .. 5  (#4596 converted ClaudeEAdapter)
#   turn_bridge/stream_tick.rs .......... 5
#   turn_bridge/stream_loop.rs .......... 2
#   turn_bridge/post_loop_finalize.rs ... 0  (#4259 post-loop slice converted 4)
#   turn_bridge/mod.rs (hotfile, solo) .. 1
#   external (router/session/tui) ....... 8
#     (headless_turn, intake_turn, provider_isolation, watchdog,
#      session_runtime/worktree, tui_prompt_relay/synthetic_start x2,
#      tui_prompt_relay/codex_idle_rollout)
#
# #4259 PR-2a: the 3 converted runtime_handoff_loop sites are the legacy
# tmux-wrapper `TmuxReady` stamps, converted to
# `save_inflight_state_if_identity_matches_allow_output_restamp` (codex r1):
# the 4-field turn identity is stable across the stamp (declines only on
# concurrent re-own), while `output_path` may legitimately restamp to the
# resolved legacy /tmp session path on a warm follow-up
# (`resolve_session_temp_path`). #4596 additionally converted ClaudeEAdapter
# to a dedicated locked identity-gated RMW that may clear `tmux_session_name`
# without overwriting a newer turn. The 5 that REMAIN blind (RuntimeReady
# ProcessBackend/ClaudeTui/CodexTui + ProcessReady + the watcher-handoff
# helper) still (re)write identity-pinned `tmux_session_name`, beyond what the
# output-restamp variant tolerates, so each needs per-flow session-name-stability
# verification or an adoption-aware variant before converting.
BASELINE = 21

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

# --- Cross-line string/comment stripper, ported verbatim from
# scripts/check_log_key_drift.py (#4218). Copied rather than imported so both
# guards remain dependency-free single-file scripts; if a bug is found here,
# fix it in both. Blanked output keeps `{` / `}` / `;` counts honest for the
# cfg(test) brace tracking below (codex r1: a single-line stripper let an
# unbalanced `{` in a multi-line raw string poison the brace depth). ---

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
    """Blank out string-literal/comment content, preserving column positions.

    Quote and comment delimiters themselves are blanked too — only real code
    survives. `state` carries over between lines so multi-line strings
    (including `\\`-newline continuations) and block comments stay stripped.
    """
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
            "      The blind-write count may only decrease. Convert a site to "
            "`save_inflight_state_if_identity_unchanged` (or a `_if_absent` / "
            "`_create_new` create variant) instead of adding a blind write.",
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
