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

    def scan(self, message_id: str = "222") -> dict:
        return durable_delivery.scan_records(
            self.root, provider="claude", channel_id="99", message_id=message_id
        )

    def test_exact_response_receipt_and_covering_frontier_are_evaluated(self):
        self.write(_record(_receipt(222), end=30))
        self.assertEqual(self.scan()["status"], "evaluated")

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

    def test_generation_rotation_before_write_has_no_acceptable_commit(self):
        self.write(_record(_receipt(222, generation=77), generation=88))
        self.assertEqual(self.scan()["status"], "failed")

    def test_generation_rotation_after_atomic_write_preserves_historical_proof(self):
        self.write(_record(_receipt(222)))
        (self.root / "AgentDesk-claude-e2e.generation").write_text("88")
        self.assertEqual(self.scan()["status"], "evaluated")

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

    def test_active_mailbox_residue_counts_as_unevaluable_without_reset(self):
        scenario = {
            "id": "E-35",
            "agent_mode": "none",
            "coverage_class": "live",
            "cells": ["claude-tui"],
            "durable_delivery_probe": True,
            "steps": [{"send_discord_prompt": "marker"}],
            "assertions": [],
        }
        residue = {
            "status": "unevaluable",
            "dirty_active_residue": True,
            "reasons": ["agent_turn_status=active"],
        }
        with tempfile.TemporaryDirectory() as root, patch.object(
            driver, "durable_probe_safety_gate", return_value=residue
        ), patch.object(driver, "reset_channel_state") as reset, patch.object(
            driver, "run_one_cell", return_value={}
        ):
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
        reset.assert_not_called()

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
