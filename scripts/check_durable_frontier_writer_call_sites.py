#!/usr/bin/env python3
"""Per-file exact-count allowlist for the durable frontier writer symbols (#5071 T1 S7').

WHY THIS EXISTS, AND WHY IT LANDS BEFORE S7. #5071 T1 S7 changes the recovery
path's durable behaviour: it stops that path from bypassing the
`shadow_mirror_delivered_frontier` funnel. Before S7 was written, NOTHING in the
repo pinned where these symbols are called from -- the only writer allowlist
that existed was `scripts/check_delivery_journal_raw_writer.py`, which pins one
different symbol (`append_delivery_journal_batch`). A behaviour change with no
gate underneath it protects nothing, so this slice builds the gate first. IT
CHANGES NO PRODUCTION BEHAVIOUR: it only records, per symbol and per file, the
exact number of production call sites that exist today.

WHAT IS PINNED. Three stores write the relay's delivery frontier:

  store 1  durable delivery record  src/services/discord/outbound/delivery_record.rs
  store 2  completed-turn ledger    src/services/discord/outbound/completed_turn_ledger.rs
  store 3  in-memory watermark      src/services/discord/tmux.rs

`EXPECTED_CALL_SITES` below names every public write API of stores 1 and 2 that
reaches a durable write helper, the one private shadow-mirror funnel body they
share, and both public entry points of store 3's watermark CAS. For each symbol
it fixes a `{file: count}` map over ALL of `src/`. Any deviation fails: a call
added, a call deleted, a call moved to another file, or a call appearing in a
file the map does not list.

TWO HOLES IN THE MODEL GATE THAT THIS ONE DELIBERATELY DOES NOT INHERIT.
  (1) ANCHOR BLINDNESS. `check_delivery_journal_raw_writer.py` reads ONE anchor
      file per family, so an uninstrumented durable write added to any other
      file of that family is caught by nothing (its own S5a comment block
      measures this). This gate has no anchors: it walks every `.rs` file under
      `src/`, so a new call site in a file nobody thought of still fails.
  (2) ONE BOOLEAN PER FAMILY. That gate reduces each family to "does the anchor
      contain the token", so N of N call sites can drop to 1 and stay green --
      observed across three consecutive slices. This gate stores an exact
      integer per (symbol, file), so losing one of two sites in the same file is
      a failure.

WHAT THIS GATE DOES NOT SEE. It is a LEXICAL SCAN over stripped source text; it
does not parse Rust, does not resolve paths or types, and does not run rustc.
Concretely, and each of these was exercised by a mutation in the accompanying
test module:

  * ALIAS AND RE-EXPORT. `use ...::write_delivered_frontier as w;` then `w(..)`
    spells neither the symbol nor a call to it. NOT SEEN -- measured, not
    assumed. Same for `pub use ... as ...` re-exports. A module alias is
    different and IS seen, because the function name survives it: `dr::f(..)`,
    `delivery_record::f(..)` and a bare `f(..)` all match identically.
  * NAME-CONSTRUCTING MACROS. A macro whose body contains the literal spelling
    IS counted (the text is there, once per textual occurrence, and expansion
    count is invisible to a text scan -- a call inside a macro invoked three
    times counts 1). A macro that ASSEMBLES the name (`paste!`,
    `concat_idents!`) is NOT seen.
  * INDIRECTION THROUGH VALUES. `let f = write_delivered_frontier;` is not a
    call site (no `(` follows the name) and the later `f(..)` is not one either.
    Trait-object dispatch to a method of the same name is likewise invisible.
  * NAME COLLISION -- PRESENT IN THIS TREE, NOT HYPOTHETICAL. Two distinct
    functions are both named `record_watcher_terminal_delivery`: the durable
    funnel at `outbound/delivery_record.rs:2332` and a local wrapper at
    `tmux_watcher/terminal_long_chunks.rs:168`. The two production sites this
    map pins for that file are one call to each (`dr::…` at :124, bare at
    :222). The integer is therefore a count of the SPELLING, not of calls to
    one function. It is still the right pin here because the wrapper's only
    job is to reach the funnel, but a future collision under a pinned name
    would be counted the same way and the map would not say so.
  * SPELLING COUPLING. Because the completion criterion is a literal grep, the
    whole contract is bound to the spelling. Renaming a symbol does not weaken
    the gate silently -- the count drops to 0 and it fails -- but any route that
    reaches the same code under a different spelling is outside it.
  * CFG. `#[cfg(unix)]` and friends are not evaluated: a call gated to one
    target counts on every target, exactly as in the model gate.
  * PROSE. Comments (`//`, `/* */`) and string literals (including raw strings)
    are BLANKED before matching, so the symbol written in a doc comment or a
    log message is not a call site. This differs from the model gate, which
    strips only the `//` suffix and therefore goes red on a symbol inside a
    string. Stripping is the right trade here because the map has to hold exact
    per-file integers, and prose would make those integers arbitrary -- but it
    does mean a call hidden by a stripper bug is a silent miss, which is why
    the cross-line stripper is exercised directly in the test module.
  * SEMANTICS. It says nothing about whether a call is reachable, reached, or
    correct. That is what the Rust runtime tests are for.

WHAT IT DOES BUY. Every textual call site under `src/`, per file, exactly
counted. That is strictly more than the model gate: it has no anchor to escape
and no boolean to saturate.

PRODUCTION vs TEST. Files named `tests.rs` / `*_tests.rs` are skipped whole, and
`#[cfg(test)]` / `#[cfg(all(test, ...))]` / `#[cfg(any(test, ...))]` regions are
resolved with balanced-brace tracking over comment/string-stripped text.

  DEVIATION FROM THE PORTED RESOLVER, DELIBERATE AND MEASURED. The equivalent
  loop in `scripts/check_inflight_blind_save_ratchet.py` resolves an armed
  `#[cfg(test)]` on `{` or `;` only. A `#[cfg(test)]` STRUCT FIELD ends in `,`,
  so the armed state there survives the field, the struct, and latches onto the
  next `{` it meets -- which in `session_relay_sink.rs` is the production
  `impl SessionBoundDiscordRelaySink {` block, silently swallowing it. Measured
  on this tree: that loop misclassifies three production call sites in that one
  file (`commit_ordered_jsonl_range`, `record_delivered_content_fingerprint`,
  `advance_watcher_confirmed_end`) as test code. This resolver therefore also
  disarms on a top-level `,`, but only until an item keyword that introduces a
  braced body (`fn`/`impl`/`mod`/`trait`/`macro_rules`) has been seen, so a
  comma inside a generic return type cannot disarm a real test fn.

  That bug is LATENT, NOT ACTIVE, in the ratchet that carries it: its symbol
  (`save_inflight_state(`) has zero production call sites under either resolver,
  so its baseline of 0 is correct today. It is left alone here rather than
  repaired, because changing a landed gate's counting rule is not this slice's
  to do -- it is recorded so the next slice can.

#5071 T1 S7 MOVED FIVE COUNTS, AND THIS IS THE PARAGRAPH THIS GATE EXISTS FOR.
The recovery path stopped bypassing the funnel, so:

  write_delivered_frontier                 recovery_engine/...  1 -> 0
  write_proven_gone_equal_range_frontier   recovery_engine/...  1 -> 0  (now {})
  append_completed_turn                    recovery_engine/...  1 -> 0
  shadow_mirror_delivered_frontier_inner   delivery_record.rs   2 -> 3
  record_recovery_terminal_delivery        recovery_engine/...  0 -> 1  (new pin)

Net -1 (42 -> 41) over one more pinned symbol (23 -> 24). Every one of those
five is the SAME move seen from a different symbol: three raw calls left the
recovery file and one funnel call took their place.

WHAT THIS GATE DOES NOT SAY ABOUT THAT MOVE, measured in the same slice. It
counts spellings, so it cannot tell that the funnel call reaches the same bytes
the raw calls did. `shadow_mirror_delivered_frontier_inner` going 2 -> 3 says a
third caller exists, not that it is the recovery one; that binding is held by
`EXPECTED_CALL_SITES["record_recovery_terminal_delivery"]` naming the recovery
file and by the Rust tests in delivery_record.rs.

TO CHANGE A COUNT. Edit `EXPECTED_CALL_SITES` in the same commit as the code
move, and say in the commit message which call site moved and why. That edit is
the reviewable artefact this gate exists to force.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# Every production call site of every pinned symbol, keyed by symbol then by
# repo-relative path. An empty map pins the symbol at zero production callers:
# the API exists and compiles, but nothing outside tests may reach it. Measured
# on 0bde0675b.
EXPECTED_CALL_SITES: dict[str, dict[str, int]] = {
    # -- store 1: durable delivery record ------------------------------------
    # #5071 T1 S7 removed this symbol's recovery_engine call site: the recovery
    # path now reaches the funnel instead of this raw writer. The one site left
    # is the dormant fresh-send one.
    "write_delivered_frontier": {
        "src/services/discord/outbound/turn_output_controller/fresh_send.rs": 1,
    },
    "commit_ordered_jsonl_range": {
        "src/services/discord/session_relay_sink.rs": 1,
    },
    "reanchor_current_generation_frontier": {
        "src/services/discord/tui_prompt_relay/claude_idle_runtime.rs": 1,
    },
    "record_watcher_owner_channel_context": {
        "src/services/discord/inflight/model.rs": 1,
    },
    "record_delivered_content_fingerprint": {
        "src/services/discord/session_relay_sink.rs": 1,
    },
    "record_fresh_send_content_fingerprint": {
        "src/services/discord/outbound/turn_output_controller/fresh_send.rs": 1,
    },
    # -- store 1: the shadow-mirror funnel and its four entry points ----------
    "shadow_mirror_delivered_frontier": {
        "src/services/discord/outbound/delivery_record.rs": 3,
        "src/services/discord/turn_bridge/terminal_controller_cutover.rs": 1,
    },
    # 2 -> 3 in #5071 T1 S7: `record_recovery_terminal_delivery` joins
    # `shadow_mirror_delivered_frontier` and `record_watcher_terminal_delivery`
    # as the funnel's third private caller.
    "shadow_mirror_delivered_frontier_inner": {
        "src/services/discord/outbound/delivery_record.rs": 3,
    },
    "record_delivered_frontier_with_body": {
        "src/services/discord/turn_bridge/terminal_delivery.rs": 1,
        "src/services/discord/turn_bridge/terminal_outcome_delivery.rs": 1,
        "src/services/discord/turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs": 1,
    },
    "record_long_chunk_terminal_delivery": {
        "src/services/discord/turn_bridge/terminal_controller_cutover.rs": 2,
    },
    "record_watcher_terminal_delivery": {
        "src/services/discord/tmux_watcher/terminal_long_chunks.rs": 2,
    },
    # #5071 T1 S7. The recovery family's single durable-frontier entry point,
    # and the replacement for the three raw calls this slice removed from
    # `recovery_engine/terminal_text_idempotency.rs`
    # (`write_delivered_frontier`, `write_proven_gone_equal_range_frontier` and
    # the `append_completed_turn` above them). Pinned at exactly one production
    # caller: a second one would mean a second recovery write path.
    "record_recovery_terminal_delivery": {
        "src/services/discord/recovery_engine/terminal_text_idempotency.rs": 1,
    },
    # -- store 1: write APIs with no production caller, pinned at zero -------
    # Each is `pub(in crate::services::discord)` and compiles, so the compiler
    # does not hold this boundary; only this map does. Reached only from
    # `#[cfg(test)]` code, or (for the lease/record lifecycle three) not even
    # that -- their tests drive the private `*_at` variants instead.
    "write_confirmed_delivery": {},
    # #5071 T1 S7: its only production caller was the recovery path's
    # proven-GONE re-anchor, which now selects
    # `EqualRangeAnchorPolicy::ReplaceProvenGone` through the funnel instead.
    # UNLIKE THE OTHER FIVE this one is `#[cfg(test)]`, so its zero is held by
    # the compiler too and this entry is the weaker of the two guards -- it is
    # kept so that un-gating the fn without a call still reads as a deliberate
    # edit here, and so the symbol cannot re-enter production silently.
    "write_proven_gone_equal_range_frontier": {},
    "upsert_lease": {},
    "clear_lease": {},
    "delete_record": {},
    "shadow_mirror_same_channel_frontier_with_body": {},
    # -- store 1 + store 3: the fresh-sink family's mutation funnel -----------
    # These three are not writers themselves; they are the fresh-sink family's
    # only route into `WatcherDeliveryMutation::{advance, persist}`
    # (tmux_watcher/terminal_long_chunks.rs:97 and :111), whose bodies call
    # `advance_watcher_confirmed_end_for_generation` and
    # `dr::record_watcher_terminal_delivery` -- both pinned above. They are
    # pinned separately because the funnel bottom cannot distinguish a new
    # fresh-sink entry point from an existing one, and S7's whole subject is
    # which paths reach the funnel.
    "begin_sink_delivery_mutation": {
        "src/services/discord/session_relay_sink/delivery_frontier.rs": 1,
        "src/services/discord/session_relay_sink/short_controller.rs": 1,
    },
    "persist_sink_delivery": {
        "src/services/discord/session_relay_sink/delivery_frontier.rs": 1,
        "src/services/discord/session_relay_sink/short_controller.rs": 1,
    },
    "finish_sink_delivery": {
        "src/services/discord/session_relay_sink.rs": 3,
        "src/services/discord/session_relay_sink/task_notification_context.rs": 1,
    },
    # -- store 2: completed-turn ledger --------------------------------------
    # 3 -> 2 in #5071 T1 S7: the recovery path no longer appends the
    # completed-turn ledger itself. Both remaining sites are the funnel's own --
    # the unknown-generation branch and the post-persist branch -- which is the
    # ordering (D5) the slice was about.
    "append_completed_turn": {
        "src/services/discord/outbound/delivery_record.rs": 2,
    },
    # -- store 3: in-memory watermark CAS ------------------------------------
    "advance_watcher_confirmed_end": {
        "src/services/discord/session_relay_sink.rs": 1,
        "src/services/discord/session_relay_sink/delivery_commit.rs": 1,
        "src/services/discord/tmux.rs": 1,
        "src/services/discord/tmux_watcher.rs": 1,
        "src/services/discord/tmux_watcher/loop_poll_prologue.rs": 1,
        "src/services/discord/tmux_watcher/no_result_exits.rs": 1,
        "src/services/discord/tmux_watcher/terminal_preflight.rs": 2,
        "src/services/discord/turn_finalizer/delivery_lease.rs": 1,
    },
    "advance_watcher_confirmed_end_for_generation": {
        "src/services/discord/tmux_watcher/terminal_long_chunks.rs": 1,
    },
}

# Scanning `src/` from the filesystem rather than `git ls-files` is deliberate:
# an untracked new module is still a new call site, and the point of dropping
# anchors is that no file gets to be invisible.
SCAN_ROOT = Path("src")

# `\b<symbol>\s*(` -- the trailing `(` is what makes longer names with the same
# prefix (`write_delivered_frontier_at`, `..._guarded_at_with_before_lock`,
# `shadow_mirror_delivered_frontier_inner` vs `..._frontier`,
# `record_delivered_content_fingerprint_for_generation`) NOT match: `_` is not
# whitespace. `\s*` tolerates the blank left by a stripped inline comment.
def _call_re(symbol: str) -> re.Pattern[str]:
    return re.compile(rf"\b{re.escape(symbol)}\s*\(")


def _defn_re(symbol: str) -> re.Pattern[str]:
    return re.compile(rf"\bfn\s+{re.escape(symbol)}\s*\(")


CALL_RES = {symbol: _call_re(symbol) for symbol in EXPECTED_CALL_SITES}
DEFN_RES = {symbol: _defn_re(symbol) for symbol in EXPECTED_CALL_SITES}

# `#[cfg(test)]`, `#[cfg(all(test, ...))]`, `#[cfg(any(test, ...))]`.
CFG_TEST_RE = re.compile(r"#\[\s*cfg\s*\(\s*(?:all|any)?\s*\(?\s*test\b")
# An item keyword that introduces a braced body, in ITEM POSITION: at the start
# of a line, or right after the `]` closing an attribute, or right after a `}`.
# The position anchor is load-bearing. A bare `\bfn\b` also matches the `fn` in a
# field type like `cb: Option<fn(u8, u8)>,`, which would suppress the comma rule
# for exactly the struct-field shape the comma rule exists to handle. See the
# DEVIATION note in the module docstring.
ITEM_START_RE = re.compile(
    r"(?:^|\]|\})\s*"
    r"(?:pub(?:\([^)]*\))?\s+)?"
    r"(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?"
    r'(?:extern\s+(?:"[^"]*"\s+)?)?'
    r"(?:fn|impl|mod|trait|macro_rules)\b"
)

# --- Cross-line string/comment stripper, ported verbatim from
# scripts/check_inflight_blind_save_ratchet.py (#4259), which ported it from
# scripts/check_log_key_drift.py (#4218). Copied rather than imported so each
# guard stays a dependency-free single-file script, per the convention those two
# state; if a bug is found here, fix it in all three. Blanked output keeps
# `{` / `}` / `;` / `,` counts honest for the cfg(test) tracking below: without
# cross-line state, an unbalanced `{` inside a multi-line raw string poisons the
# brace depth and hides every later call in the file. ---

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


def is_test_file(name: str) -> bool:
    return name == "tests.rs" or name.endswith("_tests.rs")


def production_lines(path: Path):
    """Yield ``(lineno, stripped_code, is_production)`` for one Rust file."""
    state = StripState()
    brace_depth = 0
    group_depth = 0  # `(` and `[` nesting, so `;` / `,` inside them are ignored
    mode = "normal"  # normal | armed (saw cfg(test) attr) | skip (in test item)
    skip_start_depth = 0
    saw_body_keyword = False
    for lineno, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        code = strip_line(raw, state)
        countable = mode == "normal"
        arm_at = None
        if mode == "normal":
            match = CFG_TEST_RE.search(code)
            if match:
                # Arm at the end of the attribute's own `test` token, so the
                # attribute's trailing `)]` are accounted the same way its
                # leading `#[cfg(` were, and a `,` inside `all(test, ...)`
                # cannot resolve the arm it is part of.
                arm_at = match.end()
        # Resolve the item-start keyword position BEFORE walking the line: the
        # comma that must not disarm (`-> HashMap<String, u64> {`) sits on the
        # same line as the `fn` that suppresses the comma rule, so deciding at
        # end-of-line is one line too late.
        item_start = ITEM_START_RE.search(code, arm_at if arm_at is not None else 0)
        item_start_at = item_start.end() if item_start else None
        for index, char in enumerate(code):
            if arm_at is not None and index == arm_at:
                mode = "armed"
                saw_body_keyword = False
            if item_start_at is not None and index >= item_start_at:
                saw_body_keyword = True
            if char in "([":
                group_depth += 1
            elif char in ")]":
                group_depth -= 1
            elif char == "{":
                if mode == "armed":
                    mode = "skip"
                    skip_start_depth = brace_depth
                brace_depth += 1
            elif char == "}":
                brace_depth -= 1
                if mode == "skip" and brace_depth <= skip_start_depth:
                    mode = "normal"
            elif char == ";" and mode == "armed" and group_depth <= 0:
                mode = "normal"  # `#[cfg(test)] use ...;` / `mod tests;`
            elif (
                char == ","
                and mode == "armed"
                and group_depth <= 0
                and not saw_body_keyword
            ):
                mode = "normal"  # `#[cfg(test)] field: T,` / enum variant
        yield lineno, code, countable


def production_call_sites(root: Path) -> tuple[dict[str, dict[str, int]], int, int]:
    """Return ``(counts, scanned_files, skipped_test_files)`` over ``src/``."""
    found: dict[str, dict[str, int]] = {symbol: {} for symbol in EXPECTED_CALL_SITES}
    scanned = 0
    skipped = 0
    scan_root = root / SCAN_ROOT
    for path in sorted(scan_root.rglob("*.rs")):
        if not path.is_file():
            continue
        if is_test_file(path.name):
            skipped += 1
            continue
        scanned += 1
        rel = path.relative_to(root).as_posix()
        for _lineno, code, countable in production_lines(path):
            if not countable:
                continue
            for symbol, call_re in CALL_RES.items():
                if DEFN_RES[symbol].search(code):
                    continue
                hits = len(call_re.findall(code))
                if hits:
                    found[symbol][rel] = found[symbol].get(rel, 0) + hits
    return found, scanned, skipped


LIMITS = (
    "lexical scan of stripped source, not Rust parsing; not proof of reachability; "
    "`use .. as x` aliases, re-export renames, name-constructing macros and calls "
    "through values or trait objects are NOT seen; cfg other than cfg(test) is not "
    "evaluated; textual occurrences are counted once regardless of macro expansion"
)


def check(root: Path) -> tuple[bool, str]:
    found, scanned, skipped = production_call_sites(root)
    problems: list[str] = []
    for symbol, expected in EXPECTED_CALL_SITES.items():
        actual = found[symbol]
        if actual == expected:
            continue
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
    total_expected = sum(sum(m.values()) for m in EXPECTED_CALL_SITES.values())
    total_actual = sum(sum(m.values()) for m in found.values())
    zero_pinned = sorted(s for s, m in EXPECTED_CALL_SITES.items() if not m)
    header = (
        f"durable frontier writer call sites: {total_actual} production sites across "
        f"{len(EXPECTED_CALL_SITES)} symbols "
        f"({len(zero_pinned)} pinned at zero: {', '.join(zero_pinned)}); "
        f"scanned {scanned} Rust files under {SCAN_ROOT.as_posix()}/, "
        f"skipped {skipped} test files; ({LIMITS})"
    )
    if problems:
        detail = "\n  ".join(problems)
        return False, (
            f"FAIL: durable frontier writer call sites moved (expected {total_expected}, "
            f"found {total_actual}).\n  {detail}\n"
            "Update EXPECTED_CALL_SITES in scripts/check_durable_frontier_writer_call_sites.py "
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
