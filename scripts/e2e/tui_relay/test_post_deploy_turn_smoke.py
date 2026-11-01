"""Post-deploy `!clear` + continuous-turn smoke: driver collection and verdicts.

The E-50/E-51 YAML runs through the real ``run_tui_relay.run_scenario`` against
a fake Discord view shaped like the 2026-10-03 live records, and the resulting
report row plus a recorded-shape dcserver log feed the production judge.
"""

from __future__ import annotations

import copy
import io
import os
import subprocess
import threading
import json
import tempfile
import itertools
import sys
import time
import unittest
from argparse import Namespace
from contextlib import redirect_stdout
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

import post_deploy_turn_smoke as turn_smoke  # noqa: E402
import run_tui_relay as driver  # noqa: E402
from tui_relay import assertions  # noqa: E402

SCENARIOS = ROOT / "tests/e2e/tui_relay/scenarios"
FIXTURES = ROOT / "tests/e2e/tui_relay/fixtures/turn_smoke"
RUN_ID = "post-deploy-smoke-turns-fixture"
CHANNELS = {"claude-tui": "1509350490461180105", "codex-tui": "1509350778043895902"}
NOTIFY_BOT = "1480000000000000009"
CLEARED = "세션을 초기화했어요."


class LiveShapedDiscord:
    """Discord as the live records saw it: one reply per prompt unless told otherwise.

    ``after_clear`` is ``"deliver"``, ``"withhold"`` (only the completion chrome
    lands, as in the Claude record) or ``"duplicate"``.
    """

    base_url = "http://offline.invalid"

    def __init__(self, *, after_clear: str = "deliver", provider: str = "claude", answer_author: str | None = None):
        self.after_clear = after_clear
        self.provider_bot = assertions.provider_bot_id(provider)
        self.answer_author = answer_author or self.provider_bot
        self.ids = itertools.count(1_000)
        self.messages: list[dict] = []
        self.channel_id = None

    def _post(self, content: str, *, author: str) -> dict:
        message = {
            "id": str(next(self.ids)),
            "channel_id": self.channel_id,
            "content": content,
            "type": 0,
            "author": {"id": author, "bot": True},
        }
        self.messages.append(message)
        return message

    def send_control(self, channel_id, content):
        self.channel_id = str(channel_id)
        return self._post(content, author=assertions.OUR_BOT_ID)

    def send(self, channel_id, content):  # noqa: ARG002
        sent = self._post(content, author=assertions.OUR_BOT_ID)
        if content.startswith("!clear"):
            self._post(CLEARED, author=self.provider_bot)
            return sent
        marker = content[content.index("[E2E:") : content.index("]") + 1]
        if marker.endswith(":AFTER_CLEAR]") and self.after_clear == "withhold":
            self._post("✅ 완료", author=self.provider_bot)
        elif marker.endswith(":AFTER_CLEAR]") and self.after_clear == "duplicate":
            self._post(marker, author=self.provider_bot)
            self._post(f"🆕 새 세션 시작\n\n{marker}", author=self.provider_bot)
        else:
            self._post(marker, author=self.answer_author)
        return sent

    def fetch_messages(self, channel_id, *, after_id=None, limit=100):  # noqa: ARG002
        floor = int(after_id or 0)
        return [dict(m) for m in self.messages if int(m["id"]) > floor][:limit]

    def wait_for_message(self, channel_id, *, predicate, after_id, timeout_s, debug_label=None):  # noqa: ARG002
        observed = self.fetch_messages(channel_id, after_id=after_id)
        return next((m for m in observed if predicate(m)), None), observed


def run_live_shaped(scenario_id: str, cell: str, *, after_clear: str = "deliver", answer_author: str | None = None) -> dict:
    scenario = next(
        s for s in driver.load_scenarios(SCENARIOS, cell=cell) if s["id"] == scenario_id
    )
    args = Namespace(
        cell=cell, channel_id=CHANNELS[cell], thread_channel_id=None, base_url="http://offline.invalid",
        dry_run=False, reset_before_each=False, hard_reset_session_each=False,
        allow_destructive=False, queue_runtime_root="/nonexistent-runtime",
        required_agent_mode="real_live", required_coverage_class="live",
        final_refetches=1, final_refetch_interval_s=0.0,
    )
    clock = [0.0]

    def advance(seconds):
        clock[0] += float(seconds)

    with (
        patch("run_tui_relay.time.sleep", side_effect=advance),
        patch("run_tui_relay.time.monotonic", side_effect=lambda: clock[0]),
        patch("run_tui_relay._raise_if_tui_prompt_stuck_while_idle"),
        patch("run_tui_relay._collect_wait_timeout_diagnostics", return_value={}),
        patch("run_tui_relay.assert_cell_idle", return_value={"status": "idle"}),
    ):
        return driver.run_scenario(
            scenario, args=args, run_id=RUN_ID,
            client=LiveShapedDiscord(after_clear=after_clear, provider=driver.cell_provider(cell), answer_author=answer_author),
        )


def verdict(result: dict, scenario_id: str, cell: str, log_fixture: str | None) -> tuple[bool, str]:
    log_counts = None
    if log_fixture is not None:
        with (FIXTURES / log_fixture).open(encoding="utf-8") as handle:
            log_counts = turn_smoke.scan_log(
                handle, session=driver.cell_session_name(cell), channel_id=CHANNELS[cell]
            )
    return turn_smoke.judge(
        report={"run_id": RUN_ID, "scenarios": [result]},
        scenario_id=scenario_id,
        markers=turn_smoke.expected_markers(SCENARIOS, scenario_id, RUN_ID),
        log_counts=log_counts,
        driver_rc=0 if result["status"] == "pass" else 1,
    )


class ProviderAuthorDelivery(unittest.TestCase):
    """A turn marker counts only from the cell's provider bot, never a notify/announce body quoting it."""

    def test_legacy_turn_waits_reject_notify_and_announce_bodies(self):
        for scenario_id, cell in (("E-50", "claude-tui"), ("E-51", "codex-tui")):
            for author in (NOTIFY_BOT, assertions.OUR_BOT_ID):
                with self.subTest(scenario=scenario_id, author=author):
                    result = run_live_shaped(scenario_id, cell, answer_author=author)
                    self.assertEqual(result["status"], "fail", result.get("reason"))
                    self.assertIn("timeout waiting for Discord text", result["reason"])
            self.assertEqual(run_live_shaped(scenario_id, cell)["status"], "pass")


