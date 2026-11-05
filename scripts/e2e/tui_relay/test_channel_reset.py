"""Reset contracts through the driver entrypoints and an isolated HTTP/runtime fixture."""

from __future__ import annotations

import json
import sys
import tempfile
import threading
import unittest
from argparse import Namespace
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import run_tui_relay as driver


class ResetServer:
    def __init__(self, channel="1559670200419352696", *, herdr=False):
        self.channel, self.herdr = channel, herdr
        self.cancel_status, self.cancel = 200, {"ok": True, "queued_remaining": 0}
        self.depth, self.relay_depth, self.channels, self.calls = 0, 0, [channel] if herdr else [], []
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                fixture.calls.append(("GET", self.path))
                mailbox = {"provider": "claude", "channel_id": fixture.channel,
                           "agent_turn_status": "idle", "relay_stall_state": "healthy",
                           "queue_depth": fixture.depth, "relay_health": {"queue_depth": fixture.relay_depth}}
                self.reply(200, {"herdr": {"configured_channels": fixture.channels}, "mailboxes": [mailbox]})

            def do_POST(self):
                fixture.calls.append(("POST", self.path))
                self.reply(fixture.cancel_status, fixture.cancel)

            def reply(self, status, payload):
                body = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.base_url = f"http://127.0.0.1:{self.server.server_port}"
        self.thread = threading.Thread(target=self.server.serve_forever)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_args):
        self.server.shutdown()
        self.thread.join()
        self.server.server_close()


class ChannelResetTests(unittest.TestCase):
    def seed(self, root, channel):
        paths = [root / kind / "claude" / "test-token" / f"{channel}.json"
                 for kind in ("discord_pending_queue", "discord_queued_placeholders")]
        payloads = [b'{"version":2,"items":[],"admission_receipts":["settled-ticket"],'
                    b'"terminal_identities":[{"identity":{"primary":91},"settled_at_ms":42}]}\n',
                    b'[{"user_message_id":91,"placeholder_message_id":92}]\n']
        for path, payload in zip(paths, payloads):
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(payload)
        return dict(zip(paths, payloads))

    def test_reset_preserves_server_owned_metadata_bytes(self):
        for herdr in (False, True):
            with self.subTest(herdr=herdr), tempfile.TemporaryDirectory() as tmp, ResetServer(herdr=herdr) as api:
                root = Path(tmp)
                before = self.seed(root, api.channel)
                result = driver.reset_channel_state(base_url=api.base_url, channel_id=api.channel,
                                                    runtime_root=root, provider="claude")
                self.assertEqual({path: path.read_bytes() for path in before}, before)
                self.assertEqual(result["status"], "ready")
                self.assertEqual(sum(method == "POST" for method, _ in api.calls), 0 if herdr else 1)

    def test_unconfirmed_reset_never_records_success_or_sends_first_prompt(self):
        cases = {"409": (409, {"ok": False}, 0, []), "exception": (200, {}, 0, []),
                 "explicit-failure": (200, {"ok": False}, 0, []),
                 "unmeasured-cancel": (200, {"ok": True}, 0, []),
                 "unconfirmed-cancel": (200, {"queued_remaining": 0}, 0, []),
                 "boolean-depth": (200, {"ok": True, "queued_remaining": False}, 0, []),
                 "remaining": (200, {"ok": True, "queued_remaining": 1}, 0, []),
                 "unmeasured-health": (200, {"ok": True, "queued_remaining": 0}, None, []),
                 "unmeasured-relay": (200, {"ok": True, "queued_remaining": 0}, 0, []),
                 "herdr-residue": (200, {"ok": True}, 1, ["42"]),
                 "noncanonical-channel": (200, {"ok": True, "queued_remaining": 0}, 0, ["42"]),
                 "unknown-host": (200, {"ok": True, "queued_remaining": 0}, 0, None),
                 "malformed-host": (200, {"ok": True, "queued_remaining": 0}, 0, [None])}
        for name, (status, cancel, depth, channels) in cases.items():
            with self.subTest(case=name), tempfile.TemporaryDirectory() as tmp, ResetServer("42") as api:
                api.cancel_status, api.cancel, api.depth, api.channels = status, cancel, depth, channels
                api.relay_depth = None if name == "unmeasured-relay" else depth
                root = Path(tmp)
                before = self.seed(root, api.channel)
                target = "042" if name == "noncanonical-channel" else api.channel
                args = Namespace(cell="claude-tui", channel_id=target, thread_channel_id=None,
                                 base_url=api.base_url, dry_run=False, reset_before_each=True,
                                 queue_runtime_root=tmp, hard_reset_session_each=False, allow_destructive=False)
                scenario = {"id": "E-RESET", "agent_mode": "real_live", "coverage_class": "live",
                            "steps": [{"send_prompt": "first prompt"}], "assertions": []}
                client = Mock()
                def first_input(**_kwargs):
                    client.send_prompt(api.channel, "first prompt")
                    raise driver.assertions.AssertionError("scenario reached its first input")
                original_cancel = driver.cancel_turn
                with patch.object(driver, "cancel_turn", side_effect=OSError("cancel unavailable") if name == "exception" else original_cancel), \
                     patch.object(driver, "run_one_cell", side_effect=first_input), patch.object(driver.time, "sleep"):
                    result = driver.run_scenario(scenario, args=args, client=client, run_id="reset-test")
                self.assertEqual(result["status"], "fail")
                client.send_prompt.assert_not_called()
                self.assertFalse(result.get("executed", False))
                self.assertTrue(result["resets"])
                self.assertTrue(all(reset["status"] == "fail" for reset in result["resets"]))
                self.assertNotIn("cleared", json.dumps(result["resets"]))
                self.assertEqual({path: path.read_bytes() for path in before}, before)
                if name in {"unknown-host", "malformed-host", "herdr-residue", "noncanonical-channel"}:
                    self.assertFalse(any(method == "POST" for method, _ in api.calls))


if __name__ == "__main__":
    unittest.main()
