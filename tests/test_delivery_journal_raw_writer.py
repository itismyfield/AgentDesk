from __future__ import annotations

import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path
ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check_delivery_journal_raw_writer.py"
SPEC = importlib.util.spec_from_file_location("journal_writer_guard", SCRIPT)
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)
FACADE_MARKERS = (
    " self.journal.begin_fresh();",
    " self.journal.begin_fresh();",
    " journal_watcher::begin_watcher_terminal();",
)
def write(root: Path, rel: str, body: str) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")
class RawWriterAllowlistTests(unittest.TestCase):
    def fixture(self, extra: str = "") -> Path:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        write(root, "src/services/discord/session_relay_sink/journal/pg_store.rs", "fn append_delivery_journal_batch() {}\n")
        write(root, "src/services/discord/session_relay_sink/journal.rs", "fn actor() { append_delivery_journal_batch(); }\n")
        # #5071 T1 S3a: the third instrumented family is the watcher, which spells
        # the facade through `journal_watcher::` because its anchor is a free
        # function with no `self`. Using the real token here means the fixture
        # exercises BOTH alternations of JOURNAL_FACADE_CALL, not just the sink's.
        for index, (_, rel, symbol) in enumerate(guard.FAMILY_REGISTRY):
            call = FACADE_MARKERS[index] if index < len(FACADE_MARKERS) else ""
            write(root, rel, f"fn {symbol}() {{{call}}}\n")
        if extra:
            write(root, "src/services/discord/rogue.rs", extra)
        subprocess.run(["git", "add", "-A"], cwd=root, check=True)
        return root
    def test_exact_allowlist_passes(self):
        ok, message = guard.check(self.fixture())
        self.assertTrue(ok, message)

    def test_raw_store_external_call_fails_its_own_assert(self):
        ok, message = guard.check(self.fixture("fn rogue() { append_delivery_journal_batch(); }\n"))
        self.assertFalse(ok)
        self.assertIn("exceeds monotonic baseline", message)
    def test_top_level_src_rust_rogue_call_fails_its_own_assert(self):
        root = self.fixture()
        write(root, "src/config.rs", "fn rogue() { append_delivery_journal_batch(); }\n")
        subprocess.run(["git", "add", "-A"], cwd=root, check=True)
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("src/config.rs", message)
    def test_line_comments_are_excluded_but_block_comments_and_strings_count(self):
        """Declare the exact lexical boundary: // is excluded; /* */ and strings count."""
        ok, message = guard.check(self.fixture("// append_delivery_journal_batch(x);\n"))
        self.assertTrue(ok, message)
        for marker in (
            "/* append_delivery_journal_batch(x); */\n",
            'const S: &str = "append_delivery_journal_batch(x);";\n',
        ):
            ok, message = guard.check(self.fixture(marker))
            self.assertFalse(ok, marker)
            self.assertIn("raw writer call count 2 exceeds monotonic baseline 1", message)

    def test_test_area_and_string_facade_markers_count_as_declared(self):
        """Evidence: whole-file lexical scanning counts test and string text."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[4][1]
        path.write_text(path.read_text(encoding="utf-8") +
                        'const STRING_MARKER: &str = "self.journal.finish_fresh(";\n#[cfg(all(test, unix))]\nmod tests {\n    fn dishonest() { self.journal.begin_fresh(); }\n    const TEST_MARKER: &str = "self.journal.begin_fresh(";\n}\n',
                        encoding="utf-8")
        self.assertTrue(guard.family_status(root)[0][4][1], "test/string markers are declared lexical matches")

    def test_cfg_test_fn_facade_marker_counts_as_known_limit(self):
        """Evidence: a top-level cfg(test) function is counted, not parsed away."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[4][1]
        path.write_text(path.read_text(encoding="utf-8") + '#[cfg(test)] fn journal_probe() { self.journal.begin_fresh(); }\n', encoding="utf-8")
        self.assertTrue(guard.family_status(root)[0][4][1], "cfg(test) fn marker is a declared lexical match")
        ok, message = guard.check(root); self.assertFalse(ok); self.assertIn("uninstrumented families: 2/6", message)

    def test_line_doc_comment_markers_are_known_limit_and_not_counted(self):
        """Known limitation and declared behavior: line doc comments are stripped after // on each line."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[4][1]
        path.write_text(path.read_text(encoding="utf-8") +
                        "//! self.journal.begin_fresh();\n/// self.journal.begin_fresh();\n",
                        encoding="utf-8")
        status, error = guard.family_status(root)
        self.assertEqual(error, "")
        self.assertFalse(status[4][1], "line doc comment markers are excluded by the declared lexical cut")
        ok, message = guard.check(root)
        self.assertTrue(ok, message)
        self.assertIn("uninstrumented families: 3/6", message)

    def test_block_marker_strings_do_not_hide_real_facade_calls(self):
        """Evidence: block-marker strings no longer delete calls across lines."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[0][1]
        path.write_text(path.read_text(encoding="utf-8") + 'const BLOCK_OPEN: &str = "/*";\nself.journal.begin_fresh();\nconst BLOCK_CLOSE: &str = "*/";\n', encoding="utf-8")
        ok, message = guard.check(root); self.assertTrue(ok, message); self.assertIn("uninstrumented families: 3/6", message)

    def test_raw_string_marker_is_known_lexical_false_positive(self):
        """Known limit: raw strings are not parsed and may count as calls."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[4][1]
        path.write_text(path.read_text(encoding="utf-8") +
                        'const RAW: &str = r#"x" self.journal.begin_fresh("#;\n', encoding="utf-8")
        status, error = guard.family_status(root)
        self.assertEqual(error, "")
        self.assertTrue(status[4][1], "raw-string marker intentionally pierces lexical scan")

    def test_macro_facade_marker_is_known_lexical_match(self):
        """Pin the declared behavior: facade-call text in a macro counts."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[4][1]
        path.write_text(path.read_text(encoding="utf-8") +
                        "macro_rules! journal_probe { () => { self.journal.begin_fresh(); } }\n",
                        encoding="utf-8")
        status, error = guard.family_status(root)
        self.assertEqual(error, "")
        self.assertTrue(status[4][1], "macro facade-call text is a declared lexical match")

    def test_family_baseline_is_measured_and_named(self):
        ok, message = guard.check(self.fixture())
        self.assertTrue(ok, message)
        self.assertIn("uninstrumented families: 3/6", message)
        self.assertIn("whole anchor file including tests", message)
        self.assertIn("turn_bridge / controller family", message)

    def test_instrumentation_rule_is_mechanical(self):
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[0][1]
        path.write_text(path.read_text(encoding="utf-8").replace("self.journal.begin_fresh();", ""), encoding="utf-8")
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("exceeds baseline", message)

    def test_missing_anchor_symbol_fails_closed(self):
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[0][1]
        path.write_text(path.read_text(encoding="utf-8").replace(guard.FAMILY_REGISTRY[0][2], "anchor_removed"), encoding="utf-8")
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("family anchor symbol missing", message)

    def test_missing_anchor_file_fails_closed(self):
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[0][1]
        path.unlink()
        subprocess.run(["git", "rm", "-q", "--cached", "--", guard.FAMILY_REGISTRY[0][1]], cwd=root, check=True)
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("family anchor missing", message)

    def test_baseline_increase_names_families(self):
        root = self.fixture()
        old = guard.UNINSTRUMENTED_FAMILY_BASELINE
        guard.UNINSTRUMENTED_FAMILY_BASELINE = 2
        self.addCleanup(setattr, guard, "UNINSTRUMENTED_FAMILY_BASELINE", old)
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("turn_bridge / controller family", message)

    def test_baseline_decrease_requires_repin_command(self):
        root = self.fixture()
        old = guard.UNINSTRUMENTED_FAMILY_BASELINE
        guard.UNINSTRUMENTED_FAMILY_BASELINE = 6
        self.addCleanup(setattr, guard, "UNINSTRUMENTED_FAMILY_BASELINE", old)
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("re-pin with: python3", message)
    def test_live_repository_matches_exact_allowlist(self):
        result = subprocess.run(["python3", str(SCRIPT)], cwd=ROOT, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertRegex(result.stdout, r"scanned Rust files: [1-9][0-9]*")
        self.assertIn("uninstrumented families: 3/6", result.stdout)

    # SOURCE-CONTRACT block (#5071 T1 S2). Everything below matches TEXT in .rs
    # files: call ORDER, call COUNT, symbol PRESENCE. None of it executes Rust,
    # so none of it observes what the code MEANS — a mutation that keeps the
    # tokens and inverts the semantics passes every assertion here. Named
    # `source_contract_*` so they are never read as runtime evidence. The
    # runtime guarantees are the Rust tests T1-T8, each proven by a mutation:
    # T1-T3 route/cutover boundary, T4 anchor receipt, T5 proof-derived commit,
    # T6 single settle (session_relay_sink/journal.rs::sink_direct_semantics_tests);
    # T7 mismatch preservation (formatting/long_send_rollback.rs);
    # T8 edit/fallback receipt (formatting/replace_long_message_tests.rs).
    # T9 (referenced/split receipts) is deferred to S6 with D1 — design 9.2 C7.

    def test_source_contract_sink_direct_begin_is_guarded_after_cutover(self):
        """Source text only: begin appears after the cutover return, behind the predicate."""
        source = (ROOT / "src/services/discord/session_relay_sink.rs").read_text(encoding="utf-8")
        cutover = source.index("return short_controller::deliver_short_replace_via_controller")
        guard = source.index("journal::journals_sink_direct(&route, cutover_short_replace)")
        begin = source.index("self.journal.begin_fresh(")
        self.assertGreater(begin, cutover)
        self.assertLess(guard, begin)

    def test_source_contract_sink_direct_root_has_one_facade_begin(self):
        """Source text only: pins begin_fresh at exactly 1 occurrence in
        session_relay_sink.rs, so a second call added to THAT file -- including
        one that bypasses the journals_sink_direct predicate -- fails here and
        is blocked in CI. It proves nothing about reachability, and it reads
        only this one file: a begin_fresh added in a different module (a new
        helper, say) is outside every check we have."""
        source = (ROOT / "src/services/discord/session_relay_sink.rs").read_text(encoding="utf-8")
        self.assertEqual(source.count("self.journal.begin_fresh("), 1)
        self.assertEqual(source.count("self.journal.finish_fresh("), 0)

    def test_source_contract_rollback_legacy_entrypoint_keeps_parallel_receipt_entrypoint(self):
        """Source text only: the frozen name survives beside the receipt entry point."""
        source = (ROOT / "src/services/discord/formatting/long_send_rollback.rs").read_text(encoding="utf-8")
        self.assertIn("send_long_message_raw_with_rollback(", source)
        self.assertIn("send_long_message_raw_with_rollback_returning_receipts(", source)

    def test_source_contract_sink_direct_success_arms_settle_each_terminal_arm(self):
        """Source text only: pins the literal `journal::settle(` count in
        session_relay_sink.rs at 3, so deleting one of the three terminal arms
        makes it 2 and fails here -- this test, not a runtime test, is what
        blocks that edit in CI (no runtime test can see it: begin_fresh is None
        without PG + Shadow). Being a text count is the limit: it cannot tell
        which branch a surviving call sits on, and a call commented out rather
        than deleted still counts toward the 3."""
        source = (ROOT / "src/services/discord/session_relay_sink.rs").read_text(encoding="utf-8")
        self.assertEqual(source.count("journal::settle("), 3)

    # #5071 T1 S3a additions.

    def test_watcher_facade_alternation_matches_only_its_exact_call_shape(self):
        """The S3a alternation must not be a loosening. Near misses stay
        uninstrumented; only the declared call shapes match."""
        for near_miss in (
            " journal_watcher::begin_watcher();",
            " journal_watcher.begin_watcher_terminal();",
            " watcher::begin_watcher_terminal();",
            " journal_watcher::journals_watcher_terminal();",
        ):
            self.assertIsNone(
                guard.JOURNAL_FACADE_CALL.search(near_miss),
                f"{near_miss!r} must not count as a facade call",
            )
        for exact in (
            " journal_watcher::begin_watcher_terminal(",
            " journal_watcher::settle_watcher_terminal(",
            " journal_watcher::settle_without_transport(",
            " self.journal.begin_fresh(",
            " self.journal.finish_fresh(",
        ):
            self.assertIsNotNone(
                guard.JOURNAL_FACADE_CALL.search(exact),
                f"{exact!r} is a declared facade call",
            )

    def test_watcher_family_regresses_to_uninstrumented_when_its_facade_is_removed(self):
        """Reverse mutation, in fixture form: the 4 -> 3 baseline drop is caused
        by the instrumentation, not by the widened regex. Drop the watcher token
        and the count returns over the re-pinned baseline of 3."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[2][1]
        path.write_text(
            path.read_text(encoding="utf-8").replace(FACADE_MARKERS[2], ""),
            encoding="utf-8",
        )
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("exceeds baseline", message)
        self.assertIn("watcher terminal family", message)

    def test_source_contract_watcher_anchor_begins_and_settles_exactly_once(self):
        """Source text only: pins one begin and one settle in the watcher anchor
        file, so deleting either -- which no runtime test can see, because
        begin_watcher_terminal returns None without PG + Shadow -- fails in CI.
        It cannot tell which branch the surviving call sits on."""
        source = (ROOT / "src/services/discord/tmux_watcher.rs").read_text(encoding="utf-8")
        self.assertEqual(source.count("journal_watcher::begin_watcher_terminal("), 1)
        self.assertEqual(source.count("journal_watcher::settle_watcher_terminal("), 1)
        self.assertLess(
            source.index("journal_watcher::begin_watcher_terminal("),
            source.index("journal_watcher::settle_watcher_terminal("),
            "the obligation opens before transport and settles after the commit",
        )

    # #5071 T1 S3b addition.

    def test_source_contract_five_no_transport_sites_each_settle(self):
        """Source text only: the design names exactly five no-transport frontier
        advances. This pins one settle_without_transport call per site, so adding
        a sixth advance without an observation -- or dropping one of the five --
        fails here. It is a text count: it cannot prove any call is reached."""
        sites = {
            "src/services/discord/tmux_watcher/terminal_preflight.rs": 2,
            "src/services/discord/tmux_watcher/no_result_exits.rs": 1,
            "src/services/discord/tmux_watcher/loop_poll_prologue.rs": 1,
            "src/services/discord/tmux.rs": 1,
        }
        total = 0
        for rel, expected in sites.items():
            source = (ROOT / rel).read_text(encoding="utf-8")
            found = source.count("settle_without_transport(")
            self.assertEqual(found, expected, f"{rel}: expected {expected}, found {found}")
            total += found
        self.assertEqual(total, 5, "the design names exactly five no-transport settlement sites")

    # #5071 T1 S3c addition.

    def test_source_contract_repeated_suppression_arm_gates_its_observation(self):
        """Source text only: the post-terminal suppression arm is the one site
        that re-enters with the same range, so its settlement must sit behind the
        one-shot range test. This pins that the guard is computed once and that
        the settlement call is gated by it. A text check: it cannot prove the
        gate is evaluated at runtime -- W7 and W2/W2b do that."""
        source = (
            ROOT / "src/services/discord/tmux_watcher/loop_poll_prologue.rs"
        ).read_text(encoding="utf-8")
        self.assertEqual(source.count("first_observation_of_suppressed_range("), 1)
        self.assertEqual(source.count("if first_observation_of_range {"), 2)
        self.assertLess(
            source.index("first_observation_of_suppressed_range("),
            source.index("settle_without_transport("),
            "the guard is computed before the settlement it gates",
        )
if __name__ == "__main__":
    unittest.main()
