#!/usr/bin/env python3
"""Run the opt-in unattended live Discord voice media smoke.

This lane uses a separate Discord speaker bot to stream generated speech into a
configured test voice channel. AgentDesk must already be configured to join the
same channel, normally through voice auto-join or an operator-issued
``/voice join``. The runner refuses to start unless explicit live-test
environment variables acknowledge that it will touch real Discord resources.

Normal CI should exercise this file through its unit tests only. A real run
requires optional runtime dependencies: ``discord.py`` with voice support,
``ffmpeg``, and either ``edge-tts`` or an explicit TTS command/audio fixtures.
"""

from __future__ import annotations

import argparse
import asyncio
import dataclasses
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any


DEFAULT_REPORT = Path("target/voice-live-media-smoke-report.json")
DEFAULT_API_BASE_URL = "http://127.0.0.1:8791"
DEFAULT_OBSERVABILITY_JSONL = Path.home() / ".adk" / "release" / "logs" / "observability-events.jsonl"
SAFETY_ACK = "I_UNDERSTAND_THIS_USES_LIVE_DISCORD_VOICE"
ENV_PREFIX = "ADK_VOICE_LIVE_"


class ConfigError(ValueError):
    """Raised when the opt-in live safety/config contract is not satisfied."""


class LiveSmokeError(RuntimeError):
    """Raised when the live run cannot proceed or evidence cannot be collected."""


@dataclasses.dataclass(frozen=True)
class ScenarioSpec:
    scenario_id: str
    prompt_env: str
    default_prompt: str
    expected_routes: tuple[str, ...]
    require_latency: bool = True
    require_cancel: bool = False
    setup_prompt_env: str | None = None
    default_setup_prompt: str | None = None
    setup_expected_routes: tuple[str, ...] = ()
    barge_in_delay_s: float = 1.5
    settle_s: float = 1.0

    def prompt(self, env: Mapping[str, str]) -> str:
        return env.get(self.prompt_env, self.default_prompt)

    def setup_prompt(self, env: Mapping[str, str]) -> str | None:
        if self.setup_prompt_env is None:
            return None
        return env.get(self.setup_prompt_env, self.default_setup_prompt or "")


SCENARIOS: tuple[ScenarioSpec, ...] = (
    ScenarioSpec(
        scenario_id="normal-short-live-media",
        prompt_env="ADK_VOICE_LIVE_PROMPT_NORMAL",
        default_prompt="AgentDesk live media smoke: answer briefly with the word ready.",
        expected_routes=("queued", "foreground_speak"),
    ),
    ScenarioSpec(
        scenario_id="barge-in-while-tts-active-live-media",
        prompt_env="ADK_VOICE_LIVE_PROMPT_BARGE",
        default_prompt="AgentDesk stop. Live media smoke barge-in follow-up.",
        expected_routes=("explicit_stop",),
        require_latency=False,
        require_cancel=True,
        setup_prompt_env="ADK_VOICE_LIVE_PROMPT_BARGE_SETUP",
        default_setup_prompt=(
            "AgentDesk live media smoke: give a long spoken answer so a second "
            "speaker turn can interrupt playback."
        ),
        setup_expected_routes=("queued",),
    ),
    ScenarioSpec(
        scenario_id="long-answer-background-handoff-summary-live-media",
        prompt_env="ADK_VOICE_LIVE_PROMPT_BACKGROUND",
        default_prompt=(
            "AgentDesk live media smoke: run a longer background check and speak "
            "a short summary when it finishes."
        ),
        expected_routes=("queued", "background_handoff"),
    ),
)


@dataclasses.dataclass(frozen=True)
class LiveSmokeConfig:
    report_path: Path
    api_base_url: str
    api_token: str | None
    observability_jsonl: Path
    guild_id: int
    voice_channel_id: int
    text_channel_id: int
    agentdesk_bot_id: int
    speaker_bot_token: str
    agent_id: str | None
    provider_identity: str | None
    real_provider_contacted: bool | None
    test_identity: str
    wait_timeout_s: float
    poll_interval_s: float
    discord_connect_timeout_s: float
    audio_dir: Path | None
    keep_audio: bool
    tts_command: str | None
    tts_voice: str
    macos_say_voice: str | None
    cleanup_command: str | None
    require_cleanup_check: bool
    allow_live_discord: bool

    @property
    def evidence_channel_ids(self) -> set[int]:
        return {self.voice_channel_id, self.text_channel_id}


@dataclasses.dataclass
class TimingStages:
    speaker_join_ms: int = 0
    audio_generation_ms: int = 0
    stream_ms: int = 0
    evidence_wait_ms: int = 0
    cleanup_check_ms: int = 0

    def to_json(self) -> dict[str, int]:
        return dataclasses.asdict(self)


