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
        path = root / guard.FAMILY_REGISTRY[5][1]
        path.write_text(path.read_text(encoding="utf-8") +
                        'const STRING_MARKER: &str = "self.journal.finish_fresh(";\n#[cfg(all(test, unix))]\nmod tests {\n    fn dishonest() { self.journal.begin_fresh(); }\n    const TEST_MARKER: &str = "self.journal.begin_fresh(";\n}\n',
                        encoding="utf-8")
        self.assertTrue(guard.family_status(root)[0][5][1], "test/string markers are declared lexical matches")

    def test_cfg_test_fn_facade_marker_counts_as_known_limit(self):
        """Evidence: a top-level cfg(test) function is counted, not parsed away."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[5][1]
        path.write_text(path.read_text(encoding="utf-8") + '#[cfg(test)] fn journal_probe() { self.journal.begin_fresh(); }\n', encoding="utf-8")
        self.assertTrue(guard.family_status(root)[0][5][1], "cfg(test) fn marker is a declared lexical match")
        ok, message = guard.check(root); self.assertFalse(ok); self.assertIn("uninstrumented families: 0/6", message)

    def test_line_doc_comment_markers_are_known_limit_and_not_counted(self):
        """Known limitation and declared behavior: line doc comments are stripped after // on each line."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[5][1]
        path.write_text(path.read_text(encoding="utf-8") +
                        "//! self.journal.begin_fresh();\n/// self.journal.begin_fresh();\n",
                        encoding="utf-8")
        status, error = guard.family_status(root)
        self.assertEqual(error, "")
        self.assertFalse(status[5][1], "line doc comment markers are excluded by the declared lexical cut")
        ok, message = guard.check(root)
        self.assertTrue(ok, message)
        self.assertIn("uninstrumented families: 1/6", message)

    def test_block_marker_strings_do_not_hide_real_facade_calls(self):
        """Evidence: block-marker strings no longer delete calls across lines."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[0][1]
        path.write_text(path.read_text(encoding="utf-8") + 'const BLOCK_OPEN: &str = "/*";\nself.journal.begin_fresh();\nconst BLOCK_CLOSE: &str = "*/";\n', encoding="utf-8")
        ok, message = guard.check(root); self.assertTrue(ok, message); self.assertIn("uninstrumented families: 1/6", message)

    def test_raw_string_marker_is_known_lexical_false_positive(self):
        """Known limit: raw strings are not parsed and may count as calls."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[5][1]
        path.write_text(path.read_text(encoding="utf-8") +
                        'const RAW: &str = r#"x" self.journal.begin_fresh("#;\n', encoding="utf-8")
        status, error = guard.family_status(root)
        self.assertEqual(error, "")
        self.assertTrue(status[5][1], "raw-string marker intentionally pierces lexical scan")

    def test_macro_facade_marker_is_known_lexical_match(self):
        """Pin the declared behavior: facade-call text in a macro counts."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[5][1]
        path.write_text(path.read_text(encoding="utf-8") +
                        "macro_rules! journal_probe { () => { self.journal.begin_fresh(); } }\n",
                        encoding="utf-8")
        status, error = guard.family_status(root)
        self.assertEqual(error, "")
        self.assertTrue(status[5][1], "macro facade-call text is a declared lexical match")

    def test_family_baseline_is_measured_and_named(self):
        ok, message = guard.check(self.fixture())
        self.assertTrue(ok, message)
        self.assertIn("uninstrumented families: 1/6", message)
        self.assertIn("whole anchor file including tests", message)
        self.assertIn("pipe stream epoch", message)

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
        # 0, not 1: S5b re-pinned the live baseline to 1, so 1 is no longer an
        # increase over what the fixture measures.
        guard.UNINSTRUMENTED_FAMILY_BASELINE = 0
        self.addCleanup(setattr, guard, "UNINSTRUMENTED_FAMILY_BASELINE", old)
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("pipe stream epoch", message)

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
        self.assertIn("uninstrumented families: 1/6", result.stdout)

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
    # The one family whose anchor does none of the three, carried by name rather
    # than by silence. `turn_stream_collector.rs` owns the `pause_epoch` /
    # `epoch_snapshot` / `turn_delivered` state its family is named for, but it
    # writes no durable record, runs no transport and holds no facade call, so
    # either the anchor is wrong or this family's writer lives somewhere else
    # entirely. Answering that is not #5071 T1 S5a's scope; declaring it is.
    ANCHORS_WITH_NO_DELIVERY_WORK = ("pipe stream epoch",)

    def test_every_family_anchor_sits_on_its_family_delivery_path(self):
        """The rule that would have caught the reaper anchor when it was written.

        Every family anchor must show delivery work IN ITS OWN FILE: a durable
        record write, a journal facade call, or a transport call. An anchor that
        shows none of the three is a file the gate reads while measuring a family
        it has no part in -- which is exactly what
        `tmux_reaper.rs::reap_fresh_routine_orphan` was for the recovery family.

        Exemptions are named, not silent, and an exemption must itself be EMPTY:
        a family listed in `ANCHORS_WITH_NO_DELIVERY_WORK` whose anchor starts
        showing delivery work fails here too, so the list cannot be reused to
        admit a different wrong anchor, and it has to shrink rather than drift
        when that family's anchor is settled.

        What it does not do: it reads one file per family and cannot say whether
        the delivery work it finds belongs to THAT family, nor whether the anchor
        is the best of several candidates. It says only that the anchor is not a
        bystander."""
        bystanders = []
        wrongly_exempt = []
        for name, rel, _symbol in guard.FAMILY_REGISTRY:
            code = "\n".join(
                line.split("//", 1)[0]
                for line in (ROOT / rel).read_text(encoding="utf-8").splitlines()
            )
            does_work = any(token in code for token in self.DELIVERY_WORK_TOKENS) or bool(
                guard.JOURNAL_FACADE_CALL.search(code)
            )
            if name in self.ANCHORS_WITH_NO_DELIVERY_WORK:
                if does_work:
                    wrongly_exempt.append(f"{name} ({rel})")
            elif not does_work:
                bystanders.append(f"{name} ({rel})")
        self.assertEqual(
            bystanders,
            [],
            "these family anchors show no durable write, no journal facade call and no "
            "transport, so the gate is measuring a file that takes no part in the family",
        )
        self.assertEqual(
            wrongly_exempt,
            [],
            "an anchor exempted as showing no delivery work now shows some; remove it "
            "from ANCHORS_WITH_NO_DELIVERY_WORK instead of leaving the exemption to rot",
        )
        self.assertEqual(
            self.ANCHORS_WITH_NO_DELIVERY_WORK,
            ("pipe stream epoch",),
            "the exemption list is a measured finding, not a place to park a new anchor",
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
        """The other half of the move: the file the anchor now names really does
        hold this family's durable writes, all three of them, in one funnel --
        the completed-turn ledger append, the reuse-recorded-anchor frontier
        write and the proven-GONE equal-range re-anchor. Adding a fourth durable
        write to that file, or moving one out, fails here.

        #5071 T1 S5b adds the other half: one pre-funnel begin opens the
        obligation those three writes live under, and three settles close it.
        Three, not one -- the funnel returns its own verdict, the anchor-bind
        failure arm closes the obligation itself, and a trailing single-use settle
        keeps a future early return from leaving one dangling. Eight
        `Settlement::` mentions is that arithmetic plus the six variants the
        funnel's exits name.

        Counted over the production prefix only (everything before `#[cfg(test)]
        mod tests {`), because the file's own fixtures call the same durable
        writer twice and a whole-file count would move whenever a test is added.
        It is a text count: it cannot prove any call is reached."""
        source = (
            ROOT / "src/services/discord/recovery_engine/terminal_text_idempotency.rs"
        ).read_text(encoding="utf-8")
        production = source[: source.index("#[cfg(test)]\nmod tests {")]
        self.assertEqual(production.count("completed_turn_ledger::append_completed_turn("), 1)
        self.assertEqual(production.count("delivery_record::write_delivered_frontier("), 1)
        self.assertEqual(
            production.count("delivery_record::write_proven_gone_equal_range_frontier("), 1
        )
        self.assertEqual(production.count("fn record_successful_fresh_send("), 1)
        self.assertEqual(production.count("unix_journal::begin_recovery_terminal("), 1)
        self.assertEqual(production.count("unix_journal::settle_recovery_terminal("), 3)
        self.assertEqual(production.count("unix_journal::Settlement::"), 8)
        self.assertLess(
            production.index("fn record_successful_fresh_send("),
            production.index("completed_turn_ledger::append_completed_turn("),
            "the anchor symbol is the entry point of the funnel that holds the writers",
        )
        self.assertLess(
            production.index("unix_journal::begin_recovery_terminal("),
            production.index("completed_turn_ledger::append_completed_turn("),
            "the obligation opens before the funnel it observes",
        )
        self.assertLess(
            production.index("unix_journal::begin_recovery_terminal("),
            production.index("unix_journal::settle_recovery_terminal("),
            "the obligation opens before any settle",
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
if __name__ == "__main__":
    unittest.main()
