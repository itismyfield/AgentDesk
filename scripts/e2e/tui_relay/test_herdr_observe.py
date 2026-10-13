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
real_boot_clock = herdr_restart.boot_clock
# A supported boot clock name; the restart tests drive its readings through a patched boot_clock.
CLOCK = "CLOCK_MONOTONIC"


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
                          "ignored_degraded_reasons": sorted(NODE_REASONS),
                          "ignored_degraded_reasons_unattributed": sorted(NODE_REASONS)})
        self.assertIn("herdr readiness waiver", out.getvalue())

    def test_unmatched_herdr_or_target_channel_reason_is_never_waived(self):
        for reasons, blocked in (([*NODE_REASONS, "disk_low_free_bytes:1"], "disk_low_free_bytes:1"),
                                 (["tui_o:released:herdr_reconnect"], "tui_o:released:herdr_reconnect"),
                                 (["tui_o:released:HERDR"], "tui_o:released:HERDR"),
                                 (["relay_verdict_unknown_codex_41"], "relay_verdict_unknown_codex_41")):
            with self.subTest(reasons=reasons):
                readiness = self.refused(degraded(reasons), "not waived.*" + blocked)["readiness"]
                self.assertEqual(readiness["ignored_degraded_reasons"], [])

    def test_waived_reasons_without_a_channel_id_are_marked_unattributed(self):
        other = "relay_verdict_unknown_codex_1509350490565180999"
        aggregate = ["provider:claude:pending_queue_depth:1", "tui_o:content_transform",
                     "global_active_counter_out_of_bounds:raw=18446744073709551615:provider_active_turns=0:global_finalizing=0"]
        self.args = arguments(*WAIVE, "--herdr-allowed-degraded-reason", "global_active_counter_out_of_bounds*")
        self.http.return_value = (200, degraded([*aggregate, other]))
        readiness = herdr.observe(driver, self.args)["readiness"]
        self.assertEqual(readiness["ignored_degraded_reasons"], [*aggregate, other])
        self.assertEqual(readiness["ignored_degraded_reasons_unattributed"], aggregate)
        self.assertEqual(herdr.waiver_summary(self.args, [{"readiness": readiness}])["ignored_degraded_reasons_unattributed"],
                         sorted(aggregate))
        # An observation from an older mark carries ignored reasons but no marker field.
        legacy = {"readiness": {"ignored_degraded_reasons": ["provider:claude:pending_queue_depth:1", other]}}
        summary = herdr.waiver_summary(self.args, [legacy])
        self.assertEqual((summary["ignored_degraded_reasons"], summary["ignored_degraded_reasons_unattributed"]),
                         (sorted(["provider:claude:pending_queue_depth:1", other]), ["provider:claude:pending_queue_depth:1"]))
        for reasons in (["relay_verdict_unknown_codex_41"], ["tui_o:released:herdr"]):
            with self.subTest(reasons=reasons):
                self.assertEqual(self.refused(degraded(reasons), "never_waived")["readiness"]
                                 ["ignored_degraded_reasons_unattributed"], [])

    def test_unready_or_malformed_detail_fails(self):
        for payload, code in ((degraded(status="unhealthy"), 200), (degraded(status="starting"), 200),
                              (degraded(fully_recovered=False), 200), (degraded(degraded_reasons="tui_o:released:a"), 200),
                              (degraded([1]), 200), (degraded([]), 200), (health() | {"degraded_reasons": ["tui_o:released:a"]}, 200),
                              (degraded(ok=True), 200), (degraded(degraded="yes"), 200), (health() | {"ok": False}, 200),
                              (degraded(), 503)):
            with self.subTest(payload=payload, code=code):
                self.refused(payload, code=code)
        self.cli.assert_not_called()

    def test_waiver_keeps_herdr_block_and_target_row_checks(self):
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

    def test_waiver_keeps_each_guard_failing_on_its_own(self):
        def changed(herdr_block=None, reconnect=None, **top):
            payload = degraded(**top)
            payload["herdr"]["reconnect"].update({"channels": 1, "published": 1, **(reconnect or {})})
            payload["herdr"].update(herdr_block or {})
            return payload
        other_hold = {**row(), "channel": "99", "nonce": "other", "input_hold": {"recorded_at": "now"}}
        # (label, argument change, detail, HTTP code, CLI status, reconnected, waiver at refusal, reason)
        cases = (("endpoint not selected", {"herdr_endpoint": "other"}, changed(), 200, status(), True, "applied", "explicitly selected and local"),
                 ("endpoint remote", {}, changed({"endpoints": {"test": {"local": False}}}), 200, status(), True, "applied", "explicitly selected and local"),
                 ("mixed endpoints", {}, changed({"endpoints": {"test": {"local": True}, "far": {"local": False}}}), 200, status(), True, "applied", "mixed/remote"),
                 ("reconnect missing", {}, changed({"reconnect": None}), 200, status(), False, "applied", "reconnect counts missing"),
                 ("reconnect negative", {}, changed(reconnect={"pending": -1}), 200, status(), False, "applied", "reconnect counts missing"),
                 ("reconnect not int", {}, changed(reconnect={"published": "1"}), 200, status(), False, "applied", "reconnect counts missing"),
                 ("published zero", {}, changed(reconnect={"published": 0, "channels": 0}), 200, status(), True, "applied", "unpublished"),
                 ("withheld", {}, changed(reconnect={"withheld": 1}), 200, status(), True, "applied", "unpublished"),
                 ("unknown", {}, changed(reconnect={"unknown": 1}), 200, status(), True, "applied", "unpublished"),
                 ("pending", {}, changed(reconnect={"pending": 1}), 200, status(), True, "applied", "unpublished"),
                 ("unpublished channel", {}, changed(reconnect={"channels": 2}), 200, status(), True, "applied", "publish every row"),
                 ("other row hold", {}, changed(), 200, status([row(), other_hold]), False, "applied", "input hold"),
                 ("orphan hold", {}, changed(), 200, {**status(), "other_input_holds": [{"nonce": "x"}]}, False, "applied", "other input holds"),
                 ("not fully recovered", {}, changed(fully_recovered=False), 200, status(), True, "unused", "readiness failed"),
                 ("HTTP 503", {}, changed(), 503, status(), True, "unused", "readiness failed"),
                 ("unknown status", {}, changed(status="starting"), 200, status(), True, "unused", "readiness failed"))
        for label, arg_change, payload, code, current, reconnected, waiver, reason in cases:
            with self.subTest(label=label):
                self.args = arguments(*WAIVE)
                self.http.return_value = (200, changed())
                self.cli.return_value.stdout = json.dumps(status())
                self.assertTrue(herdr.observe(driver, self.args, reconnected=reconnected)["validated"])
                vars(self.args).update(arg_change)
                self.http.return_value = (code, payload)
                self.cli.return_value.stdout = json.dumps(current)
                with self.assertRaisesRegex(driver.assertions.AssertionError, reason) as caught:
                    herdr.observe(driver, self.args, reconnected=reconnected)
                self.assertEqual(caught.exception.herdr_observation["readiness"]["degraded_reason_waiver"], waiver)

    def test_flag_with_healthy_detail_still_fails_on_herdr_block(self):
        for change, reason in (({"admission": "closed"}, "admission closed"), ({"restart_required": None}, "restart required"),
                               ({"input_holds": 1}, "input_holds")):
            payload = health()
            payload["herdr"].update(change)
            with self.subTest(change=change):
                readiness = self.refused(payload, reason)["readiness"]
                self.assertEqual((readiness["degraded_reason_waiver"], readiness["ignored_degraded_reasons"]), ("unused", []))

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

    def test_early_exits_and_text_stop_composite_record_waiver_summary(self):
        plain = dict(id="E-1", agent_mode="real_live", coverage_class="live", steps=[], assertions=[])
        cases = (("agent mode", dict(plain, agent_mode="controlled"), {"required_agent_mode": "real_live"}, "fail"),
                 ("coverage", dict(plain, coverage_class="fixture"), {"required_coverage_class": "live"}, "fail"),
                 ("restart binding", dict(plain, steps=[{"restart_dcserver": {"target": "release"}}]), {}, "fail"),
                 ("hard reset", plain, {"hard_reset_session_each": True}, "fail"),
                 ("destructive skip", dict(plain, destructive=True), {}, "skipped"),
                 ("feature skip", dict(plain, requires_features=["absent"]), {}, "skipped"),
                 ("E36 arguments", dict(plain, e36_normal_intake=True), {"reset_before_each": True}, "fail"),
                 ("E-18 composite refusal", dict(plain, id="E-18", herdr_text_stop=True, agent_mode="controlled"),
                  {"required_agent_mode": "real_live"}, "fail"),
                 ("E-18 composite dry-run", dict(plain, id="E-18", herdr_text_stop=True), {"dry_run": True}, "dry_run"))
        for flagged in (True, False):
            for label, scenario, changes, expected in cases:
                with self.subTest(label=label, flagged=flagged), redirect_stdout(io.StringIO()) as out, \
                        patch.dict("os.environ", {"AGENTDESK_E2E_ALLOW_DESTRUCTIVE": "", "AGENTDESK_E2E_FEATURES": ""}):
                    self.args = arguments(*WAIVE) if flagged else arguments()
                    vars(self.args).update(changes)
                    client = Mock()
                    result = driver.run_scenario(dict(scenario), args=self.args, client=client, run_id="test")
                    self.assertEqual(result["status"], expected, result)
                    client.send_control.assert_not_called()
                    self.assertEqual(result["herdr_readiness_waiver"],
                                     {"degraded_reason_waiver": "unused" if flagged else "none",
                                      "allowed_degraded_reasons": WAIVE[1::2] if flagged else [],
                                      "ignored_degraded_reasons": [], "ignored_degraded_reasons_unattributed": []})
                    self.assertEqual("herdr readiness waiver" in out.getvalue(), flagged)
        self.http.assert_not_called()

    def test_refused_requests_record_waiver_metadata(self):
        failures = (("loopback", "http://remote.invalid:8797", None, "loopback"),
                    ("request", None, OSError("connection refused"), "connection refused"),
                    ("non-object", None, (200, []), "must be an object"))
        for flagged, waivable, expected in ((True, True, "unused"), (True, False, "withheld"), (False, True, "none")):
            for label, base_url, response, reason in failures:
                with self.subTest(label=label, flagged=flagged, waivable=waivable):
                    self.args = arguments(*WAIVE) if flagged else arguments()
                    self.args.base_url = base_url or self.args.base_url
                    self.http.side_effect = response if isinstance(response, Exception) else None
                    self.http.return_value = response if isinstance(response, tuple) else (200, degraded())
                    with self.assertRaisesRegex(Exception, reason) as caught:
                        herdr.observe(driver, self.args, waivable=waivable)
                    readiness = caught.exception.herdr_observation["readiness"]
                    self.assertEqual((readiness["degraded_reason_waiver"], readiness["allowed_degraded_reasons"],
                                      readiness["ignored_degraded_reasons"]),
                                     (expected, WAIVE[1::2] if flagged and waivable else [], []))
        self.cli.assert_not_called()

    def test_arguments_refuse_wildcard_and_non_herdr_cells(self):
        for pattern in ("*", "", " * "):
            with self.subTest(pattern=pattern), self.assertRaises(SystemExit), patch("sys.stderr", io.StringIO()):
                arguments("--herdr-allowed-degraded-reason", pattern)
        argv = ["driver", "--cell", "claude-tui", "--channel-id", "41", "--herdr-allowed-degraded-reason", "tui_o:released*"]
        with patch.object(sys, "argv", argv), self.assertRaises(SystemExit), patch("sys.stderr", io.StringIO()) as err:
            driver.parse_args()
        self.assertIn("only to Herdr cells", err.getvalue())


