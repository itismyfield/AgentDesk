"""Local relay fixture replay primitives for the TUI E2E harness.

These fixtures intentionally avoid Discord, tmux, and live dcserver state. They
model the narrow relay contracts that are otherwise hard to trigger
deterministically from the cell matrix: Background CronCreate delivery and
modern Codex ``response_item`` + ``event_msg/task_complete`` finalization.
"""

from __future__ import annotations

import dataclasses
import datetime as dt
from typing import Any

from . import assertions


FIXTURE_BOT_ID = "agentdesk-fixture-relay"


@dataclasses.dataclass
class ReplayState:
    kind: str
    provider: str
    cell: str
    channel_id: str
    scenario_id: str
    full_response: str = ""
    assistant_text_seen: bool = False
    task_notification_kind: str | None = None
    task_notification_source: str | None = None
    task_notification_status: str | None = None
    task_notification_summary: str | None = None
    task_complete_seen: bool = False
    task_complete_turn_id: str | None = None
    result_seen: bool = False
    result_text_source: str | None = None
    finalized: bool = False
    active_turn: str = "fixture-active"
    followup_ready: bool = False
    followup_probe_accepted: bool = False
    queue_depth: int = 0
    pending_discord_callback: bool = False
    relay_stall_state: str = "healthy"
    stale_thread_proof: bool = False
    translated_events: list[dict[str, Any]] = dataclasses.field(default_factory=list)
    deliveries: list[str] = dataclasses.field(default_factory=list)

    def as_dict(self) -> dict[str, Any]:
        return {
            "kind": self.kind,
            "provider": self.provider,
            "cell": self.cell,
            "channel_id": self.channel_id,
            "scenario_id": self.scenario_id,
            "full_response": self.full_response,
            "assistant_text_seen": self.assistant_text_seen,
            "task_notification_kind": self.task_notification_kind,
            "task_notification_source": self.task_notification_source,
            "task_notification_status": self.task_notification_status,
            "task_notification_summary": self.task_notification_summary,
            "task_complete_seen": self.task_complete_seen,
            "task_complete_turn_id": self.task_complete_turn_id,
            "result_seen": self.result_seen,
            "result_text_source": self.result_text_source,
            "finalized": self.finalized,
            "active_turn": self.active_turn,
            "followup_ready": self.followup_ready,
            "followup_probe_accepted": self.followup_probe_accepted,
            "queue_depth": self.queue_depth,
            "pending_discord_callback": self.pending_discord_callback,
            "relay_stall_state": self.relay_stall_state,
            "stale_thread_proof": self.stale_thread_proof,
            "translated_events": list(self.translated_events),
            "deliveries": list(self.deliveries),
        }


def replay_fixture(
    spec: dict[str, Any],
    *,
    cell: str,
    channel_id: str,
    scenario_id: str,
    run_id: str,
) -> dict[str, Any]:
    if not isinstance(spec, dict):
        raise assertions.AssertionError(f"replay_fixture requires a mapping: {spec!r}")
    kind = str(spec.get("kind") or spec.get("fixture") or "").strip()
    if not kind:
        raise assertions.AssertionError("replay_fixture requires kind")
    frames = _frame_list(spec.get("frames"))
    provider = str(spec.get("provider") or cell.split("-", 1)[0])
    state = ReplayState(
        kind=kind,
        provider=provider,
        cell=cell,
        channel_id=str(channel_id),
        scenario_id=scenario_id,
    )

    if kind == "cron_create_background":
        _replay_agent_stream(frames, state)
        _require_cron_create_background(state)
    elif kind == "codex_modern_schema":
        _replay_codex_modern(frames, state)
        _require_codex_modern_task_complete(state)
    else:
        raise assertions.AssertionError(f"unknown replay_fixture kind: {kind!r}")

    if not state.deliveries:
        raise assertions.AssertionError(
            f"fixture {kind!r} did not produce a relay delivery; state={state.as_dict()!r}"
        )

    messages = _discord_messages_for_deliveries(state.deliveries, run_id=run_id)
    health = _healthy_fixture_snapshot(state)
    return {
        "messages": messages,
        "replay": {
            "kind": kind,
            "frames": len(frames),
            "deliveries": len(state.deliveries),
            "run_id": run_id,
        },
        "state": state.as_dict(),
        "health": health,
    }


