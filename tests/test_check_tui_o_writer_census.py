"""Discrimination tests for the TUI O writer census gate.

Each negative applies one realistic regression to a synthetic tree whose maps
match it, and asserts the specific failure reason, so a green run means the
gate still sees that regression.
"""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check_tui_o_writer_census.py"
SPEC = importlib.util.spec_from_file_location("tui_o_writer_census", SCRIPT)
census = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(census)

CUT_FILE = """pub fn deliver(ch: u64) {
    if crate::services::tui_o::cutover::o_owns_tui_output_for_tmux_session(&s) { return; }
    send_channel_message(ch, "body");
}
"""
EVID_FILE = """pub fn read_receipt() -> bool { lookup_receipt() }
"""
CUTOVER_FILE = """pub const O_TUI_WRITER: bool = false;
pub fn o_owns_tui_output(kind: Kind) -> bool { O_TUI_WRITER && kind.is_tui() }
"""
MAPS = {
    "EXPECTED_PRIMITIVES": {"sink.rs": {"send_channel_message*": 1}},
    "CENSUS": {"sink.rs": ("W20", "CUT_D")},
    "EXPECTED_GATES": {
        "src/services/discord/sink.rs": ("deliver:claim",),
    },
    "R_EVID": ("src/services/discord/outbound/delivery_record.rs",),
}


