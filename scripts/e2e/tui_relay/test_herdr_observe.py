"""Offline Herdr detail/status, local HTTP auth, and operator restart contracts."""

import io
import json
import sys
import tempfile
import unittest
from argparse import Namespace
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import run_tui_relay as driver
from tui_relay import herdr, herdr_restart, http_auth

real_run_scenario = driver.run_scenario


def health():
    return {"status": "healthy", "fully_recovered": True, "degraded_reasons": [], "uptime_secs": 12,
            "herdr": {"admission": "open", "restart_required": False, "configured_channels": ["41"],
                      "endpoints": {"test": {"local": True}}, "input_holds": 0,
                      "reconnect": dict(channels=0, published=0, withheld=0, unknown=0, pending=0)}}


def row():
    return dict(channel="41", provider="claude", state="bound", nonce="same", pane="provider_running",
                launch_evidence="recorded", input_hold=None)


def status(rows=None):
    return {"executions": [row()] if rows is None else rows, "other_input_holds": []}


def arguments(*extra):
    with patch.object(sys, "argv", ["driver", "--cell", "claude-herdr", "--channel-id", "41",
                                   "--base-url", "http://127.0.0.1:8797", "--herdr-endpoint", "test", *extra]):
        return driver.parse_args()


class Response:
    def __init__(self, payload, code=200, raw=None):
        self.status, self.headers = code, {}
        self.raw = json.dumps(payload).encode() if raw is None else raw

    def read(self):
        return self.raw

    def __enter__(self):
        return self

    def __exit__(self, *_):
        pass


