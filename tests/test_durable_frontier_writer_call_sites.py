"""Contract + discrimination tests for the durable frontier writer allowlist (#5071 T1 S7').

The tests below are split into two groups on purpose.

SOURCE CONTRACT (against the real tree) pins the measured shape of the repo: the
totals, the symbols pinned at zero, and the fact that CI actually runs the gate.
These fail when the tree moves without the map moving with it.

DISCRIMINATION (against synthetic fixtures) answers the only question that makes
a green gate worth anything: WHAT BREAKS IT. Every mutation below is applied and
asserted on, including the three that the gate provably does NOT catch --
`use .. as` aliasing, `pub use .. as` re-export renaming, and a
name-constructing macro. Those three are recorded here as MEASURED holes, not as
suspicions, and the same three are declared in the script's own docstring and in
its runtime output. A gate that hides its holes is worse than no gate.
"""

from __future__ import annotations

import importlib.util
import re
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check_durable_frontier_writer_call_sites.py"
SPEC = importlib.util.spec_from_file_location("durable_frontier_writer_guard", SCRIPT)
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)

# Measured on 0bde0675b, the base S7' branched from, then re-measured on this
# slice: #5071 T1 S7 moved five counts for a net 42 -> 41 over 23 -> 24 symbols.
# Three raw calls left `recovery_engine/terminal_text_idempotency.rs`
# (`write_delivered_frontier`, `write_proven_gone_equal_range_frontier`,
# `append_completed_turn`), one funnel call replaced them
# (`record_recovery_terminal_delivery`), and the funnel body gained its third
# private caller.
TOTAL_CALL_SITES = 41
PINNED_SYMBOLS = 24
ZERO_PINNED = {
    "write_confirmed_delivery",
    "write_proven_gone_equal_range_frontier",
    "upsert_lease",
    "clear_lease",
    "delete_record",
    "shadow_mirror_same_channel_frontier_with_body",
}


# CLASSIFIER PROBES: `(file, symbol, production_count, count_ignoring_cfg_test)`,
# each verified BY HAND against the file's `#[cfg(test)]` boundaries rather than
# taken from the scanner's own output.
#
# Why this table exists. The production/test classifier is an ORACLE the counts
# depend on: if it drifts, `EXPECTED_CALL_SITES` drifts with it and both agree
# with each other while being wrong. The pairs below break that coupling from
# both sides -- the first number moves if the classifier starts hiding
# production code, the second if the stripper or the call regex changes at all.
#
# Rows 1-4 are the shape that motivated this table. A `#[cfg(test)]` STRUCT
# FIELD in session_relay_sink.rs makes the resolver ported from
# check_inflight_blind_save_ratchet.py swallow the production impl block that
# follows it, which would report the first three of these as 0. Row 5 is the
# opposite direction: tmux.rs's only `write_confirmed_delivery(` really is
# inside `#[cfg(test)]`, so a classifier that stops excluding test regions
# turns that 0 into a 1. Row 7 pins the widest prod/test gap in the tree
# (3 of 12), and row 9 pins the file where one spelling covers two functions.
MANUAL_CLASSIFICATION = [
    ("src/services/discord/session_relay_sink.rs", "commit_ordered_jsonl_range", 1, 1),
    ("src/services/discord/session_relay_sink.rs", "record_delivered_content_fingerprint", 1, 3),
    ("src/services/discord/session_relay_sink.rs", "advance_watcher_confirmed_end", 1, 1),
    ("src/services/discord/session_relay_sink.rs", "finish_sink_delivery", 3, 4),
    ("src/services/discord/tmux.rs", "write_confirmed_delivery", 0, 1),
    ("src/services/discord/tmux.rs", "advance_watcher_confirmed_end", 1, 1),
    ("src/services/discord/outbound/delivery_record.rs", "shadow_mirror_delivered_frontier", 3, 12),
    ("src/services/discord/outbound/delivery_record.rs", "append_completed_turn", 2, 2),
    # #5071 T1 S7. Hand-verified: the recovery file's `#[cfg(test)] mod tests`
    # opens at the end of the file, and the ONE production spelling of the new
    # funnel entry point sits in `record_durable_frontier`. Both numbers pin the
    # replacement, so a raw writer sneaking back beside it moves neither.
    ("src/services/discord/recovery_engine/terminal_text_idempotency.rs",
     "record_recovery_terminal_delivery", 1, 1),
    ("src/services/discord/tmux_watcher/terminal_long_chunks.rs", "record_watcher_terminal_delivery", 2, 2),
    ("src/services/discord/turn_bridge/terminal_controller_cutover.rs", "record_long_chunk_terminal_delivery", 2, 2),
]