class TurnSmokeFromCollectedEvidence(unittest.TestCase):
    def test_clean_codex_run_passes_with_expected_reset_kills_in_log(self):
        result = run_live_shaped("E-51", "codex-tui")

        self.assertEqual(result["status"], "pass", result.get("reason"))
        passed, detail = verdict(result, "E-51", "codex-tui", "turns-clean.dcserver.log")
        self.assertTrue(passed, detail)
        self.assertIn("T1=1 T2=1 AFTER_CLEAR=1", detail)

    def test_claude_body_withheld_after_clear_fails_with_zero_deliveries(self):
        # Claude live record: after !clear only the completion chrome reached Discord.
        result = run_live_shaped("E-50", "claude-tui", after_clear="withhold")

        self.assertEqual(result["status"], "fail")
        self.assertEqual(
            result["marker_counts"],
            {f"[E2E:E50:{RUN_ID}:{label}]": hits for label, hits in (("T1", 1), ("T2", 1), ("AFTER_CLEAR", 0))},
        )
        passed, detail = verdict(result, "E-50", "claude-tui", "turns-clean.dcserver.log")
        self.assertFalse(passed)
        self.assertIn("AFTER_CLEAR body withheld after !clear (0 deliveries)", detail)

    def test_after_clear_duplicate_fails_with_two_deliveries(self):
        result = run_live_shaped("E-50", "claude-tui", after_clear="duplicate")

        passed, detail = verdict(result, "E-50", "claude-tui", "turns-clean.dcserver.log")
        self.assertFalse(passed)
        self.assertIn("AFTER_CLEAR body duplicated (2 deliveries)", detail)

    def test_codex_warm_followup_kill_fails_even_when_every_body_arrived(self):
        # Codex live record: each body reached Discord once; only the log shows the kill.
        result = run_live_shaped("E-51", "codex-tui")
        self.assertEqual(result["status"], "pass", result.get("reason"))

        passed, detail = verdict(result, "E-51", "codex-tui", "codex-warm-followup-kill.dcserver.log")
        self.assertFalse(passed)
        self.assertIn("composer_not_detected=2 warm_followup_kill=2 cold_resume=2", detail)

    def test_codex_launch_options_fallback_names_changed_fields_in_fail_reason(self):
        result = run_live_shaped("E-51", "codex-tui")
        session = driver.cell_session_name("codex-tui")
        line = (
            "2026-10-08T22:20:54.929740Z  WARN agentdesk::services::codex_tui::warm_followup: "
            "Codex TUI warm follow-up falling back to one cold resume launch "
            f'tmux_session_name="{session}" fallback_reason="launch_options_changed" '
            'detail="eligibility gate rejected reuse" launch_options_mismatch="changed" '
            'changed_fields="compact_token_limit" '
            'launch_options_detail="compact_token_limit: 272000 -> 217600"\n'
        )
        launch_options: list[str] = []
        log_counts = turn_smoke.scan_log(
            [line], session=session, channel_id=CHANNELS["codex-tui"], launch_options=launch_options
        )
        passed, detail = turn_smoke.judge(
            report={"run_id": RUN_ID, "scenarios": [result]},
            scenario_id="E-51",
            markers=turn_smoke.expected_markers(SCENARIOS, "E-51", RUN_ID),
            log_counts=log_counts,
            driver_rc=0,
            launch_options=launch_options,
        )
        self.assertFalse(passed)
        self.assertIn("cold_resume=1 launch_options=changed(compact_token_limit)", detail)

    def test_missing_evidence_never_passes(self):
        result = run_live_shaped("E-51", "codex-tui")
        passed, detail = verdict(result, "E-51", "codex-tui", None)
        self.assertFalse(passed)
        self.assertIn("dcserver log excerpt unavailable", detail)

        # A log that never names this cell (wrong file, other node) is not evidence of this run.
        foreign = turn_smoke.scan_log(
            ["2026-10-03T03:10:02Z  INFO agentdesk: [session-strategy] channel=42 tmux=AgentDesk-codex-adk-ops-cdx"],
            session=driver.cell_session_name("codex-tui"),
            channel_id=CHANNELS["codex-tui"],
        )
        passed, detail = turn_smoke.judge(
            report={"run_id": RUN_ID, "scenarios": [result]}, scenario_id="E-51",
            markers=turn_smoke.expected_markers(SCENARIOS, "E-51", RUN_ID),
            log_counts=foreign, driver_rc=0,
        )
        self.assertFalse(passed)
        self.assertIn("dcserver log excerpt has no line for this cell", detail)

        unobserved = {**result}
        unobserved.pop("marker_counts")
        passed, detail = verdict(unobserved, "E-51", "codex-tui", "turns-clean.dcserver.log")
        self.assertFalse(passed)
        self.assertIn("AFTER_CLEAR delivery count unobserved", detail)