class HerdrObserve(unittest.TestCase):
    def setUp(self):
        self.args = arguments()
        self.http = self.enterContext(patch("urllib.request.urlopen", side_effect=AssertionError("offline only")))
        self.cli = self.enterContext(patch.object(herdr.subprocess, "run", return_value=Namespace(stdout=json.dumps(status()))))
        self.client = Mock()
        self.client.send_control.return_value = {"id": "100"}
        self.client.fetch_messages.return_value = []

    def run_scenario(self):
        scenario = dict(id="E-1", agent_mode="real_live", coverage_class="live",
                        steps=[{"send_prompt": "hello"}], assertions=[])
        with redirect_stdout(io.StringIO()):
            return driver.run_scenario(scenario, args=self.args, client=self.client, run_id="test")

    def assert_no_send(self, result):
        self.assertEqual(result["status"], "fail", result)
        self.client.send_control.assert_not_called()
        self.client.send.assert_not_called()
        self.client.send_prompt.assert_not_called()
        self.assertFalse(result["real_provider_contacted"])

    def test_detail_surface_when_public_has_no_herdr(self):
        public = {k: v for k, v in health().items() if k != "herdr"}
        self.http.side_effect = lambda req, **_: Response(health() if req.full_url.endswith("/detail") else public)
        evidence = herdr.observe(driver, self.args, clean=True)
        self.assertEqual(self.http.call_args.args[0].full_url, self.args.base_url + "/api/health/detail")
        self.assertEqual(evidence["surface"], "GET /api/health/detail")
        self.assertEqual(evidence["row"]["nonce"], "same")
        self.assertEqual(evidence["server_values"], {"uptime_secs": 12})
        self.assertNotIn("Origin", json.dumps(evidence))
        self.assertNotIn("Authorization", json.dumps(evidence))

    def test_readiness_failure_sends_nothing(self):
        for changes, code in (({}, 503), ({"fully_recovered": False}, 200), ({"status": "degraded"}, 200),
                              ({"ok": False}, 200), ({"degraded": True}, 200)):
            with self.subTest(changes=changes, code=code), patch.object(driver, "_read_api_json", return_value=(code, {**health(), **changes})):
                self.assert_no_send(self.run_scenario())
        self.cli.assert_not_called()

    def test_http_errors_and_missing_fields_send_nothing(self):
        for code, raw in ((401, b'{}'), (403, b'{}'), (200, b''), (200, b'not JSON'), (503, b'{}')):
            with self.subTest(code=code, raw=raw):
                self.http.side_effect = None
                self.http.return_value = Response(None, code, raw)
                result = self.run_scenario()
                self.assert_no_send(result)
                self.assertEqual(result["herdr_observations"][0]["surface"], "GET /api/health/detail")
        for key in ("herdr", "status", "fully_recovered", "degraded_reasons"):
            payload = health()
            del payload[key]
            self.http.return_value = Response(payload)
            self.assert_no_send(self.run_scenario())
        for key in health()["herdr"]:
            payload = health()
            del payload["herdr"][key]
            self.http.return_value = Response(payload)
            self.assert_no_send(self.run_scenario())

    def test_cached_input_holds_types_and_current_holds(self):
        for value in (1, True, False, "unreadable", "not_counted_yet", "0", None):
            with self.subTest(value=value):
                payload = health()
                payload["herdr"]["input_holds"] = value
                self.http.side_effect = None
                self.http.return_value = Response(payload)
                self.assert_no_send(self.run_scenario())
        self.http.return_value = Response(health())
        for current in (status([{**row(), "input_hold": {"recorded_at": "now"}}]),
                        {**status(), "other_input_holds": [{"nonce": "orphan"}]}, {"executions": []}):
            self.cli.return_value.stdout = json.dumps(current)
            self.assert_no_send(self.run_scenario())

    def test_cold_and_post_turn_row_contract(self):
        self.http.side_effect = None
        self.http.return_value = Response(health())
        self.cli.return_value.stdout = json.dumps(status([]))
        self.assertIsNone(herdr.observe(driver, self.args, cold=True)["row"])
        with self.assertRaisesRegex(driver.assertions.AssertionError, "missing"):
            herdr.observe(driver, self.args, clean=True)
        for rows in ([row(), row()], [{**row(), "provider": "codex"}], [{"record": "unreadable"}],
                     [{**row(), "pane": "unreadable"}], [{**row(), "launch_evidence": "none"}]):
            with self.subTest(rows=rows):
                self.cli.return_value.stdout = json.dumps(status(rows))
                self.assert_no_send(self.run_scenario())
        self.cli.return_value.stdout = json.dumps(status())
        self.assertTrue(herdr.observe(driver, self.args, clean=True)["validated"])
        with self.assertRaisesRegex(driver.assertions.AssertionError, "unpublished"):
            herdr.observe(driver, self.args, reconnected=True)

    def test_cold_run_fails_if_row_is_still_absent_after_prompt(self):
        self.http.side_effect = None
        self.http.return_value = Response(health())
        self.cli.return_value.stdout = json.dumps(status([]))
        with patch.object(driver, "post_send_sleep"):
            result = self.run_scenario()
        self.assertEqual(result["status"], "fail")
        self.client.send.assert_called_once()
        self.assertIn("missing", result["reason"])

    def test_cli_unreadable_or_remote_api_never_sends(self):
        self.http.side_effect = None
        self.http.return_value = Response(health())
        self.cli.return_value.stdout = 'unreadable'
        self.assert_no_send(self.run_scenario())
        self.args.base_url = "http://other-node.invalid:8797"
        self.http.reset_mock()
        self.assert_no_send(self.run_scenario())
        self.http.assert_not_called()

    def test_origin_shared_by_health_send_control_fetch(self):
        bases = ("http://127.0.0.1:8797", "https://localhost:9443", "http://[::1]:8797", "http://localhost",
                 "http://localhost.evil:8797", "http://127.0.0.1.evil", "http://192.168.0.1", "http://remote.invalid")
        for base in bases:
            with self.subTest(base=base):
                self.http.reset_mock()
                self.http.side_effect = [Response({}), Response({}), Response({}), Response([])]
                driver._read_api_json(base, "/api/health")
                client = driver.discord.DiscordClient(base)
                client.send("41", "prompt")
                client.send_control("41", "setup")
                client.fetch_messages("41")
                expected = base if base in bases[:4] else None
                self.assertEqual([c.args[0].get_header("Origin") for c in self.http.call_args_list], [expected] * 4)
                self.assertEqual([c.args[0].get_method() for c in self.http.call_args_list], ["GET", "POST", "POST", "GET"])
        for invalid in ("http://localhost@evil", "http://evil@localhost", "http://[::1]:bad", "file://localhost"):
            self.assertIsNone(http_auth.loopback_origin(invalid))