def write(root: Path, rel: str, body: str) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")


class SourceContractTests(unittest.TestCase):
    """Pins the real tree. These are the assertions that go red on a real move."""

    def test_real_tree_passes_and_reports_its_limits(self):
        ok, message = guard.check(ROOT)
        self.assertTrue(ok, message)
        self.assertIn(f"{TOTAL_CALL_SITES} production sites", message)
        self.assertIn(f"across {PINNED_SYMBOLS} symbols", message)
        # The success path must state its own blindness, not only the failure
        # path: a reader who only ever sees green must still learn the limits.
        for limit in ("use .. as x", "not Rust parsing", "not proof of reachability"):
            self.assertIn(limit, message)

    def test_expected_map_totals_are_pinned_independently_of_the_map(self):
        """A shrunk map plus a shrunk tree would agree with itself; this does not."""
        total = sum(sum(m.values()) for m in guard.EXPECTED_CALL_SITES.values())
        self.assertEqual(total, TOTAL_CALL_SITES)
        self.assertEqual(len(guard.EXPECTED_CALL_SITES), PINNED_SYMBOLS)
        self.assertEqual(
            {s for s, m in guard.EXPECTED_CALL_SITES.items() if not m}, ZERO_PINNED
        )

    def test_scan_root_is_all_of_src(self):
        """Narrowing the scan is the cheapest way to fake a green gate."""
        self.assertEqual(guard.SCAN_ROOT.as_posix(), "src")

    def test_ci_script_checks_runs_this_gate_and_this_module(self):
        """A gate nobody runs is the #5003 shape. Pin the wiring, not the intent."""
        wiring = (ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn("scripts/check_durable_frontier_writer_call_sites.py", wiring)
        self.assertIn("tests.test_durable_frontier_writer_call_sites", wiring)

    def test_classifier_matches_hand_verified_cfg_test_boundaries(self):
        """Breaks the oracle coupling: the classifier is checked, not trusted.

        Both numbers are asserted. The production count catches a classifier
        that starts hiding production code (the struct-field bug); the
        ignore-cfg-test count catches a stripper or regex change that would move
        both numbers together and stay self-consistent.
        """
        for rel, symbol, want_prod, want_all in MANUAL_CLASSIFICATION:
            with self.subTest(file=rel, symbol=symbol):
                call = re.compile(rf"\b{symbol}\s*\(")
                defn = re.compile(rf"\bfn\s+{symbol}\s*\(")
                prod = 0
                every = 0
                for _lineno, code, is_production in guard.production_lines(ROOT / rel):
                    if defn.search(code):
                        continue
                    hits = len(call.findall(code))
                    every += hits
                    if is_production:
                        prod += hits
                self.assertEqual(prod, want_prod, f"production count for {symbol} in {rel}")
                self.assertEqual(every, want_all, f"cfg(test)-blind count for {symbol} in {rel}")

    def test_the_two_functions_sharing_one_pinned_spelling_both_still_exist(self):
        """`record_watcher_terminal_delivery` names two functions in this tree.

        The pinned integer for terminal_long_chunks.rs counts one call to each.
        If either definition disappears the pin silently changes meaning, so the
        collision is asserted rather than left in a comment.
        """
        funnel = (ROOT / "src/services/discord/outbound/delivery_record.rs").read_text(
            encoding="utf-8"
        )
        wrapper = (
            ROOT / "src/services/discord/tmux_watcher/terminal_long_chunks.rs"
        ).read_text(encoding="utf-8")
        self.assertIn("fn record_watcher_terminal_delivery(", funnel)
        self.assertIn("fn record_watcher_terminal_delivery(", wrapper)
        self.assertIn("NAME COLLISION", SCRIPT.read_text(encoding="utf-8"))

    def test_the_call_sites_no_family_anchor_covers_are_pinned(self):
        """The sites that motivated dropping anchors, asserted one by one.

        A parallel census of every production durable write on 0bde0675b found
        these outside the reach of `check_delivery_journal_raw_writer.py`'s
        six family anchors. Three sit in the turn_bridge family but not in its
        anchor file; `claude_idle_runtime.rs` belongs to no family at all, so no
        anchor could ever reach it; the two `fresh_send.rs` sites are in no
        family either and are currently dormant (`OutputPlan::SendFresh` has no
        production constructor), which is exactly the state in which an
        uninstrumented write is easiest to reintroduce unnoticed.
        """
        anchor_blind = [
            ("record_delivered_frontier_with_body",
             "src/services/discord/turn_bridge/terminal_delivery.rs", 1),
            ("record_delivered_frontier_with_body",
             "src/services/discord/turn_bridge/terminal_outcome_delivery.rs", 1),
            ("record_delivered_frontier_with_body",
             "src/services/discord/turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs", 1),
            ("reanchor_current_generation_frontier",
             "src/services/discord/tui_prompt_relay/claude_idle_runtime.rs", 1),
            ("write_delivered_frontier",
             "src/services/discord/outbound/turn_output_controller/fresh_send.rs", 1),
            ("record_fresh_send_content_fingerprint",
             "src/services/discord/outbound/turn_output_controller/fresh_send.rs", 1),
        ]
        for symbol, rel, count in anchor_blind:
            with self.subTest(symbol=symbol, file=rel):
                self.assertEqual(guard.EXPECTED_CALL_SITES[symbol].get(rel), count)

    def test_every_pinned_file_exists_and_still_spells_the_symbol(self):
        """Keeps the map from rotting into a list of paths that no longer exist."""
        for symbol, files in guard.EXPECTED_CALL_SITES.items():
            for rel in files:
                path = ROOT / rel
                self.assertTrue(path.is_file(), f"{symbol}: missing {rel}")
                self.assertIn(symbol, path.read_text(encoding="utf-8"), f"{symbol} in {rel}")


class DiscriminationTests(unittest.TestCase):
    """Every assertion here is a mutation that was applied and then reverted."""

    def fixture(self) -> Path:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        write(
            root,
            "src/services/discord/outbound/delivery_record.rs",
            "pub fn write_delivered_frontier() {}\n"
            "pub fn upsert_lease() {}\n"
            "fn funnel() { shadow_mirror_delivered_frontier_inner(); }\n",
        )
        write(
            root,
            "src/services/discord/recovery_engine/terminal_text_idempotency.rs",
            "fn record() {\n"
            "    delivery_record::write_delivered_frontier();\n"
            "    completed_turn_ledger::append_completed_turn();\n"
            "}\n",
        )
        write(
            root,
            "src/services/discord/tmux_watcher/terminal_preflight.rs",
            "fn a() { advance_watcher_confirmed_end(); }\n"
            "fn b() { advance_watcher_confirmed_end(); }\n",
        )
        return root

    def expected(self, **overrides: dict[str, int]) -> dict[str, dict[str, int]]:
        base = {symbol: {} for symbol in guard.EXPECTED_CALL_SITES}
        base["write_delivered_frontier"] = {
            "src/services/discord/recovery_engine/terminal_text_idempotency.rs": 1
        }
        base["append_completed_turn"] = {
            "src/services/discord/recovery_engine/terminal_text_idempotency.rs": 1
        }
        base["shadow_mirror_delivered_frontier_inner"] = {
            "src/services/discord/outbound/delivery_record.rs": 1
        }
        base["advance_watcher_confirmed_end"] = {
            "src/services/discord/tmux_watcher/terminal_preflight.rs": 2
        }
        base.update(overrides)
        return base

    def run_guard(self, root: Path, expected=None) -> tuple[bool, str]:
        original = guard.EXPECTED_CALL_SITES
        guard.EXPECTED_CALL_SITES = expected if expected is not None else self.expected()
        try:
            return guard.check(root)
        finally:
            guard.EXPECTED_CALL_SITES = original

    # --- baseline -----------------------------------------------------------

    def test_m0_unmutated_fixture_is_green(self):
        ok, message = self.run_guard(self.fixture())
        self.assertTrue(ok, message)

    # --- M1: one call ADDED in a file the map already lists -----------------

    def test_m1_one_extra_call_in_a_listed_file_is_caught(self):
        root = self.fixture()
        rel = "src/services/discord/tmux_watcher/terminal_preflight.rs"
        (root / rel).write_text(
            (root / rel).read_text(encoding="utf-8")
            + "fn c() { advance_watcher_confirmed_end(); }\n",
            encoding="utf-8",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn("advance_watcher_confirmed_end: " + rel + " has 3x, expected 2x", message)

    # --- M2: one call DELETED ------------------------------------------------

    def test_m2_one_deleted_call_is_caught(self):
        root = self.fixture()
        rel = "src/services/discord/tmux_watcher/terminal_preflight.rs"
        (root / rel).write_text(
            "fn a() { advance_watcher_confirmed_end(); }\nfn b() {}\n", encoding="utf-8"
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn("has 1x, expected 2x", message)

    def test_m2b_the_last_call_in_a_file_disappearing_is_caught(self):
        """The 'call site GONE' arm: file drops to zero rather than to a lower count."""
        root = self.fixture()
        rel = "src/services/discord/recovery_engine/terminal_text_idempotency.rs"
        (root / rel).write_text("fn record() {}\n", encoding="utf-8")
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn(f"write_delivered_frontier: call site GONE from {rel}", message)
        self.assertIn(f"append_completed_turn: call site GONE from {rel}", message)

    # --- M3: a call added in a file the map does NOT list --------------------

    def test_m3_call_in_an_unlisted_file_is_caught(self):
        """The hole the anchor model cannot see: a brand new file nobody listed."""
        root = self.fixture()
        write(
            root,
            "src/services/discord/recovery_paths/controller_cutover.rs",
            "fn sneaky() { dr::write_delivered_frontier(); }\n",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn(
            "write_delivered_frontier: UNLISTED call site in "
            "src/services/discord/recovery_paths/controller_cutover.rs (1x)",
            message,
        )

    def test_m3b_call_added_far_outside_the_discord_subtree_is_caught(self):
        root = self.fixture()
        write(root, "src/config.rs", "fn sneaky() { append_completed_turn(); }\n")
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn("append_completed_turn: UNLISTED call site in src/config.rs", message)

    # --- M4: calling an API pinned at zero -----------------------------------

    def test_m4_single_call_to_a_zero_pinned_api_is_caught(self):
        root = self.fixture()
        write(
            root,
            "src/services/discord/outbound/lease_user.rs",
            "fn take() { delivery_record::upsert_lease(); }\n",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn(
            "upsert_lease: UNLISTED call site in src/services/discord/outbound/lease_user.rs (1x)",
            message,
        )

    def test_m4b_each_zero_pinned_api_is_individually_discriminating(self):
        """One assertion per zero-pinned symbol: none of the five is decorative."""
        for symbol in sorted(ZERO_PINNED):
            with self.subTest(symbol=symbol):
                root = self.fixture()
                write(root, "src/probe.rs", f"fn probe() {{ {symbol}(); }}\n")
                ok, message = self.run_guard(root)
                self.assertFalse(ok, symbol)
                self.assertIn(f"{symbol}: UNLISTED call site in src/probe.rs (1x)", message)

    # --- M5: alias / re-export -- MEASURED HOLE, NOT CAUGHT -----------------

    def test_m5_use_as_alias_is_NOT_caught_and_the_script_says_so(self):
        """MEASURED HOLE. `use ... as w; w()` defeats the scan. Declared, not hidden.

        This is the direct consequence of a literal-grep completion criterion:
        the contract is bound to the spelling, so any route that reaches the same
        function under a different spelling is outside it.
        """
        root = self.fixture()
        write(
            root,
            "src/services/discord/outbound/aliased_writer.rs",
            "use crate::services::discord::outbound::delivery_record::"
            "write_delivered_frontier as w;\n"
            "fn sneaky() { w(); }\n",
        )
        ok, message = self.run_guard(root)
        self.assertTrue(ok, "alias route is a declared blind spot; if this now fails, "
                            "the gate got stronger -- update the docstring too")
        self.assertIn("`use .. as x` aliases", message)
        self.assertIn("`use ...::write_delivered_frontier as w;`", SCRIPT.read_text(encoding="utf-8"))

    def test_m5b_pub_use_reexport_rename_is_NOT_caught(self):
        """MEASURED HOLE. A renamed re-export hides the spelling from every caller."""
        root = self.fixture()
        write(
            root,
            "src/services/discord/outbound/reexport.rs",
            "pub use super::delivery_record::upsert_lease as take_lease;\n",
        )
        write(
            root,
            "src/services/discord/outbound/reexport_user.rs",
            "fn go() { super::reexport::take_lease(); }\n",
        )
        ok, message = self.run_guard(root)
        # The `pub use ... as` line itself does not spell `upsert_lease(`, and the
        # call spells only the new name, so BOTH lines are invisible.
        self.assertTrue(ok, message)

    def test_m5c_a_module_alias_is_still_caught_because_the_fn_name_survives(self):
        """The alias hole is specific: renaming the MODULE does not help at all."""
        root = self.fixture()
        write(
            root,
            "src/services/discord/outbound/mod_alias.rs",
            "use crate::services::discord::outbound::delivery_record as dr;\n"
            "fn go() { dr::write_delivered_frontier(); }\n",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn("UNLISTED call site in src/services/discord/outbound/mod_alias.rs", message)

    # --- M6: macros -- one shape caught, one MEASURED HOLE -------------------

    def test_m6_macro_body_that_spells_the_symbol_is_caught_once(self):
        """A macro is text: the literal spelling inside its body is a call site.

        Note the count is TEXTUAL. This macro is invoked twice below and the gate
        still sees exactly one occurrence, because a text scan cannot count
        expansions. That direction is under-counting, and it is declared.
        """
        root = self.fixture()
        write(
            root,
            "src/services/discord/outbound/macro_writer.rs",
            "macro_rules! commit {\n"
            "    () => { delivery_record::write_delivered_frontier() };\n"
            "}\n"
            "fn one() { commit!(); }\n"
            "fn two() { commit!(); }\n",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn(
            "write_delivered_frontier: UNLISTED call site in "
            "src/services/discord/outbound/macro_writer.rs (1x)",
            message,
        )

    def test_m6b_name_constructing_macro_is_NOT_caught(self):
        """MEASURED HOLE. `paste!`-style name assembly never spells the symbol."""
        root = self.fixture()
        write(
            root,
            "src/services/discord/outbound/paste_writer.rs",
            "paste::paste! {\n"
            "    fn go() { [<write_delivered>]_[<frontier>](); }\n"
            "}\n",
        )
        ok, message = self.run_guard(root)
        self.assertTrue(ok, "name-assembling macros are a declared blind spot")
        self.assertIn("name-constructing macros", message)

    def test_m6c_calling_through_a_function_value_is_NOT_caught(self):
        """MEASURED HOLE. The name without `(` is not a call site, and `f()` is not the name."""
        root = self.fixture()
        write(
            root,
            "src/services/discord/outbound/fn_ptr.rs",
            "fn go() {\n"
            "    let f = delivery_record::write_delivered_frontier;\n"
            "    f();\n"
            "}\n",
        )
        ok, message = self.run_guard(root)
        self.assertTrue(ok, "indirection through a value is a declared blind spot")

    # --- M7: narrowing the scan ----------------------------------------------

    def test_m7_narrowing_the_scan_root_is_caught(self):
        """The mutation that would let someone hide a whole subtree."""
        root = self.fixture()
        original = guard.SCAN_ROOT
        guard.SCAN_ROOT = Path("src/services/discord/outbound")
        try:
            ok, message = self.run_guard(root)
        finally:
            guard.SCAN_ROOT = original
        self.assertFalse(ok)
        self.assertIn("call site GONE", message)

    # --- boundary declarations ----------------------------------------------

    def test_prefix_suffixed_variants_are_not_counted_as_the_pinned_symbol(self):
        """`_at` / `_inner` / `_for_generation` variants are different symbols."""
        root = self.fixture()
        write(
            root,
            "src/services/discord/outbound/private_helpers.rs",
            "fn go() {\n"
            "    write_delivered_frontier_at();\n"
            "    write_delivered_frontier_guarded_at_with_before_lock();\n"
            "    shadow_mirror_delivered_frontier_inner_probe();\n"
            "    record_delivered_content_fingerprint_for_generation();\n"
            "}\n",
        )
        ok, message = self.run_guard(root)
        self.assertTrue(ok, message)

    def test_cfg_test_regions_and_test_files_are_excluded(self):
        root = self.fixture()
        write(
            root,
            "src/services/discord/outbound/delivery_record_tests.rs",
            "fn t() { upsert_lease(); }\n",
        )
        write(
            root,
            "src/services/discord/outbound/inline_tested.rs",
            "#[cfg(test)]\nmod tests {\n    fn t() { upsert_lease(); }\n}\n",
        )
        write(
            root,
            "src/services/discord/outbound/inline_tested_fn.rs",
            "#[cfg(all(test, unix))]\nfn probe() { clear_lease(); }\n",
        )
        ok, message = self.run_guard(root)
        self.assertTrue(ok, message)

    def test_cfg_test_struct_field_does_not_swallow_the_next_impl_block(self):
        """The exact bug this resolver refuses to inherit from the ported one.

        `#[cfg(test)] field: T,` ends in a comma. A resolver that only disarms on
        `{` or `;` stays armed through the struct and latches onto the following
        `impl ... {`, hiding every production call inside it. Measured on this
        tree, that misclassifies three real call sites in session_relay_sink.rs.
        """
        root = self.fixture()
        write(
            root,
            "src/services/discord/outbound/field_gated.rs",
            "pub struct Sink {\n"
            "    journal: &'static Observer,\n"
            "    #[cfg(test)]\n"
            "    test_probe: Option<Arc<Probe>>,\n"
            "    #[cfg(test)]\n"
            "    test_gateway: Option<Arc<dyn Gateway>>,\n"
            "}\n"
            "\n"
            "impl Sink {\n"
            "    fn commit(&self) { dr::upsert_lease(); }\n"
            "}\n",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok, "the production impl block must stay visible")
        self.assertIn("upsert_lease: UNLISTED call site in "
                      "src/services/discord/outbound/field_gated.rs (1x)", message)

    def test_generic_comma_in_a_cfg_test_fn_signature_does_not_disarm_early(self):
        """The counter-case for the comma rule: a real test fn stays excluded."""
        root = self.fixture()
        write(
            root,
            "src/services/discord/outbound/generic_test_fn.rs",
            "#[cfg(test)]\n"
            "fn probe(a: u8, b: u8) -> HashMap<String, u64> {\n"
            "    upsert_lease();\n"
            "    HashMap::new()\n"
            "}\n",
        )
        ok, message = self.run_guard(root)
        self.assertTrue(ok, message)

    def test_comments_and_string_literals_do_not_count(self):
        """Declared boundary; this is where the gate differs from the model gate.

        `check_delivery_journal_raw_writer.py` strips only the `//` suffix, so a
        symbol inside a string or a `/* */` block makes it go red. This one runs
        the cross-line stripper first, so neither does.
        """
        root = self.fixture()
        write(
            root,
            "src/services/discord/outbound/prose.rs",
            "// upsert_lease();\n"
            "/* upsert_lease(); */\n"
            'const DOC: &str = "upsert_lease();";\n'
            'const RAW: &str = r#"upsert_lease();"#;\n',
        )
        ok, message = self.run_guard(root)
        self.assertTrue(ok, message)

    def test_unbalanced_brace_in_a_raw_string_does_not_hide_later_calls(self):
        """Without cross-line stripping this `{` poisons the depth for the file."""
        root = self.fixture()
        write(
            root,
            "src/services/discord/outbound/raw_string_brace.rs",
            "#[cfg(test)]\nmod tests {\n"
            '    const SQL: &str = r#"SELECT {\n'
            '        unbalanced"#;\n'
            "}\n"
            "fn production() { delete_record(); }\n",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn("delete_record: UNLISTED call site in "
                      "src/services/discord/outbound/raw_string_brace.rs (1x)", message)

    def test_fn_definition_line_is_not_a_call_site(self):
        root = self.fixture()
        write(
            root,
            "src/services/discord/outbound/defs.rs",
            "pub(in crate::services::discord) fn clear_lease(a: u8) {}\n"
            "pub async fn delete_record(b: u8) {}\n",
        )
        ok, message = self.run_guard(root)
        self.assertTrue(ok, message)


if __name__ == "__main__":
    unittest.main()