@dataclasses.dataclass
class ScenarioEvidence:
    scenario_id: str
    started_at_ms: int
    completed_at_ms: int
    events: list[dict[str, Any]]
    cleanup_evidence: dict[str, Any]
    timing_stages: TimingStages


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--allow-live-discord",
        action="store_true",
        help="Required in addition to env safety flags before live Discord is touched.",
    )
    parser.add_argument(
        "--report",
        default=os.environ.get("ADK_VOICE_LIVE_REPORT", str(DEFAULT_REPORT)),
        help="Path for the machine-readable live smoke report.",
    )
    parser.add_argument(
        "--api-base-url",
        default=os.environ.get("ADK_API_BASE_URL", DEFAULT_API_BASE_URL),
        help="AgentDesk API base URL used for observability and optional cleanup probes.",
    )
    parser.add_argument(
        "--observability-jsonl",
        default=os.environ.get("ADK_OBSERVABILITY_EVENTS_PATH", str(DEFAULT_OBSERVABILITY_JSONL)),
        help="Fallback structured-event JSONL path.",
    )
    parser.add_argument(
        "--dry-run-config",
        action="store_true",
        help="Validate the live safety/config contract and exit before importing Discord libraries.",
    )
    parser.add_argument(
        "--wait-timeout-s",
        type=float,
        default=float(os.environ.get("ADK_VOICE_LIVE_WAIT_TIMEOUT_S", "240")),
        help="Per-scenario evidence wait timeout.",
    )
    parser.add_argument(
        "--poll-interval-s",
        type=float,
        default=float(os.environ.get("ADK_VOICE_LIVE_POLL_INTERVAL_S", "2")),
        help="Evidence polling interval.",
    )
    parser.add_argument(
        "--discord-connect-timeout-s",
        type=float,
        default=float(os.environ.get("ADK_VOICE_LIVE_DISCORD_CONNECT_TIMEOUT_S", "30")),
        help="Speaker bot voice connect timeout.",
    )
    parser.add_argument(
        "--audio-dir",
        default=os.environ.get("ADK_VOICE_LIVE_AUDIO_DIR"),
        help="Directory for generated audio. A temp dir is used when omitted.",
    )
    parser.add_argument(
        "--keep-audio",
        action="store_true",
        default=os.environ.get("ADK_VOICE_LIVE_KEEP_AUDIO") == "1",
        help="Keep generated audio files after the run.",
    )
    parser.add_argument(
        "--tts-command",
        default=os.environ.get("ADK_VOICE_LIVE_TTS_COMMAND"),
        help=(
            "Optional shell command template that writes speech audio. "
            "Supports {text}, {output}, and {voice}."
        ),
    )
    return parser.parse_args(argv)


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def _require_env(env: Mapping[str, str], name: str) -> str:
    value = env.get(name, "").strip()
    if not value:
        raise ConfigError(f"{name} must be set for live Discord voice media smoke")
    return value


def _parse_int(value: str, name: str) -> int:
    try:
        parsed = int(value)
    except ValueError as error:
        raise ConfigError(f"{name} must be an integer id; got {value!r}") from error
    if parsed <= 0:
        raise ConfigError(f"{name} must be a positive integer id; got {value!r}")
    return parsed


def _parse_bool_or_none(value: str | None) -> bool | None:
    if value is None or not value.strip():
        return None
    normalized = value.strip().lower()
    if normalized in {"1", "true", "yes", "real"}:
        return True
    if normalized in {"0", "false", "no", "controlled"}:
        return False
    raise ConfigError(f"ADK_VOICE_LIVE_REAL_PROVIDER_CONTACTED has unsupported value {value!r}")


def _read_token(env: Mapping[str, str]) -> str:
    token = env.get("ADK_VOICE_LIVE_SPEAKER_BOT_TOKEN", "").strip()
    token_file = env.get("ADK_VOICE_LIVE_SPEAKER_BOT_TOKEN_FILE", "").strip()
    if token and token_file:
        raise ConfigError(
            "set only one of ADK_VOICE_LIVE_SPEAKER_BOT_TOKEN or "
            "ADK_VOICE_LIVE_SPEAKER_BOT_TOKEN_FILE"
        )
    if token:
        return token
    if token_file:
        path = Path(token_file).expanduser()
        try:
            return path.read_text(encoding="utf-8").strip()
        except OSError as error:
            raise ConfigError(f"failed to read speaker bot token file {path}: {error}") from error
    raise ConfigError(
        "ADK_VOICE_LIVE_SPEAKER_BOT_TOKEN or ADK_VOICE_LIVE_SPEAKER_BOT_TOKEN_FILE must be set"
    )