def probe_followup_ready(
    record: dict[str, Any],
    params: dict[str, Any] | str | None,
) -> dict[str, Any]:
    state = _latest_state(record)
    _assert_followup_ready_state(state)
    prompt = params.get("prompt") if isinstance(params, dict) else params
    probe = {
        "accepted": True,
        "prompt": str(prompt or ""),
        "active_turn_before": state.get("active_turn"),
        "queue_depth_before": state.get("queue_depth"),
        "reason": "fixture turn finalized; follow-up dispatch would be accepted",
    }
    state["followup_probe_accepted"] = True
    record["fixture_state"] = state
    return probe


def assert_fixture_state(
    record: dict[str, Any] | None,
    expected: dict[str, Any],
) -> None:
    state = _latest_state(record)
    if not isinstance(expected, dict):
        raise assertions.AssertionError(f"fixture_state requires mapping: {expected!r}")
    for key, value in expected.items():
        actual = state.get(key)
        if actual != value:
            raise assertions.AssertionError(
                f"fixture_state[{key!r}]={actual!r}, expected {value!r}; state={state!r}"
            )


def assert_fixture_finalized(
    record: dict[str, Any] | None,
    params: dict[str, Any] | bool | None = None,
) -> None:
    state = _latest_state(record)
    if state.get("finalized") is not True:
        raise assertions.AssertionError(f"fixture did not finalize: state={state!r}")
    if str(state.get("active_turn") or "") != "none":
        raise assertions.AssertionError(f"fixture active_turn not released: state={state!r}")
    if isinstance(params, dict):
        assert_fixture_state(record, params)


def assert_fixture_followup_ready(record: dict[str, Any] | None) -> None:
    _assert_followup_ready_state(_latest_state(record))


def assert_fixture_no_health_degradation(record: dict[str, Any] | None) -> None:
    if not record:
        raise assertions.AssertionError("fixture_no_health_degradation requires record")
    health = record.get("fixture_health")
    if not isinstance(health, dict):
        raise assertions.AssertionError(
            f"fixture health snapshot missing from record: {record!r}"
        )
    violations = []
    if health.get("status") != "healthy":
        violations.append(f"status={health.get('status')!r}")
    if health.get("degraded_reasons"):
        violations.append(f"degraded_reasons={health.get('degraded_reasons')!r}")
    if health.get("queue_depth") != 0:
        violations.append(f"queue_depth={health.get('queue_depth')!r}")
    if health.get("active_turn") != "none":
        violations.append(f"active_turn={health.get('active_turn')!r}")
    if health.get("pending_discord_callback"):
        violations.append("pending_discord_callback=true")
    if health.get("stale_thread_proof"):
        violations.append("stale_thread_proof=true")
    if health.get("relay_stall_state") not in (None, "", "healthy"):
        violations.append(f"relay_stall_state={health.get('relay_stall_state')!r}")
    if violations:
        raise assertions.AssertionError(
            f"fixture health degraded: {violations}; health={health!r}"
        )


def assert_fixture_task_notification(
    record: dict[str, Any] | None,
    params: dict[str, Any],
) -> None:
    state = _latest_state(record)
    expected_kind = params.get("kind")
    if (
        expected_kind is not None
        and state.get("task_notification_kind") != expected_kind
    ):
        raise assertions.AssertionError(
            "fixture task notification kind mismatch: "
            f"{state.get('task_notification_kind')!r} != {expected_kind!r}; state={state!r}"
        )
    expected_source = params.get("source")
    if (
        expected_source is not None
        and state.get("task_notification_source") != expected_source
    ):
        raise assertions.AssertionError(
            "fixture task notification source mismatch: "
            f"{state.get('task_notification_source')!r} != {expected_source!r}; state={state!r}"
        )
    expected_status = params.get("status")
    if (
        expected_status is not None
        and state.get("task_notification_status") != expected_status
    ):
        raise assertions.AssertionError(
            "fixture task notification status mismatch: "
            f"{state.get('task_notification_status')!r} != {expected_status!r}; state={state!r}"
        )


