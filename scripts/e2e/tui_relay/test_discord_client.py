"""Unit tests for the E2E Discord API client."""

from __future__ import annotations

import json
import sys
import unittest
import urllib.error
from email.message import Message
from io import BytesIO
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

from tui_relay.discord import DiscordClient  # noqa: E402


class _Response:
    status = 200

    def __init__(self, payload, *, status=200, headers=None, raw=None):
        self.status = status
        self.headers = Message()
        for name, value in (headers or {}).items():
            self.headers[name] = value
        self._payload = json.dumps(payload).encode("utf-8") if raw is None else raw

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
            error = urllib.error.HTTPError(
                url="http://127.0.0.1:8791/api/agents/adk-claude-pipe-e2e/turn/start",
                code=409,
                msg="Conflict",
                hdrs={},
                fp=BytesIO(b'{"error":"agent mailbox is busy for channel 1"}'),
            )
            self.addCleanup(error.close)
            return error

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


class DiscordClientFetchMessages(unittest.TestCase):
    def setUp(self):
        self.client = DiscordClient(base_url="http://offline.invalid", timeout_s=30)
        self.now = 100.0
        self.urlopen = self.enterContext(mock.patch("urllib.request.urlopen"))
        self.urlopen.side_effect = AssertionError("unexpected offline HTTP request")
        self.sleep = self.enterContext(mock.patch("time.sleep", side_effect=self.advance))
        self.enterContext(mock.patch("time.monotonic", side_effect=lambda: self.now))
        self.enterContext(mock.patch("time.time", return_value=1_000_000_000.0))

    def advance(self, seconds):
        self.now += seconds

    @staticmethod
    def rate_limit(delay=1.25):
        # Synthetic typical Discord body; the original Gate C body was not saved.
        return {"message": "You are being rate limited.", "retry_after": delay, "global": False}

    def http_error(self, body, *, status=429, headers=None, raw=None):
        response = _Response(body, headers=headers, raw=raw)
        error = urllib.error.HTTPError(
            "http://offline.invalid/messages", status, "error", response.headers,
            BytesIO(response.read()),
        )
        self.addCleanup(error.close)
        return error

    def test_wrapped_rate_limit_then_final_refetch_ingests_messages(self):
        message = {"id": "10", "content": "final [E2E:EDIT]"}
        self.urlopen.side_effect = [
            _Response({"messages": self.rate_limit()}),
            _Response({"messages": [message]}),
        ]
        # The final-refetch caller used .get on dict keys before this repair.
        messages = sorted(
            self.client.fetch_messages("channel", after_id="9"),
            key=lambda item: int(item.get("id", "0")),
        )
        self.assertEqual(messages, [message])
        self.sleep.assert_called_once_with(1.25)
        self.assertEqual(self.urlopen.call_count, 2)
        for call in self.urlopen.call_args_list:
            request = call.args[0]
            self.assertEqual(request.full_url, "http://offline.invalid/api/discord/channels/channel/messages?limit=50&after=9")
            self.assertEqual(request.get_method(), "GET")
            self.assertEqual(request.get_header("Connection"), "close")

    def test_actual_http_429_then_success(self):
        for headers, body, delay in (
            ({}, self.rate_limit(), 1.25),
            ({"Retry-After": "2.5"}, {}, 2.5),
            ({"x-ratelimit-reset-after": "3.25"}, {}, 3.25),
            ({"Retry-After": "Sun, 09 Sep 2001 01:46:43 GMT"}, {}, 3.0),
        ):
            with self.subTest(headers=headers):
                self.sleep.reset_mock()
                self.urlopen.side_effect = [self.http_error(body, headers=headers), _Response([])]
                self.assertEqual(self.client.fetch_messages("channel"), [])
                self.sleep.assert_called_once_with(delay)

    def test_reset_delay_is_not_shortened_by_other_metadata(self):
        for response in (
            self.http_error(self.rate_limit(1), headers={"X-RateLimit-Reset-After": "4.25", "Retry-After": "2"}),
            _Response({"messages": self.rate_limit(1)}, headers={"X-RateLimit-Reset-After": "4.25"}),
        ):
            with self.subTest(response=response):
                self.sleep.reset_mock()
                self.urlopen.side_effect = [response, _Response([])]
                self.assertEqual(self.client.fetch_messages("channel"), [])
                self.sleep.assert_called_once_with(4.25)

    def test_http_429_header_delay_survives_non_json_body(self):
        for header in ("Retry-After", "X-RateLimit-Reset-After"):
            for raw in (b"", b"not json", b"\xff"):
                with self.subTest(header=header, raw=raw):
                    self.sleep.reset_mock()
                    self.urlopen.reset_mock()
                    self.urlopen.side_effect = [
                        self.http_error(None, headers={header: "2"}, raw=raw),
                        _Response([]),
                    ]
                    self.assertEqual(self.client.fetch_messages("channel"), [])
                    self.sleep.assert_called_once_with(2.0)
                    self.assertEqual(self.urlopen.call_count, 2)

    def test_non_json_429_over_budget_keeps_status_and_body(self):
        self.urlopen.side_effect = [
            self.http_error(None, headers={"Retry-After": "16"}, raw=b"not json"),
        ]
        with self.assertRaisesRegex(RuntimeError, "observed HTTP 429.*budget.*not json"):
            self.client.fetch_messages("channel")
        self.sleep.assert_not_called()
        self.assertEqual(self.urlopen.call_count, 1)

    def test_retry_attempts_are_bounded(self):
        for wrapped in (False, True):
            with self.subTest(wrapped=wrapped):
                self.urlopen.reset_mock()
                self.sleep.reset_mock()
                self.urlopen.side_effect = [
                    _Response({"messages": self.rate_limit(1)}) if wrapped
                    else self.http_error(self.rate_limit(1))
                    for _ in range(3)
                ]
                with self.assertRaisesRegex(RuntimeError, "attempt limit") as caught:
                    self.client.fetch_messages("channel")
                self.assertIn(f"observed HTTP {200 if wrapped else 429}", str(caught.exception))
                if wrapped:
                    self.assertIn("rate limit inferred", str(caught.exception))
                    self.assertNotIn("HTTP 429", str(caught.exception))
                self.assertEqual(self.urlopen.call_count, 3)
                self.assertEqual(self.sleep.call_args_list, [mock.call(1.0), mock.call(1.0)])

    def test_retry_delay_exceeding_budget_is_not_shortened(self):
        self.urlopen.side_effect = [self.http_error(self.rate_limit(16))]
        with self.assertRaisesRegex(RuntimeError, "budget"):
            self.client.fetch_messages("channel")
        self.sleep.assert_not_called()
        self.assertEqual(self.urlopen.call_count, 1)

    def test_cumulative_retry_delay_is_bounded(self):
        self.urlopen.side_effect = [self.http_error(self.rate_limit(8)) for _ in range(3)]
        with self.assertRaisesRegex(RuntimeError, "budget"):
            self.client.fetch_messages("channel")
        self.sleep.assert_called_once_with(8.0)
        self.assertEqual(self.urlopen.call_count, 2)

    def test_elapsed_request_time_consumes_retry_budget(self):
        def slow_response(*_args, **_kwargs):
            self.advance(14.0)
            return _Response({"messages": self.rate_limit(2)})

        self.urlopen.side_effect = slow_response
        with self.assertRaisesRegex(RuntimeError, "budget"):
            self.client.fetch_messages("channel")
        self.sleep.assert_not_called()
        self.assertEqual(self.urlopen.call_count, 1)

    def test_sleep_overshoot_does_not_start_another_request(self):
        self.sleep.side_effect = lambda _seconds: self.advance(16.0)
        self.urlopen.side_effect = [self.http_error(self.rate_limit(1)), _Response([])]
        with self.assertRaisesRegex(RuntimeError, "budget"):
            self.client.fetch_messages("channel")
        self.assertEqual(self.urlopen.call_count, 1)

    def test_zero_delay_has_a_positive_wait(self):
        self.urlopen.side_effect = [self.http_error(self.rate_limit(0)), _Response([])]
        self.assertEqual(self.client.fetch_messages("channel"), [])
        self.assertEqual(self.sleep.call_count, 1)
        self.assertGreater(self.sleep.call_args.args[0], 0)

    def test_invalid_or_missing_delay_fails_without_retry(self):
        invalid = (None, True, -1, float("nan"), float("inf"), float("-inf"),
                   "NaN", "inf", "-inf", "no delay", [], {})
        responses = [self.http_error({})]
        for delay in invalid:
            responses.extend((
                self.http_error(self.rate_limit(delay)),
                _Response({"messages": self.rate_limit(delay)}),
            ))
        responses.extend(
            self.http_error(self.rate_limit(1), headers={header: delay})
            for header in ("Retry-After", "X-RateLimit-Reset-After")
            for delay in ("", "-1", "NaN", "inf", "invalid")
        )
        for response in responses:
            with self.subTest(response=response):
                self.urlopen.reset_mock()
                self.urlopen.side_effect = [response]
                with self.assertRaisesRegex(RuntimeError, "delay"):
                    self.client.fetch_messages("channel")
                self.assertEqual(self.urlopen.call_count, 1)
        self.sleep.assert_not_called()

    def test_malformed_success_fails_with_status_and_bounded_body(self):
        bodies = (None, 1, "text", {}, {"other": []}, {"messages": None},
                  {"messages": {}}, {"messages": "x"}, ["bad"], {"messages": [None]},
                  {"messages": [{"id": "1"}, 2]})
        responses = [_Response(body) for body in bodies]
        responses.extend(_Response(None, raw=raw) for raw in (b"", b"not json", b"\xff", b"x" * 10000))
        for response in responses:
            with self.subTest(body=response.read()[:100]):
                self.urlopen.reset_mock()
                self.urlopen.side_effect = [response]
                with self.assertRaisesRegex(RuntimeError, "observed HTTP 200") as caught:
                    self.client.fetch_messages("channel")
                diagnostic = str(caught.exception)
                self.assertIn("body=", diagnostic)
                self.assertIn(response.read().decode("utf-8", "replace")[:20], diagnostic)
                self.assertLess(len(diagnostic), 800)
                self.assertEqual(self.urlopen.call_count, 1)
        self.sleep.assert_not_called()

    def test_ordinary_errors_are_not_inferred_from_text(self):
        for response in (
            _Response({"messages": {"message": "You are being rate limited."}}),
            _Response(self.rate_limit()),
            self.http_error(self.rate_limit(), status=503),
        ):
            with self.subTest(response=response):
                self.urlopen.reset_mock()
                self.urlopen.side_effect = [response]
                with self.assertRaises(RuntimeError):
                    self.client.fetch_messages("channel")
                self.assertEqual(self.urlopen.call_count, 1)
        self.sleep.assert_not_called()

    def test_rate_limit_without_delay_and_transport_errors_do_not_retry(self):
        for response, expected in (
            (self.http_error(None, raw=b"bad json"),
             "observed HTTP 429.*missing rate-limit delay.*bad json"),
            (urllib.error.URLError("offline transport failure"), "URL error.*offline transport failure"),
        ):
            with self.subTest(response=response):
                self.urlopen.reset_mock()
                self.urlopen.side_effect = [response]
                with self.assertRaisesRegex(RuntimeError, expected):
                    self.client.fetch_messages("channel")
                self.assertEqual(self.urlopen.call_count, 1)
        self.sleep.assert_not_called()

    def test_successful_raw_and_enveloped_lists(self):
        for messages in ([], [{"id": "10", "content": "ok"}], [{}]):
            for body in (messages, {"messages": messages}):
                with self.subTest(body=body):
                    self.urlopen.side_effect = [_Response(body)]
                    self.assertEqual(self.client.fetch_messages("channel"), messages)
        self.sleep.assert_not_called()

    def test_wait_preserves_fixed_cursor_and_same_id_edits_after_retry(self):
        initial = {"id": "10", "content": "Processing..."}
        final = {"id": "10", "content": "final", "edited_timestamp": "edited"}
        self.urlopen.side_effect = [
            _Response({"messages": [initial, {"id": "11", "content": "later"}]}),
            _Response({"messages": self.rate_limit()}),
            _Response([final]),
        ]
        found, observed = self.client.wait_for_message(
            "channel", predicate=lambda message: message.get("content") == "final",
            after_id="9", timeout_s=30, poll_interval_s=1,
        )
        self.assertEqual(found, final)
        self.assertEqual([message["content"] for message in observed], ["Processing...", "later", "final"])
        for call in self.urlopen.call_args_list:
            self.assertTrue(call.args[0].full_url.endswith("limit=100&after=9"))

    def test_post_operations_do_not_retry_rate_limits(self):
        client = DiscordClient(
            base_url="http://offline.invalid", handoff_to_agent="worker", handoff_from_agent="driver",
        )
        for operation in (client.send, client.send_control, client.send_prompt):
            with self.subTest(operation=operation):
                self.urlopen.reset_mock()
                self.urlopen.side_effect = [self.http_error(self.rate_limit())]
                with self.assertRaises((RuntimeError, urllib.error.HTTPError)):
                    operation("channel", "hello")
                self.assertEqual(self.urlopen.call_count, 1)
        self.sleep.assert_not_called()


if __name__ == "__main__":
    unittest.main()