def load_config(args: argparse.Namespace, env: Mapping[str, str] | None = None) -> LiveSmokeConfig:
    env = env or os.environ
    if env.get("ADK_VOICE_LIVE_MEDIA_SMOKE") != "1":
        raise ConfigError("ADK_VOICE_LIVE_MEDIA_SMOKE=1 is required")
    if env.get("ADK_VOICE_LIVE_SAFETY_ACK") != SAFETY_ACK:
        raise ConfigError(f"ADK_VOICE_LIVE_SAFETY_ACK must equal {SAFETY_ACK!r}")
    if not args.allow_live_discord:
        raise ConfigError("--allow-live-discord is required for live Discord resources")

    guild_id = _parse_int(_require_env(env, "ADK_VOICE_LIVE_TEST_GUILD_ID"), "ADK_VOICE_LIVE_TEST_GUILD_ID")
    voice_channel_id = _parse_int(
        _require_env(env, "ADK_VOICE_LIVE_TEST_VOICE_CHANNEL_ID"),
        "ADK_VOICE_LIVE_TEST_VOICE_CHANNEL_ID",
    )
    text_channel_id = _parse_int(
        _require_env(env, "ADK_VOICE_LIVE_TEST_TEXT_CHANNEL_ID"),
        "ADK_VOICE_LIVE_TEST_TEXT_CHANNEL_ID",
    )
    agentdesk_bot_id = _parse_int(
        _require_env(env, "ADK_VOICE_LIVE_AGENTDESK_BOT_ID"),
        "ADK_VOICE_LIVE_AGENTDESK_BOT_ID",
    )

    confirm_guild = _parse_int(
        _require_env(env, "ADK_VOICE_LIVE_CONFIRM_GUILD_ID"),
        "ADK_VOICE_LIVE_CONFIRM_GUILD_ID",
    )
    confirm_voice = _parse_int(
        _require_env(env, "ADK_VOICE_LIVE_CONFIRM_VOICE_CHANNEL_ID"),
        "ADK_VOICE_LIVE_CONFIRM_VOICE_CHANNEL_ID",
    )
    if confirm_guild != guild_id:
        raise ConfigError("ADK_VOICE_LIVE_CONFIRM_GUILD_ID must match ADK_VOICE_LIVE_TEST_GUILD_ID")
    if confirm_voice != voice_channel_id:
        raise ConfigError(
            "ADK_VOICE_LIVE_CONFIRM_VOICE_CHANNEL_ID must match "
            "ADK_VOICE_LIVE_TEST_VOICE_CHANNEL_ID"
        )

    speaker_token = _read_token(env)
    if env.get("ADK_VOICE_LIVE_SPEAKER_BOT_ID", "").strip():
        speaker_bot_id = _parse_int(
            env["ADK_VOICE_LIVE_SPEAKER_BOT_ID"],
            "ADK_VOICE_LIVE_SPEAKER_BOT_ID",
        )
        if speaker_bot_id == agentdesk_bot_id:
            raise ConfigError("speaker bot id must be different from AgentDesk bot id")

    report_path = Path(args.report)
    if not report_path.is_absolute():
        report_path = repo_root() / report_path
    jsonl_path = Path(args.observability_jsonl).expanduser()
    if not jsonl_path.is_absolute():
        jsonl_path = repo_root() / jsonl_path

    audio_dir = Path(args.audio_dir).expanduser() if args.audio_dir else None
    if audio_dir is not None and not audio_dir.is_absolute():
        audio_dir = repo_root() / audio_dir

    return LiveSmokeConfig(
        report_path=report_path,
        api_base_url=str(args.api_base_url).rstrip("/"),
        api_token=env.get("ADK_API_AUTH_TOKEN") or env.get("ADK_API_TOKEN"),
        observability_jsonl=jsonl_path,
        guild_id=guild_id,
        voice_channel_id=voice_channel_id,
        text_channel_id=text_channel_id,
        agentdesk_bot_id=agentdesk_bot_id,
        speaker_bot_token=speaker_token,
        agent_id=env.get("ADK_VOICE_LIVE_AGENT_ID", "").strip() or None,
        provider_identity=env.get("ADK_VOICE_LIVE_PROVIDER_IDENTITY", "").strip() or None,
        real_provider_contacted=_parse_bool_or_none(env.get("ADK_VOICE_LIVE_REAL_PROVIDER_CONTACTED")),
        test_identity=env.get("ADK_VOICE_LIVE_TEST_IDENTITY", "voice-live-media-smoke/real-discord"),
        wait_timeout_s=max(1.0, float(args.wait_timeout_s)),
        poll_interval_s=max(0.2, float(args.poll_interval_s)),
        discord_connect_timeout_s=max(1.0, float(args.discord_connect_timeout_s)),
        audio_dir=audio_dir,
        keep_audio=bool(args.keep_audio),
        tts_command=args.tts_command,
        tts_voice=env.get("ADK_VOICE_LIVE_TTS_VOICE", "en-US-JennyNeural"),
        macos_say_voice=env.get("ADK_VOICE_LIVE_MACOS_SAY_VOICE", "").strip() or None,
        cleanup_command=env.get("ADK_VOICE_LIVE_CLEANUP_CHECK_COMMAND", "").strip() or None,
        require_cleanup_check=env.get("ADK_VOICE_LIVE_REQUIRE_CLEANUP_CHECK") == "1",
        allow_live_discord=True,
    )


def now_ms() -> int:
    return int(time.time() * 1000)


def _request_json(url: str, *, token: str | None, timeout_s: float = 10.0) -> dict[str, Any]:
    headers = {"Accept": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers, method="GET")
    with urllib.request.urlopen(request, timeout=timeout_s) as response:
        payload = response.read().decode("utf-8")
    if not payload:
        return {}
    data = json.loads(payload)
    if not isinstance(data, dict):
        raise LiveSmokeError(f"expected object response from {url}, got {type(data).__name__}")
    return data