def assert_fixture_task_complete_finalized(
    record: dict[str, Any] | None,
    params: dict[str, Any] | bool | None = None,
) -> None:
    state = _latest_state(record)
    if state.get("task_complete_seen") is not True:
        raise assertions.AssertionError(
            f"fixture did not observe task_complete: {state!r}"
        )
    assert_fixture_finalized(record)
    if isinstance(params, dict):
        expected_turn_id = params.get("turn_id")
        if (
            expected_turn_id is not None
            and state.get("task_complete_turn_id") != expected_turn_id
        ):
            raise assertions.AssertionError(
                "fixture task_complete turn_id mismatch: "
                f"{state.get('task_complete_turn_id')!r} != {expected_turn_id!r}; "
                f"state={state!r}"
            )
        expected_source = params.get("result_text_source")
        if (
            expected_source is not None
            and state.get("result_text_source") != expected_source
        ):
            raise assertions.AssertionError(
                "fixture result_text_source mismatch: "
                f"{state.get('result_text_source')!r} != {expected_source!r}; "
                f"state={state!r}"
            )


def _frame_list(raw_frames: Any) -> list[dict[str, Any]]:
    if not isinstance(raw_frames, list) or not raw_frames:
        raise assertions.AssertionError("replay_fixture requires a non-empty frames list")
    frames: list[dict[str, Any]] = []
    for idx, frame in enumerate(raw_frames):
        if not isinstance(frame, dict):
            raise assertions.AssertionError(
                f"replay_fixture frame {idx} must be a mapping: {frame!r}"
            )
        frames.append(frame)
    return frames


def _replay_agent_stream(frames: list[dict[str, Any]], state: ReplayState) -> None:
    tool_use_names: dict[str, str] = {}
    task_tool_use_ids: dict[str, str] = {}
    task_types: dict[str, str] = {}
    for frame in frames:
        frame_type = str(frame.get("type") or "")
        if frame_type == "assistant":
            for item in _content_items(frame):
                item_type = str(item.get("type") or "")
                if item_type == "tool_use":
                    tool_use_id = str(item.get("id") or item.get("tool_use_id") or "")
                    name = str(item.get("name") or "")
                    if tool_use_id and name:
                        tool_use_names[tool_use_id] = name
                    continue
                if item_type == "text":
                    _append_response_text(state, str(item.get("text") or ""))
                    state.assistant_text_seen = True
        elif frame_type == "system" and frame.get("subtype") == "task_started":
            task_id = str(frame.get("task_id") or "")
            tool_use_id = str(frame.get("tool_use_id") or "")
            if task_id and tool_use_id:
                task_tool_use_ids[task_id] = tool_use_id
            task_type = str(frame.get("task_type") or "")
            if task_id and task_type:
                task_types[task_id] = task_type
        elif frame_type == "system" and frame.get("subtype") == "task_notification":
            task_id = str(frame.get("task_id") or "")
            source = _notification_source(
                frame,
                task_id=task_id,
                task_tool_use_ids=task_tool_use_ids,
                tool_use_names=tool_use_names,
                task_types=task_types,
            )
            state.task_notification_source = source
            state.task_notification_kind = _normalize_task_kind(
                frame.get("task_notification_kind") or frame.get("kind"),
                source=source,
                summary=str(frame.get("summary") or ""),
                task_type=task_types.get(task_id),
            )
            state.task_notification_status = str(frame.get("status") or "")
            state.task_notification_summary = str(frame.get("summary") or "")
            state.translated_events.append(
                {
                    "type": "task_notification",
                    "kind": state.task_notification_kind,
                    "source": source,
                    "status": state.task_notification_status,
                }
            )
        elif frame_type == "result":
            state.result_seen = True
            result_text = str(frame.get("result") or "")
            if result_text and not state.full_response.strip():
                state.full_response = result_text
                state.result_text_source = "result.result"
            elif result_text and state.result_text_source is None:
                state.result_text_source = "assistant.text"
            _maybe_deliver(state)


