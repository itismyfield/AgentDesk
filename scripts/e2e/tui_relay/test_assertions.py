"""Unit tests for relay E2E assertion primitives (#2838 P0-2).

These cover the completeness / ordering / duplicate-marker / latency
primitives that close the presence-only blind spot of the legacy contract.
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

from tui_relay import assertions  # noqa: E402


def _relay_msg(msg_id: int, content: str, ts: str | None = None) -> dict:
    """A bot post that qualifies as ADK relay output (not our driver, not chrome)."""

    message = {
        "id": str(msg_id),
        "content": content,
        "author": {"id": "999", "bot": True},
        "type": 0,
    }
    if ts is not None:
        message["timestamp"] = ts
    return message


def _window(*messages: dict) -> assertions.Window:
    window = assertions.Window(setup_marker_id="setup")
    for message in messages:
        window.add(message)
    return window


class OrderedTextPresent(unittest.TestCase):
    def test_passes_in_order_across_messages(self):
        window = _window(_relay_msg(1, "alpha part"), _relay_msg(2, "beta part"))
        assertions.ordered_text_present(window, needles=["alpha", "beta"])

    def test_passes_in_order_same_message(self):
        window = _window(_relay_msg(1, "alpha then beta"))
        assertions.ordered_text_present(window, needles=["alpha", "beta"])

    def test_fails_out_of_order(self):
        window = _window(_relay_msg(1, "beta"), _relay_msg(2, "alpha"))
        with self.assertRaises(assertions.AssertionError):
            assertions.ordered_text_present(window, needles=["alpha", "beta"])

    def test_fails_when_fragment_missing(self):
        window = _window(_relay_msg(1, "alpha"))
        with self.assertRaises(assertions.AssertionError):
            assertions.ordered_text_present(window, needles=["alpha", "beta"])


class NoDuplicateMarker(unittest.TestCase):
    def test_single_marker_passes(self):
        window = _window(_relay_msg(1, "the answer [E2E:T1]"))
        assertions.no_duplicate_marker(window, marker="[E2E:T1]")

    def test_duplicate_with_differing_body_fails(self):
        # Same E2E marker, different surrounding text → no_duplicate_content
        # (byte-identical only) would miss this re-emit; no_duplicate_marker
        # must catch it.
        window = _window(
            _relay_msg(1, "answer one [E2E:T1]"),
            _relay_msg(2, "answer one (resent) [E2E:T1]"),
        )
        with self.assertRaises(assertions.AssertionError):
            assertions.no_duplicate_marker(window, marker="[E2E:T1]")
        # Confirm the legacy assertion is indeed blind to this case.
        assertions.no_duplicate_content(window)


class BodyComplete(unittest.TestCase):
    def test_complete_body_passes(self):
        window = _window(_relay_msg(1, "START middle END"))
        assertions.body_complete(window, head="START", tail="END")

    def test_truncated_tail_fails(self):
        window = _window(_relay_msg(1, "START middle"))
        with self.assertRaises(assertions.AssertionError):
            assertions.body_complete(window, head="START", tail="END")


class RelayLatency(unittest.TestCase):
    def test_within_budget_passes(self):
        window = _window(
            _relay_msg(1, "a", ts="2026-05-29T00:00:00.000000+00:00"),
            _relay_msg(2, "b", ts="2026-05-29T00:00:02.000000+00:00"),
        )
        assertions.relay_latency_within(window, max_seconds=5)

    def test_exceeds_budget_fails(self):
        window = _window(
            _relay_msg(1, "a", ts="2026-05-29T00:00:00.000000+00:00"),
            _relay_msg(2, "b", ts="2026-05-29T00:00:30.000000+00:00"),
        )
        with self.assertRaises(assertions.AssertionError):
            assertions.relay_latency_within(window, max_seconds=5)

    def test_zulu_suffix_timestamp_parsed(self):
        window = _window(
            _relay_msg(1, "a", ts="2026-05-29T00:00:00Z"),
            _relay_msg(2, "b", ts="2026-05-29T00:00:01Z"),
        )
        assertions.relay_latency_within(window, max_seconds=5)

    def test_single_message_is_noop(self):
        window = _window(_relay_msg(1, "only", ts="2026-05-29T00:00:00Z"))
        assertions.relay_latency_within(window, max_seconds=0)


class RunAssertionDispatch(unittest.TestCase):
    """The YAML `run_assertion` dispatch must route the new spec keys, and every
    assertion spec used by a checked-in scenario must be dispatchable (no
    'unknown assertion' / 'bad assertion spec')."""

    def setUp(self):
        import run_tui_relay  # noqa: PLC0415

        self.run_assertion = run_tui_relay.run_assertion

    def test_ordered_text_present_dispatch(self):
        window = _window(_relay_msg(1, "a"), _relay_msg(2, "b"))
        self.run_assertion({"ordered_text_present": ["a", "b"]}, window=window)
        with self.assertRaises(assertions.AssertionError):
            self.run_assertion({"ordered_text_present": ["b", "a"]}, window=window)

    def test_no_duplicate_marker_dispatch(self):
        window = _window(_relay_msg(1, "x [M]"), _relay_msg(2, "y [M]"))
        with self.assertRaises(assertions.AssertionError):
            self.run_assertion({"no_duplicate_marker": "[M]"}, window=window)

    def test_body_complete_dispatch(self):
        window = _window(_relay_msg(1, "H mid T"))
        self.run_assertion({"body_complete": {"head": "H", "tail": "T"}}, window=window)
        with self.assertRaises(assertions.AssertionError):
            self.run_assertion({"body_complete": {"head": "H", "tail": "ZZZ"}}, window=window)

    def test_relay_latency_within_dispatch_dict_and_scalar(self):
        window = _window(
            _relay_msg(1, "a", ts="2026-05-29T00:00:00Z"),
            _relay_msg(2, "b", ts="2026-05-29T00:00:01Z"),
        )
        self.run_assertion({"relay_latency_within": {"max_seconds": 5}}, window=window)
        self.run_assertion({"relay_latency_within": 5}, window=window)

    def test_every_scenario_assertion_spec_is_dispatchable(self):
        import glob  # noqa: PLC0415

        import yaml  # noqa: PLC0415

        window = _window(_relay_msg(1, "placeholder body"))
        scenarios = sorted(glob.glob(str(ROOT / "tests/e2e/tui_relay/scenarios/*.yaml")))
        self.assertTrue(scenarios, "no scenario YAMLs found")
        for path in scenarios:
            with open(path, encoding="utf-8") as handle:
                data = yaml.safe_load(handle)
            for spec in data.get("assertions") or []:
                try:
                    self.run_assertion(spec, window=window)
                except assertions.AssertionError as error:
                    # A scenario assertion may legitimately fail against this
                    # synthetic window (e.g. text_present), but it must never be
                    # an unrouted spec.
                    message = str(error)
                    self.assertNotIn("unknown assertion", message, f"{path}: {spec}")
                    self.assertNotIn("bad assertion spec", message, f"{path}: {spec}")


if __name__ == "__main__":
    unittest.main()
