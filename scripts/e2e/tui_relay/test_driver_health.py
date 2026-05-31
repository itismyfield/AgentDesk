"""Unit tests for run_tui_relay health and mailbox safety guards (#2935)."""

from __future__ import annotations

import json
import sys
import tempfile
import threading
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
        for path, payloads in sorted(
            payloads_by_path.items(), key=lambda item: len(item[0]), reverse=True
        ):
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
        "global_active": 0,
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

    def test_current_cell_transient_global_finalizing_is_polled_until_drained(self):
        payloads = {
            "/api/health/detail": [
                (
                    503,
                    {
                        **_health_detail(
                            _idle_mailbox("42", provider="codex"),
                            status="degraded",
                        ),
                        "global_finalizing": 1,
                    },
                ),
                (200, _health_detail(_idle_mailbox("42", provider="codex"))),
            ],
            "/api/sessions": [(200, {"sessions": []})],
        }

        with (
            patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)),
            patch("run_tui_relay.time.sleep", return_value=None) as sleep,
        ):
            driver._guard_no_foreign_active_turns(  # noqa: SLF001
                "http://agentdesk.test",
                "42",
                "codex-tui",
                finalizing_drain_timeout_s=1,
                poll_interval_s=0,
            )

        sleep.assert_called_once_with(0)

    def test_global_finalizing_that_does_not_drain_blocks_restart(self):
        payloads = {
            "/api/health/detail": [
                (
                    503,
                    {
                        **_health_detail(
                            _idle_mailbox("42", provider="codex"),
                            status="degraded",
                        ),
                        "global_finalizing": 1,
                    },
                )
            ],
            "/api/sessions": [(200, {"sessions": []})],
        }

        with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
            with self.assertRaises(assertions.AssertionError) as ctx:
                driver._guard_no_foreign_active_turns(  # noqa: SLF001
                    "http://agentdesk.test",
                    "42",
                    "codex-tui",
                    finalizing_drain_timeout_s=0,
                    poll_interval_s=0,
                )

        self.assertIn("global_finalizing=1", str(ctx.exception))

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


