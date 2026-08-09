"""Fixtures for the E-35 exact durable record and live-safety contracts."""

from __future__ import annotations

import json
import signal
import sys
import tempfile
import time
import unittest
from argparse import Namespace
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

import run_tui_relay as driver  # noqa: E402
from tui_relay import durable_delivery  # noqa: E402


def _receipt(message_id: int, *, generation: int = 77, nonce: str = "turn-1") -> dict:
    return {
        "source": {
            "provider": "claude",
            "tmux_session_name": "AgentDesk-claude-e2e",
            "turn_nonce": nonce,
            "range": [10, 20],
            "generation_mtime_ns": generation,
            "offset_authority_channel_id": 42,
            "delivery_channel_id": 99,
        },
        "delivery_channel_id": 99,
        "message_id": message_id,
    }


def _record(*receipts: dict, generation: int = 77, end: int = 20) -> dict:
    return {
        "delivered_frontier": {
            "range": [10, end],
            "generation_mtime_ns": generation,
            "attempts": 1,
            "panel_msg_id": 222,
            "panel_channel_id": 99,
        },
        "confirmed_deliveries": list(receipts),
    }


class RecordFixtures(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.records = self.root / "discord_delivery_records" / "claude"
        self.records.mkdir(parents=True)

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, record: dict, owner: str = "42") -> None:
        (self.records / f"{owner}.json").write_text(json.dumps(record), encoding="utf-8")

    def scan(
        self,
        message_id: str = "222",
        *,
        channel_id: str = "99",
        provider: str = "claude",
    ) -> dict:
        return durable_delivery.scan_records(
            self.root, provider=provider, channel_id=channel_id, message_id=message_id
        )

    def test_exact_response_receipt_and_covering_frontier_are_evaluated(self):
        self.write(_record(_receipt(222), end=30))
        self.assertEqual(self.scan()["status"], "evaluated")

    def test_raw_authority_numbers_require_exact_json_integers(self):
        cases = (
            ("receipt message fractional", ("confirmed_deliveries", 0, "message_id"), 222.9),
            ("receipt message integer float", ("confirmed_deliveries", 0, "message_id"), 222.0),
            ("receipt message numeric string", ("confirmed_deliveries", 0, "message_id"), "222"),
            ("receipt message bool", ("confirmed_deliveries", 0, "message_id"), True),
            ("receipt message null", ("confirmed_deliveries", 0, "message_id"), None),
            ("receipt message list", ("confirmed_deliveries", 0, "message_id"), [222]),
            ("receipt message object", ("confirmed_deliveries", 0, "message_id"), {"id": 222}),
            ("receipt range start", ("confirmed_deliveries", 0, "source", "range", 0), 10.9),
            ("receipt range end", ("confirmed_deliveries", 0, "source", "range", 1), 20.9),
            (
                "receipt generation",
                ("confirmed_deliveries", 0, "source", "generation_mtime_ns"),
                77.9,
            ),
            (
                "offset authority channel",
                ("confirmed_deliveries", 0, "source", "offset_authority_channel_id"),
                42.9,
            ),
            (
                "source delivery channel",
                ("confirmed_deliveries", 0, "source", "delivery_channel_id"),
                99.9,
            ),
            (
                "receipt delivery channel",
                ("confirmed_deliveries", 0, "delivery_channel_id"),
                99.9,
            ),
            ("frontier range start", ("delivered_frontier", "range", 0), 10.9),
            ("frontier range end", ("delivered_frontier", "range", 1), 30.9),
            (
                "frontier generation",
                ("delivered_frontier", "generation_mtime_ns"),
                77.9,
            ),
            ("frontier channel", ("delivered_frontier", "panel_channel_id"), 99.9),
        )
        for name, path, malformed in cases:
            with self.subTest(name=name):
                record = _record(_receipt(222), end=30)
                target = record
                for component in path[:-1]:
                    target = target[component]
                target[path[-1]] = malformed
                self.write(record)
                result = self.scan()
                self.assertEqual(result["status"], "failed", result)

    def test_receipt_identity_fields_require_nonempty_strings(self):
        cases = (
            ("tmux integer", "tmux_session_name", 1),
            ("turn nonce list", "turn_nonce", ["n"]),
            ("tmux whitespace", "tmux_session_name", "  \t"),
            ("turn nonce whitespace", "turn_nonce", "\n"),
        )
        for name, field, malformed in cases:
            with self.subTest(name=name):
                receipt = _receipt(222)
                receipt["source"][field] = malformed
                self.write(_record(receipt))
                result = self.scan()
                self.assertEqual(result["status"], "failed", result)

    def test_source_and_expected_provider_require_nonempty_strings(self):
        for malformed in (1, ["claude"], {"name": "claude"}, True, "", "  "):
            with self.subTest(record_provider=malformed):
                receipt = _receipt(222)
                receipt["source"]["provider"] = malformed
                self.write(_record(receipt))
                self.assertEqual(self.scan()["status"], "failed")
            with self.subTest(expected_provider=malformed):
                result = self.scan(provider=malformed)
                self.assertEqual(result["status"], "failed", result)
                self.assertIn("invalid query identity", result["reason"])

    def test_owner_filename_requires_nonzero_ascii_decimal(self):
        for owner in (" 42", "+42", "42.0", "true", ""):
            with self.subTest(owner=owner):
                path = self.records / f"{owner}.json"
                path.write_text(json.dumps(_record(_receipt(222))), encoding="utf-8")
                result = self.scan()
                self.assertEqual(result["status"], "failed", result)
                path.unlink()

    def test_expected_ids_require_nonzero_ascii_decimal_strings(self):
        malformed_values = (
            " 99", "99 ", "+99", "-99", "99.0", "9.9e1", "true", "", "0", "９９", 99, True
        )
        self.write(_record(_receipt(222)))
        for field in ("channel_id", "message_id"):
            for malformed in malformed_values:
                with self.subTest(field=field, value=malformed):
                    kwargs = {field: malformed}
                    result = self.scan(**kwargs)
                    self.assertEqual(result["status"], "failed", result)
                    self.assertIn("invalid query identity", result["reason"])

    def test_poll_rejects_malformed_expected_id_without_retry(self):
        ticks = iter((0.0, 0.0))
        result = durable_delivery.poll_records(
            self.root,
            provider="claude",
            channel_id="99",
            message_id="222.9",
            monotonic=lambda: next(ticks),
            sleep=lambda _seconds: self.fail("malformed query retried"),
        )
        self.assertEqual(result["status"], "failed", result)
        self.assertIn("invalid query identity", result["reason"])

    def test_inbound_prompt_id_cannot_substitute_for_outbound_response_id(self):
        self.write(_record(_receipt(111)))
        result = self.scan("222")
        self.assertEqual(result["status"], "failed", result)
        self.assertEqual(result["exact_receipts"], 0)

    def test_same_message_multiple_receipts_is_deterministic_failure(self):
        self.write(_record(_receipt(222), _receipt(222, nonce="turn-2")))
        result = self.scan()
        self.assertEqual(result["status"], "failed", result)
        self.assertEqual(result["exact_receipts"], 2)

    def test_receipt_frontier_generation_mismatch_is_failed(self):
        self.write(_record(_receipt(222, generation=77), generation=88))
        self.assertEqual(self.scan()["status"], "failed")

    def test_receipt_range_requires_covering_frontier(self):
        self.write(_record(_receipt(222), end=15))
        self.assertEqual(self.scan()["status"], "failed")

    def test_frontier_panel_channel_must_match_delivery_channel(self):
        self.write(_record(_receipt(222)))
        record_path = self.records / "42.json"
        record = json.loads(record_path.read_text())
        record["delivered_frontier"]["panel_channel_id"] = 100
        record_path.write_text(json.dumps(record))
        self.assertEqual(self.scan()["status"], "failed")

    def test_receipt_requires_nonempty_tmux_session_and_turn_nonce(self):
        receipt = _receipt(222)
        receipt["source"]["tmux_session_name"] = ""
        receipt["source"]["turn_nonce"] = ""
        self.write(_record(receipt))
        self.assertEqual(self.scan()["status"], "failed")

    def test_receipt_owner_must_match_record_filename(self):
        self.write(_record(_receipt(222)), owner="7")
        self.assertEqual(self.scan()["status"], "failed")

    def test_unrelated_generation_marker_does_not_override_committed_record_proof(self):
        self.write(_record(_receipt(222)))
        (self.root / "AgentDesk-claude-e2e.generation").write_text("88")
        self.assertEqual(self.scan()["status"], "evaluated")

    def test_zero_generation_cannot_supply_durable_commit_proof(self):
        self.write(_record(_receipt(222, generation=0), generation=0))
        result = self.scan()
        self.assertEqual(result["status"], "failed", result)
        self.assertEqual(result["exact_receipts"], 0)

    def test_delayed_write_is_observed_by_bounded_poll(self):
        calls = 0

        def sleep(_seconds: float) -> None:
            nonlocal calls
            calls += 1
            self.write(_record(_receipt(222)))

        ticks = iter((0.0, 0.0, 0.1, 0.1, 0.2))
        result = durable_delivery.poll_records(
            self.root,
            provider="claude",
            channel_id="99",
            message_id="222",
            timeout_s=1,
            monotonic=lambda: next(ticks),
            sleep=sleep,
        )
        self.assertEqual(result["status"], "evaluated", result)
        self.assertEqual(calls, 1)

    def test_poll_timeout_never_promotes_old_frontier(self):
        self.write(_record(_receipt(111)))
        result = durable_delivery.poll_records(
            self.root,
            provider="claude",
            channel_id="99",
            message_id="222",
            timeout_s=0,
        )
        self.assertEqual(result["status"], "failed", result)


