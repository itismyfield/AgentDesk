"""Offline Herdr detail/status, local HTTP auth, and operator restart contracts."""

import io
import itertools
import json
import sys
import tempfile
import time
import unittest
from argparse import Namespace
from datetime import datetime, timedelta, timezone
from types import SimpleNamespace
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
        evidence = herdr.observe(driver, self.args)
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
            herdr.observe(driver, self.args)
        for rows in ([row(), row()], [{**row(), "provider": "codex"}], [{"record": "unreadable"}],
                     [{**row(), "pane": "unreadable"}], [{**row(), "launch_evidence": "none"}]):
            with self.subTest(rows=rows):
                self.cli.return_value.stdout = json.dumps(status(rows))
                self.assert_no_send(self.run_scenario())
        self.cli.return_value.stdout = json.dumps(status())
        self.assertTrue(herdr.observe(driver, self.args)["validated"])
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

    def test_origin_on_every_mutating_discord_request(self):
        for base, expected in (("http://127.0.0.1:8797", "http://127.0.0.1:8797"), ("http://remote.invalid", None)):
            with self.subTest(base=base):
                self.http.reset_mock()
                self.http.side_effect = lambda *_, **__: Response({"ok": True, "delivery": "started"})
                client = driver.discord.DiscordClient(base, handoff_to_agent="agent", handoff_from_agent="source")
                client.deliver("agent", "text", "1", "source", "origin")
                client.delete_message("41", "100", provider="claude")
                client.inject_failure("41", provider="claude", operation="send")
                client.clear_failure("41", provider="claude", operation="send")
                client.send_prompt("41", "prompt")
                requests = [c.args[0] for c in self.http.call_args_list]
                self.assertEqual([r.get_method() for r in requests], ["POST", "DELETE", "POST", "DELETE", "POST"])
                self.assertEqual([r.get_header("Origin") for r in requests], [expected] * 5)

    def test_other_channel_hold_or_unreadable_row_refuses_target_observation(self):
        # Current policy: any hold or unreadable row on the node blocks the target, not only its own row.
        self.http.side_effect = None
        self.http.return_value = Response(health())
        for other in ({**row(), "channel": "99", "nonce": "other", "input_hold": {"recorded_at": "now"}},
                      {"channel": "99", "record": "unreadable"}):
            with self.subTest(other=other):
                self.cli.return_value.stdout = json.dumps(status([row(), other]))
                with self.assertRaisesRegex(driver.assertions.AssertionError, "input hold|unreadable"):
                    herdr.observe(driver, self.args)


# Shape of the shared release node's reasons; none names the target channel 41.
NODE_REASONS = ["relay_verdict_unknown_codex_99", "provider:claude:pending_queue_depth:1",
                "tui_o:content_transform", "tui_o:released:a", "tui_o:released:b"]
WAIVE = [arg for pattern in ("relay_verdict_unknown_codex_*", "provider:claude:pending_queue_depth:1",
                             "tui_o:content_transform", "tui_o:released*")
         for arg in ("--herdr-allowed-degraded-reason", pattern)]


def degraded(reasons=NODE_REASONS, **changes):
    return {**health(), "status": "degraded", "degraded_reasons": list(reasons), **changes}