class ScenarioHealthProbe(unittest.TestCase):
    def test_assert_health_passes_with_status_and_counter_caps(self):
        payloads = {
            "/api/health/detail": [
                (200, _health_detail(_idle_mailbox("42", provider="codex")))
            ],
            "/api/health": [
                (
                    200,
                    {
                        "status": "healthy",
                        "ok": True,
                        "fully_recovered": True,
                        "degraded_reasons": [],
                    },
                )
            ],
        }

        with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
            result = driver.assert_health(
                "http://agentdesk.test",
                {
                    "require_status": "healthy",
                    "forbid_degraded_reasons": ["global_active_counter_out_of_bounds"],
                    "global_active_max": 0,
                    "global_finalizing_max": 0,
                },
            )

        self.assertEqual(result["status"], "healthy")
        self.assertEqual(result["global_active"], 0)
        self.assertEqual(result["global_finalizing"], 0)

    def test_assert_health_fails_on_forbidden_reason_and_counter_cap(self):
        payloads = {
            "/api/health/detail": [
                (
                    503,
                    {
                        **_health_detail(_busy_mailbox("42", provider="codex")),
                        "global_active": 2,
                        "global_finalizing": 1,
                    },
                )
            ],
            "/api/health": [
                (
                    503,
                    {
                        "status": "unhealthy",
                        "ok": False,
                        "fully_recovered": False,
                        "degraded": True,
                        "degraded_reasons": [
                            "global_active_counter_out_of_bounds:raw=4"
                        ],
                    },
                )
            ],
        }

        with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
            with self.assertRaises(assertions.AssertionError) as ctx:
                driver.assert_health(
                    "http://agentdesk.test",
                    {
                        "forbid_degraded_reasons": [
                            "global_active_counter_out_of_bounds"
                        ],
                        "global_active_max": 0,
                        "global_finalizing_max": 0,
                    },
                )

        message = str(ctx.exception)
        self.assertIn("forbidden_degraded_reasons", message)
        self.assertIn("global_active=2 > 0", message)
        self.assertIn("global_finalizing=1 > 0", message)

    def test_assert_health_fails_on_negative_global_counter(self):
        payloads = {
            "/api/health/detail": [
                (
                    200,
                    {
                        **_health_detail(_idle_mailbox("42", provider="codex")),
                        "global_active": -1,
                    },
                )
            ],
            "/api/health": [
                (
                    200,
                    {
                        "status": "healthy",
                        "ok": True,
                        "fully_recovered": True,
                        "degraded_reasons": [],
                    },
                )
            ],
        }

        with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
            with self.assertRaises(assertions.AssertionError) as ctx:
                driver.assert_health(
                    "http://agentdesk.test",
                    {
                        "global_active_max": 0,
                        "global_finalizing_max": 0,
                    },
                )

        self.assertIn("global_active=-1 < 0", str(ctx.exception))

    def test_assert_health_polls_until_global_counters_drain(self):
        payloads = {
            "/api/health/detail": [
                (
                    503,
                    {
                        **_health_detail(_idle_mailbox("42", provider="codex")),
                        "global_active": 1,
                        "global_finalizing": 1,
                    },
                ),
                (200, _health_detail(_idle_mailbox("42", provider="codex"))),
            ],
            "/api/health": [
                (
                    503,
                    {
                        "status": "degraded",
                        "ok": False,
                        "fully_recovered": False,
                        "degraded": True,
                        "degraded_reasons": [],
                    },
                ),
                (
                    200,
                    {
                        "status": "healthy",
                        "ok": True,
                        "fully_recovered": True,
                        "degraded_reasons": [],
                    },
                ),
            ],
        }

        with (
            patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)),
            patch("run_tui_relay.time.sleep", return_value=None) as sleep,
        ):
            result = driver.assert_health(
                "http://agentdesk.test",
                {
                    "timeout_s": 1,
                    "poll_interval_s": 0,
                    "global_active_max": 0,
                    "global_finalizing_max": 0,
                },
            )

        self.assertEqual(result["global_active"], 0)
        self.assertEqual(result["global_finalizing"], 0)
        sleep.assert_called_once_with(0.0)

    def test_assert_health_polls_healthy_status_until_same_cell_finalizing_drains(self):
        payloads = {
            "/api/health/detail": [
                (
                    200,
                    {
                        **_health_detail(_idle_mailbox("42", provider="codex")),
                        "global_finalizing": 1,
                    },
                ),
                (200, _health_detail(_idle_mailbox("42", provider="codex"))),
            ],
            "/api/health": [
                (
                    200,
                    {
                        "status": "healthy",
                        "ok": True,
                        "fully_recovered": True,
                        "degraded_reasons": [],
                    },
                ),
                (
                    200,
                    {
                        "status": "healthy",
                        "ok": True,
                        "fully_recovered": True,
                        "degraded_reasons": [],
                    },
                ),
            ],
        }

        with (
            patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)),
            patch("run_tui_relay.time.sleep", return_value=None) as sleep,
        ):
            result = driver.assert_health(
                "http://agentdesk.test",
                {
                    "timeout_s": 1,
                    "poll_interval_s": 0,
                    "global_active_max": 0,
                    "global_finalizing_max": 0,
                },
            )

        self.assertEqual(result["status"], "healthy")
        self.assertEqual(result["global_active"], 0)
        self.assertEqual(result["global_finalizing"], 0)
        sleep.assert_called_once_with(0.0)

    def test_assert_health_times_out_when_healthy_status_finalizing_persists(self):
        healthy_payload = {
            "status": "healthy",
            "ok": True,
            "fully_recovered": True,
            "degraded_reasons": [],
        }
        stuck_detail = {
            **_health_detail(_idle_mailbox("42", provider="codex")),
            "global_finalizing": 1,
        }
        payloads = {
            "/api/health/detail": [(200, stuck_detail) for _ in range(4)],
            "/api/health": [(200, healthy_payload) for _ in range(4)],
        }

        with (
            patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)),
            patch("run_tui_relay.time.sleep", return_value=None),
        ):
            with self.assertRaises(assertions.AssertionError) as ctx:
                driver.assert_health(
                    "http://agentdesk.test",
                    {
                        "timeout_s": 0.2,
                        "poll_interval_s": 0.1,
                        "global_active_max": 0,
                        "global_finalizing_max": 0,
                    },
                )

        message = str(ctx.exception)
        self.assertIn("assert_health did not pass within 0.2s", message)
        self.assertIn("global_finalizing=1 > 0", message)

    def test_assert_health_forbid_only_allows_unrelated_transitional_reasons(self):
        payloads = {
            "/api/health": [
                (
                    200,
                    {
                        "status": "degraded",
                        "ok": False,
                        "fully_recovered": False,
                        "degraded": True,
                        "degraded_reasons": ["provider:claude:reconcile_in_progress"],
                    },
                )
            ],
        }

        with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
            result = driver.assert_health(
                "http://agentdesk.test",
                {
                    "require_status": ["healthy", "degraded"],
                    "forbid_degraded_reasons": ["global_active_counter_out_of_bounds"],
                },
            )

        self.assertEqual(result["status"], "degraded")

    def test_assert_health_forbid_non_strict_still_raises_on_forbidden(self):
        payloads = {
            "/api/health": [
                (
                    200,
                    {
                        "status": "degraded",
                        "ok": False,
                        "fully_recovered": False,
                        "degraded": True,
                        "degraded_reasons": [
                            "global_active_counter_out_of_bounds:raw=4"
                        ],
                    },
                )
            ],
        }

        with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
            with self.assertRaises(assertions.AssertionError) as ctx:
                driver.assert_health(
                    "http://agentdesk.test",
                    {
                        "require_status": ["healthy", "degraded"],
                        "forbid_degraded_reasons": [
                            "global_active_counter_out_of_bounds"
                        ],
                    },
                )

        self.assertIn("forbidden_degraded_reasons", str(ctx.exception))

    def test_cancel_turn_posts_expected_endpoint(self):
        captured = {}

        def fake_urlopen(request, timeout=0):  # noqa: ANN001
            captured["url"] = request.full_url
            captured["method"] = request.get_method()
            captured["timeout"] = timeout
            return FakeResponse(
                200,
                {
                    "ok": True,
                    "queued_remaining": 0,
                    "queue_purged": True,
                    "tmux_killed": False,
                    "lifecycle_path": "preserve",
                },
            )

        with patch("run_tui_relay.urllib.request.urlopen", fake_urlopen):
            result = driver.cancel_turn(
                base_url="http://agentdesk.test",
                channel_id="42",
                force=False,
                timeout_s=7,
            )

        self.assertEqual(captured["url"], "http://agentdesk.test/api/turns/42/cancel?force=false")
        self.assertEqual(captured["method"], "POST")
        self.assertEqual(captured["timeout"], 7)
        self.assertEqual(result["queue_purged"], True)