def fetch_observability_events(config: LiveSmokeConfig, recent_limit: int = 1000) -> list[dict[str, Any]]:
    url = (
        f"{config.api_base_url}/api/analytics/observability?"
        f"{urllib.parse.urlencode({'recentLimit': str(recent_limit)})}"
    )
    try:
        data = _request_json(url, token=config.api_token, timeout_s=8.0)
        events = data.get("recent_events") or []
        if isinstance(events, list):
            return [event for event in events if isinstance(event, dict)]
    except Exception:
        # The JSONL fallback keeps the runner usable when the live API is not
        # mounted, is token-protected, or the dcserver has just restarted.
        pass
    return read_jsonl_events(config.observability_jsonl, limit=recent_limit)


def read_jsonl_events(path: Path, *, limit: int = 1000) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return []
    events: list[dict[str, Any]] = []
    for line in lines[-limit:]:
        line = line.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            events.append(value)
    return events


def event_matches_channel(event: Mapping[str, Any], channel_ids: set[int]) -> bool:
    candidates: list[Any] = [event.get("channel_id")]
    payload = event.get("payload")
    if isinstance(payload, dict):
        candidates.extend(
            payload.get(key)
            for key in (
                "voice_channel_id",
                "control_channel_id",
                "background_channel_id",
                "cancel_channel_id",
                "channel_id",
            )
        )
    for candidate in candidates:
        try:
            if candidate is not None and int(candidate) in channel_ids:
                return True
        except (TypeError, ValueError):
            continue
    return False


def events_since(
    events: Sequence[dict[str, Any]],
    *,
    since_ms: int,
    channel_ids: set[int],
) -> list[dict[str, Any]]:
    matched: list[dict[str, Any]] = []
    for event in events:
        try:
            timestamp_ms = int(event.get("timestamp_ms", 0))
        except (TypeError, ValueError):
            timestamp_ms = 0
        if timestamp_ms and timestamp_ms < since_ms:
            continue
        if not event_matches_channel(event, channel_ids):
            continue
        matched.append(event)
    return matched


def payloads_for(events: Sequence[Mapping[str, Any]], event_type: str) -> list[dict[str, Any]]:
    payloads: list[dict[str, Any]] = []
    for event in events:
        if event.get("event_type") != event_type:
            continue
        payload = event.get("payload")
        if isinstance(payload, dict):
            payloads.append(dict(payload))
    return payloads


def unique_utterance_ids(flight_events: Sequence[Mapping[str, Any]]) -> list[str]:
    seen: set[str] = set()
    ordered: list[str] = []
    for event in flight_events:
        utterance_id = event.get("utterance_id")
        if isinstance(utterance_id, str) and utterance_id and utterance_id not in seen:
            seen.add(utterance_id)
            ordered.append(utterance_id)
    return ordered


def _contains_route(flight_events: Sequence[Mapping[str, Any]], route: str) -> bool:
    return any(event.get("route") == route for event in flight_events)


def _latency_for_utterance(
    latency_events: Sequence[Mapping[str, Any]],
    utterance_ids: Sequence[str],
) -> dict[str, Any] | None:
    utterance_set = set(utterance_ids)
    for event in reversed(latency_events):
        utterance_id = event.get("utterance_id")
        if isinstance(utterance_id, str) and utterance_id in utterance_set:
            return dict(event)
    return dict(latency_events[-1]) if latency_events else None


def _failure_source(failures: Sequence[str]) -> str | None:
    if not failures:
        return None
    joined = "\n".join(failures).lower()
    if "discord" in joined or "speaker" in joined or "media" in joined:
        return "discord_media"
    if "stt" in joined or "tts" in joined or "transcript" in joined or "playback" in joined:
        return "stt_tts"
    if "background" in joined or "foreground" in joined or "agent" in joined:
        return "provider_response"
    if "cleanup" in joined or "stale" in joined or "inflight" in joined:
        return "cleanup"
    return "reporting"