class HerdrTurnSmoke(unittest.TestCase):
    def evidence(self):
        return {"surface": "GET /api/health/detail", "status_surface": "agentdesk herdr status",
                "validated": True, "current_input_holds": 0, "herdr": {"input_holds": 0},
                "row": dict(channel="41", provider="claude", state="bound", nonce="same",
                            pane="provider_running", launch_evidence="recorded", input_hold=None)}

    def collect(self, after_clear="deliver", answer_author=None):
        scenario = next(s for s in driver.load_scenarios(SCENARIOS, cell="claude-herdr") if s["id"] == "E-50")
        args = Namespace(cell="claude-herdr", channel_id="41", base_url="http://127.0.0.1:8797",
                         dry_run=False, hard_reset_session_each=False, reset_before_each=False,
                         allow_destructive=False, final_refetches=1)
        with patch.object(driver.herdr, "observe", return_value=self.evidence()), patch.object(driver.herdr.time, "sleep"):
            return driver.run_scenario(scenario, args=args, run_id=RUN_ID,
                                       client=LiveShapedDiscord(after_clear=after_clear, answer_author=answer_author))

    def judge(self, result):
        return turn_smoke.judge(report={"run_id": RUN_ID, "channel_id": "41", "cell": "claude-herdr", "scenarios": [result]},
                                scenario_id="E-50", markers=turn_smoke.expected_markers(SCENARIOS, "E-50", RUN_ID),
                                log_counts=None, driver_rc=0)

    def test_herdr_preflight_never_reads_legacy_queue_or_tmux(self):
        with patch.object(driver, "_read_health_detail", side_effect=AssertionError("legacy detail")), \
             patch.object(driver, "_runtime_queue_violations", side_effect=AssertionError("legacy queue")), \
             patch.object(driver, "cell_session_name", side_effect=AssertionError("tmux")), \
             patch.object(driver.herdr, "observe", return_value=self.evidence()) as observe:
            self.assertEqual(turn_smoke.preflight_busy_reasons(base_url="http://127.0.0.1:8797", cell="claude-herdr",
                             channel_id="41", herdr_endpoint="test", herdr_status_bin="/test/agentdesk"), [])
        args = observe.call_args.args[1]
        self.assertEqual((args.herdr_endpoint, args.herdr_status_bin), ("test", "/test/agentdesk"))
        self.assertTrue(observe.call_args.kwargs["cold"])

    def test_herdr_turn_waits_reject_notify_and_announce_bodies(self):
        for author in (NOTIFY_BOT, assertions.OUR_BOT_ID):
            with self.subTest(author=author):
                result = self.collect(answer_author=author)
                self.assertEqual(result["status"], "fail", result.get("reason"))
                self.assertIn("Herdr Discord marker timeout", result["reason"])

    def test_herdr_delivery_counts_and_missing_observations_fail_closed(self):
        for behavior, expected in (("deliver", True), ("withhold", False), ("duplicate", False)):
            result = self.collect(behavior)
            passed, detail = self.judge(result)
            self.assertEqual(passed, expected, detail)
        result = self.collect()
        result.pop("herdr_observations")
        self.assertFalse(self.judge(result)[0])
        for label in ("T1", "T2", "AFTER_CLEAR"):
            for hits in (0, 2, -1, True):
                result = self.collect()
                result["marker_counts"][f"[E2E:E50:{RUN_ID}:{label}]"] = hits
                self.assertFalse(self.judge(result)[0])

    def test_herdr_judge_requires_two_complete_observations_and_valid_final_row(self):
        evidence = self.evidence()
        cases = [("one", lambda obs: obs[:1], "observations missing"),
                 ("final row", lambda obs: [*obs[:-1], {**evidence, "row": {**evidence["row"], "provider": "codex"}}],
                  "not a verified running execution")]
        for key, value in (("surface", "GET /api/health"), ("validated", False), ("status_surface", "tmux"),
                           ("current_input_holds", True), ("current_input_holds", 1)):
            cases.append((key, lambda obs, k=key, v=value: [{**obs[0], k: v}, *obs[1:]], "evidence incomplete"))
        for name, change, reason in cases:
            with self.subTest(name):
                result = self.collect()
                result["herdr_observations"] = change(result["herdr_observations"])
                passed, detail = self.judge(result)
                self.assertFalse(passed)
                self.assertRegex(detail, f"Herdr observation unavailable: [^;]*{reason}")

    def test_herdr_judge_requires_t1_t2_after_clear_label_set(self):
        markers = turn_smoke.expected_markers(SCENARIOS, "E-50", RUN_ID)
        passed, detail = turn_smoke.judge(report={"run_id": RUN_ID, "channel_id": "41", "scenarios": [self.collect()]},
                                          scenario_id="E-50", markers=markers[:2], log_counts=None, driver_rc=0,
                                          cell="claude-herdr")
        self.assertFalse(passed)
        self.assertIn("requires T1, T2 and AFTER_CLEAR", detail)

    def test_herdr_judge_cli_ignores_tmux_log_surface(self):
        report = {"run_id": RUN_ID, "scenarios": [self.collect()]}
        with patch.object(turn_smoke, "_load_report", return_value=report), \
             patch.object(driver, "cell_session_name", side_effect=ValueError("no tmux")), \
             patch.object(turn_smoke, "open", side_effect=AssertionError("legacy log"), create=True):
            self.assertEqual(turn_smoke.main(["judge", "--report", "/unused", "--scenarios", str(SCENARIOS),
                             "--scenario", "E-50", "--cell", "claude-herdr", "--channel-id", "41",
                             "--log-excerpt", "/unused", "--driver-rc", "0"]), 0)


SAVED = FIXTURES / "saved"
SAVED_CHANNEL = "1559670200419352696"
SAVED_PASS = "e50-herdr-pass-20261101T175451Z.json"
CLEAR_REFUSAL = "세션을 초기화하지 못했어요: herdr clear refused: rotation_unsettled(59)"