class ControlFlowPrimitives(unittest.TestCase):
    def test_send_provider_hold_prompt_builds_cancel_fixture(self):
        prompt = driver.build_provider_hold_prompt(
            {
                "ok_marker": "[E2E:E18:OK]",
                "late_marker": "[E2E:E18:LATE]",
                "hold_seconds": 60,
            },
            scenario_id="E-18",
        )

        self.assertIn("[E2E:E18:OK]", prompt)
        self.assertIn("[E2E:E18:LATE]", prompt)
        self.assertIn("time.sleep(60)", prompt)
        self.assertIn("cancel this turn while the command is sleeping", prompt)

    def test_turn_identity_from_turn_start_response_parses_user_message(self):
        identity = driver.turn_identity_from_send_response(
            {
                "turn_id": "discord:1509350490461180105:9100000000000000123",
                "dispatch_id": "dispatch-e18",
                "started_at": "2026-05-31T13:53:19Z",
                "born_generation": 7,
            },
            channel_id="1509350490461180105",
        )

        self.assertEqual(identity["channel_id"], "1509350490461180105")
        self.assertEqual(identity["user_msg_id"], "9100000000000000123")
        self.assertEqual(
            identity["turn_id"],
            "discord:1509350490461180105:9100000000000000123",
        )
        self.assertEqual(identity["dispatch_id"], "dispatch-e18")
        self.assertEqual(identity["started_at"], "2026-05-31T13:53:19Z")
        self.assertEqual(identity["born_generation"], "7")

    def test_provider_hold_runtime_root_validation_fails_early(self):
        with tempfile.TemporaryDirectory() as tmp:
            bad_root = Path(tmp) / "missing-runtime"
            with self.assertRaises(assertions.AssertionError) as ctx:
                driver.wait_for_provider_hold_state(
                    runtime_root=bad_root,
                    provider="claude",
                    channel_id="42",
                    expected_identity={"channel_id": "42", "user_msg_id": "99"},
                    ok_marker="[E2E:E18:OK]",
                    late_marker="[E2E:E18:LATE]",
                    timeout_s=0.1,
                    poll_interval_s=0.01,
                )

        self.assertIn("queue_runtime_root", str(ctx.exception))

    def test_wait_for_provider_hold_state_observes_pre_tool_text(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = (
                Path(tmp)
                / "discord_inflight"
                / "claude"
                / "1509350490461180105.json"
            )
            path.parent.mkdir(parents=True)
            path.write_text(
                json.dumps(
                    {
                        "channel_id": 1509350490461180105,
                        "user_msg_id": 9100000000000000123,
                        "full_response": "[E2E:E18:OK]\n\n",
                        "any_tool_used": True,
                        "has_post_tool_text": False,
                        "terminal_delivery_committed": False,
                    }
                ),
                encoding="utf-8",
            )

            result = driver.wait_for_provider_hold_state(
                runtime_root=tmp,
                provider="claude",
                channel_id="1509350490461180105",
                expected_identity={
                    "channel_id": "1509350490461180105",
                    "user_msg_id": "9100000000000000123",
                },
                ok_marker="[E2E:E18:OK]",
                late_marker="[E2E:E18:LATE]",
                timeout_s=0.1,
                poll_interval_s=0.01,
            )

        self.assertEqual(result["path"], str(path))
        self.assertTrue(result["ok_marker_seen"])
        self.assertTrue(result["any_tool_used"])
        self.assertFalse(result["has_post_tool_text"])

    def test_wait_for_provider_hold_state_ignores_stale_late_identity(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "discord_inflight" / "claude" / "42.json"
            path.parent.mkdir(parents=True)

            def write_state(user_msg_id: int, body: str, post_tool: bool) -> None:
                path.write_text(
                    json.dumps(
                        {
                            "channel_id": 42,
                            "user_msg_id": user_msg_id,
                            "full_response": body,
                            "any_tool_used": True,
                            "has_post_tool_text": post_tool,
                            "terminal_delivery_committed": False,
                        }
                    ),
                    encoding="utf-8",
                )

            write_state(
                111,
                "[E2E:E18:OK]\n\n[E2E:E18:LATE]",
                True,
            )

            def publish_current_turn() -> None:
                threading.Event().wait(0.03)
                write_state(222, "[E2E:E18:OK]\n\n", False)

            updater = threading.Thread(target=publish_current_turn)
            updater.start()
            try:
                result = driver.wait_for_provider_hold_state(
                    runtime_root=tmp,
                    provider="claude",
                    channel_id="42",
                    expected_identity={"channel_id": "42", "user_msg_id": "222"},
                    ok_marker="[E2E:E18:OK]",
                    late_marker="[E2E:E18:LATE]",
                    timeout_s=0.5,
                    poll_interval_s=0.01,
                )
            finally:
                updater.join(timeout=1.0)

        self.assertEqual(result["turn_identity"]["user_msg_id"], "222")

    def test_wait_for_provider_hold_state_times_out_on_identity_mismatch(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "discord_inflight" / "claude" / "42.json"
            path.parent.mkdir(parents=True)
            path.write_text(
                json.dumps(
                    {
                        "channel_id": 42,
                        "user_msg_id": 111,
                        "full_response": "[E2E:E18:OK]\n\n",
                        "any_tool_used": True,
                        "has_post_tool_text": False,
                        "terminal_delivery_committed": False,
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaises(assertions.AssertionError) as ctx:
                driver.wait_for_provider_hold_state(
                    runtime_root=tmp,
                    provider="claude",
                    channel_id="42",
                    expected_identity={"channel_id": "42", "user_msg_id": "222"},
                    ok_marker="[E2E:E18:OK]",
                    late_marker="[E2E:E18:LATE]",
                    timeout_s=0.03,
                    poll_interval_s=0.01,
                )

        self.assertIn("identity_mismatch", str(ctx.exception))

    def test_wait_for_provider_hold_state_times_out_on_dispatch_mismatch(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "discord_inflight" / "claude" / "42.json"
            path.parent.mkdir(parents=True)
            path.write_text(
                json.dumps(
                    {
                        "channel_id": 42,
                        "user_msg_id": 222,
                        "dispatch_id": "old-dispatch",
                        "full_response": "[E2E:E18:OK]\n\n",
                        "any_tool_used": True,
                        "has_post_tool_text": False,
                        "terminal_delivery_committed": False,
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaises(assertions.AssertionError) as ctx:
                driver.wait_for_provider_hold_state(
                    runtime_root=tmp,
                    provider="claude",
                    channel_id="42",
                    expected_identity={
                        "channel_id": "42",
                        "user_msg_id": "222",
                        "dispatch_id": "current-dispatch",
                    },
                    ok_marker="[E2E:E18:OK]",
                    late_marker="[E2E:E18:LATE]",
                    timeout_s=0.03,
                    poll_interval_s=0.01,
                )

        self.assertIn("dispatch_id", str(ctx.exception))

    def test_wait_for_provider_hold_state_fails_if_current_turn_disappears(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "discord_inflight" / "claude" / "42.json"
            path.parent.mkdir(parents=True)
            path.write_text(
                json.dumps(
                    {
                        "channel_id": 42,
                        "user_msg_id": 222,
                        "full_response": "[E2E:E18:OK]\n\n",
                        "any_tool_used": False,
                        "has_post_tool_text": False,
                        "terminal_delivery_committed": False,
                    }
                ),
                encoding="utf-8",
            )

            def remove_current_turn() -> None:
                threading.Event().wait(0.03)
                path.unlink(missing_ok=True)

            remover = threading.Thread(target=remove_current_turn)
            remover.start()
            try:
                with self.assertRaises(assertions.AssertionError) as ctx:
                    driver.wait_for_provider_hold_state(
                        runtime_root=tmp,
                        provider="claude",
                        channel_id="42",
                        expected_identity={
                            "channel_id": "42",
                            "user_msg_id": "222",
                        },
                        ok_marker="[E2E:E18:OK]",
                        late_marker="[E2E:E18:LATE]",
                        timeout_s=0.5,
                        poll_interval_s=0.01,
                    )
            finally:
                remover.join(timeout=1.0)

        self.assertIn("disappeared before cancel", str(ctx.exception))
        self.assertIn("last_current_state=", str(ctx.exception))

    def test_wait_for_provider_hold_state_rejects_delivered_current_turn(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "discord_inflight" / "claude" / "42.json"
            path.parent.mkdir(parents=True)
            path.write_text(
                json.dumps(
                    {
                        "channel_id": 42,
                        "user_msg_id": 222,
                        "full_response": "[E2E:E18:OK]\n\n",
                        "any_tool_used": True,
                        "has_post_tool_text": False,
                        "terminal_delivery_committed": True,
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaises(assertions.AssertionError) as ctx:
                driver.wait_for_provider_hold_state(
                    runtime_root=tmp,
                    provider="claude",
                    channel_id="42",
                    expected_identity={"channel_id": "42", "user_msg_id": "222"},
                    ok_marker="[E2E:E18:OK]",
                    late_marker="[E2E:E18:LATE]",
                    timeout_s=0.1,
                    poll_interval_s=0.01,
                )

        self.assertIn("turn delivered before provider hold", str(ctx.exception))

    def test_wait_for_provider_hold_state_rejects_late_before_cancel(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "discord_inflight" / "claude" / "42.json"
            path.parent.mkdir(parents=True)
            path.write_text(
                json.dumps(
                    {
                        "channel_id": 42,
                        "user_msg_id": 222,
                        "full_response": "[E2E:E18:OK]\n\n[E2E:E18:LATE]",
                        "any_tool_used": True,
                        "has_post_tool_text": True,
                        "terminal_delivery_committed": False,
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaises(assertions.AssertionError) as ctx:
                driver.wait_for_provider_hold_state(
                    runtime_root=tmp,
                    provider="claude",
                    channel_id="42",
                    expected_identity={"channel_id": "42", "user_msg_id": "222"},
                    ok_marker="[E2E:E18:OK]",
                    late_marker="[E2E:E18:LATE]",
                    timeout_s=0.1,
                    poll_interval_s=0.01,
                )

        self.assertIn("late marker appeared", str(ctx.exception))

    def test_run_one_cell_dispatches_provider_hold_prompt(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def __init__(self):
                self.sent: list[str] = []

            def send_control(self, channel_id, content):  # noqa: ARG002
                return {"id": "1"}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                return []

            def send_prompt(self, channel_id, content, *, channel_kind="cc"):  # noqa: ARG002
                self.sent.append(content)
                return {"turn_id": "discord:42:9100000000000000123", "status": "started"}

        args = Namespace(
            cell="codex-tui",
            channel_id="42",
            thread_channel_id=None,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
        )
        scenario = {
            "id": "E-18",
            "steps": [
                {
                    "send_provider_hold_prompt": {
                        "ok_marker": "[E2E:E18:OK]",
                        "late_marker": "[E2E:E18:LATE]",
                        "hold_seconds": 60,
                    }
                }
            ],
            "assertions": [],
        }
        client = FakeClient()

        with (
            patch("run_tui_relay.time.sleep", return_value=None),
            patch(
                "run_tui_relay.assert_cell_idle",
                return_value={"status": "idle", "mailboxes_seen": 1},
            ),
        ):
            record = driver.run_one_cell(
                scenario=scenario,
                cell="codex-tui",
                channel_id="42",
                client=client,  # type: ignore[arg-type]
                run_id="run-1",
                dry_run=False,
                args=args,
            )

        self.assertEqual(len(client.sent), 1)
        self.assertIn("[E2E:E18:OK]", client.sent[0])
        self.assertIn("time.sleep(60)", client.sent[0])
        self.assertEqual(record["provider_hold_prompts"][0]["hold_seconds"], 60)
        self.assertEqual(
            record["provider_hold_prompts"][0]["turn_identity"]["user_msg_id"],
            "9100000000000000123",
        )

    def test_run_one_cell_local_fixture_does_not_touch_client_or_health(self):
        class ForbiddenClient:
            base_url = "http://agentdesk.test"

            def send_control(self, *args, **kwargs):  # noqa: ANN002, ANN003
                raise AssertionError("local fixture must not send control messages")

            def fetch_messages(self, *args, **kwargs):  # noqa: ANN002, ANN003
                raise AssertionError("local fixture must not fetch Discord messages")

            def send_prompt(self, *args, **kwargs):  # noqa: ANN002, ANN003
                raise AssertionError("local fixture must not send prompts")

        args = Namespace(
            cell="codex-tui",
            channel_id="42",
            thread_channel_id=None,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
        )
        scenario = {
            "id": "E-25",
            "execution": "fixture",
            "steps": [
                {
                    "replay_fixture": {
                        "kind": "codex_modern_schema",
                        "provider": "codex",
                        "frames": [
                            {"type": "thread.started", "thread_id": "thread-1"},
                            {
                                "type": "response_item",
                                "payload": {
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [
                                        {"type": "output_text", "text": "partial"}
                                    ],
                                },
                            },
                            {
                                "type": "event_msg",
                                "payload": {
                                    "type": "task_complete",
                                    "turn_id": "turn-1",
                                    "last_agent_message": "[E2E:E25:FINAL]",
                                },
                            },
                        ],
                    }
                },
                {"fixture_followup_probe": {"prompt": "next"}},
            ],
            "assertions": [
                {"text_present": "[E2E:E25:FINAL]"},
                {"fixture_task_complete_finalized": {"turn_id": "turn-1"}},
                {"fixture_followup_ready": True},
                {"fixture_no_health_degradation": True},
            ],
        }

        with patch("run_tui_relay.assert_cell_idle") as idle:
            record = driver.run_one_cell(
                scenario=scenario,
                cell="codex-tui",
                channel_id="42",
                client=ForbiddenClient(),  # type: ignore[arg-type]
                run_id="run-1",
                dry_run=False,
                args=args,
            )

        idle.assert_not_called()
        self.assertTrue(record["local_fixture"])
        self.assertEqual(record["relay_count"], 1)
        self.assertEqual(record["post_scenario_idle"]["source"], "local_fixture")
        self.assertTrue(record["fixture_state"]["followup_probe_accepted"])

    def test_run_one_cell_waits_for_hold_state_before_cancel(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def __init__(self):
                self.sent: list[str] = []

            def send_control(self, channel_id, content):  # noqa: ARG002
                return {"id": "1"}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                return []

            def send_prompt(self, channel_id, content, *, channel_kind="cc"):  # noqa: ARG002
                self.sent.append(content)
                return {"turn_id": "discord:42:9100000000000000123", "status": "started"}

        args = Namespace(
            cell="claude-tui",
            channel_id="42",
            thread_channel_id=None,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
        )
        scenario = {
            "id": "E-18",
            "steps": [
                {
                    "send_provider_hold_prompt": {
                        "ok_marker": "[E2E:E18:OK]",
                        "late_marker": "[E2E:E18:LATE]",
                        "hold_seconds": 60,
                    }
                },
                {
                    "wait_for_provider_hold_state": {
                        "ok_marker": "[E2E:E18:OK]",
                        "late_marker": "[E2E:E18:LATE]",
                    }
                },
                {"cancel_turn": {"force": True, "timeout_s": 15}},
            ],
            "assertions": [{"provider_hold_marker_seen": "[E2E:E18:OK]"}],
        }
        events: list[str] = []

        def fake_wait_for_hold(**kwargs):  # noqa: ANN003
            events.append("hold")
            self.assertEqual(kwargs["provider"], "claude")
            self.assertEqual(kwargs["channel_id"], "42")
            self.assertEqual(
                kwargs["expected_identity"]["user_msg_id"],
                "9100000000000000123",
            )
            return {
                "ok_marker": "[E2E:E18:OK]",
                "ok_marker_seen": True,
                "late_marker": "[E2E:E18:LATE]",
                "late_marker_seen": False,
            }

        def fake_cancel(**kwargs):  # noqa: ANN003
            events.append("cancel")
            self.assertTrue(kwargs["force"])
            return {"ok": True}

        with (
            patch("run_tui_relay.time.sleep", return_value=None),
            patch("run_tui_relay.wait_for_provider_hold_state", side_effect=fake_wait_for_hold),
            patch("run_tui_relay.cancel_turn", side_effect=fake_cancel),
            patch(
                "run_tui_relay.assert_cell_idle",
                return_value={"status": "idle", "mailboxes_seen": 1},
            ),
        ):
            record = driver.run_one_cell(
                scenario=scenario,
                cell="claude-tui",
                channel_id="42",
                client=FakeClient(),  # type: ignore[arg-type]
                run_id="run-1",
                dry_run=False,
                args=args,
            )

        self.assertEqual(events, ["hold", "cancel"])
        self.assertEqual(
            record["provider_hold_states"],
            [
                {
                    "ok_marker": "[E2E:E18:OK]",
                    "ok_marker_seen": True,
                    "late_marker": "[E2E:E18:LATE]",
                    "late_marker_seen": False,
                }
            ],
        )
        self.assertEqual(record["cancel_turns"], [{"ok": True}])
        self.assertEqual(
            record["assertions"][0],
            {
                "spec": {"provider_hold_marker_seen": "[E2E:E18:OK]"},
                "passed": True,
            },
        )

    def test_send_prompts_concurrent_overlaps_dispatches(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def __init__(self):
                self.lock = threading.Lock()
                self.active = 0
                self.max_active = 0
                self.sent: list[str] = []

            def send_control(self, channel_id, content):  # noqa: ARG002
                return {"id": "1"}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                return []

            def send_prompt(self, channel_id, content, *, channel_kind="cc"):  # noqa: ARG002
                with self.lock:
                    self.active += 1
                    self.max_active = max(self.max_active, self.active)
                    self.sent.append(content)
                threading.Event().wait(0.05)
                with self.lock:
                    self.active -= 1
                return {"id": str(len(self.sent))}

        args = Namespace(
            cell="codex-tui",
            channel_id="42",
            thread_channel_id=None,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
        )
        scenario = {
            "id": "E-X",
            "steps": [
                {
                    "send_prompts_concurrent": {
                        "prompts": ["prompt-a", "prompt-b"]
                    }
                }
            ],
            "assertions": [],
        }
        client = FakeClient()

        with (
            patch("run_tui_relay.time.sleep", return_value=None),
            patch(
                "run_tui_relay.assert_cell_idle",
                return_value={"status": "idle", "mailboxes_seen": 1},
            ),
        ):
            record = driver.run_one_cell(
                scenario=scenario,
                cell="codex-tui",
                channel_id="42",
                client=client,  # type: ignore[arg-type]
                run_id="run-1",
                dry_run=False,
                args=args,
            )

        self.assertEqual(client.max_active, 2)
        self.assertCountEqual(client.sent, ["prompt-a", "prompt-b"])
        self.assertEqual(
            [item["index"] for item in record["concurrent_prompt_batches"][0]],
            [0, 1],
        )

    def test_send_keys_sequence_sends_control_keys_as_tmux_key_args(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def send_control(self, channel_id, content):  # noqa: ARG002
                return {"id": "1"}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                return []

        args = Namespace(
            cell="codex-tui",
            channel_id="42",
            thread_channel_id=None,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
        )
        scenario = {
            "id": "E-X",
            "steps": [
                {
                    "send_keys_sequence": {
                        "keys": ["STALE_DRAFT", "C-u", "final prompt", "C-m"]
                    }
                }
            ],
            "assertions": [],
        }
        sent: list[tuple[str, tuple[str, ...]]] = []

        def fake_send_keys(session_name, *keys):
            sent.append((session_name, keys))
            return True

        with (
            patch("run_tui_relay.time.sleep", return_value=None),
            patch("run_tui_relay.tmux.send_keys", side_effect=fake_send_keys),
            patch(
                "run_tui_relay.assert_cell_idle",
                return_value={"status": "idle", "mailboxes_seen": 1},
            ),
        ):
            record = driver.run_one_cell(
                scenario=scenario,
                cell="codex-tui",
                channel_id="42",
                client=FakeClient(),  # type: ignore[arg-type]
                run_id="run-1",
                dry_run=False,
                args=args,
            )

        self.assertEqual(
            sent,
            [
                (
                    "AgentDesk-codex-adk-codex-tui-e2e",
                    ("STALE_DRAFT", "C-u", "final prompt", "C-m"),
                )
            ],
        )
        self.assertEqual(
            record["tmux_key_sequences"],
            [{"session": sent[0][0], "count": 4, "mode": "single_call"}],
        )

    def test_send_keys_sequence_can_pause_between_control_keys(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def send_control(self, channel_id, content):  # noqa: ARG002
                return {"id": "1"}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                return []

        args = Namespace(
            cell="claude-tui",
            channel_id="42",
            thread_channel_id=None,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
        )
        scenario = {
            "id": "E-21",
            "steps": [
                {
                    "send_keys_sequence": {
                        "keys": ["STALE_DRAFT", "C-u", "final prompt", "C-m"],
                        "key_interval_s": 0.35,
                    }
                }
            ],
            "assertions": [],
        }
        sent: list[tuple[str, tuple[str, ...]]] = []
        sleeps: list[float] = []

        def fake_send_keys(session_name, *keys):
            sent.append((session_name, keys))
            return True

        with (
            patch("run_tui_relay.time.sleep", side_effect=lambda seconds: sleeps.append(seconds)),
            patch("run_tui_relay.tmux.send_keys", side_effect=fake_send_keys),
            patch(
                "run_tui_relay.assert_cell_idle",
                return_value={"status": "idle", "mailboxes_seen": 1},
            ),
        ):
            record = driver.run_one_cell(
                scenario=scenario,
                cell="claude-tui",
                channel_id="42",
                client=FakeClient(),  # type: ignore[arg-type]
                run_id="run-1",
                dry_run=False,
                args=args,
            )

        self.assertEqual(
            sent,
            [
                ("AgentDesk-claude-adk-claude-tui-e2e", ("STALE_DRAFT",)),
                ("AgentDesk-claude-adk-claude-tui-e2e", ("C-u",)),
                ("AgentDesk-claude-adk-claude-tui-e2e", ("final prompt",)),
                ("AgentDesk-claude-adk-claude-tui-e2e", ("C-m",)),
            ],
        )
        self.assertEqual([sleep for sleep in sleeps if sleep == 0.35], [0.35, 0.35, 0.35])
        self.assertEqual(
            record["tmux_key_sequences"],
            [
                {
                    "session": "AgentDesk-claude-adk-claude-tui-e2e",
                    "count": 4,
                    "mode": "per_key",
                    "key_interval_s": 0.35,
                }
            ],
        )

    def test_assert_session_preserved_detects_recreated_tmux_session(self):
        before = {
            "session_name": "AgentDesk-codex-adk-codex-tui-e2e",
            "pane_count": 1,
            "panes": [
                {
                    "pane_id": "%1",
                    "pid": 100,
                    "cwd": "/tmp/adk-codex-tui-e2e",
                    "session_name": "AgentDesk-codex-adk-codex-tui-e2e",
                }
            ],
        }
        after_identity = driver.tmux.SessionIdentity(
            session_name="AgentDesk-codex-adk-codex-tui-e2e",
            panes=(
                driver.tmux.PaneInfo(
                    pane_id="%1",
                    pid=200,
                    cwd="/tmp/adk-codex-tui-e2e",
                    session_name="AgentDesk-codex-adk-codex-tui-e2e",
                ),
            ),
        )

        with patch("run_tui_relay.tmux.session_identity", return_value=after_identity):
            with self.assertRaises(assertions.AssertionError) as ctx:
                driver.assert_session_preserved(
                    before=before,
                    cell="codex-tui",
                    channel_id="42",
                    scenario={"id": "E-X"},
                )

        self.assertIn("tmux session identity changed", str(ctx.exception))


class ScenarioTeardown(unittest.TestCase):
    def test_run_scenario_includes_provider_hold_evidence_in_report_record(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def send_control(self, channel_id, content):  # noqa: ARG002
                return {"id": "1"}

        args = Namespace(
            cell="claude-tui",
            channel_id="42",
            thread_channel_id=None,
            reset_before_each=False,
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            hard_reset_session_each=False,
            allow_destructive=False,
        )
        scenario = {"id": "E-18", "steps": [], "assertions": []}
        provider_hold_state = {
            "ok_marker": "[E2E:E18:OK]",
            "ok_marker_seen": True,
            "late_marker": "[E2E:E18:LATE]",
            "late_marker_seen": False,
        }

        with patch(
            "run_tui_relay.run_one_cell",
            return_value={
                "assertions": [
                    {
                        "spec": {"provider_hold_marker_seen": "[E2E:E18:OK]"},
                        "passed": True,
                    }
                ],
                "relay_count": 0,
                "raw_count": 1,
                "message_updates": 0,
                "sample_relay": [],
                "provider_hold_prompts": [{"hold_seconds": 60}],
                "provider_hold_states": [provider_hold_state],
                "cancel_turns": [{"ok": True}],
                "health_assertions": [{"status": "healthy"}],
                "post_scenario_idle": {"status": "idle"},
            },
        ):
            result = driver.run_scenario(
                scenario,
                args=args,
                run_id="run-1",
                client=FakeClient(),  # type: ignore[arg-type]
            )

        self.assertEqual(result["status"], "pass")
        self.assertEqual(result["provider_hold_states"], [provider_hold_state])
        self.assertEqual(result["provider_hold_prompts"], [{"hold_seconds": 60}])
        self.assertEqual(result["health_assertions"], [{"status": "healthy"}])
        self.assertEqual(result["relay_count"], 0)
        self.assertEqual(result["raw_count"], 1)

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
