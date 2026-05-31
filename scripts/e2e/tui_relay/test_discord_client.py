"""Unit tests for the E2E Discord API client."""

from __future__ import annotations

import json
import sys
import unittest
import urllib.error
from io import BytesIO
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

from tui_relay.discord import DiscordClient  # noqa: E402


class _Response:
    status = 200

    def __init__(self, payload: dict[str, object]):
        self._payload = json.dumps(payload).encode("utf-8")

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False

    def read(self) -> bytes:
        return self._payload


class DiscordClientSendPrompt(unittest.TestCase):
    def test_control_marker_uses_notify_bot(self):
        captured = {}

        def fake_urlopen(request, timeout):  # noqa: ANN001
            captured["url"] = request.full_url
            captured["timeout"] = timeout
            captured["body"] = json.loads(request.data.decode("utf-8"))
            return _Response({"ok": True, "message_id": "m-1"})

        client = DiscordClient(base_url="http://127.0.0.1:8791", timeout_s=12)

        with mock.patch("urllib.request.urlopen", side_effect=fake_urlopen):
            response = client.send_control("1509350393350459434", "### E2E SETUP E-1")

        self.assertEqual(response["ok"], True)
        self.assertEqual(captured["url"], "http://127.0.0.1:8791/api/discord/send")
        self.assertEqual(captured["timeout"], 12)
        self.assertEqual(captured["body"]["target"], "channel:1509350393350459434")
        self.assertEqual(captured["body"]["bot"], "notify")
        self.assertEqual(captured["body"]["source"], "adk-e2e-orchestrator")

    def test_handoff_prompt_starts_headless_codex_turn(self):
        captured = {}

        def fake_urlopen(request, timeout):  # noqa: ANN001
            captured["url"] = request.full_url
            captured["timeout"] = timeout
            captured["body"] = json.loads(request.data.decode("utf-8"))
            return _Response({"ok": True, "turn_id": "turn-1", "status": "started"})

        client = DiscordClient(
            base_url="http://127.0.0.1:8791",
            timeout_s=12,
            handoff_to_agent="adk-codex-pipe-e2e",
            handoff_from_agent="adk-e2e-orchestrator",
        )

        with mock.patch("urllib.request.urlopen", side_effect=fake_urlopen):
            response = client.send_prompt("1509350688667205752", "hello", channel_kind="cdx")

        self.assertEqual(response["ok"], True)
        self.assertEqual(
            captured["url"],
            "http://127.0.0.1:8791/api/agents/adk-codex-pipe-e2e/turn/start",
        )
        self.assertEqual(captured["timeout"], 10.0)
        self.assertEqual(captured["body"]["prompt"], "hello")
        self.assertEqual(captured["body"]["source"], "adk-e2e-orchestrator")
        self.assertEqual(captured["body"]["provider"], "codex")
        self.assertEqual(captured["body"]["channel_id"], "1509350688667205752")

    def test_handoff_prompt_starts_headless_claude_turn(self):
        captured = {}

        def fake_urlopen(request, timeout):  # noqa: ANN001
            captured["body"] = json.loads(request.data.decode("utf-8"))
            return _Response({"ok": True})

        client = DiscordClient(
            base_url="http://127.0.0.1:8791",
            handoff_to_agent="adk-claude-pipe-e2e",
            handoff_from_agent="adk-e2e-orchestrator",
        )

        with mock.patch("urllib.request.urlopen", side_effect=fake_urlopen):
            client.send_prompt("1509350393350459434", "hello", channel_kind="cc")

        self.assertEqual(captured["body"]["provider"], "claude")

    def test_handoff_prompt_retries_busy_mailbox(self):
        attempts = []

        def busy_error() -> urllib.error.HTTPError:
            return urllib.error.HTTPError(
                url="http://127.0.0.1:8791/api/agents/adk-claude-pipe-e2e/turn/start",
                code=409,
                msg="Conflict",
                hdrs={},
                fp=BytesIO(b'{"error":"agent mailbox is busy for channel 1"}'),
            )

        def fake_urlopen(_request, timeout):  # noqa: ANN001, ARG001
            attempts.append(1)
            if len(attempts) == 1:
                raise busy_error()
            return _Response({"ok": True, "turn_id": "turn-2"})

        client = DiscordClient(
            base_url="http://127.0.0.1:8791",
            timeout_s=30,
            handoff_to_agent="adk-claude-pipe-e2e",
            handoff_from_agent="adk-e2e-orchestrator",
        )

        with (
            mock.patch("urllib.request.urlopen", side_effect=fake_urlopen),
            mock.patch("time.sleep") as sleep,
        ):
            response = client.send_prompt("1509350393350459434", "hello", channel_kind="cc")

        self.assertEqual(response["turn_id"], "turn-2")
        self.assertEqual(len(attempts), 2)
        sleep.assert_called_once_with(1.0)

    def test_wait_for_message_evaluates_same_id_edits(self):
        class EditingClient(DiscordClient):
            def __init__(self):
                super().__init__(base_url="http://127.0.0.1:8791", timeout_s=1)
                self.polls = 0

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                self.polls += 1
                if self.polls == 1:
                    return [
                        {
                            "id": "10",
                            "content": "Processing...",
                            "edited_timestamp": None,
                        }
                    ]
                return [
                    {
                        "id": "10",
                        "content": "final [E2E:EDIT]",
                        "edited_timestamp": "2026-05-31T00:00:01Z",
                    }
                ]

        client = EditingClient()
        found, observed = client.wait_for_message(
            "1509350393350459434",
            predicate=lambda message: "[E2E:EDIT]" in (message.get("content") or ""),
            after_id="9",
            timeout_s=1,
            poll_interval_s=0,
        )

        self.assertIsNotNone(found)
        self.assertEqual(found["content"], "final [E2E:EDIT]")
        self.assertEqual([message["content"] for message in observed], [
            "Processing...",
            "final [E2E:EDIT]",
        ])


if __name__ == "__main__":
    unittest.main()
