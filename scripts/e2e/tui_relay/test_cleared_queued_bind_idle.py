"""A queued bind a cleared queue left behind must not block an otherwise idle cell (#7796)."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import post_deploy_turn_smoke as smoke  # noqa: E402
import run_tui_relay as driver  # noqa: E402

CHANNEL = "1559670200419352696"


def _settled(n: int) -> dict:
    return {"receipt": f"tk:discord-r{n}", "at": {"kind": "settled", "settled_at_ms": n, "source": "fixture"}}


class ClearedQueuedBindIdle(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.target = self.root / f"discord_pending_queue/claude/discord_c2adc5461e12b989/{CHANNEL}.json"
        self.target.parent.mkdir(parents=True)
        # Shape of the 11-05 05:37 production file: settled history plus one queued bind whose item is gone.
        self.queued = {"receipt": "tk:discord-cleared", "at": {"kind": "queued", "index": 0, "item": "987bcf2c"}}
        self.snapshot = {"version": 2, "items": [], "head_injection": None, "retained_inputs": [],
                         "terminal_identities": [{"identity": {"primary": 1, "queued_generation": 1, "sources": []},
                                                  "settled_at_ms": 1}],
                         "namespace": "claude/discord_c2adc5461e12b989",
                         "admission_receipts": ["tk:discord-cleared"], "admission_sequence": 13,
                         "admission_binds": [_settled(n) for n in range(12)] + [self.queued]}

    def assert_busy(self, payload, busy):
        self.target.write_text(json.dumps(payload), encoding="utf-8")
        self.assertEqual(bool(driver._runtime_queue_violations(
            runtime_root=self.root, provider="claude", channel_id=CHANNEL)), busy)
        with patch.object(driver, "_read_health_detail", return_value={"mailboxes": []}):
            self.assertEqual(bool(smoke.preflight_busy_reasons(
                base_url="http://offline.invalid", cell="claude-tui", channel_id=CHANNEL,
                runtime_root=self.root)), busy)

    def test_queued_bind_without_its_item_is_idle(self):
        self.assert_busy(self.snapshot, False)

    def test_queued_input_still_waits_through_its_item(self):
        self.assert_busy({**self.snapshot, "items": [{"message_id": 2}]}, True)

    def test_attempt_bind_still_waits(self):
        attempt = {"receipt": "r", "at": {"kind": "attempt"}}
        self.assert_busy({**self.snapshot, "admission_binds": self.snapshot["admission_binds"] + [attempt]}, True)


if __name__ == "__main__":
    unittest.main()
