"""A posted tool card cannot count as a durable body receipt."""

import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

from tui_relay import durable_delivery  # noqa: E402


class ToolCardDurableTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / "runtime"
        self.channel = Path(self.tmp.name) / "o_store" / "99"
        self.channel.mkdir(parents=True)
        (self.channel / "init").write_text("{}", encoding="utf-8")

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, kind):
        prepared = {"type": "prepared", "serial": 0, "piece_index": 0,
                    "payload": "-# ▸ Bash  cargo test  ✓", "anchor_id": 1, "epoch": 1,
                    "unit_key": {"channel_id": 99, "provider": "claude",
                                 "native_key": "msg:0#card", "kind": kind}}
        rows = [prepared, {"type": "posted", "serial": 0, "msg_id": 222}]
        (self.channel / "ledger.jsonl").write_text(
            "".join(json.dumps({"entry": row}) + "\n" for row in rows), encoding="utf-8")

    def scan(self, **kwargs):
        return durable_delivery.scan_records(
            self.root, provider="claude", channel_id="99", message_id="222", **kwargs)

    def test_default_body_probe_rejects_posted_tool_card(self):
        self.write("tool")
        result = self.scan()
        self.assertEqual(result["status"], "failed", result)
        self.assertIn("kind", result["reason"])

    def test_explicit_tool_probe_accepts_card_and_rejects_body_or_missing_kind(self):
        for kind, expected in [("tool", "evaluated"), ("body", "failed"), (None, "failed")]:
            with self.subTest(kind=kind):
                self.write(kind)
                result = self.scan(expected_kind="tool")
                self.assertEqual(result["status"], expected, result)
        self.write("body")
        self.assertEqual(self.scan()["status"], "evaluated")

    def test_poll_propagates_expected_kind_and_rejects_invalid_queries(self):
        self.write("tool")
        result = durable_delivery.poll_records(
            self.root, provider="claude", channel_id="99", message_id="222",
            expected_kind="tool", timeout_s=0)
        self.assertEqual((result["status"], result["unit_kind"]), ("evaluated", "tool"))
        for kind in ("", "Tool", "unknown", None, True, []):
            with self.subTest(kind=kind):
                self.assertEqual(self.scan(expected_kind=kind)["status"], "failed")

    def test_legacy_receipts_cannot_claim_to_prove_a_tool_piece(self):
        (self.channel / "init").unlink()
        records = self.root / "discord_delivery_records" / "claude"
        records.mkdir(parents=True)
        source = {"provider": "claude", "tmux_session_name": "fixture", "turn_nonce": "turn",
                  "range": [10, 20], "generation_mtime_ns": 77,
                  "offset_authority_channel_id": 42, "delivery_channel_id": 99}
        receipt = {"source": source, "delivery_channel_id": 99, "message_id": 222}
        frontier = {"range": [10, 20], "generation_mtime_ns": 77, "panel_channel_id": 99}
        (records / "42.json").write_text(json.dumps(
            {"confirmed_deliveries": [receipt], "delivered_frontier": frontier}), encoding="utf-8")
        self.assertEqual(self.scan()["status"], "evaluated")
        self.assertEqual(self.scan(expected_kind="tool")["status"], "failed")


if __name__ == "__main__":
    unittest.main()