class UserVisibleTurnVerdict(unittest.TestCase):
    """E-50 is judged by the bodies the user saw, whatever session host runs the channel.

    ``saved/`` holds the 11-01 and 11-02 deploy smoke rows from
    ~/.adk/release/logs/post-deploy-smoke-turns-<stamp>/report.claude-tui.json, trimmed to
    the fields the judge reads (long reasons cut at 400 characters).
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.config = self.root / "agentdesk.yaml"
        self.write_config(herdr=True)

    def write_config(self, *, herdr):
        mapped = SAVED_CHANNEL if herdr else "1509350490461180100"
        self.config.write_text(
            "agents:\n  - id: adk-claude-tui-2-e2e\n"
            f"    channels: {{claude: {{id: '{SAVED_CHANNEL}'}}}}\n"
            f"session_hosts:\n  herdr:\n    channels: {{{mapped}: local}}\n", encoding="utf-8")

    def saved(self, name):
        return json.loads((SAVED / name).read_text(encoding="utf-8"))

    def judge_cli(self, report, *, driver_rc):
        path = self.root / "report.json"
        path.write_text(json.dumps(report), encoding="utf-8")
        output = io.StringIO()
        with redirect_stdout(output):
            rc = turn_smoke.main([
                "judge", "--config", str(self.config), "--run-id", report["run_id"], "--report", str(path),
                "--scenarios", str(SCENARIOS), "--scenario", "E-50", "--cell", "claude-tui",
                "--channel-id", SAVED_CHANNEL, "--log-excerpt", str(FIXTURES / "turns-clean.dcserver.log"),
                "--driver-rc", str(driver_rc)])
        return rc, output.getvalue().strip()

    def test_saved_herdr_run_with_every_body_delivered_is_a_plain_pass(self):
        rc, line = self.judge_cli(self.saved(SAVED_PASS), driver_rc=0)
        self.assertEqual(rc, 0, line)
        self.assertTrue(line.startswith(
            "relay E-50 cell=claude-tui result=pass path=herdr deliveries T1=1 T2=1 AFTER_CLEAR=1 "), line)
        self.write_config(herdr=False)
        rc, line = self.judge_cli(self.saved(SAVED_PASS), driver_rc=0)
        self.assertEqual(rc, 0, line)
        self.assertIn(" result=pass path=tui ", line)

    def test_saved_runs_whose_bodies_never_arrived_still_fail(self):
        for name, finding in (("e50-t1-not-committed-20261101T214319Z.json", "T1 body not delivered"),
                              ("e50-socket-timeout-20261101T192638Z.json", "T1 delivery count unobserved")):
            with self.subTest(name=name):
                rc, line = self.judge_cli(self.saved(name), driver_rc=1)
                self.assertEqual(rc, 1, line)
                self.assertTrue(line.startswith("relay E-50 cell=claude-tui result=FAIL path=herdr "), line)
                self.assertIn(finding, line)

    def test_visible_clear_refusal_fails_with_the_reply_the_user_saw(self):
        report = self.saved(SAVED_PASS)
        row = report["scenarios"][0]
        row.update(status="fail", reason="assertion: text !clear refused by the server: " + CLEAR_REFUSAL,
                   assertions=[], failure_attribution={
                       "source": "assertion", "raw_reason": "text !clear refused by the server: " + CLEAR_REFUSAL},
                   clear_refusal={"stage": "clear_ack", "after_id": "100", "reply": {
                       "id": "101", "channel_id": SAVED_CHANNEL, "author": {"bot": True}, "content": CLEAR_REFUSAL}})
        row["marker_counts"] = {m: (0 if m.endswith(":AFTER_CLEAR]") else 1) for m in row["marker_counts"]}
        rc, line = self.judge_cli(report, driver_rc=1)
        self.assertEqual(rc, 1, line)
        self.assertTrue(line.startswith("relay E-50 cell=claude-tui result=FAIL path=herdr "), line)
        self.assertIn(CLEAR_REFUSAL, line)
        self.assertIn("AFTER_CLEAR body withheld after !clear (0 deliveries)", line)

    def test_unbound_or_incomplete_reports_fail(self):
        def classify(report):
            return turn_smoke.classify(
                report=report, scenario_id="E-50", cell="claude-tui", channel_id=SAVED_CHANNEL,
                run_id=self.saved(SAVED_PASS)["run_id"], driver_rc=0, log_counts={"cell_lines": 1},
                markers=turn_smoke.expected_markers(SCENARIOS, "E-50", self.saved(SAVED_PASS)["run_id"]),
            )["classification"]

        self.assertEqual(classify(self.saved(SAVED_PASS)), "PASS")
        mutations = {
            "report cell": lambda r: r.update(cell="codex-tui"),
            "report channel": lambda r: r.update(channel_id="333"),
            "report run": lambda r: r.update(run_id="other"),
            "row cell": lambda r: r["scenarios"][0].update(cell="codex-tui"),
            "row channel": lambda r: r["scenarios"][0].update(channel_id="333"),
            "row run": lambda r: r["scenarios"][0].update(run_id="other"),
            "two rows": lambda r: r["scenarios"].append(copy.deepcopy(r["scenarios"][0])),
            "coverage violation": lambda r: r.update(coverage_class_violations=["regression"]),
            "violations missing": lambda r: r.pop("coverage_class_violations"),
            "teardown error": lambda r: r["scenarios"][0].update(teardown_error="failed"),
            "dirty residue": lambda r: r["scenarios"][0].update(dirty_active_residue={"possible": True}),
        }
        for name, mutate in mutations.items():
            report = self.saved(SAVED_PASS)
            mutate(report)
            with self.subTest(mutation=name):
                self.assertEqual(classify(report), "FAIL")
        self.assertEqual(classify(None), "FAIL")

    def test_cell_path_names_the_configured_host_and_unreadable_config_is_not_a_guess(self):
        def path_cli():
            output = io.StringIO()
            with redirect_stdout(output), patch("sys.stderr", io.StringIO()):
                rc = turn_smoke.main(["cell-path", "--config", str(self.config), "--cell", "claude-tui",
                                      "--channel-id", SAVED_CHANNEL])
            return rc, output.getvalue().strip()

        self.assertEqual(path_cli(), (0, "herdr"))
        self.write_config(herdr=False)
        self.assertEqual(path_cli(), (0, "tui"))
        self.config.write_text("agents: []\n", encoding="utf-8")
        self.assertEqual(path_cli(), (2, ""))
        self.config.write_text("[", encoding="utf-8")
        self.assertEqual(path_cli(), (2, ""))


class StepTimings(unittest.TestCase):
    def test_report_and_verdict_name_each_step_duration(self):
        result = run_live_shaped("E-50", "claude-tui")
        self.assertEqual(result["status"], "pass", result.get("reason"))
        steps = [next(iter(step)) for step in next(
            s for s in driver.load_scenarios(SCENARIOS, cell="claude-tui") if s["id"] == "E-50")["steps"]]
        self.assertEqual([row["step"] for row in result["step_timings"]], [*steps, "final_checks"])
        self.assertEqual(result["step_timings"][0]["started_s"], 0.0)
        for earlier, later in zip(result["step_timings"], result["step_timings"][1:]):
            self.assertAlmostEqual(earlier["started_s"] + earlier["duration_s"], later["started_s"], places=1)

    def test_a_hung_read_shows_as_the_step_the_run_ended_in(self):
        # 11-01 19:26Z: the read after T1 hung 30 s and the run ended with a bare TimeoutError.
        original = LiveShapedDiscord.fetch_messages

        def hanging(client, channel_id, *, after_id=None, limit=100):
            if any("[E2E:E50:" in m["content"] and m["author"]["id"] != assertions.OUR_BOT_ID for m in client.messages):
                driver.time.sleep(30)
                raise TimeoutError("timed out")
            return original(client, channel_id, after_id=after_id, limit=limit)

        with patch.object(LiveShapedDiscord, "fetch_messages", hanging):
            result = run_live_shaped("E-50", "claude-tui")
        self.assertEqual(result["reason"], "TimeoutError: timed out")
        self.assertEqual([row["step"] for row in result["step_timings"]],
                         ["send_discord_prompt", "wait_for_discord_text"])
        self.assertGreaterEqual(result["step_timings"][-1]["duration_s"], 30.0)
        passed, detail = verdict(result, "E-50", "claude-tui", "turns-clean.dcserver.log")
        self.assertFalse(passed)
        self.assertRegex(detail, r"; steps: send_discord_prompt \d+\.\ds, wait_for_discord_text 3\d\.\ds$")


class GatewayReadyTest(unittest.TestCase):
    def setUp(self):
        self.details = []
        self.read_delay = 0.0
        self.calls = {"detail": 0, "read": 0}
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def reply(self, payload):
                body = json.dumps(payload).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                if self.path == "/api/health/detail":
                    fixture.calls["detail"] += 1
                    recovered, connected = (fixture.details.pop(0) if len(fixture.details) > 1
                                            else fixture.details[0])
                    self.reply({"fully_recovered": recovered, "providers": [
                        {"name": "claude", "runtime_role": "gateway", "connected": connected},
                        {"name": "codex", "runtime_role": "gateway", "connected": True}]})
                elif self.path.startswith("/api/discord/channels/222/messages?"):
                    fixture.calls["read"] += 1
                    time.sleep(fixture.read_delay)
                    self.reply([])
                else:
                    self.send_error(404)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(thread.join)
        self.addCleanup(self.server.shutdown)
        self.base_url = f"http://127.0.0.1:{self.server.server_port}"

    def wait(self, deadline_s):
        with redirect_stdout(io.StringIO()):
            return turn_smoke.wait_gateway_ready(base_url=self.base_url, cell="claude-tui", channel_id="222",
                                                 deadline_s=deadline_s, interval_s=0.01)

    def test_ready_only_after_recovery_connection_and_a_channel_read(self):
        self.details = [(False, False), (True, False), (True, True)]
        ready, detail = self.wait(10)
        self.assertTrue(ready, detail)
        self.assertEqual(self.calls, {"detail": 3, "read": 1})

    def test_a_gateway_that_never_connects_is_not_ready_at_the_deadline(self):
        self.details = [(True, False)]
        ready, detail = self.wait(0.3)
        self.assertFalse(ready)
        self.assertIn("gateway not connected: claude", detail)
        self.assertEqual(self.calls["read"], 0)

    def test_a_channel_read_that_outlasts_the_deadline_is_not_ready(self):
        self.details = [(True, True)]
        self.read_delay = 1.5
        ready, detail = self.wait(0.5)
        self.assertFalse(ready, detail)
        self.assertGreaterEqual(self.calls["read"], 1)

    def test_cli_exits_1_with_one_fail_line_when_not_ready(self):
        self.details = [(False, False)]
        output = io.StringIO()
        with redirect_stdout(output):
            rc = turn_smoke.main(["gateway-ready", "--base-url", self.base_url, "--cell", "claude-tui",
                                  "--channel-id", "222", "--deadline-s", "0.3"])
        self.assertEqual(rc, 1)
        lines = output.getvalue().splitlines()
        self.assertEqual([line for line in lines if line.startswith("gateway-ready FAIL")], [lines[-1]])
        self.assertIn("startup recovery not finished", lines[-1])


class CellResetTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.config = self.root / "cells.yaml"
        self.lease_dir = self.root / "leases"
        self.lease_dir.mkdir()
        self.lease_patch = patch.object(driver.lease, "DEFAULT_LEASE_DIR", self.lease_dir)
        self.lease_patch.start()
        self.addCleanup(self.lease_patch.stop)
        self.config.write_text("""agents:
  - id: adk-claude-tui-2-e2e
    channels: {claude: {id: '222'}}
  - id: adk-codex-tui-e2e
    channels: {codex: {id: '333'}}