class HerdrDegradedWaiver(unittest.TestCase):
    def setUp(self):
        self.args = arguments(*WAIVE)
        self.enterContext(patch("urllib.request.urlopen", side_effect=AssertionError("offline only")))
        self.http = self.enterContext(patch.object(driver, "_read_api_json", return_value=(200, degraded())))
        self.cli = self.enterContext(patch.object(herdr.subprocess, "run", return_value=Namespace(stdout=json.dumps(status()))))

    def refused(self, payload, reason="", code=200):
        self.http.return_value = (code, payload)
        with self.assertRaisesRegex(driver.assertions.AssertionError, reason) as caught:
            herdr.observe(driver, self.args)
        return caught.exception.herdr_observation

    def test_without_flag_degraded_fails_and_evidence_says_no_waiver(self):
        self.args = arguments()
        readiness = self.refused(degraded(), "readiness failed")["readiness"]
        self.assertEqual(readiness["degraded_reasons"], NODE_REASONS)
        self.http.return_value = (200, health())
        readiness = herdr.observe(driver, self.args)["readiness"]
        self.assertEqual((readiness["degraded_reason_waiver"], readiness["allowed_degraded_reasons"],
                          readiness["ignored_degraded_reasons"]), ("none", [], []))

    def test_every_reason_waived_passes_and_is_recorded(self):
        readiness = herdr.observe(driver, self.args)["readiness"]
        self.assertEqual(readiness["status"], "degraded")
        self.assertEqual(readiness["degraded_reason_waiver"], "applied")
        self.assertEqual(readiness["allowed_degraded_reasons"], WAIVE[1::2])
        self.assertEqual(readiness["ignored_degraded_reasons"], NODE_REASONS)
        self.http.return_value = (200, degraded(ok=False, degraded=True))
        self.assertTrue(herdr.observe(driver, self.args)["validated"])
        self.http.return_value = (200, health())
        self.assertEqual(herdr.observe(driver, self.args)["readiness"]["degraded_reason_waiver"], "unused")

    def test_result_json_and_console_show_waiver(self):
        client = Mock()
        client.send_control.return_value = {"id": "100"}
        client.fetch_messages.return_value = []
        scenario = dict(id="E-1", agent_mode="real_live", coverage_class="live", steps=[], assertions=[])
        with redirect_stdout(io.StringIO()) as out, patch.object(herdr.time, "sleep"):
            result = driver.run_scenario(scenario, args=self.args, client=client, run_id="test")
        self.assertEqual(result["status"], "pass", result)
        self.assertEqual(result["herdr_readiness_waiver"],
                         {"degraded_reason_waiver": "applied", "allowed_degraded_reasons": WAIVE[1::2],
                          "ignored_degraded_reasons": sorted(NODE_REASONS)})
        self.assertIn("herdr readiness waiver", out.getvalue())

    def test_unmatched_herdr_or_target_channel_reason_is_never_waived(self):
        for reasons, blocked in (([*NODE_REASONS, "disk_low_free_bytes:1"], "disk_low_free_bytes:1"),
                                 (["tui_o:released:herdr_reconnect"], "tui_o:released:herdr_reconnect"),
                                 (["tui_o:released:HERDR"], "tui_o:released:HERDR"),
                                 (["relay_verdict_unknown_codex_41"], "relay_verdict_unknown_codex_41")):
            with self.subTest(reasons=reasons):
                readiness = self.refused(degraded(reasons), "not waived.*" + blocked)["readiness"]
                self.assertEqual(readiness["ignored_degraded_reasons"], [])

    def test_unready_or_malformed_detail_fails(self):
        for payload, code in ((degraded(status="unhealthy"), 200), (degraded(status="starting"), 200),
                              (degraded(fully_recovered=False), 200), (degraded(degraded_reasons="tui_o:released:a"), 200),
                              (degraded([1]), 200), (degraded([]), 200), (health() | {"degraded_reasons": ["tui_o:released:a"]}, 200),
                              (degraded(ok=True), 200), (degraded(degraded="yes"), 200), (health() | {"ok": False}, 200),
                              (degraded(), 503)):
            with self.subTest(payload=payload, code=code):
                self.refused(payload, code=code)
        self.cli.assert_not_called()

    def test_waiver_keeps_every_herdr_block_and_row_check(self):
        for herdr_change in ({"admission": "closed"}, {"restart_required": True}, {"input_holds": 1},
                             {"configured_channels": ["99"]}):
            payload = degraded()
            payload["herdr"].update(herdr_change)
            with self.subTest(herdr_change=herdr_change):
                self.refused(payload)
        self.http.return_value = (200, degraded())
        for rows in ([{**row(), "provider": "codex"}], [{**row(), "channel": "99"}]):
            with self.subTest(rows=rows):
                self.cli.return_value.stdout = json.dumps(status(rows))
                self.refused(degraded(), "target row")

    def test_scenario_health_steps_withhold_waiver(self):
        client = Mock()
        client.send_control.return_value = {"id": "100"}
        client.fetch_messages.return_value = []
        detail = degraded()
        detail["herdr"]["reconnect"].update(channels=1, published=1)
        self.http.side_effect = lambda _base, path, **_: (200, detail if path.endswith("/detail") else {"status": "healthy"})
        self.args.herdr_isolated_server = self.args.allow_destructive = True
        clock = itertools.count(0, 100)
        for step in ({"assert_health": {}}, {"restart_dcserver": {"target": "dev"}}):
            scenario = dict(id="E-8", agent_mode="real_live", coverage_class="live", steps=[step], assertions=[])
            with self.subTest(step=step), redirect_stdout(io.StringIO()), \
                    patch.dict("os.environ", {"AGENTDESK_E2E_ALLOW_DESTRUCTIVE": "1"}), patch.object(herdr.time, "sleep"), \
                    patch.object(herdr.time, "monotonic", side_effect=lambda: next(clock)):
                result = driver.run_scenario(scenario, args=self.args, client=client, run_id="test")
            self.assertEqual(result["status"], "fail", result)
            self.assertIn("readiness failed", result["reason"])
            self.assertEqual(result["herdr_observations"][-1]["readiness"]["degraded_reason_waiver"], "withheld")

    def test_arguments_refuse_wildcard_and_non_herdr_cells(self):
        for pattern in ("*", "", " * "):
            with self.subTest(pattern=pattern), self.assertRaises(SystemExit), patch("sys.stderr", io.StringIO()):
                arguments("--herdr-allowed-degraded-reason", pattern)
        argv = ["driver", "--cell", "claude-tui", "--channel-id", "41", "--herdr-allowed-degraded-reason", "tui_o:released*"]
        with patch.object(sys, "argv", argv), self.assertRaises(SystemExit), patch("sys.stderr", io.StringIO()) as err:
            driver.parse_args()
        self.assertIn("only to Herdr cells", err.getvalue())