def _replay_codex_modern(frames: list[dict[str, Any]], state: ReplayState) -> None:
    for frame in frames:
        frame_type = str(frame.get("type") or "")
        if frame_type == "thread.started":
            thread_id = str(frame.get("thread_id") or "")
            if thread_id:
                state.translated_events.append(
                    {"type": "system", "subtype": "init", "session_id": thread_id}
                )
            continue
        if frame_type == "response_item":
            payload = frame.get("payload")
            if not isinstance(payload, dict):
                continue
            if payload.get("type") == "message" and payload.get("role") == "assistant":
                include_in_final = payload.get("phase") != "commentary"
                for text in _codex_message_texts(payload):
                    if include_in_final:
                        _append_response_text(state, text)
                        state.result_text_source = (
                            state.result_text_source or "response_item.message"
                        )
                    state.assistant_text_seen = True
                    state.translated_events.append({"type": "assistant", "text": text})
            continue
        if frame_type == "event_msg":
            payload = frame.get("payload")
            if not isinstance(payload, dict):
                continue
            event_type = payload.get("type")
            if event_type == "task_complete":
                state.task_complete_seen = True
                state.task_complete_turn_id = str(payload.get("turn_id") or "")
                last_agent_message = str(payload.get("last_agent_message") or "")
                if last_agent_message:
                    state.full_response = last_agent_message
                    state.result_text_source = "task_complete.last_agent_message"
                state.result_seen = True
                state.translated_events.append(
                    {
                        "type": "result",
                        "subtype": "success",
                        "source": state.result_text_source,
                        "turn_id": state.task_complete_turn_id,
                    }
                )
                _maybe_deliver(state)
            else:
                state.translated_events.append(
                    {"type": "event_msg", "event_type": event_type}
                )


def _content_items(frame: dict[str, Any]) -> list[dict[str, Any]]:
    message = frame.get("message")
    content = message.get("content") if isinstance(message, dict) else None
    if not isinstance(content, list):
        return []
    return [item for item in content if isinstance(item, dict)]


def _codex_message_texts(payload: dict[str, Any]) -> list[str]:
    content = payload.get("content")
    if not isinstance(content, list):
        return []
    texts = []
    for item in content:
        if not isinstance(item, dict):
            continue
        if item.get("type") not in {"output_text", "text"}:
            continue
        text = str(item.get("text") or "")
        if text:
            texts.append(text)
    return texts


def _append_response_text(state: ReplayState, text: str) -> None:
    if not text:
        return
    if state.full_response:
        state.full_response += "\n\n"
    state.full_response += text


def _notification_source(
    frame: dict[str, Any],
    *,
    task_id: str,
    task_tool_use_ids: dict[str, str],
    tool_use_names: dict[str, str],
    task_types: dict[str, str],
) -> str:
    explicit = frame.get("source") or frame.get("source_tool") or frame.get("tool_name")
    if explicit:
        return str(explicit)
    tool_use_id = task_tool_use_ids.get(task_id)
    if tool_use_id and tool_use_id in tool_use_names:
        return tool_use_names[tool_use_id]
    if task_id in task_types:
        return task_types[task_id]
    return ""


def _normalize_task_kind(
    raw_kind: Any,
    *,
    source: str,
    summary: str,
    task_type: str | None,
) -> str:
    text = str(raw_kind or "").strip().lower()
    if text == "background":
        return "Background"
    if text in {"monitor", "monitorautoturn", "monitor_auto_turn"}:
        return "MonitorAutoTurn"
    if text == "subagent":
        return "Subagent"
    if (
        source == "Monitor"
        or task_type == "monitor"
        or summary.startswith("Monitor event:")
    ):
        return "MonitorAutoTurn"
    if task_type == "local_agent":
        return "Subagent"
    return "Background"


