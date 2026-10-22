"""Notice delegation remains visible to the Legacy writer census."""

from __future__ import annotations

import unittest

from tests import test_check_tui_o_writer_census as existing


class TerminalNoticeCensusTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = existing.CensusGateTests("test_matching_fixture_passes")
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)

    def add_transport(self, rel: str, text: str, counts: dict[str, int]) -> None:
        self.fixture.write("src/services/discord/" + rel, text)
        self.fixture.maps["EXPECTED_PRIMITIVES"][rel] = counts
        self.fixture.maps["CENSUS"][rel] = ("TURN-END", "KEEP_TRANSPORT")

    def assert_gate_passes(self) -> None:
        ok, message = self.fixture.run_gate()
        self.assertTrue(ok, message)

    def test_notice_delegation_counts_reject_extra_and_missing_calls(self) -> None:
        rel = "outbound/notice_fixture.rs"
        calls = {
            "send_terminal_notice": "confirmation::send_terminal_notice(g, ch, text);",
            "send_terminal_notice_http_once": "confirmation::send_terminal_notice_http_once(http, shared, ch, text);",
            ".send_notice_once": "gateway.send_notice_once(ch, text);",
        }
        counts = dict.fromkeys(calls, 1)
        original = "fn route() {\n" + "\n".join(calls.values()) + "\n}\n"
        self.add_transport(rel, original, counts)
        self.assert_gate_passes()
        for primitive, call in calls.items():
            for replacement, observed in ((call + call, 2), ("", 0)):
                with self.subTest(primitive=primitive, observed=observed):
                    self.fixture.write(
                        "src/services/discord/" + rel,
                        original.replace(call, replacement),
                    )
                    self.fixture.assert_fails_with(
                        f"primitive {primitive}: {rel} has {observed}x, expected 1x"
                    )

    def test_new_notice_transport_requires_a_census_row(self) -> None:
        rel = "outbound/new_notice_transport.rs"
        self.add_transport(
            rel,
            "fn route() { send_terminal_notice_http_once(http, shared, ch, text); }\n",
            {"send_terminal_notice_http_once": 1},
        )
        self.assert_gate_passes()
        del self.fixture.maps["CENSUS"][rel]
        self.fixture.assert_fails_with(
            f"census: {rel} sends but has no CENSUS row"
        )

    def test_ufcs_notice_send_is_counted_separately(self) -> None:
        rel = "notice_gateway.rs"
        call = "TurnGateway :: send_notice_once(gateway, ch, text);"
        self.add_transport(
            rel,
            "fn route() { " + call + " }\n",
            {"TurnGateway::send_notice_once": 1},
        )
        self.assert_gate_passes()
        self.fixture.write("src/services/discord/" + rel, "fn route() {}\n")
        self.fixture.assert_fails_with(
            f"primitive TurnGateway::send_notice_once: {rel} has 0x, expected 1x"
        )