CLEARED = "세션을 초기화했어요."
REFUSED = "세션을 초기화하지 못했어요: rotation_unsettled(59)"


def bot(message_id, content, **extra):
    return {"id": str(message_id), "content": content, "author": {"id": "7", "bot": True}, **extra}


class HerdrTextClear(unittest.TestCase):
    """E-50's text !clear: a server refusal fails at once; silence keeps the acknowledgement timeout."""

    def setUp(self):
        self.args = arguments()
        self.enterContext(patch("urllib.request.urlopen", side_effect=AssertionError("offline only")))
        self.enterContext(patch.object(driver, "_read_api_json", return_value=(200, health())))
        self.enterContext(patch.object(herdr.subprocess, "run", return_value=Namespace(stdout=json.dumps(status()))))
        self.enterContext(patch.object(driver, "post_send_sleep"))
        self.sleep = self.enterContext(patch.object(herdr.time, "sleep"))
        clock = itertools.count(0, 5)
        self.enterContext(patch.object(herdr.time, "monotonic", side_effect=lambda: next(clock)))
        self.client = Mock()
        self.client.send_control.return_value = {"id": "100"}
        after = bot(300, "[E2E:AFTER]")
        self.client.wait_for_message.return_value = (after, [after])

    def run_clear(self, *fetches, before=(), sent=None):
        # The send response shape of POST /api/discord/send; the !clear message id bounds its replies.
        self.client.send.return_value = sent or {"ok": True, "message_id": "160" if before else "101"}
        steps = [*({"wait_for_raw_discord_text": "READY", "timeout_s": 5} for _ in before),
                 {"send_discord_prompt": "!clear [E2E:CLEAR]"}, {"wait_for_raw_discord_text": CLEARED, "timeout_s": 20},
                 {"send_discord_prompt": "Reply with [E2E:AFTER]"}, {"wait_for_discord_text": "[E2E:AFTER]"}]
        self.client.fetch_messages.side_effect = [*before, *fetches] + [list(fetches[-1])] * 20 if fetches else [*before] + [[]] * 20
        scenario = dict(id="E-50", agent_mode="real_live", coverage_class="live", steps=steps, assertions=[])
        with redirect_stdout(io.StringIO()):
            return driver.run_scenario(scenario, args=self.args, client=self.client, run_id="test")

    def test_refusal_reply_fails_at_once_with_its_reason(self):
        for refusal in (REFUSED, "세션 전환 중이라 초기화하지 못했어요"):
            with self.subTest(refusal=refusal):
                self.client.reset_mock()
                self.sleep.reset_mock()
                self.client.send_control.return_value = {"id": "100"}
                result = self.run_clear([bot(101, "!clear [E2E:CLEAR]"), bot(102, refusal)])
                self.assertEqual(result["status"], "fail", result)
                self.assertIn(f"text !clear refused by the server: {refusal}", result["reason"])
                self.assertEqual(self.client.send.call_count, 1)
                self.assertEqual(self.client.fetch_messages.call_count, 1)
                self.sleep.assert_not_called()

    def test_completion_reply_continues_to_the_next_turn(self):
        result = self.run_clear([bot(101, "!clear [E2E:CLEAR]"), bot(102, CLEARED)])
        self.assertEqual(result["status"], "pass", result)
        self.assertEqual(self.client.send.call_args.args[1], "Reply with [E2E:AFTER]")

    def test_silence_or_unrelated_refusal_text_keeps_the_timeout(self):
        stale = [bot(150, "READY"), bot(140, REFUSED)]
        for label, rows, before in (("silence", [], ()),
                                    ("other channel", [bot(102, REFUSED, channel_id="99")], ()),
                                    ("user message", [{**bot(102, REFUSED), "author": {"id": "8", "bot": False}}], ()),
                                    ("quoted in a reply", [bot(102, f"> {REFUSED}")], ()),
                                    ("sent before the clear", stale, (stale,))):
            with self.subTest(label=label):
                self.client.reset_mock()
                self.client.send_control.return_value = {"id": "100"}
                result = self.run_clear(rows, before=before) if rows else self.run_clear(before=before)
                self.assertEqual(result["status"], "fail", result)
                self.assertIn(f"acknowledgement missing: {CLEARED}", result["reason"])
                self.assertEqual(self.client.send.call_count, 1)
        # Server wording that drifted from the prefixes loses only the fail-fast, never reads as cleared.
        with patch.object(herdr, "CLEAR_REFUSAL_PREFIXES", ("세션을 비우지 못했어요:",)):
            self.client.reset_mock()
            self.client.send_control.return_value = {"id": "100"}
            result = self.run_clear([bot(102, REFUSED)])
        self.assertEqual(result["status"], "fail", result)
        self.assertIn(f"acknowledgement missing: {CLEARED}", result["reason"])

    def test_clear_replies_are_bounded_by_the_clear_message_id(self):
        # Last observed 140, an unseen older refusal 149, this clear 150, then its own reply.
        old, cleared, refused = bot(149, REFUSED), bot(151, CLEARED), bot(152, REFUSED)
        with_id, no_id = {"ok": True, "message_id": "150"}, {"ok": True}
        for label, sent, rows, expected, reason in (
                ("older refusal, then completion", with_id, [old, cleared], "pass", None),
                ("older refusal, then completion (id key)", {"id": "150"}, [old, cleared], "pass", None),
                ("this clear refused", with_id, [old, refused], "fail", "refused by the server: " + REFUSED),
                ("no send id, refusal-like reply", no_id, [old, cleared], "fail", "cannot be attributed"),
                ("no send id, completion only", no_id, [cleared], "pass", None)):
            with self.subTest(label=label):
                self.client.reset_mock()
                self.client.send_control.return_value = {"id": "100"}
                result = self.run_clear([bot(140, "READY"), *rows], before=([bot(140, "READY")],), sent=sent)
                self.assertEqual(result["status"], expected, (label, result))
                if reason:
                    self.assertIn(reason, result["reason"])

    def test_clear_completion_shares_the_refusal_boundary(self):
        # This clear is message 150; only a bot reply after it in this channel counts, either way.
        missing = f"acknowledgement missing: {CLEARED}"
        for label, rows, expected, reason in (
                ("older completion only", [bot(149, CLEARED)], "fail", missing),
                ("new completion", [bot(151, CLEARED)], "pass", None),
                ("older and new completion", [bot(149, CLEARED), bot(151, CLEARED)], "pass", None),
                ("this clear refused", [bot(149, CLEARED), bot(152, REFUSED)], "fail", "refused by the server: " + REFUSED),
                ("completion in another channel", [bot(151, CLEARED, channel_id="99")], "fail", missing),
                ("completion typed by a user", [{**bot(151, CLEARED), "author": {"id": "8", "bot": False}}], "fail", missing),
                ("completion quoted", [bot(151, f"> {CLEARED}")], "fail", missing)):
            with self.subTest(label=label):
                self.client.reset_mock()
                self.client.send_control.return_value = {"id": "100"}
                result = self.run_clear([bot(140, "READY"), *rows], before=([bot(140, "READY")],),
                                        sent={"ok": True, "message_id": "150"})
                self.assertEqual(result["status"], expected, (label, result))
                if reason:
                    self.assertIn(reason, result["reason"])
                    self.assertEqual(self.client.send.call_count, 1)

    def test_refusal_prefixes_match_the_server_wording(self):
        # The server answers a refused text !clear with these error strings; keep them in step.
        root = Path(__file__).resolve().parents[3] / "src/services/discord/commands"
        first, second = herdr.CLEAR_REFUSAL_PREFIXES
        self.assertIn(f'"{first} {{reason}}"', (root / "control/native.rs").read_text(encoding="utf-8"))
        self.assertIn(f'"{second}"', (root / "control.rs").read_text(encoding="utf-8"))


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
        self.boot_now = 1000.0
        self.clock = self.enterContext(patch.object(herdr_restart, "boot_clock",
                                                    side_effect=lambda: {"clock": CLOCK, "seconds": self.boot_now}))

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

    def set_mark(self, observed_at=None, *, boot_age=None, **observation):
        """Age the whole mark: observation, completion and boot clock moved back together."""
        mark = json.loads(self.mark.read_text())
        moment = observed_at or datetime.now(timezone.utc) - timedelta(seconds=60)
        mark["observation"].update(observed_at=moment.isoformat(), **observation)
        mark["completed_at"] = moment.isoformat()
        age = (datetime.now(timezone.utc) - moment).total_seconds() if boot_age is None else boot_age
        mark["boot_clock"] = {"clock": CLOCK, "seconds": self.boot_now - age}
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
                    self.set_mark(now - timedelta(seconds=mark_age), boot_age=mark_age)
                    self.publishable(uptime)
                    self.assert_refused("restart not observed")
            self.set_mark(now - timedelta(seconds=60), boot_age=60)
            self.publishable(57)
            self.assertEqual(self.run_mode("verify")[0], 0)

    def edit_mark(self, drop=(), **changes):
        mark = json.loads(self.mark.read_text())
        mark.update(changes)
        for key in drop:
            del mark[key]
        self.mark.write_text(json.dumps(mark))

    def test_mark_records_completion_after_its_response(self):
        self.assertEqual(self.run_mode("mark")[0], 0)
        mark = json.loads(self.mark.read_text())
        self.assertGreaterEqual(datetime.fromisoformat(mark["completed_at"]),
                                datetime.fromisoformat(mark["observation"]["observed_at"]))
        self.assertEqual(mark["boot_clock"], {"clock": CLOCK, "seconds": 1000.0})

    def test_restart_during_mark_request_is_measured_from_mark_completion(self):
        # Mark request ran 63 s..57 s ago and the server restarted inside it; nothing restarted since.
        now = datetime.now(timezone.utc)
        self.marked()
        self.set_mark(now - timedelta(seconds=63), boot_age=57)
        self.edit_mark(completed_at=(now - timedelta(seconds=57)).isoformat())
        self.publishable(59)
        restart = self.assert_refused("restart not observed")["restart_observation"]
        self.assertEqual(restart["restart_evidence"], "not_observed")
        self.assertLess(restart["elapsed_secs"], 58)
        self.assertLess(restart["wall_elapsed_secs"], 58)

    def test_wall_clock_step_forward_uses_the_smaller_boot_clock_elapsed(self):
        # 60 s really passed, the wall clock stepped 20 s forward, and the server never restarted.
        self.marked()
        self.set_mark(datetime.now(timezone.utc) - timedelta(seconds=80), boot_age=60)
        self.publishable(72)
        restart = self.assert_refused("restart not observed")["restart_observation"]
        self.assertEqual((restart["boot_clock"], restart["boot_clock_elapsed_secs"], restart["elapsed_secs"]),
                         (CLOCK, 60.0, 60.0))
        self.assertGreaterEqual(restart["wall_elapsed_secs"], 80)
        self.set_mark(datetime.now(timezone.utc) - timedelta(seconds=40), boot_age=60)
        self.publishable(39)
        self.assertLess(self.assert_refused("restart not observed")["restart_observation"]["elapsed_secs"], 41)
        self.publishable(10)
        self.assertEqual(self.run_mode("verify")[0], 0)

    def test_restart_is_not_confirmed_without_completion_time_and_one_shared_boot_clock(self):
        now = datetime.now(timezone.utc)
        other = "CLOCK_UPTIME_RAW"
        # (label, mark age, mark boot age, mark edit, dropped mark keys, verify clock, uptime)
        cases = (("older mark, restart inside its request", 63, 57, {}, ("completed_at", "boot_clock"), CLOCK, 59),
                 ("no completion time, real restart", 60, 60, {}, ("completed_at",), CLOCK, 3),
                 ("no mark boot clock", 60, 60, {}, ("boot_clock",), CLOCK, 3),
                 ("null mark boot clock (unsupported platform)", 60, 60, {"boot_clock": None}, (), CLOCK, 3),
                 ("clock names differ, wall +20 s", 80, 60, {"boot_clock": {"clock": other, "seconds": 940.0}}, (), CLOCK, 72),
                 ("verify NaN", 80, 60, {}, (), float("nan"), 72),
                 ("verify +inf", 80, 60, {}, (), float("inf"), 72),
                 ("verify -inf", 80, 60, {}, (), float("-inf"), 72),
                 ("verify negative", 80, 60, {}, (), -1.0, 72),
                 ("verify bool", 80, 60, {}, (), True, 72),
                 ("verify unsupported platform", 80, 60, {}, (), None, 72),
                 ("negative mark", 80, 60, {"boot_clock": {"clock": CLOCK, "seconds": -5.0}}, (), CLOCK, 72),
                 ("mark bool", 80, 60, {"boot_clock": {"clock": CLOCK, "seconds": True}}, (), CLOCK, 72),
                 ("mark NaN", 80, 60, {"boot_clock": {"clock": CLOCK, "seconds": float("nan")}}, (), CLOCK, 72),
                 ("mark unsupported name", 80, 60, {"boot_clock": {"clock": "test_clock", "seconds": 940.0}}, (), CLOCK, 72),
                 ("mark not an object", 80, 60, {"boot_clock": CLOCK}, (), CLOCK, 72))
        self.marked()
        for label, age, boot_age, edit, drop, verify, uptime in cases:
            with self.subTest(label=label):
                self.set_mark(now - timedelta(seconds=age), boot_age=boot_age)
                self.edit_mark(drop=drop, **edit)
                self.clock.side_effect = (lambda: None) if verify is None else (
                    lambda: {"clock": CLOCK, "seconds": self.boot_now if verify == CLOCK else verify})
                self.publishable(uptime)
                restart = self.assert_refused("take a new mark")["restart_observation"]
                self.assertNotIn("restart_evidence", restart)

    def test_malformed_completion_time_refuses_verify(self):
        self.marked()
        now = datetime.now(timezone.utc)
        for completed in ((now + timedelta(seconds=30)).isoformat(), (now - timedelta(seconds=90)).isoformat(),
                          (now - timedelta(seconds=30)).replace(tzinfo=None).isoformat()):
            with self.subTest(completed=completed):
                self.set_mark()
                self.edit_mark(completed_at=completed)
                self.assert_refused("restart mark time invalid")

    def test_unsupported_platform_marks_without_a_boot_clock(self):
        self.clock.side_effect, self.clock.return_value = None, None
        self.assertEqual(self.run_mode("mark")[0], 0)
        self.assertIsNone(json.loads(self.mark.read_text())["boot_clock"])

    def test_boot_clock_follows_the_server_uptime_clock_per_platform(self):
        for platform, expected in (("darwin", "CLOCK_UPTIME_RAW"), ("linux", "CLOCK_MONOTONIC"), ("win32", None)):
            with self.subTest(platform=platform), patch.object(herdr_restart.sys, "platform", platform), \
                    patch.object(herdr_restart.time, "clock_gettime", return_value=5.0), \
                    patch.object(herdr_restart.time, "CLOCK_UPTIME_RAW", 8, create=True):
                self.assertEqual(real_boot_clock(), expected and {"clock": expected, "seconds": 5.0})

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

    def test_restart_summary_is_union_of_mark_and_verify_reasons(self):
        self.args.herdr_allowed_degraded_reason = WAIVE[1::2]
        self.http.return_value = (200, degraded(["tui_o:released:a"]))
        self.assertEqual(self.run_mode("mark")[0], 0)
        self.set_mark()
        for verify_reasons, expected_rc, after_key in ((["provider:claude:pending_queue_depth:1"], 0, "after"),
                                                       (["disk_low_free_bytes:1"], 1, "failed_observation")):
            with self.subTest(verify_reasons=verify_reasons):
                payload = degraded(verify_reasons, uptime_secs=3)
                payload["herdr"]["reconnect"].update(channels=1, published=1)
                self.http.return_value = (200, payload)
                rc, report = self.run_mode("verify")
                self.assertEqual(rc, expected_rc, report)
                restart = report["restart_observation"]
                self.assertEqual(restart["before"]["readiness"]["ignored_degraded_reasons"], ["tui_o:released:a"])
                after = verify_reasons if expected_rc == 0 else []
                self.assertEqual(restart[after_key]["readiness"]["ignored_degraded_reasons"], after)
                # The report summary is the union over mark and verify, not either moment alone.
                self.assertEqual(report["herdr_readiness_waiver"]["ignored_degraded_reasons"], sorted(["tui_o:released:a", *after]))

    def test_report_summary_includes_follow_up_turn_observations(self):
        self.args.herdr_allowed_degraded_reason = WAIVE[1::2]
        self.marked()
        client = self.client.return_value
        client.send_control.return_value = {"id": "100"}
        marker = f"[E2E:E19:{self.args.run_id}:POST]"
        message = {"id": "101", "content": marker, "author": {"id": "42", "bot": True}}
        client.wait_for_message.return_value = (message, [message])
        client.fetch_messages.return_value = [message]

        def detail(reasons, **block):
            payload = degraded(reasons, uptime_secs=3) if reasons else {**health(), "uptime_secs": 3}
            payload["herdr"]["reconnect"].update(channels=1, published=1)
            payload["herdr"].update(block)
            return 200, payload
        # mark and verify are healthy; only the follow-up turn's own observations waive reasons.
        for later, expected_rc, ignored in (([detail(["tui_o:released:a"])] * 2, 0, ["tui_o:released:a"]),
                                            ([detail(["tui_o:released:a"]), detail(["provider:claude:pending_queue_depth:1"], input_holds=1)],
                                             1, ["provider:claude:pending_queue_depth:1", "tui_o:released:a"])):
            with self.subTest(expected_rc=expected_rc):
                self.http.side_effect = [detail(None), *later]
                with patch.object(driver, "run_scenario", real_run_scenario), patch.object(herdr.time, "sleep"):
                    rc, report = self.run_mode("verify")
                self.assertEqual(rc, expected_rc, report)
                self.assertEqual(report["restart_observation"]["after"]["readiness"]["degraded_reason_waiver"], "unused")
                self.assertEqual(report["herdr_readiness_waiver"]["degraded_reason_waiver"], "applied")
                self.assertEqual(report["herdr_readiness_waiver"]["ignored_degraded_reasons"], ignored)
                self.assertIn("herdr readiness waiver", self.stdout)

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
