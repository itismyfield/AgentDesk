"""Post-deploy `!clear` + continuous-turn smoke: driver collection and verdicts.

The E-50/E-51 YAML runs through the real ``run_tui_relay.run_scenario`` against
a fake Discord view shaped like the 2026-10-03 live records, and the resulting
report row plus a recorded-shape dcserver log feed the production judge.
"""

from __future__ import annotations

import itertools
import sys
import unittest
from argparse import Namespace
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
PROVIDER_BOT = "1479000000000000001"
CLEARED = "세션을 초기화했어요."


class LiveShapedDiscord:
    """Discord as the live records saw it: one reply per prompt unless told otherwise.

    ``after_clear`` is ``"deliver"``, ``"withhold"`` (only the completion chrome
    lands, as in the Claude record) or ``"duplicate"``.
    """

    base_url = "http://offline.invalid"

    def __init__(self, *, after_clear: str = "deliver"):
        self.after_clear = after_clear
        self.ids = itertools.count(1_000)
        self.messages: list[dict] = []

    def _post(self, content: str, *, author: str) -> dict:
        message = {
            "id": str(next(self.ids)),
            "content": content,
            "type": 0,
            "author": {"id": author, "bot": True},
        }
        self.messages.append(message)
        return message

    def send_control(self, channel_id, content):  # noqa: ARG002
        return self._post(content, author=assertions.OUR_BOT_ID)

    def send(self, channel_id, content):  # noqa: ARG002
        sent = self._post(content, author=assertions.OUR_BOT_ID)
        if content.startswith("!clear"):
            self._post(CLEARED, author=PROVIDER_BOT)
            return sent
        marker = content[content.index("[E2E:") : content.index("]") + 1]
        if marker.endswith(":AFTER_CLEAR]") and self.after_clear == "withhold":
            self._post("✅ 완료", author=PROVIDER_BOT)
        elif marker.endswith(":AFTER_CLEAR]") and self.after_clear == "duplicate":
            self._post(marker, author=PROVIDER_BOT)
            self._post(f"🆕 새 세션 시작\n\n{marker}", author=PROVIDER_BOT)
        else:
            self._post(marker, author=PROVIDER_BOT)
        return sent

    def fetch_messages(self, channel_id, *, after_id=None, limit=100):  # noqa: ARG002
        floor = int(after_id or 0)
        return [dict(m) for m in self.messages if int(m["id"]) > floor][:limit]

    def wait_for_message(self, channel_id, *, predicate, after_id, timeout_s, debug_label):  # noqa: ARG002
        observed = self.fetch_messages(channel_id, after_id=after_id)
        return next((m for m in observed if predicate(m)), None), observed


def run_live_shaped(scenario_id: str, cell: str, *, after_clear: str = "deliver") -> dict:
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
            scenario, args=args, run_id=RUN_ID, client=LiveShapedDiscord(after_clear=after_clear)
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


if __name__ == "__main__":
    unittest.main()