class CensusGateTests(unittest.TestCase):
    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.write("src/services/discord/sink.rs", CUT_FILE)
        self.write("src/services/discord/outbound/delivery_record.rs", EVID_FILE)
        self.write("src/services/tui_o/cutover.rs", CUTOVER_FILE)
        self.maps = {key: _copy(value) for key, value in MAPS.items()}

    def write(self, rel: str, text: str) -> None:
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def run_gate(self) -> tuple[bool, str]:
        with mock.patch.multiple(census, **self.maps):
            return census.check(self.root, pinned_test_only_files=[])

    def assert_fails_with(self, reason: str) -> None:
        ok, message = self.run_gate()
        self.assertFalse(ok, message)
        self.assertIn(reason, message)

    def test_matching_fixture_passes(self) -> None:
        ok, message = self.run_gate()
        self.assertTrue(ok, message)

    def test_added_send_fails(self) -> None:
        self.write("src/services/discord/sink.rs", CUT_FILE + "fn more() { edit_channel_message(1); }\n")
        self.assert_fails_with("primitive edit_channel_message*: sink.rs has 1x, expected 0x")

    def test_send_in_new_file_needs_a_census_row(self) -> None:
        self.write("src/services/discord/new_path.rs", "fn f() { ctx.say(\"hi\"); }\n")
        self.maps["EXPECTED_PRIMITIVES"]["new_path.rs"] = {".say": 1}
        self.assert_fails_with("census: new_path.rs sends but has no CENSUS row")

    def test_removing_the_gate_from_a_cut_file_fails_even_with_updated_counts(self) -> None:
        self.write("src/services/discord/sink.rs", CUT_FILE.replace(
            "    if crate::services::tui_o::cutover::o_owns_tui_output_for_tmux_session(&s) { return; }\n", ""
        ))
        self.maps["EXPECTED_GATES"] = {}
        self.assert_fails_with("census: CUT_D row W20 has no claim gate in src/services/discord/sink.rs")

    def test_a_body_gate_swapped_for_a_peek_fails_with_or_without_updated_counts(self) -> None:
        self.write("src/services/discord/sink.rs", CUT_FILE.replace(
            "cutover::o_owns_tui_output_for_tmux_session", "cutover::peek_o_owns_tui_output_for_tmux_session"
        ))
        self.assert_fails_with(
            "gate sites: src/services/discord/sink.rs has ['deliver:peek'], expected ['deliver:claim']"
        )
        self.maps["EXPECTED_GATES"]["src/services/discord/sink.rs"] = ("deliver:peek",)
        self.assert_fails_with("census: CUT_D row W20 has no claim gate in src/services/discord/sink.rs")

    def test_a_peek_turned_into_a_claim_fails_the_pins(self) -> None:
        self.write("src/services/discord/sink.rs", CUT_FILE + "fn probe() -> bool { bridge_o_body_peek_decision(k, true) }\n")
        self.maps["EXPECTED_GATES"]["src/services/discord/sink.rs"] = ("deliver:claim", "probe:peek")
        ok, message = self.run_gate()
        self.assertTrue(ok, message)
        self.write("src/services/discord/sink.rs", CUT_FILE + "fn probe() -> bool { bridge_o_body_cut_decision(k, true) }\n")
        self.assert_fails_with("has ['deliver:claim', 'probe:claim'], expected ['deliver:claim', 'probe:peek']")

    def test_gates_trading_roles_within_a_file_fail_with_unchanged_counts(self) -> None:
        paired = (
            "pub fn deliver(ch: u64, body: bool) {\n"
            "    let owns = if body { NO_BODY } else { BODY };\n"
            "    if owns(ch).unwrap_or(true) { return; }\n"
            "    send_channel_message(ch, \"body\");\n"
            "}\n"
            "fn probe(ch: u64) -> bool { CHECK(ch) }\n"
        )
        def tree(no_body, body, check):
            return paired.replace("NO_BODY", no_body).replace("BODY", body).replace("CHECK", check)
        claim, peek = "o_owns_tui_output_for_channel", "peek_o_owns_tui_output_for_channel"
        self.maps["EXPECTED_GATES"]["src/services/discord/sink.rs"] = (
            "deliver:peek", "deliver:claim", "probe:peek"
        )
        self.write("src/services/discord/sink.rs", tree(peek, claim, peek))
        ok, message = self.run_gate()
        self.assertTrue(ok, message)
        swaps = {
            "branches swapped": tree(claim, peek, peek),
            "claim moved to another fn": tree(peek, peek, claim),
        }
        for label, text in swaps.items():
            with self.subTest(swap=label):
                self.write("src/services/discord/sink.rs", text)
                self.assert_fails_with("gate sites: src/services/discord/sink.rs has")

    def test_undecided_target_fails(self) -> None:
        for target in ("TBD", "?", "COV:TBD"):
            with self.subTest(target=target):
                self.maps["CENSUS"]["sink.rs"] = ("W20", target)
                self.assert_fails_with(f"census: sink.rs has undecided target {target!r}")

    def test_helper_in_r_evid_file_fails_even_with_updated_counts(self) -> None:
        for kind, helper in (("claim", "o_owns_tui_output"), ("peek", "peek_o_owns_tui_output")):
            with self.subTest(kind=kind):
                self.write("src/services/discord/outbound/delivery_record.rs",
                           f"pub fn read_receipt() -> bool {{ {helper}(kind) || lookup_receipt() }}\n")
                self.maps["EXPECTED_GATES"]["src/services/discord/outbound/delivery_record.rs"] = (
                    f"read_receipt:{kind}",
                )
                self.assert_fails_with("gate: cutover helper in R-EVID file src/services/discord/outbound/delivery_record.rs")

    def test_gate_count_change_needs_the_map_in_the_same_change(self) -> None:
        self.write("src/services/discord/sink.rs", CUT_FILE + "fn g() -> bool { bridge_o_body_cut_decision(k, true) }\n")
        self.assert_fails_with("has ['deliver:claim', 'g:claim'], expected ['deliver:claim']")
        self.maps["EXPECTED_GATES"]["src/services/discord/sink.rs"] = ("deliver:claim", "g:claim")
        ok, message = self.run_gate()
        self.assertTrue(ok, message)

    def test_flag_token_outside_cutover_fails(self) -> None:
        self.write("src/services/discord/other.rs", "fn f() -> bool { cutover::O_TUI_WRITER }\n")
        self.assert_fails_with("flag: O_TUI_WRITER outside")

    def test_cfg_test_sends_and_helper_definitions_are_not_counted(self) -> None:
        self.write("src/services/discord/sink.rs", CUT_FILE + (
            "#[cfg(test)]\nmod tests { fn t() { send_channel_message(1); } }\n"
        ))
        self.write("src/services/tui_o/cutover.rs", CUTOVER_FILE + "pub fn bridge_o_body_cut_decision() {}\n")
        ok, message = self.run_gate()
        self.assertTrue(ok, message)

    def test_deferred_row_still_passes_the_census(self) -> None:
        self.maps["CENSUS"]["sink.rs"] = ("W20", "DEFER_A1_4B")
        ok, message = self.run_gate()
        self.assertTrue(ok, message)
        self.assertIn("1 rows deferred", message)

    def test_real_tree_passes(self) -> None:
        ok, message = census.check(ROOT)
        self.assertTrue(ok, message)