def build_scenario_report(
    spec: ScenarioSpec,
    evidence: ScenarioEvidence,
    config: LiveSmokeConfig,
    env: Mapping[str, str] | None = None,
) -> dict[str, Any]:
    env = env or {}
    flight_events = payloads_for(evidence.events, "voice_flight_event")
    latency_events = payloads_for(evidence.events, "voice_latency_turn")
    utterance_ids = unique_utterance_ids(flight_events)
    stt_events = [
        event
        for event in flight_events
        if event.get("utterance_id") and (event.get("stt_mode") or event.get("stt_latency_ms") is not None)
    ]
    routes = [event.get("route") for event in flight_events if event.get("route")]

    failures: list[str] = []
    for route in spec.setup_expected_routes + spec.expected_routes:
        if not _contains_route(flight_events, route):
            failures.append(f"missing voice_flight_event route {route!r}")
    if not utterance_ids:
        failures.append("no utterance_id observed from live voice media path")
    if not stt_events:
        failures.append("no STT/transcript voice_flight_event observed")
    if spec.require_latency and not latency_events:
        failures.append("voice_latency_turn metric was not emitted")
    if spec.require_cancel:
        explicit_stop = [event for event in flight_events if event.get("route") == "explicit_stop"]
        if not explicit_stop:
            failures.append("barge-in did not emit explicit_stop route")
        elif not any(event.get("cancelled") is True or event.get("cancel_source") for event in explicit_stop):
            failures.append("explicit_stop route did not carry cancellation evidence")

    cleanup_status = evidence.cleanup_evidence.get("status")
    if cleanup_status == "failed":
        failures.extend(str(reason) for reason in evidence.cleanup_evidence.get("raw_failure_reasons", []))
    elif config.require_cleanup_check and cleanup_status in {"not_configured", "unavailable"}:
        failures.append("cleanup check was required but no cleanup evidence was available")

    latency = _latency_for_utterance(latency_events, utterance_ids)
    first_stt = stt_events[0] if stt_events else {}
    foreground_decision = next(
        (
            event.get("foreground_decision")
            for event in flight_events
            if event.get("foreground_decision")
        ),
        None,
    )
    tts_observation = next(
        (
            {
                "tts_chars": event.get("tts_chars"),
                "tts_first_audio_ms": event.get("tts_first_audio_ms"),
            }
            for event in flight_events
            if event.get("tts_chars") is not None or event.get("tts_first_audio_ms") is not None
        ),
        None,
    )

    return {
        "scenario_id": spec.scenario_id,
        "status": "passed" if not failures else "failed",
        "agent_mode": "real_live",
        "utterance_id": utterance_ids[-1] if utterance_ids else None,
        "utterance_ids": utterance_ids,
        "guild_id": config.guild_id,
        "channel_id": config.voice_channel_id,
        "control_text_channel_id": config.text_channel_id,
        "test_identity": f"{config.test_identity}/{spec.scenario_id}",
        "spoken_prompt": spec.prompt(env),
        "setup_spoken_prompt": spec.setup_prompt(env),
        "expected_routes": list(spec.setup_expected_routes + spec.expected_routes),
        "observed_routes": routes,
        "media_receive_counters": {
            "voice_flight_events": len(flight_events),
            "stt_events": len(stt_events),
            "latency_events": len(latency_events),
            "unique_utterances": len(utterance_ids),
        },
        "transcript_result": {
            "stt_mode": first_stt.get("stt_mode"),
            "stt_latency_ms": first_stt.get("stt_latency_ms"),
            "transcript_chars": first_stt.get("transcript_chars"),
        },
        "routing_decision": routes[-1] if routes else None,
        "foreground_decision": foreground_decision,
        "tts_observation": tts_observation,
        "playback_observation": (
            "cancelled"
            if spec.require_cancel and _contains_route(flight_events, "explicit_stop")
            else ("voice_latency_turn_observed" if latency else None)
        ),
        "voice_latency_turn": latency,
        "voice_flight_events": flight_events,
        "cleanup_evidence": evidence.cleanup_evidence,
        "timing_stages": evidence.timing_stages.to_json(),
        "failure_attribution": {
            "source": _failure_source(failures),
            "real_provider_contacted": config.real_provider_contacted,
        },
        "raw_failure_reasons": failures,
    }


def validate_report(report: Mapping[str, Any]) -> None:
    if report.get("agent_mode") != "real_live":
        raise ValueError(f"unexpected agent_mode: {report.get('agent_mode')!r}")
    if report.get("live_discord_media_transport_covered") is not True:
        raise ValueError("live Discord media transport must be marked covered")
    scenarios = report.get("scenarios")
    if not isinstance(scenarios, list) or not scenarios:
        raise ValueError("live media smoke report has no scenarios")
    failures = []
    for scenario in scenarios:
        if not isinstance(scenario, dict):
            failures.append(f"non-object scenario: {scenario!r}")
        elif scenario.get("status") != "passed":
            failures.append(f"{scenario.get('scenario_id')}: {scenario.get('raw_failure_reasons')}")
    if failures:
        raise ValueError("voice live media smoke failures:\n" + "\n".join(failures))


def write_report(report: Mapping[str, Any], path: Path) -> None:
    if path.parent:
        path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def _format_tts_command(command_template: str, *, text: str, output: Path, voice: str) -> str:
    return command_template.format(
        text=shlex.quote(text),
        output=shlex.quote(str(output)),
        voice=shlex.quote(voice),
    )


def generate_audio_clip(text: str, output: Path, config: LiveSmokeConfig) -> dict[str, Any]:
    output.parent.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    generator: str
    if config.tts_command:
        command = _format_tts_command(
            config.tts_command,
            text=text,
            output=output,
            voice=config.tts_voice,
        )
        subprocess.run(command, shell=True, check=True)
        generator = "custom-command"
    elif shutil.which("edge-tts"):
        subprocess.run(
            [
                "edge-tts",
                "--voice",
                config.tts_voice,
                "--text",
                text,
                "--write-media",
                str(output),
            ],
            check=True,
        )
        generator = "edge-tts"
    elif sys.platform == "darwin" and shutil.which("say") and shutil.which("ffmpeg"):
        voice_args = ["-v", config.macos_say_voice] if config.macos_say_voice else []
        aiff = output.with_suffix(".aiff")
        subprocess.run(["say", *voice_args, "-o", str(aiff), text], check=True)
        subprocess.run(["ffmpeg", "-y", "-hide_banner", "-loglevel", "error", "-i", str(aiff), str(output)], check=True)
        generator = "macos-say"
    else:
        raise LiveSmokeError(
            "no TTS generator available; install edge-tts, run on macOS with say+ffmpeg, "
            "or set ADK_VOICE_LIVE_TTS_COMMAND"
        )
    if not output.exists() or output.stat().st_size == 0:
        raise LiveSmokeError(f"TTS generator did not write audio file: {output}")
    return {
        "generator": generator,
        "path": str(output),
        "bytes": output.stat().st_size,
        "elapsed_ms": int((time.monotonic() - started) * 1000),
    }