def _maybe_deliver(state: ReplayState) -> None:
    if state.finalized:
        return
    allows_delivery = (
        state.task_notification_kind is None
        or state.task_notification_kind == "Background"
        or state.assistant_text_seen
    )
    if state.result_seen and state.full_response.strip() and allows_delivery:
        state.deliveries.append(state.full_response)
        state.finalized = True
        state.active_turn = "none"
        state.followup_ready = True
        state.queue_depth = 0
        state.pending_discord_callback = False


def _require_cron_create_background(state: ReplayState) -> None:
    if state.task_notification_source != "CronCreate":
        raise assertions.AssertionError(
            "cron_create_background fixture must carry exact CronCreate source; "
            f"state={state.as_dict()!r}"
        )
    if state.task_notification_kind != "Background":
        raise assertions.AssertionError(
            "cron_create_background fixture did not classify as Background; "
            f"state={state.as_dict()!r}"
        )
    if not state.finalized:
        raise assertions.AssertionError(
            f"cron_create_background fixture did not finalize: state={state.as_dict()!r}"
        )


def _require_codex_modern_task_complete(state: ReplayState) -> None:
    if not state.task_complete_seen:
        raise assertions.AssertionError(
            f"codex_modern_schema fixture did not include task_complete: {state.as_dict()!r}"
        )
    if state.result_text_source != "task_complete.last_agent_message":
        raise assertions.AssertionError(
            "codex_modern_schema fixture must finalize from task_complete.last_agent_message; "
            f"state={state.as_dict()!r}"
        )
    if not state.finalized:
        raise assertions.AssertionError(
            f"codex_modern_schema fixture did not finalize: state={state.as_dict()!r}"
        )


def _discord_messages_for_deliveries(
    deliveries: list[str],
    *,
    run_id: str,
) -> list[dict[str, Any]]:
    base = dt.datetime(2026, 5, 31, 0, 0, 0, tzinfo=dt.timezone.utc)
    messages: list[dict[str, Any]] = []
    for idx, body in enumerate(deliveries, start=1):
        messages.append(
            {
                "id": str(900_000_000_000_000_000 + idx),
                "content": body,
                "author": {"id": FIXTURE_BOT_ID, "bot": True},
                "type": 0,
                "timestamp": (base + dt.timedelta(seconds=idx)).isoformat(),
                "fixture_run_id": run_id,
            }
        )
    messages.append(
        {
            "id": str(900_000_000_000_000_000 + len(deliveries) + 1),
            "content": "Processing...",
            "author": {"id": FIXTURE_BOT_ID, "bot": True},
            "type": 0,
            "timestamp": (base + dt.timedelta(seconds=len(deliveries) + 1)).isoformat(),
            "fixture_run_id": run_id,
        }
    )
    return messages


def _healthy_fixture_snapshot(state: ReplayState) -> dict[str, Any]:
    return {
        "status": "healthy",
        "degraded_reasons": [],
        "active_turn": state.active_turn,
        "queue_depth": state.queue_depth,
        "pending_discord_callback": state.pending_discord_callback,
        "stale_thread_proof": state.stale_thread_proof,
        "relay_stall_state": state.relay_stall_state,
        "followup_ready": state.followup_ready,
    }


def _latest_state(record: dict[str, Any] | None) -> dict[str, Any]:
    if not record:
        raise assertions.AssertionError("fixture assertion requires record")
    state = record.get("fixture_state")
    if not isinstance(state, dict):
        raise assertions.AssertionError(f"fixture_state missing from record: {record!r}")
    return state


def _assert_followup_ready_state(state: dict[str, Any]) -> None:
    if state.get("followup_ready") is not True:
        raise assertions.AssertionError(f"fixture follow-up not ready: state={state!r}")
    if state.get("active_turn") != "none":
        raise assertions.AssertionError(
            f"fixture active turn still present: state={state!r}"
        )
    if state.get("queue_depth") != 0:
        raise assertions.AssertionError(f"fixture queue not drained: state={state!r}")
    if state.get("pending_discord_callback"):
        raise assertions.AssertionError(
            f"fixture pending Discord callback still present: state={state!r}"
        )