class FlipReadinessTests(unittest.TestCase):
    """Census PASS must not read as flip readiness: each gap keeps flip_ready false."""

    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        tests = self.root / "src/services/discord/funnel_tests.rs"
        tests.parent.mkdir(parents=True)
        tests.write_text("#[test]\nfn o_delegated_funnel_cut() {}\n", encoding="utf-8")

    def readiness(self, **overrides) -> tuple[bool, str]:
        maps = {
            "CENSUS": {"sink.rs": ("W20", "CUT_D")},
            "FLIP_READY_TESTS": {"W20": ("o_delegated_funnel_cut",)},
            **overrides,
        }
        with mock.patch.multiple(census, **maps):
            return census.flip_readiness(self.root)

    def test_complete_funnels_are_flip_ready(self) -> None:
        self.assertEqual(self.readiness(), (True, "flip_ready=true: 1 funnel tests over 1 rows"))

    def test_each_gap_keeps_flip_ready_false(self) -> None:
        gaps = {
            "deferred census rows: sink.rs": ("CENSUS", {"sink.rs": ("W30", "DEFER_A1_4B")}),
            "FLIP_READY_TESTS is empty": ("FLIP_READY_TESTS", {}),
            "funnel tests missing from src/: o_delegated_gone": (
                "FLIP_READY_TESTS", {"W20": ("o_delegated_funnel_cut", "o_delegated_gone")}
            ),
        }
        for reason, (key, value) in gaps.items():
            with self.subTest(reason=reason):
                ready, verdict = self.readiness(**{key: value})
                self.assertFalse(ready, verdict)
                self.assertIn(reason, verdict)

    def test_empty_funnel_blocks_readiness_with_another_funnel_present(self) -> None:
        ready, verdict = self.readiness(FLIP_READY_TESTS={
            "W20": ("o_delegated_funnel_cut",), "W33": (),
        })
        self.assertFalse(ready, verdict)
        self.assertIn("funnel test lists empty: W33", verdict)

    def test_funnel_names_require_attached_test_attributes_outside_prose(self) -> None:
        source = self.root / "src/services/discord/funnel_tests.rs"
        cases = {
            "line comment": ("// #[test]\n// fn required() {}\n", False),
            "nested block comment": ("/* outer /* #[test] */ fn required() {} */", False),
            "string": ('const TEXT: &str = "#[test] fn required() {}";', False),
            "raw string": ('const TEXT: &str = r#"\n#[tokio::test]\nasync fn required() {}\n"#;', False),
            "ordinary function": ("fn required() {}", False),
            "commented attribute": ("// #[test]\nfn required() {}", False),
            "attribute on preceding function": ("#[test]\nfn other() {}\nfn required() {}", False),
            "ignored test": ("#[test]\n#[ignore]\nfn required() {}", False),
            "ignore before async test": (
                '#[ignore = "slow"]\n#[tokio::test]\nasync fn required() {}', False,
            ),
            "synchronous test": ("#[test]\nfn required() {}", True),
            "async test": ("#[tokio::test]\nasync fn required() {}", True),
            "async test options and attributes": (
                '#[tokio::test(flavor = "multi_thread", worker_threads = 2)]\n'
                '#[cfg(test)]\n/* note */ async fn required() {}', True,
            ),
        }
        for label, (text, expected) in cases.items():
            with self.subTest(case=label):
                source.write_text(text, encoding="utf-8")
                ready, verdict = self.readiness(FLIP_READY_TESTS={"W20": ("required",)})
                self.assertEqual(ready, expected, verdict)
                if not expected:
                    self.assertIn("funnel tests missing from src/: required", verdict)

    def test_require_flip_ready_turns_a_false_verdict_into_rc_1(self) -> None:
        passing = (True, "OK")
        for ready, args, rc in ((False, [], 0), (False, ["--require-flip-ready"], 1),
                                (True, ["--require-flip-ready"], 0)):
            with self.subTest(ready=ready, args=args):
                with mock.patch.object(census, "check", return_value=passing), \
                        mock.patch.object(census, "flip_readiness", return_value=(ready, "v")), \
                        mock.patch("sys.stdout"):
                    self.assertEqual(census.main(args), rc)


def _copy(value):
    if isinstance(value, dict):
        return {key: _copy(inner) for key, inner in value.items()}
    return value


if __name__ == "__main__":
    unittest.main()