def build_audio_plan(config: LiveSmokeConfig, env: Mapping[str, str]) -> tuple[tempfile.TemporaryDirectory[str] | None, dict[str, Path], dict[str, Any]]:
    temp_dir: tempfile.TemporaryDirectory[str] | None = None
    audio_dir = config.audio_dir
    if audio_dir is None:
        if config.keep_audio:
            audio_dir = repo_root() / "target" / "voice-live-media-audio"
        else:
            temp_dir = tempfile.TemporaryDirectory(prefix="adk-voice-live-media-")
            audio_dir = Path(temp_dir.name)
    audio_dir.mkdir(parents=True, exist_ok=True)

    audio_paths: dict[str, Path] = {}
    generation: dict[str, Any] = {}
    for spec in SCENARIOS:
        entries = [(spec.scenario_id, spec.prompt(env))]
        setup = spec.setup_prompt(env)
        if setup:
            entries.insert(0, (f"{spec.scenario_id}-setup", setup))
        for key, text in entries:
            output = audio_dir / f"{key}.mp3"
            info = generate_audio_clip(text, output, config)
            audio_paths[key] = output
            generation[key] = info
    return temp_dir, audio_paths, generation


async def wait_for_evidence(
    spec: ScenarioSpec,
    config: LiveSmokeConfig,
    started_at_ms: int,
    timing: TimingStages,
) -> list[dict[str, Any]]:
    deadline = time.monotonic() + config.wait_timeout_s
    last: list[dict[str, Any]] = []
    wait_started = time.monotonic()
    while time.monotonic() < deadline:
        events = await asyncio.to_thread(fetch_observability_events, config)
        last = events_since(
            events,
            since_ms=started_at_ms - 2_000,
            channel_ids=config.evidence_channel_ids,
        )
        flight = payloads_for(last, "voice_flight_event")
        latency = payloads_for(last, "voice_latency_turn")
        routes = {event.get("route") for event in flight}
        expected = set(spec.setup_expected_routes + spec.expected_routes)
        route_ok = expected.issubset(routes)
        stt_ok = any(event.get("utterance_id") and event.get("stt_mode") for event in flight)
        latency_ok = bool(latency) or not spec.require_latency
        cancel_ok = (
            not spec.require_cancel
            or any(
                event.get("route") == "explicit_stop"
                and (event.get("cancelled") is True or event.get("cancel_source"))
                for event in flight
            )
        )
        if route_ok and stt_ok and latency_ok and cancel_ok:
            break
        await asyncio.sleep(config.poll_interval_s)
    timing.evidence_wait_ms = int((time.monotonic() - wait_started) * 1000)
    return last


def _cleanup_from_agent_turn(config: LiveSmokeConfig) -> dict[str, Any] | None:
    if not config.agent_id:
        return None
    target = urllib.parse.quote(config.agent_id, safe="")
    url = f"{config.api_base_url}/api/agents/{target}/turn"
    try:
        status = _request_json(url, token=config.api_token, timeout_s=10.0)
    except Exception as error:
        return {
            "status": "unavailable",
            "source": "agent_turn_api",
            "raw_failure_reasons": [str(error)],
        }
    failures: list[str] = []
    if status.get("status") in {"working", "running", "busy"}:
        failures.append(f"agent turn status still active: {status.get('status')}")
    if status.get("pending_queue_depth") not in {None, 0}:
        failures.append(f"pending queue depth is {status.get('pending_queue_depth')}")
    if status.get("inflight_age_secs") is not None:
        failures.append("agent turn reports saved inflight state")
    return {
        "status": "failed" if failures else "passed",
        "source": "agent_turn_api",
        "agent_id": config.agent_id,
        "snapshot": status,
        "raw_failure_reasons": failures,
    }