""", encoding="utf-8")
        self.identities = {
            provider: dict(provider=provider, channel_id=channel, generation=7,
                           runtime="fixture", user_message_id=444,
                           turn_nonce=f"{provider}-episode", started_at="2026-10-25T19:02:21.431Z")
            for provider, channel in (("claude", 222), ("codex", 333))
        }
        self.inflight = {"222": True, "333": True}
        self.sessions = []
        self.cleared = set()
        self.calls = []
        self.inspects = 0
        self.change_on_inspect = 0
        self.change_provider = None
        self.change_on_release = False
        self.change_after_release = False
        self.release_response = None
        self.release_responses = {}
        self.lock_observations = []
        self.clear_content = "세션을 초기화했어요. 대기 메시지 0건을 비웠어요."
        self.inspect_override = False
        self.inspect_payload = None
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def reply(self, payload, status=200):
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(json.dumps(payload).encode())

            def do_GET(self):
                fixture.calls.append(("GET", self.path, None))
                if self.path.startswith("/api/turn-lease/"):
                    provider = self.path.split("/")[3]
                    if fixture.change_provider in (None, provider):
                        fixture.inspects += 1
                    if fixture.inspects == fixture.change_on_inspect and fixture.change_provider in (None, provider):
                        fixture.identities[provider]["turn_nonce"] = "successor"
                    value = fixture.inspect_payload if fixture.inspect_override else fixture.identities[provider]
                    self.reply(value)
                elif self.path == "/api/health/detail":
                    self.reply({"mailboxes": [
                        {"provider": provider, "channel_id": channel,
                         "agent_turn_status": "active" if fixture.identities[provider] else "idle",
                         "has_cancel_token": fixture.identities[provider] is not None,
                         "inflight_state_present": fixture.inflight[channel], "queue_depth": 0}
                        for provider, channel in (("claude", "222"), ("codex", "333"))
                    ]})
                elif self.path == "/api/sessions":
                    self.reply({"sessions": fixture.sessions})
                elif "/messages?" in self.path:
                    channel = self.path.split("/")[4]
                    cell = {"222": "claude-tui", "333": "codex-tui", "999": "claude-tui"}[channel]
                    fixture.lock_observations.append(("ack", driver.lease.lease_path_for(cell).exists()))
                    self.reply([{"id": "501", "channel_id": channel,
                                 "author": {"id": "999", "bot": True},
                                 "content": fixture.clear_content}])
                else:
                    self.reply({"error": "unexpected fixture GET"}, 404)

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                fixture.calls.append(("POST", self.path, body))
                if self.path == "/api/turn-lease/release":
                    provider = body["expected"]["provider"]
                    fixture.lock_observations.append(("release", driver.lease.lease_path_for(f"{provider}-tui").exists()))
                    if fixture.change_on_release:
                        fixture.identities[provider]["generation"] += 1
                    if body["expected"] != fixture.identities[provider]:
                        self.reply({"error": "lease changed; inspect again"}, 409)
                        return
                    fixture.identities[provider] = None
                    fixture.inflight[str(body["expected"]["channel_id"])] = False
                    if fixture.change_after_release:
                        fixture.identities[provider] = {**body["expected"], "turn_nonce": "successor"}
                    self.reply(fixture.release_responses.get(provider) or fixture.release_response
                               or {"released": True, "status": "operator_released"})
                elif self.path == "/api/discord/send":
                    cell = {"222": "claude-tui", "333": "codex-tui", "999": "claude-tui"}[body["channel_id"]]
                    fixture.lock_observations.append(("clear", driver.lease.lease_path_for(cell).exists()))
                    fixture.cleared.add(body["channel_id"])
                    fixture.inflight[body["channel_id"]] = False
                    fixture.sessions = [s for s in fixture.sessions if str(s.get("channel_id")) != body["channel_id"]]
                    self.reply({"message_id": "500"})
                else:
                    self.reply({"error": "unexpected fixture POST"}, 404)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(thread.join)
        self.addCleanup(self.server.shutdown)
        self.base_url = f"http://127.0.0.1:{self.server.server_port}"
        self.output = io.StringIO()

    def reset(self, cell="claude-tui", channel="222"):
        with redirect_stdout(self.output):
            turn_smoke.cell_reset(base_url=self.base_url, cell=cell, channel_id=channel, config_path=self.config,
                                  runtime_root=self.root)

    def preflight(self, cell="claude-tui", channel="222"):
        with redirect_stdout(self.output):
            return turn_smoke.main(["preflight", "--base-url", self.base_url, "--cell", cell,
                               "--channel-id", channel, "--queue-runtime-root", str(self.root)])

    def mutations(self):
        return [call for call in self.calls if call[0] == "POST"]

    def test_busy_residue_reset_then_real_preflight_idle_for_both_providers(self):
        for cell, channel in (("claude-tui", "222"), ("codex-tui", "333")):
            with self.subTest(cell=cell):
                expected = copy.deepcopy(self.identities[cell.split("-")[0]])
                self.assertEqual(self.preflight(cell, channel), 1)
                self.reset(cell, channel)
                self.assertEqual(self.preflight(cell, channel), 0)
                release, clear = self.mutations()[-2:]
                self.assertEqual(release[2]["expected"], expected)
                self.assertEqual(clear[2], {"channel_id": channel, "content": "!clear"})
        self.assertEqual(self.cleared, {"222", "333"})
        for action in ("allowlist", "inspect", "identity", "release", "!clear", "ack"):
            self.assertIn(action, self.output.getvalue())

    def test_outside_allowlist_refused_without_any_http(self):
        self.identities["claude"]["channel_id"] = 999
        with self.assertRaisesRegex(ValueError, "allowlist"):
            self.reset(channel="999")
        self.assertEqual(self.calls, [])

    def test_unreadable_or_missing_allowlist_refused_without_any_http(self):
        for text in (None, "[", "{}", "agents: []", "agents: [{id: adk-claude-tui-e2e}]"):
            with self.subTest(config=text):
                if text is None:
                    self.config.unlink()
                else:
                    self.config.write_text(text, encoding="utf-8")
                with self.assertRaises(Exception):
                    self.reset()
                self.assertEqual(self.calls, [])

    def test_identity_changes_between_inspects_prevent_release_and_clear(self):
        self.change_on_inspect = 2
        with self.assertRaisesRegex((ValueError, RuntimeError), "identity changed|lease changed"):
            self.reset()
        self.assertEqual(self.mutations(), [])

    def test_every_identity_axis_is_compared_before_release(self):
        for field, value in (("generation", 8), ("runtime", "other"), ("user_message_id", 445),
                             ("turn_nonce", "other"), ("started_at", "2026-10-26T00:00:00Z")):
            with self.subTest(field=field):
                expected = copy.deepcopy(self.identities["claude"])
                changed = {**expected, field: value}
                with patch.object(turn_smoke.cell_driver, "_read_api_json", side_effect=[(200, expected), (200, changed)]):
                    with self.assertRaisesRegex(ValueError, "identity changed"):
                        self.reset()
                self.assertEqual(self.mutations(), [])

    def test_server_cas_refusal_after_second_inspect_preserves_successor(self):
        self.change_on_release = True
        with self.assertRaises(Exception):
            self.reset()
        self.assertEqual(len(self.mutations()), 1)
        self.assertEqual(self.cleared, set())
        self.assertIsNotNone(self.identities["claude"])

    def test_successor_after_release_prevents_clear(self):
        self.change_after_release = True
        with self.assertRaisesRegex(ValueError, "identity changed"):
            self.reset()
        self.assertEqual(self.cleared, set())

    def test_nonrelease_response_does_not_clear(self):
        self.release_response = {"released": False, "status": "already_released"}
        with self.assertRaisesRegex(ValueError, "release"):
            self.reset()
        self.assertEqual(self.cleared, set())

    def test_idle_cell_preserves_warm_session_without_mutations(self):
        self.identities["claude"] = None
        self.inflight["222"] = False
        self.reset()
        self.assertEqual(self.mutations(), [])
        self.assertEqual(self.cleared, set())
        self.assertIn("reset not needed", self.output.getvalue())
        self.assertEqual(self.preflight(), 0)

    def test_null_lease_with_inflight_residue_only_clears_session(self):
        self.identities["claude"] = None
        self.reset()
        self.assertEqual([c[1] for c in self.mutations()], ["/api/discord/send"])
        self.assertEqual(self.preflight(), 0)

    def test_session_only_residue_triggers_one_clear(self):
        self.identities["claude"] = None
        self.inflight["222"] = False
        self.sessions = [{"channel_id": "222", "status": "turn_active"}]
        self.reset()
        self.assertEqual([c[1] for c in self.mutations()], ["/api/discord/send"])
        self.assertEqual(self.cleared, {"222"})
        self.assertEqual(self.sessions, [])

    def test_session_busy_predicate_preserves_e1_target_matching(self):
        workspace = driver.cell_workspace_substring("claude-tui")
        for payload, expected in (
            ([{"channel_id": "222", "status": "turn_active"}], True),
            ({"sessions": [{"channelId": "222", "status": "TURN_BUSY"}]}, True),
            ([{"channel_id": "999", "session_key": f"claude/host:{workspace}-t999", "status": "active"}], True),
            ([{"channel_id": "222", "status": "idle"}], False),
            ([{"channel_id": "999", "status": "turn_active"}, None], False),
        ):
            with self.subTest(payload=payload):
                reasons = turn_smoke.cell_session_busy_reasons(cell="claude-tui", channel_id="222", sessions_payload=payload)
                self.assertEqual(bool(reasons), expected)
        for payload in (None, {}, {"sessions": "unreadable"}):
            with self.subTest(invalid=payload), self.assertRaises(ValueError):
                turn_smoke.cell_session_busy_reasons(cell="claude-tui", channel_id="222", sessions_payload=payload)

    def test_foreign_driver_lease_skips_without_http_or_mutations(self):
        with driver.lease.acquire("foreign-driver", cell="claude-tui") as path:
            held = path.read_bytes()
            with self.assertRaisesRegex(RuntimeError, "skipped: foreign active turn on cell"):
                self.reset()
            self.assertEqual(path.read_bytes(), held)
        self.assertEqual(self.calls, [])
        self.assertEqual(self.mutations(), [])

    def test_driver_lease_is_held_through_clear_ack_then_released(self):
        self.reset()
        self.assertEqual(self.lock_observations, [("release", True), ("clear", True), ("ack", True)])
        self.assertFalse(driver.lease.lease_path_for("claude-tui").exists())

    def test_foreign_driver_cli_is_skip_and_retains_its_lease(self):
        state = self.root / "reset.json"
        with driver.lease.acquire("foreign-driver", cell="claude-tui") as path:
            held = path.read_bytes()
            with redirect_stdout(self.output):
                rc = turn_smoke.main(["cell-reset", "--base-url", self.base_url, "--config", str(self.config),
                                      "--cell", "claude-tui", "--state-output", str(state)])
            self.assertEqual(rc, 3)
            self.assertEqual(path.read_bytes(), held)
        self.assertEqual(self.calls, [])
        self.assertEqual(turn_smoke.reset_status(state, "claude-tui", "222")[0], 1)
        self.assertIn("skipped: foreign active turn on cell", self.output.getvalue())

    def test_codex_reset_failure_does_not_fail_claude_reset(self):
        self.change_provider, self.change_on_inspect = "codex", 2
        with redirect_stdout(self.output):
            rc = turn_smoke.main(["cell-reset", "--base-url", self.base_url, "--config", str(self.config),
                                  "--cell", "claude-tui", "--cell", "codex-tui"])
        self.assertEqual(rc, 0)
        self.assertEqual(self.cleared, {"222"})
        self.assertEqual([c[2]["expected"]["provider"] for c in self.mutations()
                          if c[1] == "/api/turn-lease/release"], ["claude"])

    def test_malformed_or_wrong_target_identity_never_mutates(self):
        for payload in ({}, [], {**self.identities["claude"], "provider": "codex"},
                        {**self.identities["claude"], "channel_id": 999}):
            with self.subTest(payload=payload):
                self.inspect_override, self.inspect_payload = True, payload
                with self.assertRaises(ValueError):
                    self.reset()
                self.assertEqual(self.mutations(), [])

    def test_clear_refusal_is_not_reset_success(self):
        self.clear_content = "세션을 초기화하지 못했어요: fixture refusal"
        with self.assertRaisesRegex(ValueError, "clear refused"):
            self.reset()

    def test_clear_ack_must_follow_this_clear(self):
        with patch.object(turn_smoke.cell_driver.discord.DiscordClient, "wait_for_message", return_value=(None, [])):
            with self.assertRaisesRegex(ValueError, "ack unavailable"):
                self.reset()

    def test_herdr_and_pipe_cells_have_no_reset_effects(self):
        for cell in ("claude-herdr", "codex-herdr", "claude-pipe", "codex-pipe"):
            self.reset(cell)
        self.assertEqual(self.calls, [])

    def test_tui_cell_mapped_to_herdr_has_no_reset_effects(self):
        with self.config.open("a", encoding="utf-8") as handle:
            handle.write("session_hosts: {herdr: {channels: {'222': local}}}\n")
        self.reset()
        self.assertEqual(self.calls, [])

    def test_cli_resets_cells_once_then_refreshes_preflight_snapshots(self):
        detail, sessions = self.root / "detail.json", self.root / "sessions.json"
        with redirect_stdout(self.output):
            rc = turn_smoke.main(["cell-reset", "--base-url", self.base_url, "--config", str(self.config),
                             "--cell", "claude-tui", "--cell", "codex-tui", "--cell", "claude-tui",
                             "--health-detail-output", str(detail), "--sessions-output", str(sessions)])
        self.assertEqual(rc, 0)
        self.assertEqual(len(self.mutations()), 4)
        self.assertEqual(json.loads(sessions.read_text()), {"sessions": []})
        self.assertTrue(all(row["agent_turn_status"] == "idle" for row in json.loads(detail.read_text())["mailboxes"]))
        self.assertEqual(self.calls[-2][1:], ("/api/health/detail", None))
        self.assertEqual(self.calls[-1][1:], ("/api/sessions", None))

    def test_cli_unreadable_allowlist_fails_closed(self):
        for text in ("[", "{}", "agents: []"):
            with self.subTest(config=text):
                self.config.write_text(text, encoding="utf-8")
                with redirect_stdout(self.output):
                    rc = turn_smoke.main(["cell-reset", "--base-url", self.base_url, "--config", str(self.config),
                                     "--cell", "claude-tui"])
                self.assertEqual(rc, 2)
                self.assertEqual(self.calls, [])

    def deploy_round_trip(self, *, run_all=False):
        deploy = Path(os.environ.get("DEPLOY_SH_OVERRIDE", ROOT / "scripts/deploy-release.sh")).read_text()
        names = ["_release_config_path", "_post_deploy_smoke_check_relay_round_trip",
                 "_post_deploy_smoke_check_durable_record",
                 "_post_deploy_smoke_run_turn_scenario", "_post_deploy_smoke_check_turn_scenarios"]
        functions = "\n".join(name + "() {\n" + deploy.split(name + "() {\n", 1)[1].split("\n}\n", 1)[0]
                              + "\n}" for name in names)
        release = self.root / "release"
        (release / "config").mkdir(parents=True)
        (release / "config/agentdesk.yaml").write_bytes(self.config.read_bytes())
        (self.root / "detail.json").write_text(json.dumps({"mailboxes": [
            {"provider": "claude", "channel_id": "222", "has_cancel_token": True,
             "active_user_message_id": 444, "agent_turn_status": "active"}]}))
        (self.root / "sessions.json").write_text(json.dumps({"sessions": [
            {"provider": "claude", "channel_id": "222", "status": "active"}]}))
        (self.root / "health.json").write_text('{"cluster_standby": false}')
        (release / "logs").mkdir()
        (release / "logs/dcserver.stdout.log").write_text("fixture\n")
        (self.root / "fake_driver.py").write_text('''import argparse, json, sys
from pathlib import Path
p = argparse.ArgumentParser()
for name in ("--filter", "--cell", "--output"):
    p.add_argument(name)
a, _ = p.parse_known_args()
with (Path(__file__).parent / "injected").open("a") as f:
    f.write(a.filter + "\\n")
out = Path(a.output)
out.mkdir(parents=True, exist_ok=True)
(out / f"report.{a.cell}.json").write_text(json.dumps({"scenarios": [{"id": a.filter,
    "durable_record_probe": {"status": "evaluated"}}]}))
''')
        script = """set -euo pipefail