class RestartObserveOnly(unittest.TestCase):
    def setUp(self):
        self.temp = self.enterContext(tempfile.TemporaryDirectory())
        self.mark = Path(self.temp) / "mark.json"
        self.args = arguments("--herdr-restart-observe-only", "mark", "--herdr-restart-mark", str(self.mark), "--output", self.temp)
        self.enterContext(patch("urllib.request.urlopen", side_effect=AssertionError("offline only")))
        self.http = self.enterContext(patch.object(driver, "_read_api_json", return_value=(200, health())))
        self.cli = self.enterContext(patch.object(herdr.subprocess, "run", return_value=Namespace(stdout=json.dumps(status()))))
        self.client = self.enterContext(patch.object(driver.discord, "DiscordClient"))
        self.scenario = self.enterContext(patch.object(driver, "run_scenario", return_value={"id": "E-19-observe-only", "status": "pass"}))

    def run_mode(self, mode):
        self.args.herdr_restart_observe_only = mode
        with redirect_stdout(io.StringIO()):
            rc = herdr_restart.run(driver, self.args)
        return rc, json.loads((Path(self.temp) / "report.claude-herdr.json").read_text())

    def test_mark_and_verify_never_restart_and_keep_timestamps(self):
        self.assertEqual(self.run_mode("mark")[0], 0)
        self.client.assert_not_called()
        self.scenario.assert_not_called()
        before = json.loads(self.mark.read_text())
        payload = health()
        payload["herdr"]["reconnect"].update(channels=1, published=1)
        self.http.return_value = (200, payload)
        rc, report = self.run_mode("verify")
        self.assertEqual(rc, 0, report)
        restart = report["restart_observation"]
        self.assertEqual(restart["before"]["observed_at"], before["observation"]["observed_at"])
        self.assertEqual(restart["after"]["server_values"], {"uptime_secs": 12})
        self.assertIn("not proven", restart["limitation"])
        self.assertTrue(all(c.args[0] == ["agentdesk", "herdr", "status"] for c in self.cli.call_args_list))
        self.assertEqual(self.scenario.call_args.args[0]["id"], "E-19-observe-only")

    def test_missing_broken_mark_and_discontinuity_block_followup(self):
        for content in (None, "{broken", "{}"):
            if content is not None:
                self.mark.write_text(content)
            self.assertEqual(self.run_mode("verify")[0], 1)
        self.mark.unlink()
        self.run_mode("mark")
        payload = health()
        payload["herdr"]["reconnect"].update(channels=1, published=1)
        self.http.return_value = (200, payload)
        for change in ({"nonce": "replacement"}, {"channel": "42"}, {"input_hold": {"recorded_at": "now"}}):
            self.cli.return_value.stdout = json.dumps(status([{**row(), **change}]))
            self.assertEqual(self.run_mode("verify")[0], 1)
        self.client.assert_not_called()
        self.scenario.assert_not_called()

    def test_unpublished_counts_and_restart_scenario_fail_before_input(self):
        self.run_mode("mark")
        for key in ("withheld", "unknown", "pending", "published"):
            payload = health()
            payload["herdr"]["reconnect"].update(channels=1, published=1)
            payload["herdr"]["reconnect"][key] = 0 if key == "published" else 1
            self.http.return_value = (200, payload)
            self.assertEqual(self.run_mode("verify")[0], 1)
        self.args.filter = "E-8"
        self.assertEqual(self.run_mode("verify")[0], 1)
        self.client.assert_not_called()

    def test_default_followup_runs_real_runner_without_restart_steps(self):
        self.run_mode("mark")
        payload = health()
        payload["herdr"]["reconnect"].update(channels=1, published=1)
        self.http.return_value = (200, payload)
        client = self.client.return_value
        client.send_control.return_value = {"id": "100"}
        marker = f"[E2E:E19:{self.args.run_id}:POST]"
        message = {"id": "101", "content": marker, "author": {"id": "42", "bot": True}}
        client.wait_for_message.return_value = (message, [message])
        client.fetch_messages.return_value = [message]
        with patch.object(driver, "run_scenario", real_run_scenario), patch.object(herdr.time, "sleep"):
            rc, report = self.run_mode("verify")
        self.assertEqual(rc, 0, report)
        self.assertEqual(report["scenarios"][0]["status"], "pass")
        self.assertTrue(all(c.args[0] == ["agentdesk", "herdr", "status"] for c in self.cli.call_args_list))

    def test_arguments_reject_isolated_and_dry_run_is_contact_free(self):
        with self.assertRaises(SystemExit), patch("sys.stderr", io.StringIO()):
            arguments("--herdr-restart-observe-only", "mark", "--herdr-restart-mark", str(self.mark), "--herdr-isolated-server")
        self.args.dry_run = True
        self.assertEqual(self.run_mode("mark")[0], 0)
        self.assertEqual(self.run_mode("verify")[0], 0)
        self.assertFalse(self.mark.exists())
        self.cli.assert_not_called()
        self.http.assert_not_called()
        self.client.assert_not_called()


if __name__ == "__main__":
    unittest.main()