def _cleanup_from_command(config: LiveSmokeConfig, spec: ScenarioSpec, utterance_ids: Sequence[str]) -> dict[str, Any] | None:
    if not config.cleanup_command:
        return None
    env = os.environ.copy()
    env["ADK_VOICE_LIVE_SCENARIO_ID"] = spec.scenario_id
    env["ADK_VOICE_LIVE_UTTERANCE_IDS"] = ",".join(utterance_ids)
    env["ADK_VOICE_LIVE_VOICE_CHANNEL_ID"] = str(config.voice_channel_id)
    env["ADK_VOICE_LIVE_TEXT_CHANNEL_ID"] = str(config.text_channel_id)
    result = subprocess.run(
        config.cleanup_command,
        shell=True,
        text=True,
        capture_output=True,
        env=env,
        check=False,
    )
    raw_failure_reasons: list[str] = []
    payload: dict[str, Any]
    if result.stdout.strip():
        try:
            value = json.loads(result.stdout)
            payload = value if isinstance(value, dict) else {"stdout": result.stdout}
        except json.JSONDecodeError:
            payload = {"stdout": result.stdout}
    else:
        payload = {}
    if result.returncode != 0:
        raw_failure_reasons.append(f"cleanup command exited {result.returncode}: {result.stderr.strip()}")
    if payload.get("ok") is False:
        raw_failure_reasons.append("cleanup command returned ok=false")
    for key in (
        "stale_voice_session",
        "playback_task_active",
        "foreground_call_active",
        "voice_turn_link_active",
    ):
        if payload.get(key) is True:
            raw_failure_reasons.append(f"{key}=true")
    return {
        "status": "failed" if raw_failure_reasons else "passed",
        "source": "cleanup_command",
        "payload": payload,
        "stderr": result.stderr.strip(),
        "raw_failure_reasons": raw_failure_reasons,
    }


async def run_cleanup_check(
    config: LiveSmokeConfig,
    spec: ScenarioSpec,
    events: Sequence[dict[str, Any]],
    timing: TimingStages,
) -> dict[str, Any]:
    started = time.monotonic()
    flight_events = payloads_for(events, "voice_flight_event")
    utterance_ids = unique_utterance_ids(flight_events)
    checks = []
    command_check = await asyncio.to_thread(_cleanup_from_command, config, spec, utterance_ids)
    if command_check is not None:
        checks.append(command_check)
    api_check = await asyncio.to_thread(_cleanup_from_agent_turn, config)
    if api_check is not None:
        checks.append(api_check)
    timing.cleanup_check_ms = int((time.monotonic() - started) * 1000)
    if not checks:
        return {
            "status": "not_configured",
            "source": "none",
            "raw_failure_reasons": [],
        }
    failures = [
        reason
        for check in checks
        if check.get("status") == "failed"
        for reason in check.get("raw_failure_reasons", [])
    ]
    return {
        "status": "failed" if failures else "passed",
        "checks": checks,
        "raw_failure_reasons": failures,
    }


async def play_audio(discord_module: Any, voice_client: Any, path: Path, timeout_s: float) -> int:
    started = time.monotonic()
    loop = asyncio.get_running_loop()
    done: asyncio.Future[None] = loop.create_future()

    def after(error: Exception | None) -> None:
        if error is not None:
            loop.call_soon_threadsafe(done.set_exception, error)
        else:
            loop.call_soon_threadsafe(done.set_result, None)

    source = discord_module.FFmpegPCMAudio(str(path))
    voice_client.play(source, after=after)
    await asyncio.wait_for(done, timeout=timeout_s)
    return int((time.monotonic() - started) * 1000)


async def run_scenario(
    spec: ScenarioSpec,
    config: LiveSmokeConfig,
    discord_module: Any,
    voice_client: Any,
    audio_paths: Mapping[str, Path],
) -> ScenarioEvidence:
    timing = TimingStages()
    started_at_ms = now_ms()
    if spec.setup_prompt_env:
        setup_key = f"{spec.scenario_id}-setup"
        timing.stream_ms += await play_audio(
            discord_module,
            voice_client,
            audio_paths[setup_key],
            timeout_s=max(20.0, config.wait_timeout_s / 2),
        )
        await asyncio.sleep(spec.barge_in_delay_s)
    timing.stream_ms += await play_audio(
        discord_module,
        voice_client,
        audio_paths[spec.scenario_id],
        timeout_s=max(20.0, config.wait_timeout_s / 2),
    )
    await asyncio.sleep(spec.settle_s)
    events = await wait_for_evidence(spec, config, started_at_ms, timing)
    cleanup = await run_cleanup_check(config, spec, events, timing)
    return ScenarioEvidence(
        scenario_id=spec.scenario_id,
        started_at_ms=started_at_ms,
        completed_at_ms=now_ms(),
        events=events,
        cleanup_evidence=cleanup,
        timing_stages=timing,
    )