REPO=$1; fixture=$2; REL_PORT=$3; ADK_DEFAULT_LOOPBACK=127.0.0.1
SCRIPT_DIR=$REPO/scripts
unset AGENTDESK_CONFIG
ADK_REL=$fixture/release
POST_DEPLOY_SMOKE_RELAY_CELL=claude-tui; POST_DEPLOY_SMOKE_CODEX_TURNS_CELL=codex-tui
POST_DEPLOY_SMOKE_READY=true; POST_DEPLOY_SMOKE_RELAY_CHANNEL_ID=''
POST_DEPLOY_SMOKE_HEALTH_BODY=$fixture/health.json
POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY=$fixture/detail.json
POST_DEPLOY_SMOKE_SESSIONS_BODY=$fixture/sessions.json
POST_DEPLOY_SMOKE_EVIDENCE=$fixture/evidence.log
POST_DEPLOY_SMOKE_STAMP=fixture; POST_DEPLOY_SMOKE_TMP_DIR=$fixture
POST_DEPLOY_SMOKE_CLAUDE_TURNS_DEADLINE_S=120; POST_DEPLOY_SMOKE_CODEX_TURNS_DEADLINE_S=150
POST_DEPLOY_SMOKE_E35_DEADLINE_S=900; POST_DEPLOY_SMOKE_LOG_PATH=$ADK_REL/logs/dcserver.stdout.log
POST_DEPLOY_SMOKE_LOG_FINGERPRINT_CAP=4096; POST_DEPLOY_SMOKE_TURN_COVERAGE=''
POST_DEPLOY_SMOKE_GATEWAY_READY_S=120; POST_DEPLOY_SMOKE_RELAY_PATH=''
_post_deploy_smoke_note() { printf '%s\\n' "$1"; }
_post_deploy_smoke_fail() { printf 'FAIL: %s\\n' "$1"; }
_post_deploy_smoke_resolve_cluster_standby() { printf false; }
_post_deploy_smoke_resolve_health_module() { printf true; }
_post_deploy_smoke_log_identity_and_size() { printf 'fixture 0\\n'; }
_post_deploy_smoke_log_head_fingerprint() { printf fixture; }
python3() {
    if [ "${1:-}" = scripts/e2e/run_tui_relay.py ]; then
        shift; command python3 "$fixture/fake_driver.py" "$@"; return
    fi
    case "${1:-}" in
        scripts/e2e/post_deploy_turn_smoke.py|*/scripts/e2e/post_deploy_turn_smoke.py)
            shift
            command python3 -c 'import sys; from pathlib import Path; sys.path.insert(0, str(Path(sys.argv[1]) / "scripts/e2e")); import post_deploy_turn_smoke as smoke; smoke.cell_driver.lease.DEFAULT_LEASE_DIR = Path(sys.argv[2]) / "leases"; a = sys.argv[3:]; print("relay fixture result=pass") if a[0] == "judge" else None; sys.exit(0 if a[0] in ("judge", "gateway-ready") else smoke.main(a))' "$REPO" "$fixture" "$@"
            return ;;
    esac
    command python3 "$@"
}
""" + functions + """
rc=0
_post_deploy_smoke_check_relay_round_trip || rc=1
if [ "$4" = all ]; then
    _post_deploy_smoke_check_durable_record || rc=1
    _post_deploy_smoke_check_turn_scenarios || rc=1
    printf 'COVERAGE=%s\\n' "$POST_DEPLOY_SMOKE_TURN_COVERAGE"
