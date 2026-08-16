#!/usr/bin/env python3
"""Per-file exact-count allowlist for the durable frontier writer symbols (#5071 T1 S8).

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

  * ALIAS AND RE-EXPORT. The CALL-SITE subscan does not see
    `use ...::write_delivered_frontier as w;` followed by `w(..)`, nor a
    renamed `pub use`. S8-1b's separate `EXPECTED_PINNED_USE_ALIASES` scan
    rejects aliases whose captured spelling starts with ASCII `[A-Za-z_]`.
    That includes a raw-identifier alias such as `r#w` (the lexical matcher
    captures its ASCII `r` prefix); an alias whose first character is non-ASCII
    remains unseen. A module alias is different and IS seen, because the
    function name survives it: `dr::f(..)`, `delivery_record::f(..)` and a bare
    `f(..)` all match identically.
  * NAME-CONSTRUCTING MACROS. A macro whose body contains the literal spelling
    IS counted (the text is there, once per textual occurrence, and expansion
    count is invisible to a text scan -- a call inside a macro invoked three
    times counts 1). A macro that ASSEMBLES the name (`paste!`,
    `concat_idents!`) is NOT seen.
  * INDIRECTION THROUGH VALUES. `let f = write_delivered_frontier;` is not a
    CALL-SITE match (no `(` follows the name) and the later `f(..)` is not one
    either. S8-1b's bare-reference scan catches the named capture itself.
    Receiver-method matching is type-agnostic, so
    `obj.reset_confirmed_frontier(..)` is counted even when `obj` is a trait
    object; the lexical match cannot prove that the receiver has the pinned
    type.
  * NAME COLLISION -- PRESENT IN THIS TREE, NOT HYPOTHETICAL. Two distinct
    functions are both named `record_watcher_terminal_delivery`: the durable
    funnel at `outbound/delivery_record.rs:2520` and a local wrapper at
    `tmux_watcher/terminal_long_chunks.rs:168`. The two production sites this
    map pins for that file are one call to each (`dr::…` at :124, bare at
    :222). The integer is therefore a count of the SPELLING, not of calls to
    one function. It is still the right pin here because the wrapper's only
    job is to reach the funnel, but a future collision under a pinned name
    would be counted the same way and the map would not say so.
    `EXPECTED_DEFINITION_FILE_COUNTS` below closes that hole: every pinned
    spelling whose production `fn` definitions occupy two or more files must
    have an exact per-file definition map. The current collision is
    `2 = leaf 1 + wrapper 1`.
  * SPELLING COUPLING. Because the completion criterion is a literal grep, the
    whole contract is bound to the spelling. Renaming a symbol does not weaken
    the gate silently -- the count drops to 0 and it fails -- but any route that
    reaches the same code under a different spelling is outside it.
  * CFG. The lexical cfg classifier excludes `#[cfg(test)]` and cfg expressions
    that require `test` (including nested `all(test, ...)`) from production,
    while `cfg(any(test, X))` and `cfg(not(test))` remain production because
    they have a non-test configuration. Other target/feature predicates are
    not compiler-evaluated, so a target-gated call counts on every target.
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

S8 RAW ATOMIC AND FUNCTION-VALUE GATES. `EXPECTED_RAW_ATOMIC_MUTATIONS` counts
`confirmed_end_offset` calls to an explicit allowlist of direct
AtomicU64/AtomicUsize mutator methods (`compare_exchange`,
`compare_exchange_weak`, `store`, `swap`, the listed `fetch_*` mutators, and
legacy `compare_and_swap`) over stripped production file text (not per-line
text, so rustfmt receiver chains are seen).
`EXPECTED_BARE_REFERENCES` counts pinned names that are neither calls,
definitions, nor `use` items and pins the current historical function-value
capture. `EXPECTED_PINNED_USE_ALIASES` rejects `use ..::<pinned> as <other>;`;
the `delivery_record as dr` module alias is outside this rule. Output totals
are derived from these maps.

WHAT IT DOES BUY. Every supported textual call site in every regular `.rs` file
under `src/`, per file, is exactly counted. Supported method forms include the
receiver form (`coord.reset_confirmed_frontier(...)`) and pinned UFCS form
(`TmuxRelayCoord::reset_confirmed_frontier(coord, ...)`); the raw atomic gate
enumerates the direct method spellings above. The gate enumerates all regular
files first and fails closed if any non-`.rs` file is present, so an extension
cannot make a file invisible. That is strictly more than the model gate: it has
no anchor to escape and no boolean to saturate.

PRODUCTION vs TEST. Files named `tests.rs` / `*_tests.rs` and files the shared
inventory resolver classifies as test-only are skipped whole. Every such path
must pass the single lexical pin in `scripts/test_only_module_skip_pin.py`;
`src/` file/directory symlinks are rejected and the skipped census must equal
the pin count. The symlink check is lexical rather than atomic against a
post-enumeration replacement; CI assumes a static checkout while the gate
runs. In remaining files, only cfg expressions proven by the lexical boolean
classifier to require `test` are stripped; `cfg(any(test, X))` is intentionally
scanned as production.

RESOLVER NON-GUARANTEES. The reused resolver is intentionally unchanged and
does not guarantee at least seven measured forms: `#[path]` separated from
`mod` by a comment; macro-generated `mod`; `cfg(not(test))` plus `include!`;
`cfg(any(test, feature))` plus `include!`; `cfg_attr(path=)`; raw-string
`#[path]`; or ungated `include!`. The pin guarantees set membership only: a
content change can make an already pinned file production-reachable without a
set delta. Compiler-backed reachability is follow-up slice work.

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

import importlib.util
import re
import sys
from collections.abc import Iterable
from pathlib import Path

try:
    from rust_cfg import find_test_only_cfg_attribute as _cfg_test_only_match
except ModuleNotFoundError:  # imported from a repo-root unittest
    from scripts.rust_cfg import find_test_only_cfg_attribute as _cfg_test_only_match

try:
    from rust_lex import StripState, strip_line
except ModuleNotFoundError:  # imported from a repo-root unittest
    from scripts.rust_lex import StripState, strip_line

# Every production call site of every pinned symbol, keyed by symbol then by
# repo-relative path. An empty map pins the symbol at zero production callers:
# the API exists and compiles, but nothing outside tests may reach it. Measured
# on e60416050248aaa4d2157dd3077b1edfc099cb76 (the S8-1b base).
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
    # -- S8-1b: previously unpinned pinned-source and bridge writers ---------
    "record_current_pinned_delivery": {
        "src/services/discord/turn_bridge/terminal_delivery.rs": 1,
    },
    "record_pinned_delivery_metadata": {
        "src/services/discord/turn_bridge/terminal_delivery.rs": 1,
    },
    # The current writes are reached through a function-value binding; 1b pins
    # that bare reference and 1c will flatten the calls. Direct count is zero.
    "record_historical_pinned_delivery": {},
    "advance_tmux_relay_confirmed_end": {
        "src/services/discord/turn_bridge/terminal_delivery.rs": 1,
        "src/services/discord/turn_bridge/terminal_controller_cutover.rs": 2,
    },
    # Qualified key: this is a receiver-including TmuxRelayCoord method.
    "TmuxRelayCoord::reset_confirmed_frontier": {
        "src/services/discord/tmux_session_files.rs": 2,
    },
}

# Collision pin: two or more production definition files require exact counts.
EXPECTED_DEFINITION_FILE_COUNTS: dict[str, dict[str, int]] = {
    "record_watcher_terminal_delivery": {
        "src/services/discord/outbound/delivery_record.rs": 1,
        "src/services/discord/tmux_watcher/terminal_long_chunks.rs": 1,
    },
}

# Raw atomic pin by file; totals are derived from the map.
EXPECTED_RAW_ATOMIC_MUTATIONS: dict[str, int] = {
    "src/services/discord/tmux.rs": 1,
    "src/services/discord/relay_health/frontier.rs": 1,
    "src/services/discord/turn_bridge/terminal_delivery.rs": 1,
}

# Bare-reference pin by symbol/file; current capture is the `record` binding.
EXPECTED_BARE_REFERENCES: dict[str, dict[str, int]] = {
    "record_historical_pinned_delivery": {
        "src/services/discord/turn_bridge/terminal_delivery.rs": 1,
    },
}

# Pinned-function renames in `use` items are prohibited; empty is a deliberate
# zero pin and aggregate output is derived by summing it.
EXPECTED_PINNED_USE_ALIASES: dict[str, dict[str, int]] = {}

# Scanning `src/` from the filesystem rather than `git ls-files` is deliberate:
# an untracked new module is still a new call site. Every regular `.rs` file
# under `src/` is enumerated; a regular non-`.rs` file is rejected before any
# skip/resolver classification, so no regular source file under `src/` gets to
# be invisible. Call sites in files reached by `#[path]`/`include!` targets
# resolving outside `src/` are not seen; fail-closed handling for that boundary
# is follow-up slice work.
SCAN_ROOT = Path("src")


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

# `\b<symbol>\s*(` -- the trailing `(` is what makes longer names with the same
# prefix (`write_delivered_frontier_at`, `..._guarded_at_with_before_lock`,
# `shadow_mirror_delivered_frontier_inner` vs `..._frontier`,
# `record_delivered_content_fingerprint_for_generation`) NOT match: `_` is not
# whitespace. `\s*` tolerates the blank left by a stripped inline comment.
def _symbol_basename(symbol: str) -> str:
    """Return the function spelling from a simple or qualified map key."""

    return symbol.rsplit("::", 1)[-1]


def _call_re(symbol: str) -> re.Pattern[str]:
    basename = _symbol_basename(symbol)
    if "::" in symbol:
        # Calls to a method are pinned in receiver-including form. The receiver
        # is intentionally lexical (an identifier or a chained `)`), because
        # Rust type resolution is outside this gate; a bare
        # `reset_confirmed_frontier()` is not this pinned method entry point.
        # The second branch is the UFCS spelling (`Type::method(receiver, ...)`)
        # for this pinned type/method pair. Inside the type's impl, `Self` is
        # the same natural receiver type; angle-bracketed UFCS may also end in
        # a fully qualified path to the pinned type. Keep the terminal type
        # exact: a lexical scan cannot prove that another type's same-named
        # method is this writer, and `Self` is counted in any impl rather than
        # only an impl of the pinned type.
        type_name, _ = symbol.rsplit("::", 1)
        terminal_type = rf"(?:{re.escape(type_name)}|Self)"
        ufcs_type = (
            rf"(?:\b{terminal_type}\b"
            rf"|<\s*(?:(?:\b(?:crate|self|super)\b|[A-Za-z_]\w*)\s*::\s*)*"
            rf"{terminal_type}\s*>)"
        )
        return re.compile(
            rf"(?:"
            rf"(?:\b[A-Za-z_]\w*|\))\s*\.\s*{re.escape(basename)}"
            rf"|{ufcs_type}\s*::\s*{re.escape(basename)}"
            rf")\s*\("
        )
    return re.compile(rf"\b{re.escape(basename)}\s*\(")


def _defn_re(symbol: str) -> re.Pattern[str]:
    return re.compile(rf"\bfn\s+{re.escape(_symbol_basename(symbol))}\s*\(")


CALL_RES = {symbol: _call_re(symbol) for symbol in EXPECTED_CALL_SITES}
DEFN_RES = {symbol: _defn_re(symbol) for symbol in EXPECTED_CALL_SITES}

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

# Cross-line string/comment stripper imported from scripts/rust_lex.py.
# Blanked output keeps `{` / `}` / `;` / `,` counts honest for the cfg(test)
# tracking below: without cross-line state, an unbalanced `{` inside a
# multi-line raw string poisons the brace depth and hides every later call.


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
            match = _cfg_test_only_match(code)
            if match:
                # Arm at the closing parenthesis of the complete test-only
                # cfg expression, so commas inside nested `all(...)`/`any(...)`
                # cannot resolve the arm they are part of.
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


def _production_text(path: Path) -> str:
    """Return stripped production text while preserving cross-line chains."""

    lines: list[str] = []
    for _lineno, code, countable in production_lines(path):
        if countable:
            # Mask compact `#[cfg(test)] fn ... { raw_atomic(); }` items too.
            cfg_test = _cfg_test_only_match(code)
            if cfg_test:
                code = code[: cfg_test.start()] + " " * (len(code) - cfg_test.start())
        lines.append(code if countable else " " * len(code))
    return "\n".join(lines)


def _scan_inputs(
    root: Path,
    pinned_test_only_files: Iterable[str],
) -> tuple[list[Path], frozenset[Path]]:
    return _SKIP_PIN.validated_scan_files(
        root,
        SCAN_ROOT,
        is_test_file,
        pinned_paths=pinned_test_only_files,
    )


def _production_paths(
    all_files: list[Path],
    whole_file_skips: frozenset[Path],
) -> list[Path]:
    return [path for path in all_files if path not in whole_file_skips]


def _production_texts(
    all_files: list[Path],
    whole_file_skips: frozenset[Path],
) -> dict[Path, str]:
    return {
        path: _production_text(path)
        for path in _production_paths(all_files, whole_file_skips)
    }


def _call_count_outside_definitions(text: str, symbol: str) -> int:
    definitions = [match.span() for match in DEFN_RES[symbol].finditer(text)]
    return sum(
        1
        for match in CALL_RES[symbol].finditer(text)
        if not any(start <= match.start() < end for start, end in definitions)
    )


def production_call_sites(
    root: Path,
    *,
    pinned_test_only_files: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
    _scan: tuple[list[Path], frozenset[Path]] | None = None,
    _texts: dict[Path, str] | None = None,
) -> tuple[dict[str, dict[str, int]], int, int]:
    """Return ``(counts, scanned_files, skipped_test_files)`` over ``src/``."""
    found: dict[str, dict[str, int]] = {symbol: {} for symbol in EXPECTED_CALL_SITES}
    all_files, whole_file_skips = _scan or _scan_inputs(root, pinned_test_only_files)
    scanned = 0
    skipped = 0
    for path in all_files:
        if path in whole_file_skips:
            skipped += 1
            continue
        scanned += 1
        rel = path.relative_to(root).as_posix()
        text = _texts[path] if _texts is not None else _production_text(path)
        for symbol in EXPECTED_CALL_SITES:
            hits = _call_count_outside_definitions(text, symbol)
            if hits:
                found[symbol][rel] = hits
    return found, scanned, skipped


def production_definition_file_counts(
    root: Path,
    *,
    pinned_test_only_files: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
    _scan: tuple[list[Path], frozenset[Path]] | None = None,
    _texts: dict[Path, str] | None = None,
) -> dict[str, dict[str, int]]:
    all_files, whole_file_skips = _scan or _scan_inputs(root, pinned_test_only_files)
    found: dict[str, dict[str, int]] = {
        symbol: {} for symbol in EXPECTED_CALL_SITES
    }
    for path in _production_paths(all_files, whole_file_skips):
        rel = path.relative_to(root).as_posix()
        text = _texts[path] if _texts is not None else _production_text(path)
        for symbol in EXPECTED_CALL_SITES:
            count = len(DEFN_RES[symbol].findall(text))
            if count:
                found[symbol][rel] = count
    return found


# Keep this list explicit rather than accepting an open-ended `fetch_*`: the
# gate pins these direct mutator method spellings, including the legacy spelling
# retained by older code. It does not claim the complete mutating API surface:
# `as_ptr`/`get_mut` can expose storage for mutation through raw pointers or
# UnsafeCell, and those paths are outside this lexical gate.
RAW_ATOMIC_MUTATORS = (
    "compare_exchange",
    "compare_exchange_weak",
    "store",
    "swap",
    "fetch_add",
    "fetch_sub",
    "fetch_and",
    "fetch_or",
    "fetch_xor",
    "fetch_nand",
    "fetch_max",
    "fetch_min",
    "fetch_update",
    "compare_and_swap",
)
RAW_ATOMIC_RE = re.compile(
    r"\bconfirmed_end_offset\s*\.\s*(?:"
    + "|".join(re.escape(mutator) for mutator in RAW_ATOMIC_MUTATORS)
    + r")\s*\("
)


def production_raw_atomic_mutations(
    root: Path,
    *,
    pinned_test_only_files: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
    _scan: tuple[list[Path], frozenset[Path]] | None = None,
    _texts: dict[Path, str] | None = None,
) -> dict[str, int]:
    """Count raw confirmed-end mutations over whole stripped file text."""

    all_files, whole_file_skips = _scan or _scan_inputs(root, pinned_test_only_files)
    found: dict[str, int] = {}
    for path in _production_paths(all_files, whole_file_skips):
        text = _texts[path] if _texts is not None else _production_text(path)
        count = len(RAW_ATOMIC_RE.findall(text))
        if count:
            found[path.relative_to(root).as_posix()] = count
    return found


USE_ITEM_RE = re.compile(r"\b(?:pub(?:\([^)]*\))?\s+)?use\b.*?;", re.DOTALL)
CALL_TAIL_RE = re.compile(r"\s*(?:::\s*<[^>]*>)?\s*\(")
ALIAS_TAIL_RE = re.compile(r"\s+as\s+([A-Za-z_]\w*)\b")


def _bare_reference_counts_in_text(
    text: str,
    symbols: Iterable[str],
) -> tuple[dict[str, int], dict[str, int]]:
    """Return bare-reference and prohibited-alias counts for one file."""

    use_spans = [match.span() for match in USE_ITEM_RE.finditer(text)]
    bare: dict[str, int] = {}
    aliases: dict[str, int] = {}
    symbol_list = tuple(symbols)
    by_basename: dict[str, list[str]] = {}
    for symbol in symbol_list:
        by_basename.setdefault(_symbol_basename(symbol), []).append(symbol)
    token_re = re.compile(
        r"\b(?:" + "|".join(re.escape(name) for name in by_basename) + r")\b"
    )
    definition_re = re.compile(
        r"\bfn\s+(?P<name>(?:"
        + "|".join(re.escape(name) for name in by_basename)
        + r"))\s*\("
    )
    definition_tokens = {
        (match.start("name"), match.end("name"))
        for match in definition_re.finditer(text)
    }
    for token in token_re.finditer(text):
        start, end = token.span()
        basename = token.group(0)
        in_use_span = next(
            (
                (begin, finish)
                for begin, finish in use_spans
                if begin <= start < finish
            ),
            None,
        )
        in_definition = (start, end) in definition_tokens
        is_call = bool(CALL_TAIL_RE.match(text, end))
        for symbol in by_basename[basename]:
            if in_use_span is not None:
                use_tail = text[end : in_use_span[1]]
                alias = ALIAS_TAIL_RE.match(use_tail)
                if alias and alias.group(1) != "_":
                    aliases[symbol] = aliases.get(symbol, 0) + 1
            elif not in_definition and not is_call:
                bare[symbol] = bare.get(symbol, 0) + 1
    return bare, aliases


def production_bare_references(
    root: Path,
    *,
    pinned_test_only_files: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
    _scan: tuple[list[Path], frozenset[Path]] | None = None,
    _texts: dict[Path, str] | None = None,
) -> tuple[dict[str, dict[str, int]], dict[str, dict[str, int]]]:
    """Count bare pinned references and prohibited `use` aliases by file."""

    all_files, whole_file_skips = _scan or _scan_inputs(root, pinned_test_only_files)
    bare: dict[str, dict[str, int]] = {symbol: {} for symbol in EXPECTED_CALL_SITES}
    aliases: dict[str, dict[str, int]] = {
        symbol: {} for symbol in EXPECTED_CALL_SITES
    }
    for path in _production_paths(all_files, whole_file_skips):
        rel = path.relative_to(root).as_posix()
        file_bare, file_aliases = _bare_reference_counts_in_text(
            _texts[path] if _texts is not None else _production_text(path),
            EXPECTED_CALL_SITES,
        )
        for symbol, count in file_bare.items():
            bare[symbol][rel] = count
        for symbol, count in file_aliases.items():
            aliases[symbol][rel] = count
    return bare, aliases


LIMITS = (
    "lexical scan of stripped source, not Rust parsing; not proof of reachability; "
    "call-site matching covers direct calls, receiver method calls, and pinned "
    "`Type::method(receiver, ...)`, `Self::method(self, ...)`, `<Type>::method`, "
    "and `<Self>::method` UFCS calls (including angle-bracketed paths ending in "
    "the pinned type); receiver matching is type-agnostic, so same-named methods "
    "on any receiver, including trait objects, fail closed without proving the "
    "receiver type; renamed calls through `use .. as x` or re-exports are not "
    "resolved; raw-identifier or non-ASCII alias spellings are not uniformly "
    "unseen: the pinned-alias subgate rejects aliases whose captured spelling "
    "starts with ASCII `[A-Za-z_]`, including the ASCII prefix of raw-identifier "
    "aliases, while aliases whose first character is non-ASCII remain unseen; "
    "calls through values are not resolved, but the bare-reference subgate rejects "
    "a named capture; name-constructing macros, type-alias receiver UFCS, and "
    "associated-projection receiver calls remain unseen; the cfg "
    "classifier treats cfg(test)/test-required all(...) as test-only and "
    "cfg(any(test, X))/cfg(not(test)) as production without compiler target "
    "evaluation; non-.rs regular files fail closed before classification; whole-file "
    "skips use one lexical pin, reject src symlinks, and check "
    "the skipped census; symlink rejection is non-atomic outside a static CI "
    "checkout; the scan root is `src/`; call sites in files reached by "
    "`#[path]`/`include!` targets resolving outside `src/` are not seen; "
    "fail-closed handling for that boundary is follow-up work; at least seven "
    "resolver forms are not guaranteed (path/mod comment, "
    "macro mod, two cfg/include forms, cfg_attr path, raw path, ungated include); pin "
    "membership cannot detect production reachability changes inside pinned files; "
    "compiler-backed reachability is follow-up work; textual occurrences are counted "
    "once regardless of macro expansion; the raw atomic "
    "scan does not see dereferenced stores such as `(*ptr).store(...)` or mutation "
    "through `as_ptr`/`get_mut` plus raw-pointer/UnsafeCell access, nor receiver UFCS "
    "such as `AtomicU64::store(&field, ...)`; raw atomic and "
    "bare-reference gates are likewise lexical and do not prove runtime "
    "reachability; replacing these enumerated regexes with AST/`syn`-based Rust "
    "parsing is tracked by follow-up issue #5370"
)


def _nested_map_problems(
    label: str,
    expected: dict[str, dict[str, int]],
    actual: dict[str, dict[str, int]],
    *,
    missing_word: str,
) -> list[str]:
    problems: list[str] = []
    for symbol in sorted(set(expected) | set(actual)):
        wanted = expected.get(symbol, {})
        found = actual.get(symbol, {})
        for rel in sorted(set(wanted) | set(found)):
            want = wanted.get(rel, 0)
            have = found.get(rel, 0)
            if want == have:
                continue
            if want == 0:
                problems.append(
                    f"{symbol}: {missing_word} in {rel} ({have}x)"
                )
            elif have == 0:
                problems.append(
                    f"{symbol}: {label} gone from {rel} (expected {want}x)"
                )
            else:
                problems.append(
                    f"{symbol}: {label} in {rel} has {have}x, expected {want}x"
                )
    return problems


def _flat_map_problems(
    label: str,
    expected: dict[str, int],
    actual: dict[str, int],
) -> list[str]:
    """Compare a file/count pin and return deterministic diagnostics."""

    problems: list[str] = []
    for rel in sorted(set(expected) | set(actual)):
        want = expected.get(rel, 0)
        have = actual.get(rel, 0)
        if want == have:
            continue
        if want == 0:
            problems.append(f"{label}: UNLISTED mutation in {rel} ({have}x)")
        elif have == 0:
            problems.append(
                f"{label}: mutation gone from {rel} (expected {want}x)"
            )
        else:
            problems.append(
                f"{label}: {rel} has {have}x, expected {want}x"
            )
    return problems


def _definition_collision_problems(
    root: Path,
    definitions: dict[str, dict[str, int]],
) -> list[str]:
    """Require a machine-readable map for every observed multi-file collision."""

    problems: list[str] = []
    for symbol, actual in sorted(definitions.items()):
        if len(actual) < 2:
            continue
        expected = EXPECTED_DEFINITION_FILE_COUNTS.get(symbol)
        if expected == actual:
            continue
        if expected is None:
            problems.append(
                f"{symbol}: {len(actual)} production definition files require "
                "an EXPECTED_DEFINITION_FILE_COUNTS entry"
            )
        else:
            problems.append(
                f"{symbol}: definition file map has {actual!r}, expected {expected!r}"
            )

    # On the real checkout, also catch a deleted/moved collision definition;
    # synthetic fixtures omit Cargo.toml and use the observed branch above.
    if (root / "Cargo.toml").is_file():
        for symbol, expected in sorted(EXPECTED_DEFINITION_FILE_COUNTS.items()):
            actual = definitions.get(symbol, {})
            if actual != expected:
                problems.append(
                    f"{symbol}: definition file map has {actual!r}, expected {expected!r}"
                )
    return problems


def check(
    root: Path,
    *,
    pinned_test_only_files: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
) -> tuple[bool, str]:
    try:
        scan = _scan_inputs(root, pinned_test_only_files)
        texts = _production_texts(*scan)
        found, scanned, skipped = production_call_sites(
            root,
            pinned_test_only_files=pinned_test_only_files,
            _scan=scan,
            _texts=texts,
        )
        definitions = production_definition_file_counts(
            root,
            pinned_test_only_files=pinned_test_only_files,
            _scan=scan,
            _texts=texts,
        )
        raw_atomic = production_raw_atomic_mutations(
            root,
            pinned_test_only_files=pinned_test_only_files,
            _scan=scan,
            _texts=texts,
        )
        bare_references, use_aliases = production_bare_references(
            root,
            pinned_test_only_files=pinned_test_only_files,
            _scan=scan,
            _texts=texts,
        )
    except RuntimeError as exc:
        return False, str(exc)
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
    problems.extend(_definition_collision_problems(root, definitions))
    problems.extend(
        _flat_map_problems(
            "raw confirmed_end_offset atomic mutations",
            EXPECTED_RAW_ATOMIC_MUTATIONS,
            raw_atomic,
        )
    )
    problems.extend(
        _nested_map_problems(
            "bare reference",
            EXPECTED_BARE_REFERENCES,
            bare_references,
            missing_word="UNLISTED bare reference",
        )
    )
    problems.extend(
        _nested_map_problems(
            "prohibited use alias",
            EXPECTED_PINNED_USE_ALIASES,
            use_aliases,
            missing_word="PROHIBITED use alias",
        )
    )
    total_expected = sum(sum(m.values()) for m in EXPECTED_CALL_SITES.values())
    total_actual = sum(sum(m.values()) for m in found.values())
    zero_pinned = sorted(s for s, m in EXPECTED_CALL_SITES.items() if not m)
    raw_expected_total = sum(EXPECTED_RAW_ATOMIC_MUTATIONS.values())
    raw_actual_total = sum(raw_atomic.values())
    bare_expected_total = sum(
        sum(files.values()) for files in EXPECTED_BARE_REFERENCES.values()
    )
    bare_actual_total = sum(sum(files.values()) for files in bare_references.values())
    alias_expected_total = sum(
        sum(files.values()) for files in EXPECTED_PINNED_USE_ALIASES.values()
    )
    alias_actual_total = sum(sum(files.values()) for files in use_aliases.values())
    header = (
        f"durable frontier writer call sites: {total_actual} production sites across "
        f"{len(EXPECTED_CALL_SITES)} symbols "
        f"({len(zero_pinned)} pinned at zero: {', '.join(zero_pinned)}); "
        f"raw confirmed_end_offset atomic mutations {raw_actual_total} "
        f"(expected {raw_expected_total}); "
        f"bare pinned references {bare_actual_total} (expected {bare_expected_total}); "
        f"pinned use aliases {alias_actual_total} (expected {alias_expected_total}); "
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
