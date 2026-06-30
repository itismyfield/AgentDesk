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


class AgentModeContract(unittest.TestCase):
    def test_load_scenarios_requires_agent_mode_metadata(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "missing.yaml"
            path.write_text(
                """
id: E-X
cells: [codex-tui]
steps: []
assertions: []
""",
                encoding="utf-8",
            )

            with self.assertRaises(ValueError) as ctx:
                driver.load_scenarios(Path(tmp), cell="codex-tui")

        self.assertIn("agent_mode", str(ctx.exception))

    def test_load_scenarios_rejects_none_metadata_for_real_prompt(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "mismatch.yaml"
            path.write_text(
                """
id: E-X
agent_mode: none
cells: [codex-tui]
steps:
  - send_prompt: "hello"
assertions: []
""",
                encoding="utf-8",
            )

            with self.assertRaises(ValueError) as ctx:
                driver.load_scenarios(Path(tmp), cell="codex-tui")

        self.assertIn("declares agent_mode='none'", str(ctx.exception))
        self.assertIn("plan 'real_live'", str(ctx.exception))

    def test_load_scenarios_requires_coverage_class_metadata(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "missing-coverage.yaml"
            path.write_text(
                """
id: E-X
agent_mode: real_live
cells: [codex-tui]
steps:
  - send_prompt: "hello"
assertions: []
""",
                encoding="utf-8",
            )

            with self.assertRaises(ValueError) as ctx:
                driver.load_scenarios(Path(tmp), cell="codex-tui")

        self.assertIn("coverage_class", str(ctx.exception))

    def test_load_scenarios_rejects_live_coverage_for_fixture_lane(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "mismatch-coverage.yaml"
            path.write_text(
                """
id: E-X
agent_mode: none
coverage_class: live
cells: [codex-tui]
execution: fixture
steps:
  - replay_fixture:
      kind: codex_modern_schema
      provider: codex
      frames: []
assertions: []
""",
                encoding="utf-8",
            )

            with self.assertRaises(ValueError) as ctx:
                driver.load_scenarios(Path(tmp), cell="codex-tui")

        self.assertIn("declares coverage_class='live'", str(ctx.exception))
        self.assertIn("plan 'fixture'", str(ctx.exception))

    def test_required_agent_mode_gate_fails_shallower_scenario(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

        args = Namespace(
            cell="codex-tui",
            channel_id="42",
            thread_channel_id=None,
            reset_before_each=False,
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            hard_reset_session_each=False,
            allow_destructive=False,
            required_agent_mode="controlled",
        )
        scenario = {"id": "E-X", "agent_mode": "none", "steps": [], "assertions": []}

        result = driver.run_scenario(
            scenario,
            args=args,
            run_id="run-1",
            client=FakeClient(),  # type: ignore[arg-type]
        )

        self.assertEqual(result["status"], "fail")
        self.assertEqual(result["failure_attribution"]["source"], "agent_mode_gate")

    def test_required_coverage_class_gate_fails_fixture_scenario(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

        args = Namespace(
            cell="codex-tui",
            channel_id="42",
            thread_channel_id=None,
            reset_before_each=False,
            dry_run=True,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            hard_reset_session_each=False,
            allow_destructive=False,
            required_coverage_class="live",
        )
        scenario = {
            "id": "E-X",
            "agent_mode": "none",
            "coverage_class": "fixture",
            "execution": "fixture",
            "steps": [],
            "assertions": [],
        }

        result = driver.run_scenario(
            scenario,
            args=args,
            run_id="run-1",
            client=FakeClient(),  # type: ignore[arg-type]
        )

        self.assertEqual(result["status"], "fail")
        self.assertEqual(result["coverage_class"], "fixture")
        self.assertEqual(result["failure_attribution"]["source"], "coverage_class_gate")
        self.assertIn("declares fixture", result["reason"])

    def test_required_agent_mode_gate_fails_skipped_scenario_with_no_actual_mode(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

        args = Namespace(
            cell="claude-tui",
            channel_id="42",
            thread_channel_id=None,
            reset_before_each=False,
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            hard_reset_session_each=False,
            allow_destructive=False,
            required_agent_mode="controlled",
        )
        scenario = {
            "id": "E-16",
            "agent_mode": "controlled",
            "skip_reason": "stub until runtime hook exists",
            "steps": [],
            "assertions": [],
        }

        result = driver.run_scenario(
            scenario,
            args=args,
            run_id="run-1",
            client=FakeClient(),  # type: ignore[arg-type]
        )

        self.assertEqual(result["status"], "fail")
        self.assertEqual(result["agent_mode_actual"], "none")
        self.assertEqual(result["failure_attribution"]["source"], "agent_mode_gate")
        self.assertIn("observed agent_mode_actual=none", result["reason"])

    def test_controlled_execution_metadata_without_evidence_records_none_actual_mode(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def __init__(self):
                self.next_id = 100

            def send_control(self, channel_id, content):  # noqa: ARG002
                self.next_id += 1
                return {"id": str(self.next_id)}

            def fetch_messages(self, channel_id, *, after_id=None, limit=100):  # noqa: ARG002
                return []

        args = Namespace(queue_runtime_root="/tmp/agentdesk-e2e-test-runtime")
        scenario = {
            "id": "E-X",
            "agent_mode": "controlled",
            "execution": "controlled",
            "steps": [],
            "assertions": [],
        }

        with patch("run_tui_relay.time.sleep"), patch(
            "run_tui_relay.assert_cell_idle",
            return_value={"ok": True, "mailbox": "idle"},
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

        self.assertEqual(record["agent_mode_actual"], "none")
        self.assertFalse(record["real_provider_contacted"])
        self.assertFalse(record["agent_mode_contract"]["satisfied"])

    def test_controlled_harness_step_records_controlled_actual_mode(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def __init__(self):
                self.next_id = 100

            def send_control(self, channel_id, content):  # noqa: ARG002
                self.next_id += 1
                return {"id": str(self.next_id)}

            def fetch_messages(self, channel_id, *, after_id=None, limit=100):  # noqa: ARG002
                return []

        args = Namespace(queue_runtime_root="/tmp/agentdesk-e2e-test-runtime")
        scenario = {
            "id": "E-X",
            "agent_mode": "controlled",
            "execution": "controlled",
            "steps": [{"assert_health": {}}],
            "assertions": [],
        }

        with (
            patch("run_tui_relay.time.sleep"),
            patch("run_tui_relay.assert_health", return_value={"status": "healthy"}),
            patch(
                "run_tui_relay.assert_cell_idle",
                return_value={"ok": True, "mailbox": "idle"},
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

        self.assertEqual(record["agent_mode_actual"], "controlled")
        self.assertEqual(record["controlled_harness_evidence"], ["assert_health"])
        self.assertFalse(record["real_provider_contacted"])
        self.assertTrue(record["agent_mode_contract"]["satisfied"])


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

    def test_wait_for_discord_text_accepts_direct_input_reply_body(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def fetch_messages(self, channel_id, *, after_id=None, limit=100):  # noqa: ARG002
                return [
                    {
                        "id": "2",
                        "content": "[E2E:E21:HEAD]\nDIRECT_E21_OK\n[E2E:E21:TAIL]",
                        "author": {"id": "999", "bot": True},
                        "type": 19,
                    }
                ]

        found, observed = driver.wait_for_discord_text_with_tui_idle_draft_guard(
            client=FakeClient(),  # type: ignore[arg-type]
            channel_id="42",
            cell="claude-tui",
            after_id="1",
            needle="[E2E:E21:TAIL]",
            prompt="direct input prompt",
            thread_channel_id=None,
            timeout_s=1,
            debug_label="E-21::wait_for_tail",
        )

        self.assertIsNotNone(found)
        self.assertEqual(found["type"], 19)
        self.assertEqual(len(observed), 1)


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

    def test_assert_cell_idle_captures_mailbox_idle_evidence(self):
        # #3797 (E-16): the idle invariant now carries the explicit
        # /api/health/detail field evidence that the tested mailbox released.
        payloads = {
            "/api/health/detail": [
                (200, _health_detail(_idle_mailbox("42", provider="claude")))
            ]
        }

        with tempfile.TemporaryDirectory() as tmpdir:
            with patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)):
                result = driver.assert_cell_idle(
                    base_url="http://agentdesk.test",
                    channel_id="42",
                    cell="claude-tui",
                    runtime_root=Path(tmpdir),
                    timeout_s=1,
                    poll_interval_s=0,
                )
        self.assertEqual(result["status"], "idle")
        self.assertTrue(result["queue_files_clear"])
        evidence = result["mailbox_idle_evidence"]
        self.assertEqual(evidence["agent_turn_status"], "idle")
        self.assertEqual(evidence["queue_depth"], 0)
        self.assertFalse(evidence["has_cancel_token"])
        self.assertFalse(evidence["inflight_state_present"])
        self.assertIsNone(evidence["active_user_message_id"])
        self.assertIsNone(evidence["relay_health"]["pending_discord_callback_msg_id"])

    def test_claude_tui_idle_draft_guard_detects_stuck_prompt(self):
        pane = "\n".join(
            [
                "✻ Baked for 10m 9s",
                "────────────────────────────────────────────────────────────────────────────",
                "❯\u00a0응답에 정확히 한 줄로 [E2E:E6:AFTER] 만 출력해줘.",
                "────────────────────────────────────────────────────────────────────────────",
                "  CLAUDE.md: 1, MCP: 2 │ Tools: 5 done",
                "  ⏵⏵ bypass permissions on",
            ]
        )
        payloads = {
            "/api/health/detail": [
                (200, _health_detail(_idle_mailbox("42", provider="claude")))
            ]
        }

        with (
            patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)),
            patch("run_tui_relay.tmux.capture_pane", return_value=pane),
        ):
            with self.assertRaises(assertions.AssertionError) as ctx:
                driver._raise_if_tui_prompt_stuck_while_idle(  # noqa: SLF001
                    base_url="http://agentdesk.test",
                    channel_id="42",
                    cell="claude-tui",
                    prompt="응답에 정확히 한 줄로 [E2E:E6:AFTER] 만 출력해줘.",
                    thread_channel_id=None,
                )

        self.assertIn("prompt remained in Claude TUI input buffer", str(ctx.exception))
        self.assertIn("mailbox", str(ctx.exception))

    def test_claude_tui_idle_draft_guard_ignores_busy_mailbox(self):
        payloads = {
            "/api/health/detail": [
                (503, _health_detail(_busy_mailbox("42", provider="claude")))
            ]
        }

        with (
            patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)),
            patch("run_tui_relay.tmux.capture_pane") as capture,
        ):
            driver._raise_if_tui_prompt_stuck_while_idle(  # noqa: SLF001
                base_url="http://agentdesk.test",
                channel_id="42",
                cell="claude-tui",
                prompt="응답에 정확히 한 줄로 [E2E:E6:AFTER] 만 출력해줘.",
                thread_channel_id=None,
            )

        capture.assert_not_called()

    def test_wait_timeout_diagnostics_preserve_body_truncation_evidence(self):
        window = assertions.Window(setup_marker_id="1")
        window.add(
            {
                "id": "2",
                "content": "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\nfinal prompt\n```",
                "author": {"id": "agentdesk", "bot": True},
                "type": 0,
            }
        )
        window.add(
            {
                "id": "3",
                "content": "[E2E:E21:HEAD]\nDIRECT_E21_OK",
                "author": {"id": "agentdesk", "bot": True},
                "type": 0,
            }
        )
        scenario = {
            "id": "E-21",
            "assertions": [
                {
                    "body_complete": {
                        "head": "[E2E:E21:HEAD]",
                        "tail": "[E2E:E21:TAIL]",
                    }
                }
            ],
        }
        payloads = {
            "/api/health/detail": [
                (200, _health_detail(_idle_mailbox("42", provider="claude")))
            ]
        }

        with (
            patch("run_tui_relay.urllib.request.urlopen", _fake_urlopen_for(payloads)),
            patch("run_tui_relay.tmux.capture_pane", return_value="❯\u00a0"),
        ):
            diagnostic = driver._collect_wait_timeout_diagnostics(  # noqa: SLF001
                base_url="http://agentdesk.test",
                channel_id="42",
                cell="claude-tui",
                scenario=scenario,
                thread_channel_id=None,
                after_id="1",
                wait_kind="relay",
                needle="[E2E:E21:TAIL]",
                prompt="final prompt",
                window=window,
            )

        self.assertEqual(
            diagnostic["classification"],
            "body_truncated_or_tail_missing_after_head",
        )
        self.assertTrue(
            diagnostic["marker_presence"]["[E2E:E21:HEAD]"]["relay"]
        )
        self.assertFalse(
            diagnostic["marker_presence"]["[E2E:E21:TAIL]"]["relay"]
        )
        self.assertEqual(diagnostic["tmux"]["prompt_draft_present"], False)

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
        self.assertEqual(result["classification"], "provider_hold_observed")
        self.assertTrue(result["provider_hold_observed"])
        self.assertTrue(result["ok_marker_seen"])
        self.assertTrue(result["any_tool_used"])
        self.assertFalse(result["has_post_tool_text"])
        self.assertFalse(result["terminal_delivery_committed"])

    def test_wait_for_provider_hold_state_classifies_fast_terminal_completion(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "discord_inflight" / "codex" / "42.json"
            path.parent.mkdir(parents=True)
            path.write_text(
                json.dumps(
                    {
                        "channel_id": 42,
                        "user_msg_id": 222,
                        "full_response": "[E2E:E18:OK]\n\nfast terminal completion",
                        "any_tool_used": False,
                        "has_post_tool_text": False,
                        "terminal_delivery_committed": True,
                    }
                ),
                encoding="utf-8",
            )

            result = driver.wait_for_provider_hold_state(
                runtime_root=tmp,
                provider="codex",
                channel_id="42",
                expected_identity={"channel_id": "42", "user_msg_id": "222"},
                ok_marker="[E2E:E18:OK]",
                late_marker="[E2E:E18:LATE]",
                timeout_s=0.1,
                poll_interval_s=0.01,
            )

        self.assertEqual(result["path"], str(path))
        self.assertEqual(
            result["classification"],
            "fast_terminal_completion_before_hold",
        )
        self.assertFalse(result["provider_hold_observed"])
        self.assertTrue(result["ok_marker_seen"])
        self.assertFalse(result["late_marker_seen"])
        self.assertFalse(result["any_tool_used"])
        self.assertFalse(result["has_post_tool_text"])
        self.assertTrue(result["terminal_delivery_committed"])

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

    def test_wait_for_provider_hold_state_rejects_late_fast_completion(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "discord_inflight" / "codex" / "42.json"
            path.parent.mkdir(parents=True)
            path.write_text(
                json.dumps(
                    {
                        "channel_id": 42,
                        "user_msg_id": 222,
                        "full_response": (
                            "[E2E:E18:OK]\n\n"
                            "[E2E:E18:LATE]"
                        ),
                        "any_tool_used": False,
                        "has_post_tool_text": False,
                        "terminal_delivery_committed": True,
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaises(assertions.AssertionError) as ctx:
                driver.wait_for_provider_hold_state(
                    runtime_root=tmp,
                    provider="codex",
                    channel_id="42",
                    expected_identity={"channel_id": "42", "user_msg_id": "222"},
                    ok_marker="[E2E:E18:OK]",
                    late_marker="[E2E:E18:LATE]",
                    timeout_s=0.1,
                    poll_interval_s=0.01,
                )

        self.assertIn("late marker appeared", str(ctx.exception))

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
            "agent_mode": "real_live",
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
        self.assertEqual(record["agent_mode_actual"], "real_live")
        self.assertTrue(record["real_provider_contacted"])

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
            "agent_mode": "none",
            "coverage_class": "fixture",
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
        self.assertEqual(record["agent_mode_actual"], "none")
        self.assertEqual(record["coverage_class_actual"], "fixture")
        self.assertTrue(record["coverage_class_contract"]["satisfied"])
        self.assertFalse(record["real_provider_contacted"])

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
            "agent_mode": "real_live",
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
        self.assertEqual(record["agent_mode_actual"], "real_live")
        self.assertEqual(
            record["assertions"][0],
            {
                "spec": {"provider_hold_marker_seen": "[E2E:E18:OK]"},
                "passed": True,
            },
        )

    def test_e18_contract_allows_zero_relay_surface_messages_before_cancel(self):
        scenarios_dir = ROOT / "tests" / "e2e" / "tui_relay" / "scenarios"
        e18 = next(
            scenario
            for scenario in driver.load_scenarios(scenarios_dir, cell="claude-pipe")
            if scenario.get("id") == "E-18"
        )
        window = assertions.Window(setup_marker_id="setup")
        record = {
            "provider_hold_states": [
                {
                    "ok_marker": "[E2E:E18:OK]",
                    "ok_marker_seen": True,
                    "late_marker": "[E2E:E18:LATE]",
                    "late_marker_seen": False,
                }
            ]
        }

        for spec in e18["assertions"]:
            driver.run_assertion(spec, window=window, record=record)

    def test_e18_late_marker_absence_still_fails_on_relay_surface(self):
        window = assertions.Window(setup_marker_id="setup")
        window.add(
            {
                "id": "1",
                "content": "[E2E:E18:LATE]",
                "author": {"id": "agentdesk", "bot": True},
                "type": 0,
            }
        )

        with self.assertRaises(assertions.AssertionError):
            driver.run_assertion(
                {
                    "marker_absent": {
                        "marker": "[E2E:E18:LATE]",
                        "surface": "relay",
                    }
                },
                window=window,
                record={"provider_hold_states": []},
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
            "agent_mode": "real_live",
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
        self.assertEqual(record["agent_mode_actual"], "real_live")
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
            "agent_mode": "real_live",
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
        self.assertEqual(
            record["direct_input_prompts"],
            [{"mode": "send_keys_sequence", "prompt_preview": "final prompt"}],
        )
        self.assertTrue(record["real_provider_contacted"])

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
            "agent_mode": "real_live",
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

    def test_send_keys_sequence_wait_guard_uses_inferred_direct_prompt(self):
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
            "agent_mode": "real_live",
            "steps": [
                {
                    "send_keys_sequence": {
                        "keys": ["STALE_DRAFT", "C-u", "final prompt", "C-m"],
                    }
                },
                {"wait_for_discord_text": "TAIL", "timeout_s": 1},
            ],
            "assertions": [],
        }
        prompts: list[str | None] = []

        def fake_wait(**kwargs):
            prompts.append(kwargs.get("prompt"))
            return {"id": "2", "content": "TAIL", "author": {"bot": True}}, []

        with (
            patch("run_tui_relay.time.sleep", return_value=None),
            patch("run_tui_relay.tmux.send_keys", return_value=True),
            patch(
                "run_tui_relay.wait_for_discord_text_with_tui_idle_draft_guard",
                side_effect=fake_wait,
            ),
            patch(
                "run_tui_relay.assert_cell_idle",
                return_value={"status": "idle", "mailboxes_seen": 1},
            ),
        ):
            driver.run_one_cell(
                scenario=scenario,
                cell="claude-tui",
                channel_id="42",
                client=FakeClient(),  # type: ignore[arg-type]
                run_id="run-1",
                dry_run=False,
                args=args,
            )

        self.assertEqual(prompts, ["final prompt"])

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
        scenario = {
            "id": "E-18",
            "agent_mode": "real_live",
            "coverage_class": "live",
            "steps": [],
            "assertions": [],
        }
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
                "agent_mode": "real_live",
                "agent_mode_actual": "real_live",
                "real_provider_contacted": True,
                "agent_mode_contract": {
                    "declared": "real_live",
                    "actual": "real_live",
                    "dry_run": False,
                    "real_provider_contacted": True,
                    "satisfied": True,
                },
                "coverage_class": "live",
                "coverage_class_actual": "live",
                "coverage_class_contract": {
                    "declared": "live",
                    "actual": "live",
                    "dry_run": False,
                    "satisfied": True,
                },
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

    def test_run_scenario_includes_partial_record_on_step_assertion_failure(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def __init__(self):
                self.control_messages: list[str] = []

            def send_control(self, channel_id, content):  # noqa: ARG002
                self.control_messages.append(content)
                return {"id": str(len(self.control_messages))}

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
        scenario = {"id": "E-21", "agent_mode": "real_live", "steps": [], "assertions": []}
        partial = {
            "assertions": [],
            "relay_count": 1,
            "raw_count": 2,
            "tmux_key_sequences": [{"mode": "per_key", "count": 4}],
            "wait_timeouts": [{"classification": "direct_input_notified_no_tail_observed"}],
            "agent_mode": "real_live",
            "agent_mode_actual": "real_live",
            "real_provider_contacted": True,
        }

        with patch(
            "run_tui_relay.run_one_cell",
            side_effect=driver.ScenarioStepAssertionError(
                "timeout waiting for Discord text '[E2E:E21:TAIL]'",
                record=partial,
            ),
        ):
            result = driver.run_scenario(
                scenario,
                args=args,
                run_id="run-1",
                client=FakeClient(),  # type: ignore[arg-type]
            )

        self.assertEqual(result["status"], "fail")
        self.assertEqual(result["relay_count"], 1)
        self.assertEqual(result["raw_count"], 2)
        self.assertEqual(result["tmux_key_sequences"], partial["tmux_key_sequences"])
        self.assertEqual(result["wait_timeouts"], partial["wait_timeouts"])
        self.assertEqual(
            result["failure_attribution"]["wait_timeout_classifications"],
            ["direct_input_notified_no_tail_observed"],
        )

    def test_run_scenario_preserves_agent_mode_record_on_final_assertion_failure(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def __init__(self):
                self.control_messages: list[str] = []

            def send_control(self, channel_id, content):  # noqa: ARG002
                self.control_messages.append(content)
                return {"id": str(len(self.control_messages))}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                return []

            def send_prompt(self, channel_id, content, *, channel_kind="cc"):  # noqa: ARG002
                return {"id": "101"}

        args = Namespace(
            cell="codex-tui",
            channel_id="42",
            thread_channel_id=None,
            reset_before_each=False,
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            hard_reset_session_each=False,
            allow_destructive=False,
            required_agent_mode="real_live",
        )
        scenario = {
            "id": "E-X",
            "agent_mode": "real_live",
            "steps": [{"send_prompt": "hello"}],
            "assertions": [{"text_present": "never observed"}],
        }
        client = FakeClient()

        with (
            patch("run_tui_relay.time.sleep", return_value=None),
            patch(
                "run_tui_relay.run_assertion",
                side_effect=assertions.AssertionError("text missing"),
            ),
        ):
            result = driver.run_scenario(
                scenario,
                args=args,
                run_id="run-1",
                client=client,  # type: ignore[arg-type]
            )

        self.assertEqual(result["status"], "fail")
        self.assertTrue(result["real_provider_contacted"])
        self.assertEqual(result["agent_mode_actual"], "real_live")
        self.assertTrue(result["agent_mode_contract"]["satisfied"])
        self.assertEqual(result["failure_attribution"]["source"], "assertion")
        self.assertTrue(
            any(message.startswith("### E2E TEARDOWN") for message in client.control_messages),
            client.control_messages,
        )

    def test_run_scenario_preserves_agent_mode_record_on_provider_hold_failure(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def __init__(self):
                self.control_messages: list[str] = []

            def send_control(self, channel_id, content):  # noqa: ARG002
                self.control_messages.append(content)
                return {"id": str(len(self.control_messages))}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                return []

            def send_prompt(self, channel_id, content, *, channel_kind="cc"):  # noqa: ARG002
                return {"id": "101"}

        args = Namespace(
            cell="claude-tui",
            channel_id="42",
            thread_channel_id=None,
            reset_before_each=False,
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            hard_reset_session_each=False,
            allow_destructive=False,
            required_agent_mode="real_live",
        )
        scenario = {
            "id": "E-X",
            "agent_mode": "real_live",
            "steps": [
                {
                    "send_provider_hold_prompt": {
                        "ok_marker": "[OK]",
                        "late_marker": "[LATE]",
                    }
                },
                {
                    "wait_for_provider_hold_state": {
                        "ok_marker": "[OK]",
                        "late_marker": "[LATE]",
                    }
                },
            ],
            "assertions": [],
        }
        client = FakeClient()

        with (
            patch("run_tui_relay.time.sleep", return_value=None),
            patch(
                "run_tui_relay.wait_for_provider_hold_state",
                side_effect=assertions.AssertionError("provider hold missing"),
            ),
        ):
            result = driver.run_scenario(
                scenario,
                args=args,
                run_id="run-1",
                client=client,  # type: ignore[arg-type]
            )

        self.assertEqual(result["status"], "fail")
        self.assertTrue(result["real_provider_contacted"])
        self.assertEqual(result["agent_mode_actual"], "real_live")
        self.assertTrue(result["agent_mode_contract"]["satisfied"])
        self.assertEqual(result["failure_attribution"]["source"], "assertion")
        self.assertEqual(len(result["provider_hold_prompts"]), 1)
        self.assertTrue(
            any(message.startswith("### E2E TEARDOWN") for message in client.control_messages),
            client.control_messages,
        )

    def test_run_scenario_preserves_agent_mode_record_on_post_send_step_assertion(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def __init__(self):
                self.control_messages: list[str] = []

            def send_control(self, channel_id, content):  # noqa: ARG002
                self.control_messages.append(content)
                return {"id": str(len(self.control_messages))}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                return []

            def send_prompt(self, channel_id, content, *, channel_kind="cc"):  # noqa: ARG002
                return {"id": "101"}

        args = Namespace(
            cell="codex-tui",
            channel_id="42",
            thread_channel_id=None,
            reset_before_each=False,
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            hard_reset_session_each=False,
            allow_destructive=False,
            required_agent_mode="real_live",
        )
        scenario = {
            "id": "E-X",
            "agent_mode": "real_live",
            "steps": [{"send_prompt": "hello"}, {"assert_health": {"status": "healthy"}}],
            "assertions": [],
        }
        client = FakeClient()

        with (
            patch("run_tui_relay.time.sleep", return_value=None),
            patch(
                "run_tui_relay.assert_health",
                side_effect=assertions.AssertionError("health failed"),
            ),
        ):
            result = driver.run_scenario(
                scenario,
                args=args,
                run_id="run-1",
                client=client,  # type: ignore[arg-type]
            )

        self.assertEqual(result["status"], "fail")
        self.assertTrue(result["real_provider_contacted"])
        self.assertEqual(result["agent_mode_actual"], "real_live")
        self.assertTrue(result["agent_mode_contract"]["satisfied"])
        self.assertEqual(result["failure_attribution"]["source"], "assertion")
        self.assertTrue(
            any(message.startswith("### E2E TEARDOWN") for message in client.control_messages),
            client.control_messages,
        )

    def test_run_scenario_preserves_agent_mode_record_on_post_send_exception(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def __init__(self):
                self.control_messages: list[str] = []
                self.fetch_calls = 0

            def send_control(self, channel_id, content):  # noqa: ARG002
                self.control_messages.append(content)
                return {"id": str(len(self.control_messages))}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                self.fetch_calls += 1
                if self.fetch_calls >= 2:
                    raise RuntimeError("discord history read failed")
                return []

            def send_prompt(self, channel_id, content, *, channel_kind="cc"):  # noqa: ARG002
                return {"id": "101"}

        args = Namespace(
            cell="codex-tui",
            channel_id="42",
            thread_channel_id=None,
            reset_before_each=False,
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            hard_reset_session_each=False,
            allow_destructive=False,
            required_agent_mode="real_live",
        )
        scenario = {
            "id": "E-X",
            "agent_mode": "real_live",
            "steps": [{"send_prompt": "hello"}],
            "assertions": [],
        }
        client = FakeClient()

        with patch("run_tui_relay.time.sleep", return_value=None):
            result = driver.run_scenario(
                scenario,
                args=args,
                run_id="run-1",
                client=client,  # type: ignore[arg-type]
            )

        self.assertEqual(result["status"], "fail")
        self.assertTrue(result["real_provider_contacted"])
        self.assertEqual(result["agent_mode_actual"], "real_live")
        self.assertTrue(result["agent_mode_contract"]["satisfied"])
        self.assertEqual(result["failure_attribution"]["source"], "exception")
        self.assertIn("RuntimeError: discord history read failed", result["reason"])
        self.assertTrue(
            any(message.startswith("### E2E TEARDOWN") for message in client.control_messages),
            client.control_messages,
        )

    def test_run_scenario_does_not_mark_real_provider_when_send_prompt_fails(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def __init__(self):
                self.control_messages: list[str] = []

            def send_control(self, channel_id, content):  # noqa: ARG002
                self.control_messages.append(content)
                return {"id": str(len(self.control_messages))}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                return []

            def send_prompt(self, channel_id, content, *, channel_kind="cc"):  # noqa: ARG002
                raise RuntimeError("dispatch refused before provider contact")

        args = Namespace(
            cell="codex-tui",
            channel_id="42",
            thread_channel_id=None,
            reset_before_each=False,
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            hard_reset_session_each=False,
            allow_destructive=False,
            required_agent_mode="real_live",
        )
        scenario = {
            "id": "E-X",
            "agent_mode": "real_live",
            "steps": [{"send_prompt": "hello"}],
            "assertions": [],
        }
        client = FakeClient()

        with patch("run_tui_relay.time.sleep", return_value=None):
            result = driver.run_scenario(
                scenario,
                args=args,
                run_id="run-1",
                client=client,  # type: ignore[arg-type]
            )

        self.assertEqual(result["status"], "fail")
        self.assertFalse(result["real_provider_contacted"])
        self.assertEqual(result["agent_mode_actual"], "none")
        self.assertFalse(result["agent_mode_contract"]["satisfied"])
        self.assertEqual(result["failure_attribution"]["source"], "exception")
        self.assertIn("dispatch refused before provider contact", result["reason"])
        self.assertTrue(
            any(message.startswith("### E2E TEARDOWN") for message in client.control_messages),
            client.control_messages,
        )

    def test_run_scenario_does_not_mark_real_provider_when_direct_input_send_fails(self):
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
            cell="claude-tui",
            channel_id="42",
            thread_channel_id=None,
            reset_before_each=False,
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            hard_reset_session_each=False,
            allow_destructive=False,
            required_agent_mode="real_live",
        )
        scenario = {
            "id": "E-X",
            "agent_mode": "real_live",
            "steps": [{"send_keys": "hello"}],
            "assertions": [],
        }
        client = FakeClient()

        with (
            patch("run_tui_relay.time.sleep", return_value=None),
            patch("run_tui_relay.tmux.send_keys", return_value=False),
        ):
            result = driver.run_scenario(
                scenario,
                args=args,
                run_id="run-1",
                client=client,  # type: ignore[arg-type]
            )

        self.assertEqual(result["status"], "fail")
        self.assertFalse(result["real_provider_contacted"])
        self.assertEqual(result["agent_mode_actual"], "none")
        self.assertFalse(result["agent_mode_contract"]["satisfied"])
        self.assertEqual(result["failure_attribution"]["source"], "assertion")
        self.assertIn("tmux send-keys failed", result["reason"])
        self.assertTrue(
            any(message.startswith("### E2E TEARDOWN") for message in client.control_messages),
            client.control_messages,
        )

    def test_run_scenario_preserves_partial_concurrent_send_success(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def __init__(self):
                self.control_messages: list[str] = []

            def send_control(self, channel_id, content):  # noqa: ARG002
                self.control_messages.append(content)
                return {"id": str(len(self.control_messages))}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                return []

            def send_prompt(self, channel_id, content, *, channel_kind="cc"):  # noqa: ARG002
                if content == "fail":
                    raise RuntimeError("dispatch refused before provider contact")
                return {"id": "101"}

        args = Namespace(
            cell="codex-tui",
            channel_id="42",
            thread_channel_id=None,
            reset_before_each=False,
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            hard_reset_session_each=False,
            allow_destructive=False,
            required_agent_mode="real_live",
        )
        scenario = {
            "id": "E-X",
            "agent_mode": "real_live",
            "steps": [{"send_prompts_concurrent": ["ok", "fail"]}],
            "assertions": [],
        }
        client = FakeClient()

        with patch("run_tui_relay.time.sleep", return_value=None):
            result = driver.run_scenario(
                scenario,
                args=args,
                run_id="run-1",
                client=client,  # type: ignore[arg-type]
            )

        self.assertEqual(result["status"], "fail")
        self.assertTrue(result["real_provider_contacted"])
        self.assertEqual(result["agent_mode_actual"], "real_live")
        self.assertTrue(result["agent_mode_contract"]["satisfied"])
        self.assertEqual(result["failure_attribution"]["source"], "assertion")
        self.assertEqual(
            result["concurrent_prompt_batches"],
            [[{"index": 0, "channel_id": "42", "message_id": "101"}]],
        )
        self.assertIn("send_prompts_concurrent failed", result["reason"])
        self.assertTrue(
            any(message.startswith("### E2E TEARDOWN") for message in client.control_messages),
            client.control_messages,
        )

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
        scenario = {"id": "E-X", "agent_mode": "none", "steps": [], "assertions": []}
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


class Issue3797E16QuiescenceRelease(unittest.TestCase):
    """#3797: E-16 is an executable live scenario, not a #2935 stub."""

    def setUp(self):
        self.scenarios_dir = ROOT / "tests" / "e2e" / "tui_relay" / "scenarios"
        self.e16 = next(
            scenario
            for scenario in driver.load_scenarios(self.scenarios_dir, cell="claude-tui")
            if scenario.get("id") == "E-16"
        )

    def test_e16_metadata_is_executable_live_real_provider(self):
        self.assertNotIn("skip_reason", self.e16)
        self.assertEqual(driver.scenario_agent_mode(self.e16), "real_live")
        self.assertEqual(driver.scenario_coverage_class(self.e16), "live")
        # The second prompt must be sent before the second marker wait, with no
        # intervening wait_idle_s — that is the post-delivery release window.
        step_keys = [next(iter(step)) for step in self.e16["steps"]]
        self.assertEqual(
            step_keys,
            [
                "send_prompt",
                "wait_for_discord_text",
                "send_prompt",
                "wait_for_discord_text",
                "wait_idle_s",
                "assert_health",
            ],
        )

    def test_e16_immediate_followup_relays_both_markers_and_returns_idle(self):
        class FakeClient:
            base_url = "http://agentdesk.test"

            def __init__(self):
                self.next_id = 5000
                self.prompts: list[str] = []

            def _id(self) -> str:
                self.next_id += 1
                return str(self.next_id)

            def send_control(self, channel_id, content):  # noqa: ARG002
                return {"id": self._id()}

            def send_prompt(self, channel_id, content, *, channel_kind="cc"):  # noqa: ARG002
                self.prompts.append(content)
                return {"id": self._id()}

            def fetch_messages(self, channel_id, *, after_id=None, limit=100):  # noqa: ARG002
                return []

        client = FakeClient()
        idle_calls: list[dict] = []

        def fake_wait(**kwargs):
            needle = str(kwargs.get("needle"))
            message = {
                "id": client._id(),
                "content": needle,
                "author": {"id": "adk-relay-bot", "bot": True},
                "type": 0,
                "timestamp": "2026-05-31T00:00:00Z",
            }
            return message, [message]

        def fake_idle(**kwargs):
            idle_calls.append(kwargs)
            return {
                "status": "idle",
                "channel_id": kwargs.get("channel_id"),
                "mailboxes_seen": 1,
                "queue_files_clear": True,
                "mailbox_idle_evidence": {
                    "agent_turn_status": "idle",
                    "queue_depth": 0,
                    "has_cancel_token": False,
                    "inflight_state_present": False,
                    "active_user_message_id": None,
                    "relay_health": {"pending_discord_callback_msg_id": None},
                },
            }

        args = Namespace(
            cell="claude-tui",
            channel_id="42",
            thread_channel_id=None,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
        )

        with (
            patch("run_tui_relay.time.sleep", return_value=None),
            patch(
                "run_tui_relay.wait_for_discord_text_with_tui_idle_draft_guard",
                side_effect=fake_wait,
            ),
            patch("run_tui_relay.assert_health", return_value={"status": "healthy"}),
            patch("run_tui_relay.assert_cell_idle", side_effect=fake_idle),
            patch("run_tui_relay.send_teardown_marker", return_value=None),
        ):
            record = driver.run_one_cell(
                scenario=self.e16,
                cell="claude-tui",
                channel_id="42",
                client=client,  # type: ignore[arg-type]
                run_id="run-1",
                dry_run=False,
                args=args,
            )

        # Two real prompts dispatched, both markers relayed, all scenario
        # assertions plus the post-scenario idle invariant passed.
        self.assertEqual(len(client.prompts), 2)
        self.assertIn("[E2E:E16:ONE]", client.prompts[0])
        self.assertIn("[E2E:E16:TWO]", client.prompts[1])
        self.assertTrue(record["real_provider_contacted"])
        self.assertEqual(record["agent_mode_actual"], "real_live")
        self.assertEqual(record["coverage_class_actual"], "live")
        self.assertEqual(len(idle_calls), 1)
        idle_specs = [
            entry
            for entry in record["assertions"]
            if entry.get("spec", {}).get("post_scenario_cell_idle")
        ]
        self.assertEqual(len(idle_specs), 1)
        self.assertTrue(idle_specs[0]["passed"])
        self.assertEqual(
            idle_specs[0]["details"]["mailbox_idle_evidence"]["queue_depth"], 0
        )


if __name__ == "__main__":
    unittest.main()
