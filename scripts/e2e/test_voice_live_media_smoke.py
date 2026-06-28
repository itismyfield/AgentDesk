#!/usr/bin/env python3
"""Unit tests for the opt-in live Discord voice media smoke runner."""

from __future__ import annotations

import asyncio
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

import run_voice_live_media_smoke as smoke  # noqa: E402


def _args(*extra: str):
    return smoke.parse_args(["--allow-live-discord", "--report", "target/test-live-report.json", *extra])


def _env(**overrides: str) -> dict[str, str]:
    env = {
        "ADK_VOICE_LIVE_MEDIA_SMOKE": "1",
        "ADK_VOICE_LIVE_SAFETY_ACK": smoke.SAFETY_ACK,
        "ADK_VOICE_LIVE_TEST_GUILD_ID": "111",
        "ADK_VOICE_LIVE_TEST_VOICE_CHANNEL_ID": "222",
        "ADK_VOICE_LIVE_TEST_TEXT_CHANNEL_ID": "333",
        "ADK_VOICE_LIVE_CONFIRM_GUILD_ID": "111",
        "ADK_VOICE_LIVE_CONFIRM_VOICE_CHANNEL_ID": "222",
        "ADK_VOICE_LIVE_AGENTDESK_BOT_ID": "444",
        "ADK_VOICE_LIVE_SPEAKER_BOT_TOKEN": "speaker-token",
        "ADK_VOICE_LIVE_AGENT_ID": "project-agentdesk",
        "ADK_VOICE_LIVE_REAL_PROVIDER_CONTACTED": "true",
    }
    env.update(overrides)
    return env


def _event(event_type: str, timestamp_ms: int, payload: dict[str, object]) -> dict[str, object]:
    return {
        "event_type": event_type,
        "channel_id": payload.get("voice_channel_id") or payload.get("channel_id") or 222,
        "provider": "voice",
        "timestamp_ms": timestamp_ms,
        "payload": payload,
    }


class LiveVoiceMediaConfigTests(unittest.TestCase):
    def test_refuses_without_live_env_flag(self):
        env = _env()
        env.pop("ADK_VOICE_LIVE_MEDIA_SMOKE")

        with self.assertRaisesRegex(smoke.ConfigError, "ADK_VOICE_LIVE_MEDIA_SMOKE=1"):
            smoke.load_config(_args(), env)

    def test_refuses_without_cli_live_flag(self):
        args = smoke.parse_args(["--report", "target/test-live-report.json"])

        with self.assertRaisesRegex(smoke.ConfigError, "--allow-live-discord"):
            smoke.load_config(args, _env())

    def test_refuses_mismatched_confirmed_voice_channel(self):
        env = _env(ADK_VOICE_LIVE_CONFIRM_VOICE_CHANNEL_ID="999")

        with self.assertRaisesRegex(smoke.ConfigError, "CONFIRM_VOICE_CHANNEL_ID"):
            smoke.load_config(_args(), env)

    def test_refuses_same_speaker_and_agentdesk_bot_id(self):
        env = _env(ADK_VOICE_LIVE_SPEAKER_BOT_ID="444")

        with self.assertRaisesRegex(smoke.ConfigError, "different from AgentDesk"):
            smoke.load_config(_args(), env)

    def test_reads_speaker_token_from_file(self):
        with tempfile.TemporaryDirectory() as temp:
            token_path = Path(temp) / "token.txt"
            token_path.write_text("file-token\n", encoding="utf-8")
            env = _env(
                ADK_VOICE_LIVE_SPEAKER_BOT_TOKEN="",
                ADK_VOICE_LIVE_SPEAKER_BOT_TOKEN_FILE=str(token_path),
            )

            config = smoke.load_config(_args(), env)

        self.assertEqual(config.speaker_bot_token, "file-token")


