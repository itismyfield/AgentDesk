from __future__ import annotations

import importlib.util
import re
import subprocess
import tempfile
import unittest
from pathlib import Path
ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check_delivery_journal_raw_writer.py"
SPEC = importlib.util.spec_from_file_location("journal_writer_guard", SCRIPT)
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)
# #5071 T1 S6. The successor gate, imported READ-ONLY and never modified here.
# `test_source_contract_no_pipe_gated_production_file_holds_delivery_work` takes
# its writer vocabulary, its cross-line string/comment stripper and its
# cfg(test) filter from this module rather than re-declaring them, so the two
# gates cannot drift apart: a symbol added to S7' `EXPECTED_CALL_SITES` widens
# the pipe co-existence pin in the same commit, for free.
FRONTIER_SCRIPT = ROOT / "scripts/check_durable_frontier_writer_call_sites.py"
FRONTIER_SPEC = importlib.util.spec_from_file_location("frontier_writer_guard", FRONTIER_SCRIPT)
frontier = importlib.util.module_from_spec(FRONTIER_SPEC)
FRONTIER_SPEC.loader.exec_module(frontier)
FACADE_MARKERS = (
    " self.journal.begin_fresh();",
    " self.journal.begin_fresh();",
    " journal_watcher::begin_watcher_terminal();",
    " unix_journal::begin_controller_terminal();",
    " unix_journal::begin_recovery_terminal();",
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
        # function with no `self`. #5071 T1 S4 adds the fourth, the turn_bridge
        # cutover, spelling it through `unix_journal::`. #5071 T1 S5b adds the
        # fifth, the recovery family, whose door happens to be named
        # `unix_journal` too -- so the fixture also proves the two doors' shapes
        # stay separated by function name. Using the real tokens here means the
        # fixture exercises ALL FOUR alternations of JOURNAL_FACADE_CALL, not
        # just the sink's.
        #
        # #5071 T1 S6: one marker per family, asserted rather than padded. Until
        # S6 the loop fell back to "" for any family past the end of
        # FACADE_MARKERS, which is how the sixth family was left uninstrumented
        # in the fixture. With the baseline at 0 that fallback would now make
        # every fixture-based test fail for an unrelated reason, so a new family
        # arriving without a marker has to be loud here instead.
        self.assertEqual(
            len(FACADE_MARKERS),
            len(guard.FAMILY_REGISTRY),
            "every family needs its own fixture facade marker; the fixture baseline is 0",
        )
        for index, (_, rel, symbol) in enumerate(guard.FAMILY_REGISTRY):
            write(root, rel, f"fn {symbol}() {{{FACADE_MARKERS[index]}}}\n")
        if extra:
            write(root, "src/services/discord/rogue.rs", extra)
        subprocess.run(["git", "add", "-A"], cwd=root, check=True)
        return root

    # #5071 T1 S6. Before S6 the sixth family carried no facade marker, so the
    # lexical-limit tests below could append their probe text to an
    # already-uninstrumented anchor and watch it flip. Every family in the
    # fixture is instrumented now, so those tests clear a marker first. The
    # family they clear is arbitrary; index 4 (recovery) is used throughout so
    # the diff reads as one substitution rather than five.
    PROBE_FAMILY = 4

    def uninstrument(self, root: Path, index: int) -> Path:
        """Strip one family's fixture facade marker and return its anchor path."""
        path = root / guard.FAMILY_REGISTRY[index][1]
        path.write_text(
            path.read_text(encoding="utf-8").replace(FACADE_MARKERS[index], ""),
            encoding="utf-8",
        )
        self.assertFalse(
            guard.family_status(root)[0][index][1],
            "clearing the marker must leave the family uninstrumented",
        )
        return path

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
        path = self.uninstrument(root, self.PROBE_FAMILY)
        path.write_text(path.read_text(encoding="utf-8") +
                        'const STRING_MARKER: &str = "self.journal.finish_fresh(";\n#[cfg(all(test, unix))]\nmod tests {\n    fn dishonest() { self.journal.begin_fresh(); }\n    const TEST_MARKER: &str = "self.journal.begin_fresh(";\n}\n',
                        encoding="utf-8")
        self.assertTrue(guard.family_status(root)[0][self.PROBE_FAMILY][1], "test/string markers are declared lexical matches")

    def test_cfg_test_fn_facade_marker_counts_as_known_limit(self):
        """Evidence: a top-level cfg(test) function is counted, not parsed away.

        Stronger than its pre-S6 form: with the baseline at 0 the cleared family
        makes the whole gate RED, and appending nothing but a `#[cfg(test)] fn`
        body turns it green again. The marker is not merely counted, it is on its
        own sufficient to satisfy the gate."""
        root = self.fixture()
        path = self.uninstrument(root, self.PROBE_FAMILY)
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("exceeds baseline", message)
        path.write_text(path.read_text(encoding="utf-8") + '#[cfg(test)] fn journal_probe() { self.journal.begin_fresh(); }\n', encoding="utf-8")
        self.assertTrue(guard.family_status(root)[0][self.PROBE_FAMILY][1], "cfg(test) fn marker is a declared lexical match")
        ok, message = guard.check(root); self.assertTrue(ok, message); self.assertIn("uninstrumented families: 0/5", message)

    def test_line_doc_comment_markers_are_known_limit_and_not_counted(self):
        """Known limitation and declared behavior: line doc comments are stripped after // on each line."""
        root = self.fixture()
        path = self.uninstrument(root, self.PROBE_FAMILY)
        path.write_text(path.read_text(encoding="utf-8") +
                        "//! self.journal.begin_fresh();\n/// self.journal.begin_fresh();\n",
                        encoding="utf-8")
        status, error = guard.family_status(root)
        self.assertEqual(error, "")
        self.assertFalse(status[self.PROBE_FAMILY][1], "line doc comment markers are excluded by the declared lexical cut")
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("exceeds baseline", message)

    def test_block_marker_strings_do_not_hide_real_facade_calls(self):
        """Evidence: block-marker strings no longer delete calls across lines."""
        root = self.fixture()
        path = self.uninstrument(root, 0)
        path.write_text(path.read_text(encoding="utf-8") + 'const BLOCK_OPEN: &str = "/*";\nself.journal.begin_fresh();\nconst BLOCK_CLOSE: &str = "*/";\n', encoding="utf-8")
        ok, message = guard.check(root); self.assertTrue(ok, message); self.assertIn("uninstrumented families: 0/5", message)

    def test_raw_string_marker_is_known_lexical_false_positive(self):
        """Known limit: raw strings are not parsed and may count as calls."""
        root = self.fixture()
        path = self.uninstrument(root, self.PROBE_FAMILY)
        path.write_text(path.read_text(encoding="utf-8") +
                        'const RAW: &str = r#"x" self.journal.begin_fresh("#;\n', encoding="utf-8")
        status, error = guard.family_status(root)
        self.assertEqual(error, "")
        self.assertTrue(status[self.PROBE_FAMILY][1], "raw-string marker intentionally pierces lexical scan")

    def test_macro_facade_marker_is_known_lexical_match(self):
        """Pin the declared behavior: facade-call text in a macro counts."""
        root = self.fixture()
        path = self.uninstrument(root, self.PROBE_FAMILY)
        path.write_text(path.read_text(encoding="utf-8") +
                        "macro_rules! journal_probe { () => { self.journal.begin_fresh(); } }\n",
                        encoding="utf-8")
        status, error = guard.family_status(root)
        self.assertEqual(error, "")
        self.assertTrue(status[self.PROBE_FAMILY][1], "macro facade-call text is a declared lexical match")

    def test_family_baseline_is_measured_and_named(self):
        """#5071 T1 S6: the count is 0/5 now, and the caveat must ride with it.

        `0` is the most misreadable number this gate can print, so the summary
        that carries it also has to carry what it does not mean and where the
        question went. Asserted on the message, not on prose in a comment, so the
        caveat cannot be quietly dropped from the output."""
        ok, message = guard.check(self.fixture())
        self.assertTrue(ok, message)
        self.assertIn("uninstrumented families: 0/5", message)
        self.assertIn("whole anchor file including tests", message)
        self.assertNotIn("pipe stream epoch", message)
        self.assertIn("ANCHOR-SCOPED", message)
        self.assertIn("0 does NOT mean no uninstrumented durable write exists", message)
        self.assertIn("scripts/check_durable_frontier_writer_call_sites.py", message)
        for outside in (
            "terminal_delivery.rs",
            "terminal_outcome_delivery.rs",
            "cancel_prompt_replace.rs",
            "tui_prompt_relay/claude_idle_runtime.rs",
            "outbound/turn_output_controller/fresh_send.rs",
        ):
            self.assertIn(outside, message, f"the caveat must name {outside}")

    def test_instrumentation_rule_is_mechanical(self):
        root = self.fixture()
        self.uninstrument(root, 0)
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
        """#5071 T1 S6: the baseline is 0, so ANY uninstrumented family is an
        increase and no longer needs the baseline to be moved to provoke one.
        The assertion is that the message NAMES the offender rather than
        reporting a bare count."""
        root = self.fixture()
        self.uninstrument(root, 0)
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("fresh sink vertical slice", message)

    def test_baseline_decrease_requires_repin_command(self):
        """The exact failure S6 itself had to clear: 1 pinned, 0 measured.

        Dropping the sixth family without re-pinning the baseline in the same
        commit produces this, which is why the removal and the re-pin are one
        commit. The emitted command is the fix, and it is asserted so the ratchet
        cannot regress into a bare 'below baseline' with no way out."""
        root = self.fixture()
        old = guard.UNINSTRUMENTED_FAMILY_BASELINE
        guard.UNINSTRUMENTED_FAMILY_BASELINE = 1
        self.addCleanup(setattr, guard, "UNINSTRUMENTED_FAMILY_BASELINE", old)
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("below baseline 1", message)
        self.assertIn("re-pin with: python3", message)
        self.assertIn("UNINSTRUMENTED_FAMILY_BASELINE = 0", message)
    def test_live_repository_matches_exact_allowlist(self):
        result = subprocess.run(["python3", str(SCRIPT)], cwd=ROOT, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertRegex(result.stdout, r"scanned Rust files: [1-9][0-9]*")
        self.assertIn("uninstrumented families: 0/5", result.stdout)
        # The caveat has to survive the trip through the real script's stdout,
        # not just through `check()`: the printed line is what a reader sees.
        self.assertIn("ANCHOR-SCOPED", result.stdout)
        self.assertIn("scripts/check_durable_frontier_writer_call_sites.py", result.stdout)

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
        and the count returns over the baseline S3a re-pinned to 3 -- and over
        every lower baseline since, which is why the assertion reads the message
        rather than a number."""
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

    # #5071 T1 S4 additions.

    def test_controller_facade_alternation_matches_only_its_exact_call_shape(self):
        """The S4 alternation must not be a loosening either. Near misses stay
        uninstrumented; only the two declared call shapes match."""
        for near_miss in (
            " unix_journal::begin_controller();",
            " unix_journal.begin_controller_terminal();",
            " ctl::begin_controller_terminal();",
            " unix_journal::controller_obligation_id();",
            " unix_journal::settle_controller();",
        ):
            self.assertIsNone(
                guard.JOURNAL_FACADE_CALL.search(near_miss),
                f"{near_miss!r} must not count as a facade call",
            )
        for exact in (
            " unix_journal::begin_controller_terminal(",
            " unix_journal::settle_controller_terminal(",
        ):
            self.assertIsNotNone(
                guard.JOURNAL_FACADE_CALL.search(exact),
                f"{exact!r} is a declared facade call",
            )

    def test_controller_family_regresses_to_uninstrumented(self):
        """Reverse mutation, in fixture form: the 3 -> 2 baseline drop is caused
        by the instrumentation, not by the widened regex. Drop the controller
        token and the count returns over the baseline S4 re-pinned to 2 -- and
        over every lower baseline since, which is why the assertion reads the
        message rather than a number."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[3][1]
        path.write_text(
            path.read_text(encoding="utf-8").replace(FACADE_MARKERS[3], ""),
            encoding="utf-8",
        )
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("exceeds baseline", message)
        self.assertIn("turn_bridge / controller family", message)

    def test_source_contract_controller_anchor_covers_every_durable_writer(self):
        """Source text only: the S4 design names exactly three durable delivered-
        frontier writes in the cutover anchor -- one short-replace mirror and two
        long-chunk records -- and each is opened by its own pre-transport begin.
        Adding a fourth durable write, or dropping a begin, fails here. Five
        settles, not three: the two long-chunk sites settle `true` inside their
        commit arm and call the single-use settle once more on the way out, so a
        completed-but-uncommitted delivery closes as `U` instead of dangling. It
        is a text count -- it cannot prove any call is reached."""
        source = (
            ROOT / "src/services/discord/turn_bridge/terminal_controller_cutover.rs"
        ).read_text(encoding="utf-8")
        self.assertEqual(source.count("dr::shadow_mirror_delivered_frontier("), 1)
        self.assertEqual(source.count("dr::record_long_chunk_terminal_delivery("), 2)
        self.assertEqual(source.count("unix_journal::begin_controller_terminal("), 3)
        self.assertEqual(source.count("unix_journal::settle_controller_terminal("), 5)
        self.assertLess(
            source.index("unix_journal::begin_controller_terminal("),
            source.index("unix_journal::settle_controller_terminal("),
            "the first obligation opens before any settle",
        )
        self.assertLess(
            source.rindex("unix_journal::begin_controller_terminal("),
            source.rindex("unix_journal::settle_controller_terminal("),
            "the last obligation opens before its settle",
        )

    def test_source_contract_turn_bridge_reaches_the_journal_through_one_cfg_gated_door(self):
        """Source text only, and the one check that would have caught the S4
        windows regression. `mod session_relay_sink` is `#[cfg(unix)]` while `mod
        turn_bridge` is not, so ANY reference from turn_bridge into that module
        which is not itself behind `#[cfg(unix)]` breaks windows-latest with
        E0433 -- which is exactly how S4 first landed. This pins both halves of
        the fix: the reference lives in exactly ONE turn_bridge file
        (`unix_journal.rs`, the single door), and every occurrence there is
        immediately preceded by `#[cfg(unix)]`.

        What it is not: it does not compile anything, least of all for windows.
        It is a line scan that strips only `//` suffixes, it looks only inside
        `turn_bridge/`, and it says nothing about cross-`cfg` references
        elsewhere in the tree. It substitutes a text rule for a target this
        repository cannot build locally (the msvc target needs a Windows C
        toolchain), so CI's `Fast check + non-PG tests (windows-latest)` stays
        the authority."""
        mod_rs = (ROOT / "src/services/discord/mod.rs").read_text(encoding="utf-8")
        self.assertIn(
            "#[cfg(unix)]\nmod session_relay_sink;",
            mod_rs,
            "the gate this contract protects moved; re-derive the rule instead of relaxing it",
        )
        door = "src/services/discord/turn_bridge/terminal_controller_cutover/unix_journal.rs"
        # This literal is self-validating below: point it at the wrong file and
        # the real door lands in `offenders`. The gate script's PLATFORM
        # BLINDNESS block sends readers to the same path in prose, where nothing
        # validates it -- it went stale the moment the door moved under
        # terminal_controller_cutover/. Pin the two copies together here so the
        # next move cannot leave a pointer to a file that does not exist.
        self.assertTrue(
            door in SCRIPT.read_text(encoding="utf-8"),
            f"scripts/check_delivery_journal_raw_writer.py points readers at a "
            f"door path that is not {door}",
        )
        offenders = []
        gated = 0
        for path in sorted((ROOT / "src/services/discord/turn_bridge").rglob("*.rs")):
            rel = path.relative_to(ROOT).as_posix()
            lines = path.read_text(encoding="utf-8").splitlines()
            for index, line in enumerate(lines):
                if "session_relay_sink" not in line.split("//", 1)[0]:
                    continue
                previous = ""
                for candidate in reversed(lines[:index]):
                    stripped = candidate.strip()
                    if stripped and not stripped.startswith("//"):
                        previous = stripped
                        break
                if rel == door and previous == "#[cfg(unix)]":
                    gated += 1
                else:
                    offenders.append(f"{rel}:{index + 1}: {line.strip()}")
        self.assertEqual(
            offenders,
            [],
            "turn_bridge may reach session_relay_sink only from unix_journal.rs, "
            "and only directly behind #[cfg(unix)]",
        )
        self.assertEqual(gated, 1, "the door is a single re-export, not a scattered set")

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
    # #5071 T1 S5a additions — the family MAP, not the instrumentation.

    # A file that participates in its family's delivery does at least one of three
    # observable things: it writes the durable record, it reaches the journal, or
    # it runs the transport. This vocabulary is the text form of that claim. It is
    # a token list, so it inherits every lexical limit declared above -- but it is
    # a RULE about what an anchor must contain, which is what the previous
    # snapshot-shaped checks could not express.
    DELIVERY_WORK_TOKENS = (
        "write_delivered_frontier(",
        "write_proven_gone_equal_range_frontier(",
        # #5071 T1 S7: the recovery family's anchor stopped calling raw writers
        # in that slice. Without this token the rule below would call it a
        # bystander on its transport calls alone.
        "record_recovery_terminal_delivery(",
        "shadow_mirror_delivered_frontier(",
        "record_long_chunk_terminal_delivery(",
        "commit_ordered_jsonl_range(",
        "record_delivered_content_fingerprint(",
        "append_completed_turn(",
        "finish_sink_delivery(",
        "settle_without_transport(",
        "send_long_message",
        "replace_long_message",
    )
    # #5071 T1 S6 deleted `ANCHORS_WITH_NO_DELIVERY_WORK` and the
    # `wrongly_exempt` arm that read it. S5a carried exactly one exemption, the
    # "pipe stream epoch" family, and S6 removed that family from the registry
    # rather than re-anchoring it, which left the exemption list empty and the
    # arm unreachable. An empty named-exemption list is not a weaker rule than a
    # one-entry one: the rule below now applies to EVERY family with no escape
    # hatch, so a bystander anchor -- including the collector, if a later slice
    # tries to bring it back -- has nowhere to be parked.

    def test_every_family_anchor_sits_on_its_family_delivery_path(self):
        """The rule that would have caught the reaper anchor when it was written.

        Every family anchor must show delivery work IN ITS OWN FILE: a durable
        record write, a journal facade call, or a transport call. An anchor that
        shows none of the three is a file the gate reads while measuring a family
        it has no part in -- which is exactly what
        `tmux_reaper.rs::reap_fresh_routine_orphan` was for the recovery family,
        and what `tmux_watcher/turn_stream_collector.rs` was for the deleted
        "pipe stream epoch" family until #5071 T1 S6.

        There are no exemptions. S5a's single named exemption existed because
        that family's anchor question was open; S6 answered it by deleting the
        family, so the rule is now unconditional.

        What it does not do: it reads one file per family and cannot say whether
        the delivery work it finds belongs to THAT family, nor whether the anchor
        is the best of several candidates. It says only that the anchor is not a
        bystander."""
        bystanders = []
        for name, rel, _symbol in guard.FAMILY_REGISTRY:
            code = "\n".join(
                line.split("//", 1)[0]
                for line in (ROOT / rel).read_text(encoding="utf-8").splitlines()
            )
            does_work = any(token in code for token in self.DELIVERY_WORK_TOKENS) or bool(
                guard.JOURNAL_FACADE_CALL.search(code)
            )
            if not does_work:
                bystanders.append(f"{name} ({rel})")
        self.assertEqual(
            bystanders,
            [],
            "these family anchors show no durable write, no journal facade call and no "
            "transport, so the gate is measuring a file that takes no part in the family",
        )

    def test_source_contract_reaper_anchor_named_a_file_that_writes_no_delivery(self):
        """Why the recovery family's anchor moved, kept measurable.

        Until S5a this family was anchored on
        `tmux_reaper.rs::reap_fresh_routine_orphan`, which matched by NAME
        ("fresh", "orphan") and not by behaviour: the reaper kills tmux sessions
        and finalizes stale-busy turns, and writes no delivery of any kind. This
        pins that measurement, so the move cannot quietly become wrong -- the day
        the reaper does advance a frontier, append to the ledger or reach the
        journal, this fails and the family map has to be re-derived rather than
        left pointing somewhere else.

        `reap_fresh_routine_orphan` is asserted to still exist: the reaper is not
        claimed to have disappeared, only to have no delivery in it."""
        source = (ROOT / "src/services/discord/tmux_reaper.rs").read_text(encoding="utf-8")
        self.assertIn("async fn reap_fresh_routine_orphan(", source)
        for absent in (
            "delivery_record",
            "delivered_frontier",
            "append_completed_turn",
            "shadow_mirror",
            "journal",
        ):
            self.assertEqual(
                source.count(absent),
                0,
                f"tmux_reaper.rs now mentions {absent!r}; the family map cannot keep "
                f"treating it as a file that writes no delivery",
            )

    def test_source_contract_recovery_anchor_holds_the_family_durable_writers(self):
        """The other half of the S5a move: the file the anchor now names really
        does hold this family's durable write.

        #5071 T1 S7 REWROTE THIS TEST, AND THE ASSERTION IT REPLACES IS NAMED
        RATHER THAN DELETED. Until S7 this asserted THREE raw calls in that file
        -- `completed_turn_ledger::append_completed_turn`,
        `delivery_record::write_delivered_frontier` and
        `delivery_record::write_proven_gone_equal_range_frontier` -- because the
        recovery path bypassed the `shadow_mirror_delivered_frontier` funnel and
        wrote the record itself. S7 joined that path to the funnel, so all three
        are now asserted ABSENT and the single
        `delivery_record::record_recovery_terminal_delivery` that replaced them
        is asserted present. The absences are the load-bearing half: they are
        what fails if the bypass is reintroduced here.

        #5071 T1 S5b's half is unchanged: one pre-funnel begin opens the
        obligation the write lives under, and three settles close it. Three, not
        one -- the funnel returns its own verdict, the anchor-bind failure arm
        closes the obligation itself, and a trailing single-use settle keeps a
        future early return from leaving one dangling.

        Counted over the production prefix only (everything before `#[cfg(test)]
        mod tests {`). It is a text count: it cannot prove any call is reached."""
        source = (
            ROOT / "src/services/discord/recovery_engine/terminal_text_idempotency.rs"
        ).read_text(encoding="utf-8")
        production = source[: source.index("#[cfg(test)]\nmod tests {")]
        for gone in (
            "completed_turn_ledger::append_completed_turn(",
            "delivery_record::write_delivered_frontier(",
            "delivery_record::write_proven_gone_equal_range_frontier(",
        ):
            self.assertEqual(
                production.count(gone),
                0,
                f"{gone} is the pre-S7 funnel bypass; the recovery path must reach "
                "delivery_record::record_recovery_terminal_delivery instead",
            )
        self.assertEqual(
            production.count("delivery_record::record_recovery_terminal_delivery("), 1
        )
        # D2: the admission the join added. One call, and it is inside the
        # durable-write funnel rather than at a constructor, so a second one
        # would mean a second write path.
        self.assertEqual(
            production.count("acquire_relay_frontier_mutation_for_incarnation("), 1
        )
        self.assertEqual(production.count("fn record_successful_fresh_send("), 1)
        self.assertEqual(production.count("unix_journal::begin_recovery_terminal("), 1)
        self.assertEqual(production.count("unix_journal::settle_recovery_terminal("), 3)
        self.assertEqual(production.count("unix_journal::Settlement::"), 10)
        self.assertLess(
            production.index("fn record_successful_fresh_send("),
            production.index("delivery_record::record_recovery_terminal_delivery("),
            "the anchor symbol is the entry point of the funnel that holds the write",
        )
        self.assertLess(
            production.index("unix_journal::begin_recovery_terminal("),
            production.index("delivery_record::record_recovery_terminal_delivery("),
            "the obligation opens before the funnel it observes",
        )
        self.assertLess(
            production.index("unix_journal::begin_recovery_terminal("),
            production.index("unix_journal::settle_recovery_terminal("),
            "the obligation opens before any settle",
        )
        self.assertLess(
            production.index("acquire_relay_frontier_mutation_for_incarnation("),
            production.index("delivery_record::record_recovery_terminal_delivery("),
            "admission is taken before the durable write it admits, and held across it",
        )

    def test_source_contract_dormant_fresh_send_writer_is_pinned_uninstrumented(self):
        """The family's other named durable writer, and why the map does not move
        to it either.

        `outbound/turn_output_controller/fresh_send.rs` calls
        `write_delivered_frontier` once, but `OutputPlan::SendFresh` has no
        production constructor -- every mention below is a pattern or a match arm
        except the one in `fresh_send_tests.rs`, which is a test fixture. Nothing
        in production reaches that write today, so it is neither the family's
        anchor nor a gap in coverage; it is dormant.

        It is pinned rather than argued: this dict is the complete set of
        `OutputPlan::SendFresh` mentions in `src/`. The S1r-2~5 owner cutovers
        that make the plan reachable have to add one, which fails here and forces
        the question -- anchor, instrumentation, or both -- to be answered then."""
        mentions = {}
        for path in sorted((ROOT / "src").rglob("*.rs")):
            count = path.read_text(encoding="utf-8").count("OutputPlan::SendFresh")
            if count:
                mentions[path.relative_to(ROOT).as_posix()] = count
        self.assertEqual(
            mentions,
            {
                "src/services/discord/outbound/turn_output_controller.rs": 3,
                "src/services/discord/outbound/turn_output_controller/fresh_send.rs": 1,
                "src/services/discord/outbound/turn_output_controller/fresh_send_tests.rs": 2,
            },
            "OutputPlan::SendFresh gained or lost a mention; if a production owner now "
            "builds it, this family's map and its durable frontier write both need "
            "re-deriving (#5071 T1)",
        )
        fresh_send = (
            ROOT / "src/services/discord/outbound/turn_output_controller/fresh_send.rs"
        ).read_text(encoding="utf-8")
        self.assertEqual(fresh_send.count("delivery_record::write_delivered_frontier("), 1)
        self.assertIsNone(
            guard.JOURNAL_FACADE_CALL.search(fresh_send),
            "fresh_send.rs carries no facade token; it is declared uninstrumented, not covered",
        )
    # #5071 T1 S5b additions — the instrumentation.

    def test_recovery_facade_alternation_matches_only_its_exact_call_shape(self):
        """The S5b alternation must not be a loosening either. It shares the
        `unix_journal::` prefix with S4's controller door — two different modules
        now carry that name — so the two alternatives stay separated by function
        name and nothing else is admitted."""
        for near_miss in (
            " unix_journal::begin_recovery();",
            " unix_journal.begin_recovery_terminal();",
            " rec::begin_recovery_terminal();",
            " unix_journal::recovery_obligation_id();",
            " unix_journal::settle_recovery();",
            " unix_journal::Settlement::FrontierPersisted;",
        ):
            self.assertIsNone(
                guard.JOURNAL_FACADE_CALL.search(near_miss),
                f"{near_miss!r} must not count as a facade call",
            )
        for exact in (
            " unix_journal::begin_recovery_terminal(",
            " unix_journal::settle_recovery_terminal(",
        ):
            self.assertIsNotNone(
                guard.JOURNAL_FACADE_CALL.search(exact),
                f"{exact!r} is a declared facade call",
            )

    def test_recovery_family_regresses_to_uninstrumented(self):
        """Reverse mutation, in fixture form: the 2 -> 1 baseline drop is caused
        by the instrumentation, not by the widened regex. Drop the recovery token
        and the count returns over the re-pinned baseline of 1."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[4][1]
        path.write_text(
            path.read_text(encoding="utf-8").replace(FACADE_MARKERS[4], ""),
            encoding="utf-8",
        )
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("exceeds baseline", message)
        self.assertIn("recovery / fresh-send / orphan family", message)

    def test_source_contract_recovery_reaches_the_journal_through_one_cfg_gated_door(self):
        """Source text only, and the S5b half of the check that would have caught
        the S4 windows regression. `mod session_relay_sink` is `#[cfg(unix)]`
        while `mod recovery_engine`, `mod recovery_paths` and `mod outbound` are
        not, so ANY reference from those three subtrees into that module which is
        not itself behind `#[cfg(unix)]` breaks windows-latest with E0433. This
        pins both halves: the reference lives in exactly ONE file
        (`recovery_engine/unix_journal.rs`, the single door), and every occurrence
        there is immediately preceded by `#[cfg(unix)]`.

        What it is not: it does not compile anything, least of all for windows. It
        is a line scan that strips only `//` suffixes, and it says nothing about
        cross-`cfg` references outside these three subtrees. It substitutes a text
        rule for a target this repository cannot build locally (the msvc target
        needs a Windows C toolchain), so CI's `Fast check + non-PG tests
        (windows-latest)` stays the authority."""
        mod_rs = (ROOT / "src/services/discord/mod.rs").read_text(encoding="utf-8")
        self.assertIn(
            "#[cfg(unix)]\nmod session_relay_sink;",
            mod_rs,
            "the gate this contract protects moved; re-derive the rule instead of relaxing it",
        )
        for ungated in (
            "\nmod recovery_engine;",
            "\nmod recovery_paths;",
            "\npub(crate) mod outbound;",
        ):
            self.assertIn(
                ungated,
                mod_rs,
                f"{ungated.strip()!r} is expected to carry no cfg gate; if it gained one, "
                f"this contract's premise changed",
            )
        door = "src/services/discord/recovery_engine/unix_journal.rs"
        # Self-validating exactly as the turn_bridge contract's literal is: point
        # this at the wrong file and the real door lands in `offenders`. The gate
        # script names the same path in prose, where nothing validates it, so the
        # two copies are pinned together here.
        self.assertTrue(
            door in SCRIPT.read_text(encoding="utf-8"),
            f"scripts/check_delivery_journal_raw_writer.py points readers at a "
            f"door path that is not {door}",
        )
        paths = [ROOT / "src/services/discord/recovery_engine.rs"]
        for root in (
            ROOT / "src/services/discord/recovery_engine",
            ROOT / "src/services/discord/recovery_paths",
            ROOT / "src/services/discord/outbound",
        ):
            paths.extend(sorted(root.rglob("*.rs")))
        offenders = []
        gated = 0
        for path in paths:
            rel = path.relative_to(ROOT).as_posix()
            lines = path.read_text(encoding="utf-8").splitlines()
            for index, line in enumerate(lines):
                if "session_relay_sink" not in line.split("//", 1)[0]:
                    continue
                previous = ""
                for candidate in reversed(lines[:index]):
                    stripped = candidate.strip()
                    if stripped and not stripped.startswith("//"):
                        previous = stripped
                        break
                if rel == door and previous == "#[cfg(unix)]":
                    gated += 1
                else:
                    offenders.append(f"{rel}:{index + 1}: {line.strip()}")
        self.assertEqual(
            offenders,
            [],
            "recovery_engine / recovery_paths / outbound may reach session_relay_sink "
            "only from recovery_engine/unix_journal.rs, and only directly behind #[cfg(unix)]",
        )
        self.assertEqual(gated, 1, "the door is a single re-export, not a scattered set")

    def test_source_contract_recovery_family_emits_no_transport_receipt(self):
        """The signal this family does NOT invent, pinned in source as well as in
        the runtime test R5.

        No transport on this path returns a `DiscordTransportReceipt`, and the
        obligation opens after the transport in any case, so there is no honest
        `T` to emit. Synthesising one from the anchor message id would make
        `requested == returned` true by construction — a receipt that could never
        trip the `channel_mismatch` branch. This pins that the recovery facade
        names no receipt type and builds no `T` event, so a later slice cannot add
        one without deleting an assertion and reading why it was there.

        The runtime proof is `r5_recovery_family_never_classifies_as_delivered`;
        this is the cheap text guard that also covers a `T` added on a branch no
        test happens to drive."""
        facade = (
            ROOT / "src/services/discord/session_relay_sink/journal/recovery.rs"
        ).read_text(encoding="utf-8")
        code = "\n".join(line.split("//", 1)[0] for line in facade.splitlines())
        production = code[: code.index("#[cfg(test)]")]
        self.assertEqual(
            production.count("DiscordTransportReceipt"),
            0,
            "the recovery facade must not name a transport receipt type",
        )
        self.assertEqual(
            production.count('"T"'),
            0,
            "the recovery facade must not construct a T event: no receipt is observable here",
        )
        self.assertIn(
            'r5_recovery_family_never_classifies_as_delivered',
            facade,
            "the runtime test that proves the ceiling must live beside the ceiling",
        )

    # #5071 T1 S6 additions -- the two pins that replace the deleted family.
    #
    # WHY PINS AT ALL. S5a's N1 measured that the gate SCRIPT does not detect
    # anchor restoration: put the family and its old anchor back and
    # `check_delivery_journal_raw_writer.py` still exits 0. Deleting a family
    # therefore removes signal unless something else takes it over. These two
    # tests are that something else, and between them they cover both directions
    # a revert can come from: putting the entry back (pin 1), and building the
    # thing the entry was originally guessing at (pin 2).

    COLLECTOR = "src/services/discord/tmux_watcher/turn_stream_collector.rs"

    def test_source_contract_stream_collector_is_no_longer_a_family_anchor(self):
        """Pin 1. Why "pipe stream epoch" was deleted rather than re-anchored.

        Same shape as
        `test_source_contract_reaper_anchor_named_a_file_that_writes_no_delivery`:
        a measurement of a file that was an anchor and should not have been, kept
        alive so the deletion cannot quietly become wrong.

        Three claims, each independently able to fail:

        1. THE REGISTRY. Five families, and none of them is named "pipe stream
           epoch" or anchored on the collector. Restoring the entry fails here --
           and this is the only mechanical objection to that restore, because the
           gate script itself stays green through it.
        2. THE BYSTANDER MEASUREMENT. The collector writes no durable record,
           runs no transport and holds no facade token, so re-anchoring it would
           re-introduce exactly the defect S5a named. The day it does any of
           those, this fails and the family map has to be re-derived rather than
           left as it is. (`test_every_family_anchor_sits_on_its_family_delivery_path`
           now catches the same restore from the other side, since S6 deleted the
           exemption that used to let this anchor through.)
        3. THE MISSING COORDINATE SYSTEM. `stream_epoch` appears nowhere in
           `src/`, and the shipped journal schema carries no `source_kind`,
           `pipe_stream_epoch` or `pipe_sequence` column, so there is no durable
           coordinate a pipe obligation could be keyed by; the journal has one
           key type. The collector also has exactly ONE caller, in
           `tmux_watcher.rs`, shared by pipe and TUI alike -- pipe is a runtime
           overlay on a shared path, not a module family. Any of those three
           facts changing is a signal to reconsider the family, which is why they
           are asserted here instead of asserted in prose.

        What it is not: a text scan, with every limit the gate script declares.
        It cannot prove the collector is unreachable from a durable write; it
        proves only that no durable write is SPELLED in it."""
        anchors = [rel for _name, rel, _symbol in guard.FAMILY_REGISTRY]
        names = [name for name, _rel, _symbol in guard.FAMILY_REGISTRY]
        self.assertEqual(len(guard.FAMILY_REGISTRY), 5, "S6 pinned the registry at five families")
        self.assertNotIn("pipe stream epoch", names, "the deleted family is back; see the S6 block")
        self.assertNotIn(
            self.COLLECTOR,
            anchors,
            "turn_stream_collector.rs is a bystander, not a delivery anchor; re-anchoring it "
            "repeats the defect #5071 T1 S5a named in tmux_reaper.rs",
        )

        source = (ROOT / self.COLLECTOR).read_text(encoding="utf-8")
        self.assertIn("async fn collect_turn_stream_until_terminal(", source)
        for absent in (
            "delivery_record",
            "delivered_frontier",
            "append_completed_turn",
            "shadow_mirror",
            "journal",
            "send_long_message",
            "replace_long_message",
            "settle_without_transport",
            "advance_watcher_confirmed_end",
        ):
            self.assertEqual(
                source.count(absent),
                0,
                f"turn_stream_collector.rs now mentions {absent!r}; it is no longer the file "
                f"that writes no delivery, so the S6 deletion has to be re-argued",
            )

        callers = {}
        epoch_mentions = {}
        for path in sorted((ROOT / "src").rglob("*.rs")):
            code = "\n".join(
                line.split("//", 1)[0]
                for line in path.read_text(encoding="utf-8").splitlines()
            )
            rel = path.relative_to(ROOT).as_posix()
            calls = code.count("collect_turn_stream_until_terminal(") - code.count(
                "fn collect_turn_stream_until_terminal("
            )
            if calls:
                callers[rel] = calls
            if "stream_epoch" in code:
                epoch_mentions[rel] = code.count("stream_epoch")
        self.assertEqual(
            callers,
            {"src/services/discord/tmux_watcher.rs": 1},
            "the collector gained or lost a caller; the claim that pipe and TUI join on one "
            "shared watcher, with no pipe-only delivery path, has to be re-measured",
        )
        self.assertEqual(
            epoch_mentions,
            {},
            "`stream_epoch` now exists in src/; the coordinate system whose absence is the "
            "reason the family was deleted may have arrived -- revive the family (S6 block)",
        )
        schema = (ROOT / "migrations/postgres/0103_delivery_journal.sql").read_text(encoding="utf-8")
        for column in ("source_kind", "pipe_stream_epoch", "pipe_sequence"):
            self.assertNotIn(
                column,
                schema,
                f"the delivery journal schema grew {column!r}; a pipe obligation can now be "
                f"keyed durably, which is the stated condition for reviving the family",
            )

    # Pin 2's writer vocabulary. The S7' map's symbols are the durable writes;
    # `DELIVERY_WORK_TOKENS` adds the transport and settlement spellings that map
    # does not pin, so the union is "delivery work" in the same sense
    # `test_every_family_anchor_sits_on_its_family_delivery_path` means it --
    # deliberately WIDER than "durable write", because a pipe-only path that
    # transports without recording is the same regression wearing a different
    # hat. Derived, never hand-copied: S7' owns its own list.
    PIPE_VARIANTS = ("LegacyTmuxWrapper", "ProcessBackend")

    def delivery_work_vocabulary(self):
        symbols = set(frontier.EXPECTED_CALL_SITES)
        symbols.update(token.rstrip("(") for token in self.DELIVERY_WORK_TOKENS)
        return sorted(symbols)

    def test_source_contract_no_pipe_gated_production_file_holds_delivery_work(self):
        """Pin 2. The machine's notice that the deleted family should come back.

        THE CONDITION IT WATCHES. "pipe stream epoch" was deleted because pipe
        (`RuntimeHandoffKind::LegacyTmuxWrapper` / `::ProcessBackend`) has no
        delivery code of its own: it and TUI merge onto the same watcher, and
        every durable write on the path is reached identically by both. The
        moment that stops being true -- a durable write, or a transport, reached
        on a branch that only pipe takes -- the family is a real family again and
        the registry has to grow back. Nothing else in the tree says so: the gate
        script cannot see it, and the S7' gate sees the new call site but reports
        it as an unlisted call site, which a reviewer fixes by editing a map.

        THE APPROXIMATION, STATED PLAINLY. "A pipe-gated branch reaches a durable
        write" is a reachability question and this is a text scan, so the
        computable stand-in is CO-EXISTENCE IN ONE PRODUCTION FILE: a file that
        both names one of the two pipe variants and calls a delivery-work symbol.
        That is neither sound nor complete, and both directions are real:

          * FALSE POSITIVES -- two exist today and are listed below with why each
            is not a pipe-only writer. Listing them by name and by symbol is what
            makes the pin usable: a third one, or a new symbol in one of the two,
            fails and gets read.
          * FALSE NEGATIVES -- a pipe-gated call whose durable write lives one
            file away is invisible here, exactly as the anchor gate's own
            one-file-per-family hole is. The pipe branch and the write have to
            land in the same file to be seen. This is the honest ceiling of the
            pin and the reason it is a NOTICE, not a proof.

        WHY CO-EXISTENCE ANYWAY. It is the narrowest lexical condition that is
        actually implied by the thing being watched: a pipe-only writer has to
        be gated somewhere, and the gate for a delivery write in this tree sits
        beside the write far more often than not (both of today's two hits are
        of that shape, and neither is delivery). A stricter form -- same
        function, or same `match` arm -- needs a Rust parser to be honest, and a
        looser one -- module or directory -- turns `tmux_watcher/` into one
        permanent offender and says nothing.

        The vocabulary is IMPORTED from
        scripts/check_durable_frontier_writer_call_sites.py rather than copied,
        so a writer symbol added there widens this pin in the same commit. The
        stripper and cfg(test) filter come from there too, which is why string
        literals and doc prose naming a variant do not count -- unlike the anchor
        gate, which strips only `//` suffixes."""
        vocabulary = self.delivery_work_vocabulary()
        self.assertIn("write_delivered_frontier", vocabulary)
        self.assertIn("send_long_message", vocabulary)
        pipe_re = re.compile(r"\b(?:" + "|".join(self.PIPE_VARIANTS) + r")\b")
        call_res = {s: re.compile(rf"\b{re.escape(s)}\s*\(") for s in vocabulary}
        defn_res = {s: re.compile(rf"\bfn\s+{re.escape(s)}\s*\(") for s in vocabulary}
        found = {}
        for path in sorted((ROOT / "src").rglob("*.rs")):
            if not path.is_file() or frontier.is_test_file(path.name):
                continue
            code = "\n".join(
                text for _lineno, text, production in frontier.production_lines(path) if production
            )
            if not pipe_re.search(code):
                continue
            writes = sorted(
                symbol
                for symbol in vocabulary
                if call_res[symbol].search(code) and not defn_res[symbol].search(code)
            )
            if writes:
                found[path.relative_to(ROOT).as_posix()] = writes
        self.assertEqual(
            found,
            {
                # `runtime_kind_for_recovery()` CLASSIFIES a binding as legacy or
                # process-backend, and `from_str` parses the marker back. Neither
                # gates the durable write in this file:
                # `record_watcher_owner_channel_context` runs from
                # `set_watcher_owner_channel_id`, on every runtime kind alike.
                "src/services/discord/inflight/model.rs": [
                    "record_watcher_owner_channel_context",
                ],
                # The LegacyTmuxWrapper branch here gates
                # `observe_legacy_wrapper_direct_prompt_from_pane`, a pane-prompt
                # OBSERVATION with no delivery in it. The two delivery-work calls
                # sit on the post-terminal suppressed-range path, which every
                # runtime kind reaches.
                "src/services/discord/tmux_watcher/loop_poll_prologue.rs": [
                    "advance_watcher_confirmed_end",
                    "settle_without_transport",
                ],
            },
            "a production file now gates on LegacyTmuxWrapper/ProcessBackend AND performs "
            "delivery work. If the write is reached only on the pipe branch, pipe has its own "
            "delivery path and the 'pipe stream epoch' family deleted in #5071 T1 S6 has to be "
            "restored to FAMILY_REGISTRY with that file as its anchor. If it is shared with "
            "ClaudeTui, add it here with the reason, the way the two entries above are",
        )
if __name__ == "__main__":
    unittest.main()