fi
exit "$rc"
"""
        return subprocess.run(["bash", "-c", script, "fixture", str(ROOT), str(self.root),
                               str(self.server.server_port), "all" if run_all else "e1"],
                              capture_output=True, text=True, timeout=30)

    def test_deploy_resets_both_cells_before_first_idle_guard(self):
        with patch.dict(os.environ, {"AGENTDESK_CONFIG": str(self.root / "ambient-unused.yaml")}):
            result = self.deploy_round_trip()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((self.root / "injected").exists(), result.stdout + result.stderr)
        self.assertEqual(len(self.mutations()), 4)
        self.assertEqual(self.cleared, {"222", "333"})

    def test_deploy_reset_refusal_stops_injection(self):
        self.change_on_inspect = 2
        result = self.deploy_round_trip()
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse((self.root / "injected").exists())
        self.assertFalse(any(c[1] == "/api/discord/send" and c[2]["channel_id"] == "222" for c in self.mutations()))

    def test_codex_partial_reset_failure_preserves_all_claude_scenarios(self):
        self.release_responses["codex"] = {"released": False, "status": "already_released"}
        result = self.deploy_round_trip(run_all=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertEqual((self.root / "injected").read_text().splitlines(), ["E-1", "E-35", "E-50"])
        self.assertIn("COVERAGE=E-50=pass E-51=FAIL", result.stdout)
        self.assertIsNone(self.identities["codex"])
        self.assertFalse(self.inflight["333"])

    def test_claude_reset_failure_does_not_block_codex_scenario(self):
        self.change_provider, self.change_on_inspect = "claude", 2
        result = self.deploy_round_trip(run_all=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertEqual((self.root / "injected").read_text().splitlines(), ["E-51"])
        self.assertIn("COVERAGE=E-50=skipped E-51=pass", result.stdout)
        findings = [line for line in result.stdout.splitlines() if line.startswith("FAIL:")]
        self.assertEqual(len(findings), 1, result.stdout)
        self.assertIn("relay E-1: cell-reset refused", findings[0])
        self.assertIn("relay E-50=skipped: blocked by E-1 FAIL", result.stdout)

    def test_foreign_driver_deploy_logs_existing_skip_without_touching_its_cell(self):
        with driver.lease.acquire("foreign-driver", cell="claude-tui") as path:
            held = path.read_bytes()
            result = self.deploy_round_trip()
            self.assertEqual(path.read_bytes(), held)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("relay E-1=skipped: foreign active turn on cell", result.stdout)
        self.assertFalse((self.root / "injected").exists())
        self.assertFalse(any("/claude/" in c[1] for c in self.calls))

    def test_reset_status_missing_malformed_or_wrong_channel_fails_closed(self):
        state = self.root / "reset.json"
        for text in (None, "[", "{}", '{"codex-tui": {"status":"ready","reason":"ok","channel_id":"999"}}'):
            with self.subTest(state=text):
                if text is not None:
                    state.write_text(text)
                with redirect_stdout(self.output):
                    rc = turn_smoke.main(["preflight", "--base-url", self.base_url, "--cell", "codex-tui",
                                          "--channel-id", "333", "--reset-state", str(state)])
                self.assertEqual(rc, 4)
                self.assertEqual(self.calls, [])

    def test_normal_preflight_never_mutates(self):
        self.assertEqual(self.preflight(), 1)
        self.assertEqual(self.mutations(), [])



if __name__ == "__main__":
    unittest.main()