class SafetyAndDeadlineFixtures(unittest.TestCase):
    def _args(self, root: str) -> Namespace:
        return Namespace(
            base_url="http://agentdesk.test",
            cell="claude-tui",
            channel_id="99",
            thread_channel_id=None,
            reset_before_each=True,
            dry_run=False,
            queue_runtime_root=root,
            hard_reset_session_each=False,
            allow_destructive=False,
            required_agent_mode=None,
            required_coverage_class=None,
        )

    def _scenario(self) -> dict:
        return {
            "id": "E-35",
            "agent_mode": "none",
            "coverage_class": "live",
            "cells": ["claude-tui"],
            "durable_delivery_probe": True,
            "steps": [{"send_discord_prompt": "marker"}],
            "assertions": [],
        }

    def test_preexisting_active_mailbox_is_unevaluable_without_reset(self):
        scenario = {**self._scenario()}
        busy_mailbox = {
            "provider": "claude",
            "channel_id": "99",
            "agent_turn_status": "active",
            "relay_stall_state": "healthy",
            "relay_health": {"active_turn": "none"},
        }
        with tempfile.TemporaryDirectory() as root, patch.object(
            driver,
            "_read_api_json",
            side_effect=[
                (200, {"cluster_standby": False}),
                (200, {"sessions": []}),
            ],
        ) as read_api, patch.object(
            driver, "_read_health_detail", return_value={"mailboxes": [busy_mailbox]}
        ) as read_detail, patch.object(
            driver.lease,
            "_read_lease",
            return_value={"run_id": "claude-tui-fixture", "acquired_at": 1.0},
        ), patch.object(driver, "reset_channel_state") as reset, patch.object(
            driver, "run_one_cell", return_value={}
        ) as run_one:
            result = driver.run_scenario(
                scenario,
                args=self._args(root),
                run_id="fixture",
                client=object(),
            )
        self.assertEqual(result["status"], "fail", result)
        self.assertEqual(
            (result.get("durable_record_probe") or {}).get("status"),
            "unevaluable",
            result,
        )
        self.assertTrue(result["dirty_active_residue"]["dirty_active_residue"])
        self.assertIn("agent_turn_status=active", result["dirty_active_residue"]["reasons"])
        self.assertEqual(read_api.call_count, 2)
        read_detail.assert_called_once()
        run_one.assert_not_called()
        reset.assert_not_called()

    def test_safety_gate_requires_current_probe_lease_ownership(self):
        idle_mailbox = {
            "provider": "claude",
            "channel_id": "99",
            "agent_turn_status": "idle",
            "relay_stall_state": "healthy",
            "relay_health": {"active_turn": "none"},
        }
        with tempfile.TemporaryDirectory() as root, patch.object(
            driver,
            "_read_api_json",
            side_effect=[
                (200, {"cluster_standby": False}),
                (200, {"sessions": []}),
            ],
        ), patch.object(
            driver, "_read_health_detail", return_value={"mailboxes": [idle_mailbox]}
        ), patch.object(driver.lease, "_read_lease", return_value=None):
            result = driver.durable_probe_safety_gate(
                base_url="http://agentdesk.test",
                cell="claude-tui",
                channel_id="99",
                runtime_root=Path(root),
                lease_run_id="claude-tui-fixture",
            )
        self.assertEqual(result["status"], "unevaluable", result)
        self.assertIn("E2E cell lease is not held by this probe", result["reasons"])

    def test_idle_gate_pass_never_enters_reset_for_durable_probe(self):
        with tempfile.TemporaryDirectory() as root, patch.object(
            driver,
            "durable_probe_safety_gate",
            return_value={"status": "idle", "dirty_active_residue": False},
        ) as gate, patch.object(driver, "reset_channel_state") as reset, patch.object(
            driver, "run_one_cell", return_value={}
        ) as run_one, patch.object(driver.time, "sleep"):
            result = driver.run_scenario(
                self._scenario(),
                args=self._args(root),
                run_id="fixture",
                client=object(),
            )
        self.assertNotEqual(result.get("reason"), "E-35 safety gate refused injection")
        gate.assert_called_once()
        run_one.assert_called_once()
        reset.assert_not_called()

    def test_prompt_recheck_refuses_new_busy_state_without_cleanup(self):
        class Client:
            def send_control(self, _channel, _content): return {"message_id": "100"}
            def fetch_messages(self, _channel, **_kwargs): return []
            def send(self, _channel, _content): raise AssertionError("prompt sent")

        idle = {"status": "idle", "dirty_active_residue": False}
        busy = {
            "status": "unevaluable",
            "dirty_active_residue": True,
            "reasons": ["agent_turn_status=active"],
        }
        with tempfile.TemporaryDirectory() as root, patch.object(
            driver, "durable_probe_safety_gate", side_effect=[idle, busy]
        ) as gate, patch.object(driver, "reset_channel_state") as reset, patch.object(
            driver.time, "sleep"
        ):
            result = driver.run_scenario(
                {**self._scenario(), "agent_mode": "real_live"},
                args=self._args(root),
                run_id="fixture",
                client=Client(),
            )
        self.assertEqual(gate.call_count, 2)
        self.assertEqual(result["status"], "fail", result)
        self.assertEqual(result["durable_record_probe"]["status"], "unevaluable")
        self.assertEqual(result["dirty_active_residue"], busy)
        reset.assert_not_called()

    def test_recheck_to_send_residual_window_and_honesty_claim_are_pinned(self):
        state = {"prompt_sent_after_recheck_race": False}
        class Client:
            base_url = "http://agentdesk.test"
            def send_control(self, _channel, _content): return {"message_id": "100"}
            def fetch_messages(self, _channel, **_kwargs): return []

            def send(self, _channel, _content):
                state["prompt_sent_after_recheck_race"] = True
                return {"message_id": "111"}

        scenario = {
            **self._scenario(),
            "agent_mode": "real_live",
            "steps": [
                {"send_discord_prompt": "marker"},
                {"wait_for_discord_text": "marker", "timeout_s": 1},
            ],
        }
        idle = {"status": "idle", "dirty_active_residue": False}
        with tempfile.TemporaryDirectory() as root, patch.object(
            driver, "durable_probe_safety_gate", return_value=idle
        ) as gate, patch.object(driver.time, "sleep"), patch.object(
            driver,
            "wait_for_discord_text_with_tui_idle_draft_guard",
            return_value=({"id": "222"}, []),
        ), patch.object(
            driver.durable_delivery,
            "poll_records",
            return_value={"status": "evaluated", "reason": "fixture"},
        ), patch.object(driver, "assert_cell_idle", return_value={"status": "idle"}):
            result = driver.run_scenario(
                scenario,
                args=self._args(root),
                run_id="fixture",
                client=Client(),
            )
        claim = " ".join((ROOT / "docs/e2e/multi-provider-e2e.md").read_text().split())
        self.assertIn(
            "the prompt-time recheck narrows the nominal gate-return-to-prompt window "
            "from 368 seconds to 0 seconds, and the last-mailbox-snapshot-to-prompt "
            "window from 373 seconds to 5 seconds. it does not close the toctou",
            claim.lower(),
        )
        self.assertEqual(gate.call_count, 2)
        self.assertTrue(state["prompt_sent_after_recheck_race"])
        self.assertEqual(result["status"], "pass", result)

    @unittest.skipUnless(hasattr(signal, "setitimer"), "POSIX wall-clock timer required")
    def test_phase_deadline_interrupts_blocking_work(self):
        previous = driver._arm_phase_deadline(0.02)  # noqa: SLF001
        try:
            with self.assertRaises(driver.PhaseDeadlineExpired):
                time.sleep(1)
        finally:
            driver._disarm_phase_deadline(previous)  # noqa: SLF001


if __name__ == "__main__":
    unittest.main()