async def run_live_discord(
    config: LiveSmokeConfig,
    audio_paths: Mapping[str, Path],
) -> tuple[list[ScenarioEvidence], dict[str, Any]]:
    try:
        import discord  # type: ignore[import-not-found]
    except ImportError as error:
        raise LiveSmokeError(
            "discord.py with voice support is required for live Discord media smoke"
        ) from error

    intents = discord.Intents.default()
    intents.guilds = True
    intents.voice_states = True
    client = discord.Client(intents=intents)
    state: dict[str, Any] = {"scenarios": []}

    @client.event
    async def on_ready() -> None:  # noqa: ANN202
        join_started = time.monotonic()
        try:
            guild = client.get_guild(config.guild_id) or await client.fetch_guild(config.guild_id)
            channel = guild.get_channel(config.voice_channel_id)
            if channel is None:
                channel = await client.fetch_channel(config.voice_channel_id)
            if getattr(channel, "guild", None) is None or getattr(channel.guild, "id", None) != config.guild_id:
                raise LiveSmokeError("configured voice channel does not belong to configured guild")
            members = list(getattr(channel, "members", []) or [])
            agent_present = any(getattr(member, "id", None) == config.agentdesk_bot_id for member in members)
            if not agent_present:
                raise LiveSmokeError(
                    "AgentDesk bot is not present in the configured test voice channel; "
                    "configure voice auto-join or run /voice join first"
                )
            if client.user and client.user.id == config.agentdesk_bot_id:
                raise LiveSmokeError("speaker bot token resolved to the AgentDesk bot user")
            voice_client = await channel.connect(
                timeout=config.discord_connect_timeout_s,
                reconnect=False,
            )
            state["speaker_user_id"] = client.user.id if client.user else None
            state["speaker_user_name"] = str(client.user) if client.user else None
            state["join_ms"] = int((time.monotonic() - join_started) * 1000)
            try:
                for spec in SCENARIOS:
                    evidence = await run_scenario(
                        spec,
                        config,
                        discord,
                        voice_client,
                        audio_paths,
                    )
                    evidence.timing_stages.speaker_join_ms = state["join_ms"]
                    state["scenarios"].append(evidence)
            finally:
                await voice_client.disconnect(force=True)
        except Exception as error:  # noqa: BLE001 - stored for main to report.
            state["error"] = error
        finally:
            await client.close()

    await client.start(config.speaker_bot_token)
    if "error" in state:
        raise state["error"]
    return list(state["scenarios"]), {
        "speaker_user_id": state.get("speaker_user_id"),
        "speaker_user_name": state.get("speaker_user_name"),
        "library": "discord.py",
        "join_ms": state.get("join_ms"),
    }


def build_top_level_report(
    config: LiveSmokeConfig,
    scenarios: Sequence[dict[str, Any]],
    speaker: Mapping[str, Any],
    audio_generation: Mapping[str, Any],
    *,
    started_at_ms: int,
    completed_at_ms: int,
    raw_failure_reasons: Sequence[str] = (),
    transport_covered: bool = True,
) -> dict[str, Any]:
    return {
        "schema_version": 1,
        "issue": "#3802",
        "lane": "voice_live_media_smoke",
        "agent_mode": "real_live",
        "live_discord_media_transport_covered": transport_covered,
        "receive_boundary": "Discord voice gateway/songbird -> VoiceReceiver -> DiscordVoiceBargeInHook",
        "run_started_at_ms": started_at_ms,
        "run_completed_at_ms": completed_at_ms,
        "guild_id": config.guild_id,
        "voice_channel_id": config.voice_channel_id,
        "control_text_channel_id": config.text_channel_id,
        "agentdesk_bot_id": config.agentdesk_bot_id,
        "speaker_client": {
            **dict(speaker),
            "token_redacted": True,
        },
        "test_identity": config.test_identity,
        "provider_identity": config.provider_identity,
        "real_provider_contacted": config.real_provider_contacted,
        "observability": {
            "api_base_url": config.api_base_url,
            "jsonl_path": str(config.observability_jsonl),
        },
        "audio_generation": audio_generation,
        "safety": {
            "media_smoke_env": True,
            "safety_ack": True,
            "confirmed_guild_id": config.guild_id,
            "confirmed_voice_channel_id": config.voice_channel_id,
        },
        "scenarios": list(scenarios),
        "raw_failure_reasons": list(raw_failure_reasons),
    }


async def run(config: LiveSmokeConfig, env: Mapping[str, str]) -> dict[str, Any]:
    started_at_ms = now_ms()
    temp_dir: tempfile.TemporaryDirectory[str] | None = None
    try:
        temp_dir, audio_paths, audio_generation = build_audio_plan(config, env)
        scenario_evidence, speaker = await run_live_discord(config, audio_paths)
        scenario_reports = [
            build_scenario_report(spec, evidence, config, env)
            for spec, evidence in zip(SCENARIOS, scenario_evidence, strict=True)
        ]
        report = build_top_level_report(
            config,
            scenario_reports,
            speaker,
            audio_generation,
            started_at_ms=started_at_ms,
            completed_at_ms=now_ms(),
        )
        return report
    finally:
        if temp_dir is not None:
            temp_dir.cleanup()


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        config = load_config(args)
    except ConfigError as error:
        print(f"Refusing live voice media smoke: {error}", file=sys.stderr)
        return 2

    if args.dry_run_config:
        print("Live voice media smoke config is valid; no Discord resources touched.")
        return 0

    try:
        report = asyncio.run(run(config, os.environ))
    except Exception as error:  # noqa: BLE001
        failure_report = build_top_level_report(
            config,
            [],
            {},
            {},
            started_at_ms=now_ms(),
            completed_at_ms=now_ms(),
            raw_failure_reasons=[str(error)],
            transport_covered=False,
        )
        write_report(failure_report, config.report_path)
        print(f"Voice live media smoke failed: {error}", file=sys.stderr)
        return 1

    write_report(report, config.report_path)
    try:
        validate_report(report)
    except Exception as error:  # noqa: BLE001
        print(f"Voice live media smoke failed: {error}", file=sys.stderr)
        return 1

    print("Voice live media smoke passed")
    print(f"Report: {config.report_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