class RestartObserveOnly(unittest.TestCase):
    def setUp(self):
        self.temp = self.enterContext(tempfile.TemporaryDirectory())
        self.mark = Path(self.temp) / "mark.json"
        self.lease_dir = Path(self.temp) / "leases"
        self.lease_dir.mkdir()
        self.lease_file = self.lease_dir / "agentdesk-e2e-relay.claude-tui.lease"
        self.enterContext(patch.object(driver.lease, "DEFAULT_LEASE_DIR", self.lease_dir))
        self.args = arguments("--herdr-restart-observe-only", "mark", "--herdr-restart-mark", str(self.mark), "--output", self.temp)
        self.enterContext(patch("urllib.request.urlopen", side_effect=AssertionError("offline only")))
        self.http = self.enterContext(patch.object(driver, "_read_api_json", return_value=(200, health())))
        self.cli = self.enterContext(patch.object(herdr.subprocess, "run", return_value=Namespace(stdout=json.dumps(status()))))
        self.client = self.enterContext(patch.object(driver.discord, "DiscordClient"))
        self.scenario = self.enterContext(patch.object(driver, "run_scenario", return_value={"id": "E-19-observe-only", "status": "pass"}))

    def run_mode(self, mode):
        self.args.herdr_restart_observe_only = mode
        with redirect_stdout(io.StringIO()) as out:
            rc = herdr_restart.run(driver, self.args)
        self.stdout = out.getvalue()
        return rc, json.loads((Path(self.temp) / "report.claude-herdr.json").read_text())

    def publishable(self, uptime=12):
        payload = health()
        payload["herdr"]["reconnect"].update(channels=1, published=1)
        payload.pop("uptime_secs")
        payload.update({} if uptime is None else {"uptime_secs": uptime})
        self.http.return_value = (200, payload)

    def set_mark(self, observed_at=None, **observation):
        mark = json.loads(self.mark.read_text())
        moment = observed_at or datetime.now(timezone.utc) - timedelta(seconds=60)
        mark["observation"].update(observed_at=moment.isoformat(), **observation)
        self.mark.write_text(json.dumps(mark))

    def marked(self, uptime=3):
        """A mark taken 60 s ago, then health from a server that started after it."""
        self.assertEqual(self.run_mode("mark")[0], 0)
        self.set_mark()
        self.publishable(uptime)

    def assert_refused(self, reason):
        rc, report = self.run_mode("verify")
        self.assertEqual(rc, 1, report)
        self.assertIn(reason, report["restart_observation"]["reason"])
        self.client.assert_not_called()
        self.scenario.assert_not_called()
        return report

    def test_mark_and_verify_never_restart_and_keep_timestamps(self):
        self.assertEqual(self.run_mode("mark")[0], 0)
        self.client.assert_not_called()
        self.scenario.assert_not_called()
        self.set_mark()
        before = json.loads(self.mark.read_text())
        self.publishable(3)
        rc, report = self.run_mode("verify")
        self.assertEqual(rc, 0, report)
        restart = report["restart_observation"]
        self.assertEqual(restart["before"]["observed_at"], before["observation"]["observed_at"])
        self.assertEqual(restart["after"]["server_values"], {"uptime_secs": 3})
        self.assertEqual((restart["restart_evidence"], restart["uptime_secs"]), ("server_uptime", 3))
        self.assertGreaterEqual(restart["elapsed_secs"], 60)
        self.assertTrue(all(c.args[0] == ["agentdesk", "herdr", "status"] for c in self.cli.call_args_list))
        self.assertEqual(self.scenario.call_args.args[0]["id"], "E-19-observe-only")

    def test_server_uptime_older_than_mark_refuses_followup(self):
        self.run_mode("mark")
        self.publishable(99999)
        report = self.assert_refused("restart not observed")
        restart = report["restart_observation"]
        self.assertEqual((restart["restart_evidence"], restart["uptime_secs"]), ("not_observed", 99999))
        self.assertLess(restart["elapsed_secs"], 60)

    def test_uptime_boundaries_and_clock_regressions_refuse(self):
        now = datetime.now(timezone.utc) - timedelta(seconds=100)
        clock = type("Clock", (datetime,), {"now": classmethod(lambda cls, tz=None: now)})
        self.run_mode("mark")
        with patch.object(herdr, "dt", SimpleNamespace(datetime=clock, timezone=timezone)):
            for mark_age, uptime in ((60, 60), (60, 59), (60, 58), (0, 0), (-10, 0)):
                with self.subTest(mark_age=mark_age, uptime=uptime):
                    self.set_mark(now - timedelta(seconds=mark_age))
                    self.publishable(uptime)
                    self.assert_refused("restart not observed")
            self.set_mark(now - timedelta(seconds=60))
            self.publishable(57)
            self.assertEqual(self.run_mode("verify")[0], 0)

    def test_unusable_uptime_keeps_unproven_limitation(self):
        self.run_mode("mark")
        for uptime in (None, True, -1, "12", 1.5):
            with self.subTest(uptime=uptime):
                self.publishable(uptime)
                rc, report = self.run_mode("verify")
                self.assertEqual(rc, 0, report)
                self.assertEqual(report["restart_observation"]["restart_evidence"], "not_proven")
                self.assertIn("not proven", report["restart_observation"]["limitation"])
                self.assertIn("not proven", self.stdout)
        self.assertEqual(self.scenario.call_count, 5)

    def test_verify_holds_cell_lease_and_releases_it_on_every_path(self):
        self.marked()
        held = []
        def followup(*_, **__):
            held.append(self.lease_file.read_text().split("|")[0])
            return {"id": "E-19-observe-only", "status": "pass"}
        outcomes = (followup, lambda *_, **__: {"id": "E-19-observe-only", "status": "fail"}, RuntimeError("followup crashed"))
        with patch.object(driver.lease, "acquire", wraps=driver.lease.acquire) as acquire:
            for expected_rc, effect in zip((0, 1, 1), outcomes):
                self.scenario.side_effect = effect
                self.assertEqual(self.run_mode("verify")[0], expected_rc)
                self.assertFalse(self.lease_file.exists())
            self.cli.return_value.stdout = json.dumps(status([{**row(), "nonce": "replacement"}]))
            self.assertEqual(self.run_mode("verify")[0], 1)
            self.assertFalse(self.lease_file.exists())
        self.assertEqual(acquire.call_count, 4)
        self.assertEqual(held, [f"claude-herdr-{self.args.run_id}"])

    def test_held_cell_lease_refuses_before_observation_or_client(self):
        self.marked()
        self.lease_file.write_text(f"claude-tui-other|{time.time()}\n")
        self.http.reset_mock()
        self.assert_refused("existing lease")
        self.http.assert_not_called()
        self.assertTrue(self.lease_file.read_text().startswith("claude-tui-other|"))

    def test_missing_broken_mark_and_discontinuity_block_followup(self):
        for content in (None, "{broken", "{}"):
            if content is not None:
                self.mark.write_text(content)
            self.assertEqual(self.run_mode("verify")[0], 1)
        self.mark.unlink()
        self.marked()
        for change, reason in (({"nonce": "replacement"}, "continuity"), ({"channel": "42"}, "missing"),
                               ({"input_hold": {"recorded_at": "now"}}, "input hold")):
            self.cli.return_value.stdout = json.dumps(status([{**row(), **change}]))
            self.assert_refused(reason)

    def test_mark_refuses_overwrite_and_missing_target_row(self):
        self.assertEqual(self.run_mode("mark")[0], 0)
        first = self.mark.read_text()
        self.assertEqual(self.run_mode("mark")[0], 1)
        self.assertEqual(self.mark.read_text(), first)
        self.mark.unlink()
        self.cli.return_value.stdout = json.dumps(status([]))
        self.assertEqual(self.run_mode("mark")[0], 1)
        self.assertFalse(self.mark.exists())

    def test_mark_for_other_target_or_unvalidated_observation_is_refused(self):
        self.marked()
        self.args.base_url = "http://127.0.0.1:8791"
        self.assert_refused("target/version mismatch")
        self.args.base_url = "http://127.0.0.1:8797"
        self.set_mark(validated=False)
        self.assert_refused("observation missing")

    def test_mark_and_verify_apply_the_same_waiver(self):
        self.http.return_value = (200, degraded())
        self.assertEqual(self.run_mode("mark")[0], 1)
        self.args.herdr_allowed_degraded_reason = WAIVE[1::2]
        self.assertEqual(self.run_mode("mark")[0], 0)
        mark = json.loads(self.mark.read_text())["observation"]["readiness"]
        self.assertEqual(mark["degraded_reason_waiver"], "applied")
        self.set_mark()
        payload = degraded()
        payload["herdr"]["reconnect"].update(channels=1, published=1)
        payload["uptime_secs"] = 3
        self.http.return_value = (200, payload)
        rc, report = self.run_mode("verify")
        self.assertEqual(rc, 0, report)
        self.assertEqual(report["restart_observation"]["after"]["readiness"]["ignored_degraded_reasons"], NODE_REASONS)
        self.assertEqual(report["herdr_readiness_waiver"]["degraded_reason_waiver"], "applied")
        self.assertIn("herdr readiness waiver", self.stdout)
        self.args.herdr_allowed_degraded_reason = None
        self.client.reset_mock()
        self.scenario.reset_mock()
        self.assert_refused("readiness failed")

    def test_unpublished_counts_and_restart_scenario_fail_before_input(self):
        self.marked()
        for key in ("withheld", "unknown", "pending", "published"):
            payload = health()
            payload["herdr"]["reconnect"].update(channels=1, published=1)
            payload["herdr"]["reconnect"][key] = 0 if key == "published" else 1
            self.http.return_value = (200, payload)
            self.assert_refused("unpublished")
        self.publishable(3)
        self.args.filter = "E-8"
        self.args.scenarios = str(Path(__file__).resolve().parents[3] / "tests/e2e/tui_relay/scenarios")
        self.assert_refused("restart_dcserver")

    def test_default_followup_runs_real_runner_without_restart_steps(self):
        self.marked()
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

    def test_driver_main_dispatches_observe_only_without_sending(self):
        argv = ["driver", "--cell", "claude-herdr", "--channel-id", "41", "--base-url", "http://127.0.0.1:8797",
                "--herdr-endpoint", "test", "--herdr-restart-observe-only", "mark",
                "--herdr-restart-mark", str(self.mark), "--output", self.temp]
        with patch.object(sys, "argv", argv), redirect_stdout(io.StringIO()):
            self.assertEqual(driver.main(), 0)
        self.assertTrue(self.mark.exists())
        self.client.assert_not_called()
        self.scenario.assert_not_called()

    def test_arguments_reject_isolated_and_dry_run_is_contact_free(self):
        with self.assertRaises(SystemExit), patch("sys.stderr", io.StringIO()):
            arguments("--herdr-restart-observe-only", "mark", "--herdr-restart-mark", str(self.mark), "--herdr-isolated-server")
        self.args.restart_script = "/unused/restart.sh"
        self.assertEqual(self.run_mode("mark")[0], 1)
        self.args.restart_script = None
        self.args.dry_run = True
        self.assertEqual(self.run_mode("mark")[0], 0)
        self.assertEqual(self.run_mode("verify")[0], 0)
        self.assertFalse(self.mark.exists())
        self.cli.assert_not_called()
        self.http.assert_not_called()
        self.client.assert_not_called()


if __name__ == "__main__":
    unittest.main()