class LiveVoiceMediaReportTests(unittest.TestCase):
    def setUp(self):
        self.config = smoke.load_config(_args(), _env())

    def test_build_scenario_report_passes_with_flight_and_latency_evidence(self):
        events = [
            _event(
                "voice_flight_event",
                1_000,
                {
                    "route": "queued",
                    "voice_channel_id": 222,
                    "control_channel_id": 333,
                    "utterance_id": "utt-1",
                    "stt_mode": "file",
                    "stt_latency_ms": 51,
                    "transcript_chars": 14,
                },
            ),
            _event(
                "voice_flight_event",
                1_100,
                {
                    "route": "foreground_speak",
                    "voice_channel_id": 222,
                    "utterance_id": "utt-1",
                    "foreground_decision": "speak",
                    "tts_chars": 21,
                },
            ),
            _event(
                "voice_latency_turn",
                1_200,
                {
                    "channel_id": 222,
                    "utterance_id": "utt-1",
                    "stt_ms": 51,
                    "agent_ms": 7,
                    "tts_synth_ms": 42,
                    "first_audio_out_ms": 18,
                    "tts_play_ms": 18,
                    "total_ms": 118,
                    "recorded_at_ms": 1_200,
                },
            ),
        ]
        evidence = smoke.ScenarioEvidence(
            scenario_id=smoke.SCENARIOS[0].scenario_id,
            started_at_ms=900,
            completed_at_ms=1_300,
            events=events,
            cleanup_evidence={"status": "passed", "raw_failure_reasons": []},
            timing_stages=smoke.TimingStages(evidence_wait_ms=300),
        )

        report = smoke.build_scenario_report(smoke.SCENARIOS[0], evidence, self.config)

        self.assertEqual(report["status"], "passed")
        self.assertEqual(report["utterance_id"], "utt-1")
        self.assertEqual(report["media_receive_counters"]["stt_events"], 1)
        self.assertEqual(report["voice_latency_turn"]["utterance_id"], "utt-1")

    def test_build_scenario_report_fails_missing_latency(self):
        events = [
            _event(
                "voice_flight_event",
                1_000,
                {
                    "route": "queued",
                    "voice_channel_id": 222,
                    "utterance_id": "utt-1",
                    "stt_mode": "file",
                },
            ),
            _event(
                "voice_flight_event",
                1_100,
                {
                    "route": "foreground_speak",
                    "voice_channel_id": 222,
                    "utterance_id": "utt-1",
                },
            ),
        ]
        evidence = smoke.ScenarioEvidence(
            scenario_id=smoke.SCENARIOS[0].scenario_id,
            started_at_ms=900,
            completed_at_ms=1_300,
            events=events,
            cleanup_evidence={"status": "passed", "raw_failure_reasons": []},
            timing_stages=smoke.TimingStages(),
        )

        report = smoke.build_scenario_report(smoke.SCENARIOS[0], evidence, self.config)

        self.assertEqual(report["status"], "failed")
        self.assertIn("voice_latency_turn metric was not emitted", report["raw_failure_reasons"])

    def test_build_scenario_report_rejects_latency_from_unrelated_utterance(self):
        events = [
            _event(
                "voice_flight_event",
                1_000,
                {
                    "route": "queued",
                    "voice_channel_id": 222,
                    "utterance_id": "utt-current",
                    "stt_mode": "file",
                },
            ),
            _event(
                "voice_flight_event",
                1_100,
                {
                    "route": "foreground_speak",
                    "voice_channel_id": 222,
                    "utterance_id": "utt-current",
                },
            ),
            _event(
                "voice_latency_turn",
                1_200,
                {
                    "channel_id": 222,
                    "utterance_id": "utt-previous",
                    "total_ms": 118,
                    "recorded_at_ms": 1_200,
                },
            ),
        ]
        evidence = smoke.ScenarioEvidence(
            scenario_id=smoke.SCENARIOS[0].scenario_id,
            started_at_ms=900,
            completed_at_ms=1_300,
            events=events,
            cleanup_evidence={"status": "passed", "raw_failure_reasons": []},
            timing_stages=smoke.TimingStages(),
        )

        report = smoke.build_scenario_report(smoke.SCENARIOS[0], evidence, self.config)

        self.assertEqual(report["status"], "failed")
        self.assertIsNone(report["voice_latency_turn"])
        self.assertIn(
            "voice_latency_turn metric did not match observed utterance_id",
            report["raw_failure_reasons"],
        )

    def test_barge_in_report_requires_cancellation_evidence(self):
        spec = smoke.SCENARIOS[1]
        events = [
            _event(
                "voice_flight_event",
                1_000,
                {
                    "route": "queued",
                    "voice_channel_id": 222,
                    "utterance_id": "utt-setup",
                    "stt_mode": "file",
                },
            ),
            _event(
                "voice_flight_event",
                1_100,
                {
                    "route": "explicit_stop",
                    "voice_channel_id": 222,
                    "utterance_id": "utt-stop",
                    "stt_mode": "file",
                    "cancelled": True,
                    "cancel_source": "voice_barge_in_explicit_stop",
                },
            ),
        ]
        evidence = smoke.ScenarioEvidence(
            scenario_id=spec.scenario_id,
            started_at_ms=900,
            completed_at_ms=1_300,
            events=events,
            cleanup_evidence={"status": "passed", "raw_failure_reasons": []},
            timing_stages=smoke.TimingStages(),
        )

        report = smoke.build_scenario_report(spec, evidence, self.config)

        self.assertEqual(report["status"], "passed")
        self.assertEqual(report["playback_observation"], "cancelled")

    def test_validate_report_rejects_failed_scenario(self):
        report = smoke.build_top_level_report(
            self.config,
            [{"scenario_id": "normal-short-live-media", "status": "failed", "raw_failure_reasons": ["x"]}],
            {"library": "discord.py"},
            {},
            started_at_ms=1,
            completed_at_ms=2,
        )

        with self.assertRaisesRegex(ValueError, "voice live media smoke failures"):
            smoke.validate_report(report)


class LiveVoiceMediaCleanupTests(unittest.TestCase):
    def setUp(self):
        self.config = smoke.load_config(_args(), _env())

    def test_cleanup_without_any_probe_fails_closed(self):
        config = smoke.dataclasses.replace(self.config, agent_id=None, cleanup_command=None)

        result = asyncio.run(
            smoke.run_cleanup_check(
                config,
                smoke.SCENARIOS[0],
                [],
                smoke.TimingStages(),
            )
        )

        self.assertEqual(result["status"], "failed")
        self.assertIn("cleanup verification was not configured", result["raw_failure_reasons"])

    def test_cleanup_unavailable_fails_closed(self):
        unavailable = {
            "status": "unavailable",
            "source": "agent_turn_api",
            "raw_failure_reasons": ["connection refused"],
        }

        with mock.patch.object(smoke, "_cleanup_from_agent_turn", return_value=unavailable):
            result = asyncio.run(
                smoke.run_cleanup_check(
                    self.config,
                    smoke.SCENARIOS[0],
                    [],
                    smoke.TimingStages(),
                )
            )

        self.assertEqual(result["status"], "failed")
        self.assertIn("connection refused", result["raw_failure_reasons"])

    def test_cleanup_requires_explicit_false_for_voice_lifecycle_flags(self):
        failures = smoke._cleanup_flag_failures(
            {
                "stale_voice_session": False,
                "playback_task_active": False,
                "foreground_call_active": False,
            },
            source="cleanup command",
        )

        self.assertEqual(
            failures,
            ["cleanup command did not prove voice_turn_link_active=false"],
        )


if __name__ == "__main__":
    unittest.main()
