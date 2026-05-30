"""Unit tests for run_tui_relay health and mailbox safety guards (#2935)."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
import urllib.error
from argparse import Namespace
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

import run_tui_relay as driver  # noqa: E402
from tui_relay import assertions  # noqa: E402


class FakeResponse:
    def __init__(self, status: int, payload: object):
        self.status = status
        self._body = json.dumps(payload).encode("utf-8")

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    def read(self) -> bytes:
        return self._body


class FakeRawResponse:
    def __init__(self, status: int, body: str | bytes):
        self.status = status
        self._body = body if isinstance(body, bytes) else body.encode("utf-8")

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    def read(self) -> bytes:
        return self._body


def _fake_urlopen_for(payloads_by_path):
    def fake_urlopen(request, timeout=0):  # noqa: ARG001
        url = getattr(request, "full_url", str(request))
        for path, payloads in payloads_by_path.items():
            if path in url:
                if len(payloads) > 1:
                    item = payloads.pop(0)
                else:
                    item = payloads[0]
                if isinstance(item, BaseException):
                    raise item
                status, payload = item
                if isinstance(payload, (bytes, str)):
                    return FakeRawResponse(status, payload)
                return FakeResponse(status, payload)
        raise AssertionError(f"unexpected URL: {url}")

    return fake_urlopen


def _health_detail(*mailboxes: dict, status: str = "healthy") -> dict:
    return {
        "status": status,
        "ok": status == "healthy",
        "fully_recovered": status == "healthy",
        "global_finalizing": 0,
        "mailboxes": list(mailboxes),
    }


def _idle_mailbox(channel_id: str = "42", provider: str = "codex") -> dict:
    return {
        "provider": provider,
        "channel_id": channel_id,
        "agent_turn_status": "idle",
        "has_cancel_token": False,
        "queue_depth": 0,
        "recovery_started": False,
        "active_user_message_id": None,
        "inflight_state_present": False,
        "active_dispatch_present": False,
        "relay_stall_state": "healthy",
        "relay_health": {
            "active_turn": "none",
            "bridge_inflight_present": False,
            "mailbox_has_cancel_token": False,
            "mailbox_active_user_msg_id": None,
            "queue_depth": 0,
            "pending_discord_callback_msg_id": None,
            "pending_thread_proof": False,
            "stale_thread_proof": False,
            "desynced": False,
        },
    }


def _busy_mailbox(channel_id: str = "99", provider: str = "claude") -> dict:
    mailbox = _idle_mailbox(channel_id=channel_id, provider=provider)
    mailbox.update(
        {
            "agent_turn_status": "active",
            "has_cancel_token": True,
            "queue_depth": 1,
            "active_user_message_id": 1510404747188768880,
            "inflight_state_present": True,
            "relay_stall_state": "tmux_alive_relay_dead",
        }
    )
    mailbox["relay_health"].update(
        {
            "active_turn": "foreground",
            "bridge_inflight_present": True,
            "mailbox_has_cancel_token": True,
            "mailbox_active_user_msg_id": 1510404747188768880,
            "queue_depth": 1,
            "pending_discord_callback_msg_id": 1510408165194203269,
            "desynced": True,
        }
    )
    return mailbox


class HealthWait(unittest.TestCase):
    def test_wait_for_health_requires_json_healthy_not_just_http_200(self):
        degraded = {
            "status": "degraded",
            "ok": False,
            "degraded": True,
            "fully_recovered": False,
        }
        healthy = {"status": "healthy", "ok": True, "fully_recovered": True}
        payloads = {"/api/health": [(200, degraded), (200, healthy)]}

        with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
            driver.wait_for_health("http://agentdesk.test", timeout_s=1, poll_interval_s=0)

    def test_wait_for_health_times_out_on_degraded_payload(self):
        degraded = {
            "status": "degraded",
            "ok": False,
            "degraded": True,
            "fully_recovered": False,
        }
        payloads = {"/api/health": [(200, degraded)]}

        with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
            with self.assertRaises(assertions.AssertionError) as ctx:
                driver.wait_for_health(
                    "http://agentdesk.test",
                    timeout_s=0.01,
                    poll_interval_s=0.01,
                )
        self.assertIn("status=degraded", str(ctx.exception))

    def test_wait_for_health_preserves_non_json_body_in_timeout(self):
        payloads = {"/api/health": [(200, "not-json-body")]}

        with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
            with self.assertRaises(assertions.AssertionError) as ctx:
                driver.wait_for_health(
                    "http://agentdesk.test",
                    timeout_s=0.01,
                    poll_interval_s=0.01,
                )
        message = str(ctx.exception)
        self.assertIn("non-JSON", message)
        self.assertIn("not-json-body", message)


class RestartGuard(unittest.TestCase):
    def test_foreign_active_mailbox_blocks_restart_even_when_sessions_empty(self):
        payloads = {
            "/api/health/detail": [(503, _health_detail(_busy_mailbox(), status="unhealthy"))],
            "/api/sessions": [(200, {"sessions": []})],
        }

        with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
            with self.assertRaises(assertions.AssertionError) as ctx:
                driver._guard_no_foreign_active_turns(  # noqa: SLF001
                    "http://agentdesk.test",
                    "42",
                    "codex-tui",
                )
        message = str(ctx.exception)
        self.assertIn("claude:99", message)
        self.assertIn("inflight_state_present=true", message)
        self.assertIn("relay_stall_state=tmux_alive_relay_dead", message)

    def test_foreign_guard_fails_closed_with_context_on_health_read_error(self):
        payloads = {
            "/api/health/detail": [urllib.error.URLError("connection refused")],
        }

        with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
            with self.assertRaises(assertions.AssertionError) as ctx:
                driver._guard_no_foreign_active_turns(  # noqa: SLF001
                    "http://agentdesk.test",
                    "42",
                    "codex-tui",
                )
        message = str(ctx.exception)
        self.assertIn("unable to read /api/health/detail", message)
        self.assertIn("connection refused", message)

    def test_current_channel_active_mailbox_is_not_foreign(self):
        payloads = {
            "/api/health/detail": [
                (
                    503,
                    _health_detail(
                        _busy_mailbox("42", provider="codex"),
                        status="unhealthy",
                    ),
                )
            ],
            "/api/sessions": [(200, {"sessions": []})],
        }

        with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
            driver._guard_no_foreign_active_turns(  # noqa: SLF001
                "http://agentdesk.test",
                "42",
                "codex-tui",
            )

    def test_health_detail_missing_mailboxes_includes_status_and_payload(self):
        payloads = {
            "/api/health/detail": [
                (503, {"status": "unhealthy", "error": "shape changed"})
            ],
        }

        with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
            with self.assertRaises(assertions.AssertionError) as ctx:
                driver._read_health_detail("http://agentdesk.test")  # noqa: SLF001
        message = str(ctx.exception)
        self.assertIn("HTTP 503", message)
        self.assertIn("shape changed", message)


class PostScenarioIdle(unittest.TestCase):
    def test_assert_cell_idle_polls_until_mailbox_is_clean(self):
        payloads = {
            "/api/health/detail": [
                (503, _health_detail(_busy_mailbox("42", provider="codex"), status="unhealthy")),
                (200, _health_detail(_idle_mailbox("42", provider="codex"))),
            ]
        }

        with tempfile.TemporaryDirectory() as tmpdir:
            with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
                result = driver.assert_cell_idle(
                    base_url="http://agentdesk.test",
                    channel_id="42",
                    cell="codex-tui",
                    runtime_root=Path(tmpdir),
                    timeout_s=1,
                    poll_interval_s=0,
                )
        self.assertEqual(result["status"], "idle")
        self.assertEqual(result["mailboxes_seen"], 1)

    def test_assert_cell_idle_requires_matching_mailbox(self):
        payloads = {"/api/health/detail": [(200, _health_detail())]}

        with tempfile.TemporaryDirectory() as tmpdir:
            with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
                with self.assertRaises(assertions.AssertionError) as ctx:
                    driver.assert_cell_idle(
                        base_url="http://agentdesk.test",
                        channel_id="42",
                        cell="codex-tui",
                        runtime_root=Path(tmpdir),
                        timeout_s=0.01,
                        poll_interval_s=0.01,
                    )
        self.assertIn("no matching mailbox", str(ctx.exception))

    def test_assert_cell_idle_polls_through_transient_health_read_error(self):
        payloads = {
            "/api/health/detail": [
                urllib.error.URLError("temporary reset"),
                (200, _health_detail(_idle_mailbox("42", provider="codex"))),
            ]
        }

        with tempfile.TemporaryDirectory() as tmpdir:
            with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
                result = driver.assert_cell_idle(
                    base_url="http://agentdesk.test",
                    channel_id="42",
                    cell="codex-tui",
                    runtime_root=Path(tmpdir),
                    timeout_s=1,
                    poll_interval_s=0,
                )
        self.assertEqual(result["status"], "idle")

    def test_assert_cell_idle_fails_on_nonempty_placeholder_file(self):
        payloads = {
            "/api/health/detail": [(200, _health_detail(_idle_mailbox("42", provider="codex")))]
        }

        with tempfile.TemporaryDirectory() as tmpdir:
            placeholder = (
                Path(tmpdir)
                / "discord_queued_placeholders"
                / "codex"
                / "token-a"
                / "42.json"
            )
            placeholder.parent.mkdir(parents=True)
            placeholder.write_text(json.dumps([{"message_id": "1510408165194203269"}]))
            with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
                with self.assertRaises(assertions.AssertionError) as ctx:
                    driver.assert_cell_idle(
                        base_url="http://agentdesk.test",
                        channel_id="42",
                        cell="codex-tui",
                        runtime_root=Path(tmpdir),
                        timeout_s=0.01,
                        poll_interval_s=0.01,
                    )
        self.assertIn("queued_placeholders", str(ctx.exception))


class ScenarioTeardown(unittest.TestCase):
    def test_run_scenario_posts_teardown_when_idle_assertion_fails(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def __init__(self):
                self.control_messages: list[str] = []

            def send_control(self, channel_id, content):  # noqa: ARG002
                self.control_messages.append(content)
                return {"id": str(len(self.control_messages))}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                return []

        args = Namespace(
            cell="codex-tui",
            channel_id="42",
            thread_channel_id=None,
            reset_before_each=False,
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            hard_reset_session_each=False,
            allow_destructive=False,
        )
        scenario = {"id": "E-X", "steps": [], "assertions": []}
        client = FakeClient()

        with (
            patch("run_tui_relay.time.sleep", return_value=None),
            patch(
                "run_tui_relay.assert_cell_idle",
                side_effect=assertions.AssertionError("idle failed"),
            ),
        ):
            result = driver.run_scenario(
                scenario,
                args=args,
                run_id="run-1",
                client=client,  # type: ignore[arg-type]
            )

        self.assertEqual(result["status"], "fail")
        self.assertTrue(
            any(message.startswith("### E2E TEARDOWN") for message in client.control_messages),
            client.control_messages,
        )


if __name__ == "__main__":
    unittest.main()
