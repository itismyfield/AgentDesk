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
        self.other_mailboxes = []
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                fixture.calls.append(("GET", self.path))
                mailbox = {"provider": "claude", "channel_id": fixture.channel,
                           "agent_turn_status": "idle", "relay_stall_state": "healthy",
                           "queue_depth": fixture.depth, "relay_health": {"queue_depth": fixture.relay_depth}}
                # Like the real server: only the detail endpoint carries the Herdr block.
                route = self.path.split("?")[0]
                if route not in ("/api/health", "/api/health/detail"):
                    return self.reply(404, {"error": "not found"})
                payload = {"mailboxes": [mailbox, *fixture.other_mailboxes]}
                if route == "/api/health/detail":
                    payload["herdr"] = {"configured_channels": fixture.channels}
                self.reply(200, payload)

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


class MatrixResetTeardownTests(unittest.TestCase):
    def run_matrix(self, api, runtime_root, *, target="111", send_error=None):
        import run_multi_provider_matrix as matrix
        scenarios = Path(__file__).resolve().parents[3] / "tests/e2e/tui_relay/scenarios"
        scenario = next(row for row in matrix.load_restart_guard_scenarios(scenarios) if row["id"] == "E-17")
        args = Namespace(base_url=api.base_url, dry_run=False, queue_runtime_root=str(runtime_root),
                         reset_before_each=True, turn_start_timeout_s=5, allow_destructive=True,
                         required_agent_mode=None, required_coverage_class=None)
        with patch.dict("os.environ", {"AGENTDESK_E2E_ALLOW_DESTRUCTIVE": "1"}), \
             patch.object(driver.discord.DiscordClient, "send_prompt",
                          side_effect=send_error or OSError("unexpected hold input")) as send_prompt:
            result = matrix.run_foreign_active_restart_guard_scenario(
                scenario, args=args, run_id="matrix-reset", channel_ids={"claude-tui": target, "codex-tui": "222"},
                selected_cells=["claude-tui", "codex-tui"], pass_index=1)
        return result, send_prompt

    def test_reset_refusal_never_cancels_a_hold_this_run_did_not_attempt(self):
        cases = [("herdr-residue", ["111"], 1, "111", 200, 0, "queue depths"),
                 ("unknown-host", None, 0, "111", 200, 0, "mapping unreadable"),
                 ("noncanonical-channel", ["111"], 0, "0111", 200, 0, "channel is unreadable"),
                 ("legacy-409", [], 0, "111", 409, 1, "HTTP 409")]
        for name, channels, depth, target, status, expected_posts, reason in cases:
            with self.subTest(case=name), tempfile.TemporaryDirectory() as tmp, ResetServer("111") as api:
                api.channels, api.depth, api.relay_depth, api.cancel_status = channels, depth, depth, status
                root = Path(tmp)
                before = ChannelResetTests().seed(root, api.channel)
                result, send_prompt = self.run_matrix(api, root, target=target)
                self.assertEqual(result["status"], "fail")
                self.assertIn(reason, result["reason"])
                send_prompt.assert_not_called()
                self.assertEqual(sum(method == "POST" for method, _ in api.calls), expected_posts)
                self.assertNotIn("foreign_hold_cancel", result.get("teardown", {}))
                self.assertEqual({path: path.read_bytes() for path in before}, before)

    def test_lost_hold_response_still_cancels_after_the_submission_attempt(self):
        with tempfile.TemporaryDirectory() as tmp, ResetServer("111", herdr=True) as api:
            api.other_mailboxes = [{"provider": "codex", "channel_id": "222", "queue_depth": 0,
                                    "relay_health": {"queue_depth": 0}}]
            result, send_prompt = self.run_matrix(api, Path(tmp), send_error=OSError("hold response lost"))
            self.assertEqual(result["status"], "fail")
            self.assertIn("hold response lost", result["reason"])
            self.assertEqual(send_prompt.call_count, 1)
            self.assertEqual(api.calls.count(("POST", "/api/turns/111/cancel?force=true")), 1)
            self.assertEqual(api.calls.count(("POST", "/api/turns/222/cancel?force=true")), 1)
            self.assertTrue(result["teardown"]["foreign_hold_cancel"]["ok"])


if __name__ == "__main__":
    unittest.main()
