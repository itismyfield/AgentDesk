#!/usr/bin/env python3
"""Cell-based E2E smoke driver for AgentDesk Discord relay.

Each invocation runs the YAML scenarios under ``tests/e2e/tui_relay/scenarios``
against a single (provider, runtime) cell — e.g. ``claude-pipe`` against the
``adk-claude-pipe-e2e`` channel. Multi-cell runs are orchestrated by the
``adk-e2e-orchestrator`` agent, which invokes this script once per cell.

Cell format: ``<provider>-<runtime>`` (e.g. ``claude-pipe``, ``claude-tui``,
``claude-e``, ``codex-pipe``, ``codex-tui``). A scenario is executed only when
its ``cells:`` list includes the requested cell.

Safety guards:
- Lease file at ``/tmp/agentdesk-e2e-relay.<cell>.lease`` (per-cell).
- Destructive scenarios are skipped unless ``--allow-destructive`` AND
  ``AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1``.
- ``--dry-run`` prints intended steps without sending anything.

Usage:
    scripts/e2e/run_tui_relay.py \\
        --base-url http://127.0.0.1:8791 \\
        --cell claude-pipe \\
        --channel-id 1506... \\
        --output out/e2e/tui_relay/<run_id>
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime as dt
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

import yaml  # type: ignore[import-untyped]

sys.path.insert(0, str(Path(__file__).resolve().parent))

from tui_relay import assertions, discord, lease, tmux  # noqa: E402


SUPPORTED_CELLS: tuple[str, ...] = (
    "claude-pipe",
    "claude-tui",
    "claude-e",
    "codex-pipe",
    "codex-tui",
)

IDLE_MAILBOX_STATUSES = {"", "idle", "none"}
IDLE_RELAY_STALL_STATES = {"", "healthy"}
RESTART_GUARD_FINALIZING_DRAIN_TIMEOUT_S = 30.0
RESTART_GUARD_POLL_INTERVAL_S = 1.0
DEFAULT_PROVIDER_HOLD_SECONDS = 60
RUNTIME_QUEUE_DIRS: tuple[tuple[str, str], ...] = (
    ("pending_queue", "discord_pending_queue"),
    ("queued_placeholders", "discord_queued_placeholders"),
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--base-url", default="http://127.0.0.1:8791")
    parser.add_argument(
        "--cell",
        required=True,
        choices=SUPPORTED_CELLS,
        help="Provider+runtime cell to drive (e.g. claude-pipe).",
    )
    parser.add_argument(
        "--channel-id",
        required=True,
        help="Discord channel id bound to the cell's worker agent.",
    )
    parser.add_argument(
        "--thread-channel-id",
        default=os.environ.get("AGENTDESK_E2E_THREAD_CHANNEL_ID"),
        help="Discord thread id for scenarios that exercise parent-channel thread relay.",
    )
    parser.add_argument(
        "--scenarios",
        default="tests/e2e/tui_relay/scenarios",
        help="Directory of YAML scenario files.",
    )
    parser.add_argument(
        "--filter",
        default=None,
        help="Comma-separated scenario ids (exact match, e.g. E-1,E-5).",
    )
    parser.add_argument("--output", default=None)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument(
        "--allow-destructive",
        action="store_true",
        help="Permit destructive steps. Also requires AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1.",
    )
    parser.add_argument(
        "--reset-before-each",
        action="store_true",
        default=True,
        help="Cancel inflight + truncate relay queue before every scenario.",
    )
    parser.add_argument(
        "--no-reset-before-each",
        dest="reset_before_each",
        action="store_false",
        help="Skip cancel/reset.",
    )
    parser.add_argument(
        "--queue-runtime-root",
        default=str(Path.home() / ".adk" / "release" / "runtime"),
        help="ADK runtime root used to truncate on-disk relay queues during reset.",
    )
    parser.add_argument(
        "--hard-reset-session-each",
        action="store_true",
        default=False,
        help="Kill the cell's tmux session before each scenario (claude TUI context burn).",
    )
    parser.add_argument(
        "--handoff-to-agent",
        default=None,
        help="Worker agent id whose channel binding receives send-to-agent prompts. "
        "Defaults to adk-<cell>-e2e.",
    )
    parser.add_argument(
        "--handoff-from-agent",
        default="adk-e2e-orchestrator",
        help="Source agent id stamped in the send-to-agent envelope.",
    )
    parser.add_argument(
        "--restart-script",
        default=os.environ.get("AGENTDESK_E2E_RESTART_SCRIPT"),
        help="Safe dcserver restart wrapper for restart_dcserver steps.",
    )
    parser.add_argument(
        "--restart-target-override",
        choices=("dev", "release"),
        default=os.environ.get("AGENTDESK_E2E_RESTART_TARGET"),
        help="Override restart_dcserver target.",
    )
    parser.add_argument(
        "--turn-start-timeout-s",
        type=float,
        default=float(os.environ.get("AGENTDESK_E2E_TURN_START_TIMEOUT_S", "180")),
        help="How long send_prompt retries transient mailbox-busy turn/start responses.",
    )
    return parser.parse_args()


def cell_provider(cell: str) -> str:
    return cell.split("-", 1)[0]


def cell_runtime(cell: str) -> str:
    return cell.split("-", 1)[1]


def cell_session_name(cell: str, *, thread_channel_id: str | None = None) -> str:
    """tmux session name owned by the cell's worker agent."""
    suffix = f"-t{thread_channel_id}" if thread_channel_id else ""
    return f"AgentDesk-{cell_provider(cell)}-adk-{cell}-e2e{suffix}"


def cell_default_agent(cell: str) -> str:
    return f"adk-{cell}-e2e"


def cell_channel_kind(cell: str) -> str:
    return "cdx" if cell_provider(cell) == "codex" else "cc"


def cell_workspace_substring(cell: str) -> str:
    """Substring tagged onto runtime/jsonl paths to safely target this cell."""
    return f"adk-{cell}-e2e"


def resolve_output_dir(arg: str | None, cell: str) -> Path:
    if arg:
        path = Path(arg)
    else:
        run_id = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
        path = Path("out/e2e/tui_relay") / cell / run_id
    path.mkdir(parents=True, exist_ok=True)
    return path


def load_scenarios(scenarios_dir: Path, *, cell: str) -> list[dict[str, Any]]:
    scenarios: list[dict[str, Any]] = []
    for yaml_path in sorted(scenarios_dir.glob("*.yaml")):
        with yaml_path.open("r", encoding="utf-8") as fp:
            data = yaml.safe_load(fp)
        if not isinstance(data, dict):
            raise ValueError(f"{yaml_path} did not parse to a mapping")
        cells = data.get("cells") or []
        if not isinstance(cells, list):
            raise ValueError(f"{yaml_path} has non-list cells field")
        if cell not in cells:
            continue
        data["__path__"] = str(yaml_path)
        scenarios.append(data)
    return scenarios


def is_destructive(scenario: dict[str, Any]) -> bool:
    for step in scenario.get("steps") or []:
        if not isinstance(step, dict):
            continue
        for key in (
            "restart_dcserver",
            "kill_pane",
            "kill_tui_process",
            "send_keys_no_enter",
            "poison_claude_tui_relay_offset",
            "cancel_turn",
        ):
            if key in step:
                return True
    return False


def _scenario_thread_channel_id(
    scenario: dict[str, Any], channel_id: str
) -> str | None:
    return channel_id if scenario.get("requires_thread_channel") else None


def scenario_channel_id(scenario: dict[str, Any], args: argparse.Namespace) -> str | None:
    if scenario.get("requires_thread_channel"):
        return str(args.thread_channel_id).strip() if args.thread_channel_id else None
    return str(args.channel_id)


def _truncate_queue_file(path: Path) -> None:
    try:
        path.write_text("[]")
    except OSError:
        pass


def hard_reset_provider_session(
    *,
    cell: str,
    channel_id: str,
    runtime_root: Path,
    thread_channel_id: str | None = None,
) -> dict[str, Any]:
    """Burn the cell's TUI session so the next prompt starts fresh."""
    session_name = cell_session_name(cell, thread_channel_id=thread_channel_id)
    workspace_substring = cell_workspace_substring(cell)
    summary: dict[str, Any] = {
        "cell": cell,
        "session_name": session_name,
        "actions": [],
    }
    if tmux.kill_session(session_name, reverify_substring=workspace_substring):
        summary["actions"].append("tmux_session_killed")

    sessions_root = runtime_root / "sessions"
    if sessions_root.is_dir():
        removed: list[str] = []
        for artefact in sessions_root.glob(f"*{session_name}*"):
            try:
                artefact.unlink()
                removed.append(artefact.name)
            except OSError:
                pass
        if removed:
            summary["actions"].append({"runtime_session_files_removed": removed})

    inflight_dir = runtime_root / "discord_inflight"
    provider = cell_provider(cell)
    inflight_path = inflight_dir / provider / f"{channel_id}.json"
    try:
        if inflight_path.exists():
            inflight_path.unlink()
            summary["actions"].append("inflight_cleared")
    except OSError:
        pass

    if provider == "claude":
        archived: list[str] = []
        for workspace in Path.home().joinpath(".claude/projects").glob(
            f"*{workspace_substring}*"
        ):
            for jsonl in workspace.glob("*.jsonl"):
                try:
                    target = Path("/tmp") / f"archived-{jsonl.name}-{int(time.time())}"
                    jsonl.rename(target)
                    archived.append(str(target))
                except OSError:
                    pass
        if archived:
            summary["actions"].append({"claude_jsonl_archived": archived})
    return summary


def reset_channel_state(
    *,
    base_url: str,
    channel_id: str,
    runtime_root: Path,
    provider: str,
) -> dict[str, Any]:
    summary: dict[str, Any] = {"channel_id": channel_id, "actions": []}
    try:
        summary["actions"].append(
            {
                "cancel_turn": cancel_turn(
                    base_url=base_url,
                    channel_id=channel_id,
                    force=True,
                    timeout_s=15,
                )
            }
        )
    except Exception as error:  # noqa: BLE001
        summary["actions"].append({"cancel_turn_error": f"{type(error).__name__}: {error}"})

    for kind, subdir in (
        ("pending_queue", "discord_pending_queue"),
        ("placeholders", "discord_queued_placeholders"),
    ):
        cleared: list[str] = []
        provider_dir = runtime_root / subdir / provider
        if provider_dir.is_dir():
            for token_dir in provider_dir.iterdir():
                target = token_dir / f"{channel_id}.json"
                if target.exists():
                    _truncate_queue_file(target)
                    cleared.append(str(target))
        summary["actions"].append({kind: cleared})
    return summary


def cancel_turn(
    *,
    base_url: str,
    channel_id: str,
    force: bool = True,
    timeout_s: float = 15.0,
) -> dict[str, Any]:
    """POST the reusable turn-cancel endpoint used by reset and scenarios."""

    url = f"{base_url.rstrip('/')}/api/turns/{channel_id}/cancel?force={'true' if force else 'false'}"
    req = urllib.request.Request(
        url,
        data=b"",
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout_s) as resp:
            raw = resp.read().decode("utf-8", "replace")
            status = int(getattr(resp, "status", 200))
    except urllib.error.HTTPError as error:
        body = error.read().decode("utf-8", "replace")
        raise assertions.AssertionError(
            f"cancel_turn HTTP {error.code} for channel={channel_id}: {body[:500]!r}"
        ) from error
    except urllib.error.URLError as error:
        raise assertions.AssertionError(
            f"cancel_turn URL error for channel={channel_id}: {error}"
        ) from error

    try:
        payload = json.loads(raw) if raw.strip() else {}
    except json.JSONDecodeError as error:
        raise assertions.AssertionError(
            f"cancel_turn returned non-JSON HTTP {status}: {raw[:500]!r}"
        ) from error
    if not isinstance(payload, dict):
        raise assertions.AssertionError(
            f"cancel_turn returned non-object HTTP {status}: {payload!r}"
        )
    if status >= 400 or payload.get("ok") is False:
        raise assertions.AssertionError(
            f"cancel_turn failed HTTP {status} for channel={channel_id}: "
            f"{_payload_summary(payload)}"
        )
    return {
        "ok": payload.get("ok"),
        "queued_remaining": payload.get("queued_remaining"),
        "queue_purged": payload.get("queue_purged"),
        "tmux_killed": payload.get("tmux_killed"),
        "lifecycle_path": payload.get("lifecycle_path"),
    }


def poison_claude_tui_relay_offset(
    *,
    cell: str,
    channel_id: str,
    runtime_root: Path,
) -> dict[str, Any]:
    """Force a stale Claude TUI offset so restart rehydrate must prefer launch state."""
    if cell_provider(cell) != "claude" or cell_runtime(cell) != "tui":
        raise assertions.AssertionError("poison_claude_tui_relay_offset requires claude-tui")
    session_name = cell_session_name(cell, thread_channel_id=channel_id)
    sessions_root = runtime_root / "sessions"
    sessions_root.mkdir(parents=True, exist_ok=True)
    matches = sorted(sessions_root.glob(f"*{session_name}.claude-tui-relay-offset.json"))
    offset_path = matches[0] if matches else sessions_root / (
        f"agentdesk-e2e-{session_name}.claude-tui-relay-offset.json"
    )
    stale_path = Path("/tmp") / f"agentdesk-e2e-stale-{session_name}.jsonl"
    try:
        stale_path.unlink()
    except OSError:
        pass
    payload = {"last_offset": 999_999_999, "output_path": str(stale_path)}
    offset_path.write_text(json.dumps(payload), encoding="utf-8")
    return {
        "session_name": session_name,
        "offset_path": str(offset_path),
        "stale_output_path": str(stale_path),
    }


def capture_session_identity(
    *,
    cell: str,
    channel_id: str,
    scenario: dict[str, Any],
) -> dict[str, Any]:
    session_name = cell_session_name(
        cell,
        thread_channel_id=_scenario_thread_channel_id(scenario, channel_id),
    )
    identity = tmux.session_identity(session_name)
    if identity is None:
        raise assertions.AssertionError(
            f"no tmux session identity for {session_name!r}"
        )
    return identity.as_dict()


def assert_session_preserved(
    *,
    before: dict[str, Any],
    cell: str,
    channel_id: str,
    scenario: dict[str, Any],
) -> dict[str, Any]:
    after = capture_session_identity(cell=cell, channel_id=channel_id, scenario=scenario)
    if after != before:
        raise assertions.AssertionError(
            "tmux session identity changed: "
            f"before={_payload_summary(before)} after={_payload_summary(after)}"
        )
    return after


def _normalize_concurrent_prompt_specs(
    params: Any,
    *,
    channel_id: str,
    cell: str,
) -> list[dict[str, Any]]:
    if isinstance(params, list):
        prompts = params
    elif isinstance(params, dict):
        prompts = params.get("prompts") or []
    else:
        raise assertions.AssertionError(
            f"send_prompts_concurrent requires a list or mapping: {params!r}"
        )
    if not isinstance(prompts, list) or not prompts:
        raise assertions.AssertionError("send_prompts_concurrent requires non-empty prompts")

    specs: list[dict[str, Any]] = []
    for idx, item in enumerate(prompts):
        if isinstance(item, str):
            specs.append(
                {
                    "index": idx,
                    "channel_id": channel_id,
                    "content": item,
                    "channel_kind": cell_channel_kind(cell),
                }
            )
            continue
        if not isinstance(item, dict):
            raise assertions.AssertionError(
                f"send_prompts_concurrent prompt {idx} must be string or mapping"
            )
        content = item.get("content") or item.get("send_prompt") or item.get("prompt")
        if content is None:
            raise assertions.AssertionError(
                f"send_prompts_concurrent prompt {idx} missing content"
            )
        target_channel = str(item.get("channel_id") or channel_id)
        specs.append(
            {
                "index": idx,
                "channel_id": target_channel,
                "content": str(content),
                "channel_kind": str(item.get("channel_kind") or cell_channel_kind(cell)),
            }
        )
    return specs


def send_prompts_concurrent(
    *,
    client: discord.DiscordClient,
    channel_id: str,
    cell: str,
    params: Any,
) -> list[dict[str, Any]]:
    """Start multiple prompt dispatches without the per-step sleep gap."""

    specs = _normalize_concurrent_prompt_specs(params, channel_id=channel_id, cell=cell)
    results: list[dict[str, Any]] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(specs)) as executor:
        futures = {
            executor.submit(
                client.send_prompt,
                spec["channel_id"],
                spec["content"],
                channel_kind=spec["channel_kind"],
            ): spec
            for spec in specs
        }
        for future in concurrent.futures.as_completed(futures):
            spec = futures[future]
            try:
                response = future.result()
            except Exception as error:  # noqa: BLE001 - report every failed branch
                raise assertions.AssertionError(
                    "send_prompts_concurrent failed "
                    f"index={spec['index']} channel={spec['channel_id']}: "
                    f"{type(error).__name__}: {error}"
                ) from error
            results.append(
                {
                    "index": spec["index"],
                    "channel_id": spec["channel_id"],
                    "message_id": response.get("message_id") or response.get("id"),
                }
            )
    return sorted(results, key=lambda item: int(item["index"]))


def build_provider_hold_prompt(params: Any, *, scenario_id: str) -> str:
    """Build a provider-agnostic stop-mid-turn prompt fixture.

    The fixture asks the model to emit an early marker, then block in its normal
    shell/terminal tool before any late marker can be produced. E-18 cancels
    during that blocking window.
    """

    if not isinstance(params, dict):
        raise assertions.AssertionError(
            f"send_provider_hold_prompt requires a mapping: {params!r}"
        )
    ok_marker = str(params.get("ok_marker") or params.get("marker") or "").strip()
    late_marker = str(params.get("late_marker") or "").strip()
    if not ok_marker:
        raise assertions.AssertionError("send_provider_hold_prompt requires ok_marker")
    if not late_marker:
        raise assertions.AssertionError("send_provider_hold_prompt requires late_marker")
    hold_seconds = int(params.get("hold_seconds") or DEFAULT_PROVIDER_HOLD_SECONDS)
    if hold_seconds <= 0:
        raise assertions.AssertionError(
            f"send_provider_hold_prompt hold_seconds must be positive: {hold_seconds}"
        )

    return (
        f"E2E {scenario_id} stop-mid-turn cancellation fixture.\n\n"
        "Follow these steps exactly:\n"
        f"1. First, emit assistant text containing exactly one line: {ok_marker}\n"
        "2. Immediately after that line, use your normal shell/terminal command tool to run:\n"
        f"   python3 -c \"import time; time.sleep({hold_seconds})\"\n"
        f"3. Do not write, echo, quote, or mention {late_marker} before the command returns.\n"
        f"4. Only if the turn is still running after the command returns, send exactly: {late_marker}\n\n"
        "The harness will cancel this turn while the command is sleeping. If cancellation arrives, stop without sending the late marker."
    )


def provider_inflight_state_path(
    *, runtime_root: str | Path, provider: str, channel_id: str
) -> Path:
    return Path(runtime_root) / "discord_inflight" / provider / f"{channel_id}.json"


def validate_provider_hold_runtime_root(runtime_root: str | Path) -> Path:
    root = Path(runtime_root)
    if not root.is_dir():
        raise assertions.AssertionError(
            f"queue_runtime_root does not exist or is not a directory: {root}"
        )
    known_runtime_children = ("discord_inflight", "sessions", "dcserver.lock")
    if not any((root / child).exists() for child in known_runtime_children):
        raise assertions.AssertionError(
            "queue_runtime_root does not look like an AgentDesk runtime root: "
            f"{root} (expected one of {', '.join(known_runtime_children)})"
        )
    return root


def _parse_discord_turn_id(turn_id: str) -> dict[str, str] | None:
    parts = turn_id.split(":")
    if len(parts) != 3 or parts[0] != "discord":
        return None
    channel_id, user_msg_id = parts[1], parts[2]
    if not channel_id.isdigit() or not user_msg_id.isdigit():
        return None
    return {"turn_id": turn_id, "channel_id": channel_id, "user_msg_id": user_msg_id}


def turn_identity_from_send_response(
    response: dict[str, Any], *, channel_id: str
) -> dict[str, str]:
    """Extract the current turn identity from a prompt send response."""

    def _with_optional_fields(identity: dict[str, str]) -> dict[str, str]:
        for key in ("dispatch_id", "started_at", "born_generation"):
            value = str(response.get(key) or "").strip()
            if value:
                identity[key] = value
        return identity

    turn_id = str(response.get("turn_id") or "").strip()
    if turn_id:
        parsed = _parse_discord_turn_id(turn_id)
        if parsed is None:
            raise assertions.AssertionError(
                f"turn/start returned malformed turn_id: {turn_id!r}"
            )
        if parsed["channel_id"] != str(channel_id):
            raise assertions.AssertionError(
                "turn/start channel mismatch: "
                f"response={parsed['channel_id']} expected={channel_id}"
            )
        return _with_optional_fields(parsed)

    message_id = str(response.get("message_id") or response.get("id") or "").strip()
    if message_id.isdigit():
        return _with_optional_fields(
            {"channel_id": str(channel_id), "user_msg_id": message_id}
        )

    raise assertions.AssertionError(
        "prompt send response did not include turn_id, message_id, or id; "
        f"cannot bind provider hold witness to the current turn: {response!r}"
    )


def _state_identity_summary(state: dict[str, Any]) -> str:
    return (
        f"channel_id={state.get('channel_id')!r} "
        f"user_msg_id={state.get('user_msg_id')!r} "
        f"dispatch_id={state.get('dispatch_id')!r} "
        f"started_at={state.get('started_at')!r} "
        f"born_generation={state.get('born_generation')!r}"
    )


def _provider_hold_identity_mismatch(
    state: dict[str, Any], expected_identity: dict[str, str]
) -> str | None:
    expected_channel = str(expected_identity.get("channel_id") or "")
    expected_user_msg = str(expected_identity.get("user_msg_id") or "")
    if not expected_channel or not expected_user_msg:
        return "expected turn identity is missing channel_id or user_msg_id"

    actual_channel = str(state.get("channel_id") or "")
    actual_user_msg = str(state.get("user_msg_id") or "")
    if actual_channel != expected_channel:
        return f"channel_id {actual_channel!r} != expected {expected_channel!r}"
    if actual_user_msg != expected_user_msg:
        return f"user_msg_id {actual_user_msg!r} != expected {expected_user_msg!r}"

    expected_dispatch = str(expected_identity.get("dispatch_id") or "").strip()
    if expected_dispatch:
        actual_dispatch = str(state.get("dispatch_id") or "").strip()
        if actual_dispatch != expected_dispatch:
            return (
                f"dispatch_id {actual_dispatch!r} != expected {expected_dispatch!r}"
            )
    expected_started_at = str(expected_identity.get("started_at") or "").strip()
    if expected_started_at:
        actual_started_at = str(state.get("started_at") or "").strip()
        if actual_started_at != expected_started_at:
            return (
                f"started_at {actual_started_at!r} != expected {expected_started_at!r}"
            )
    expected_generation = str(expected_identity.get("born_generation") or "").strip()
    if expected_generation:
        actual_generation = str(state.get("born_generation") or "").strip()
        if actual_generation != expected_generation:
            return (
                f"born_generation {actual_generation!r} "
                f"!= expected {expected_generation!r}"
            )
    return None


def _provider_hold_state_summary(
    state: dict[str, Any], *, ok_marker: str, late_marker: str
) -> str:
    full_response = str(state.get("full_response") or "")
    return (
        f"{_state_identity_summary(state)} "
        f"full_response_len={len(full_response)} "
        f"ok_seen={ok_marker in full_response} "
        f"late_seen={late_marker in full_response} "
        f"any_tool_used={state.get('any_tool_used') is True} "
        f"has_post_tool_text={state.get('has_post_tool_text') is True} "
        f"terminal_delivery_committed={state.get('terminal_delivery_committed') is True}"
    )


def wait_for_provider_hold_state(
    *,
    runtime_root: str | Path,
    provider: str,
    channel_id: str,
    expected_identity: dict[str, str],
    ok_marker: str,
    late_marker: str,
    timeout_s: float,
    poll_interval_s: float,
) -> dict[str, Any]:
    """Wait until the provider has emitted pre-tool OK text and entered a tool hold.

    E-18 must cancel while the provider is still active. Waiting for the OK
    marker on Discord is too late for TUI relay paths because pre-tool and
    post-tool assistant text may be delivered in one terminal replacement.
    The durable inflight row is the stable witness that OK was captured before
    the tool call and no post-tool text has been produced yet. The row must
    match the current prompt's turn identity so stale E-18 rows cannot satisfy
    or fail the current run.
    """

    if timeout_s <= 0:
        raise assertions.AssertionError(
            f"wait_for_provider_hold_state timeout_s must be positive: {timeout_s}"
        )
    if poll_interval_s <= 0:
        raise assertions.AssertionError(
            "wait_for_provider_hold_state poll_interval_s must be positive: "
            f"{poll_interval_s}"
        )
    root = validate_provider_hold_runtime_root(runtime_root)
    path = provider_inflight_state_path(
        runtime_root=root,
        provider=provider,
        channel_id=channel_id,
    )
    deadline = time.monotonic() + timeout_s
    last_state = f"inflight state missing at {path}"
    while time.monotonic() < deadline:
        try:
            raw = path.read_text(encoding="utf-8")
        except FileNotFoundError:
            last_state = f"inflight state missing at {path}"
        except OSError as error:
            last_state = f"inflight state unreadable at {path}: {error}"
        else:
            try:
                state = json.loads(raw)
            except json.JSONDecodeError as error:
                last_state = f"inflight state invalid JSON at {path}: {error}"
            else:
                if not isinstance(state, dict):
                    last_state = f"inflight state is not an object at {path}"
                else:
                    summary = _provider_hold_state_summary(
                        state,
                        ok_marker=ok_marker,
                        late_marker=late_marker,
                    )
                    identity_mismatch = _provider_hold_identity_mismatch(
                        state,
                        expected_identity,
                    )
                    if identity_mismatch:
                        last_state = f"identity_mismatch={identity_mismatch}; {summary}"
                        time.sleep(poll_interval_s)
                        continue
                    full_response = str(state.get("full_response") or "")
                    if late_marker in full_response:
                        raise assertions.AssertionError(
                            "late marker appeared in provider response before "
                            f"the cancel step could observe a hold: {summary}"
                        )
                    if state.get("terminal_delivery_committed") is True:
                        raise assertions.AssertionError(
                            "turn delivered before provider hold was observed: "
                            f"{summary}"
                        )
                    if (
                        ok_marker in full_response
                        and state.get("any_tool_used") is True
                        and state.get("has_post_tool_text") is False
                    ):
                        # Both Claude and Codex relay parsers persist
                        # `any_tool_used` from provider tool_use frames. If a
                        # provider stops doing that, this current-turn witness
                        # times out instead of matching stale content.
                        return {
                            "path": str(path),
                            "turn_identity": dict(expected_identity),
                            "full_response_len": len(full_response),
                            "ok_marker_seen": True,
                            "any_tool_used": True,
                            "has_post_tool_text": bool(
                                state.get("has_post_tool_text")
                            ),
                        }
                    last_state = summary
        time.sleep(poll_interval_s)

    raise assertions.AssertionError(
        "timeout waiting for provider hold state before cancel: "
        f"path={path} last_state={last_state}"
    )


def scenario_teardown_marker(scenario_id: str, *, cell: str, run_id: str) -> str:
    return f"### E2E TEARDOWN {scenario_id} cell={cell} run={run_id}"


def send_teardown_marker(
    *,
    client: discord.DiscordClient,
    channel_id: str,
    scenario_id: str,
    cell: str,
    run_id: str,
) -> dict[str, Any]:
    return client.send_control(
        channel_id,
        scenario_teardown_marker(scenario_id, cell=cell, run_id=run_id),
    )


def _read_api_json(base_url: str, path: str, *, timeout: float = 5.0) -> tuple[int, Any]:
    url = f"{base_url.rstrip('/')}{path}"
    request = urllib.request.Request(
        url,
        method="GET",
        headers={"Connection": "close"},
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            raw = response.read().decode("utf-8", "replace")
            status = int(getattr(response, "status", 200))
    except urllib.error.HTTPError as error:
        raw = error.read().decode("utf-8", "replace")
        status = int(error.code)
    if not raw.strip():
        return status, {}
    try:
        return status, json.loads(raw)
    except json.JSONDecodeError as error:
        raise assertions.AssertionError(
            f"{path} returned non-JSON HTTP {status}: {raw[:240]!r}"
        ) from error


def _payload_summary(payload: Any, *, max_chars: int = 500) -> str:
    try:
        text = json.dumps(payload, ensure_ascii=False, sort_keys=True)
    except TypeError:
        text = repr(payload)
    if len(text) > max_chars:
        return f"{text[:max_chars]}..."
    return text


def _read_health_detail(base_url: str, *, timeout: float = 5.0) -> dict[str, Any]:
    status, payload = _read_api_json(base_url, "/api/health/detail", timeout=timeout)
    if not isinstance(payload, dict):
        raise assertions.AssertionError(
            f"/api/health/detail returned non-object HTTP {status}: {payload!r}"
        )
    if status in {401, 403}:
        raise assertions.AssertionError(
            f"/api/health/detail unavailable HTTP {status}: {payload}"
        )
    if "mailboxes" not in payload:
        raise assertions.AssertionError(
            "/api/health/detail missing mailboxes "
            f"(HTTP {status}, payload={_payload_summary(payload)})"
        )
    return payload


def _health_reason_allowed(reason: str, allowed: tuple[str, ...]) -> bool:
    for pattern in allowed:
        if pattern.endswith("*") and reason.startswith(pattern[:-1]):
            return True
        if reason == pattern:
            return True
    return False


def _health_ready_violations(
    payload: dict[str, Any],
    *,
    allowed_statuses: tuple[str, ...] = ("healthy",),
    allowed_degraded_reasons: tuple[str, ...] = (),
) -> list[str]:
    status = str(payload.get("status") or "").lower()
    allowed_status_set = {s.lower() for s in allowed_statuses}
    strict_healthy = allowed_status_set == {"healthy"}
    violations: list[str] = []
    if status not in allowed_status_set:
        violations.append(f"status={status or '<missing>'}")
    if strict_healthy and payload.get("ok") is False:
        violations.append("ok=false")
    if strict_healthy and payload.get("degraded") is True:
        violations.append("degraded=true")
    if strict_healthy and payload.get("fully_recovered") is False:
        violations.append("fully_recovered=false")

    degraded_reasons = payload.get("degraded_reasons") or []
    if isinstance(degraded_reasons, list):
        blocked = [
            str(reason)
            for reason in degraded_reasons
            if not _health_reason_allowed(str(reason), allowed_degraded_reasons)
        ]
        if blocked:
            violations.append(f"degraded_reasons={blocked}")
    elif degraded_reasons:
        violations.append(f"degraded_reasons={degraded_reasons!r}")
    return violations


def _health_summary(
    *,
    http_status: int | None,
    payload: dict[str, Any] | None,
    violations: list[str] | None = None,
    last_error: str | None = None,
) -> str:
    if payload is None:
        suffix = f" error={last_error}" if last_error else ""
        return f"http={http_status or '<none>'} payload=<unavailable>{suffix}"
    fields = {
        "http": http_status,
        "status": payload.get("status"),
        "ok": payload.get("ok"),
        "fully_recovered": payload.get("fully_recovered"),
        "degraded": payload.get("degraded"),
        "degraded_reasons": payload.get("degraded_reasons"),
        "violations": violations or [],
    }
    if last_error:
        fields["last_error"] = last_error
    return json.dumps(fields, ensure_ascii=False, sort_keys=True)


def _as_string_tuple(value: Any, *, default: tuple[str, ...] = ()) -> tuple[str, ...]:
    if value is None:
        return default
    if isinstance(value, str):
        return (value,)
    if isinstance(value, list):
        return tuple(str(item) for item in value)
    return (str(value),)


def _reason_matches(pattern: str, reason: str) -> bool:
    if pattern.endswith("*"):
        return reason.startswith(pattern[:-1])
    return pattern == reason or pattern in reason


def _as_counter_int(value: Any) -> int | None:
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, int):
        return value
    if isinstance(value, str):
        try:
            return int(value)
        except ValueError:
            return None
    return None


def _counter_from_payloads(
    name: str, payloads: list[dict[str, Any]]
) -> tuple[int | None, str | None]:
    for payload in payloads:
        if name in payload:
            return _as_counter_int(payload.get(name)), name
    return None, None


def assert_health(
    base_url: str,
    params: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Scenario-level health probe with explicit status/reason/counter checks."""

    options = params or {}
    timeout_s = float(options.get("timeout_s") or 0)
    poll_interval_s = float(options.get("poll_interval_s", 1.0))
    attempts = 0
    max_attempts = (
        max(1, int(timeout_s / max(poll_interval_s, 0.1)) + 2)
        if timeout_s > 0
        else 1
    )
    deadline = time.monotonic() + max(timeout_s, 0.0)
    last_error: assertions.AssertionError | None = None
    while attempts < max_attempts:
        attempts += 1
        try:
            return _assert_health_once(base_url, options)
        except assertions.AssertionError as error:
            last_error = error
            if timeout_s <= 0 or time.monotonic() >= deadline:
                raise
            time.sleep(poll_interval_s)

    if last_error is not None:
        raise assertions.AssertionError(
            f"assert_health did not pass within {timeout_s}s: {last_error}"
        ) from last_error
    raise assertions.AssertionError("assert_health failed without a captured error")


def _assert_health_once(
    base_url: str,
    options: dict[str, Any],
) -> dict[str, Any]:
    """Single health probe attempt for assert_health polling."""

    status_code, health = _read_api_json(base_url, "/api/health", timeout=5)
    if not isinstance(health, dict):
        raise assertions.AssertionError(
            f"assert_health /api/health returned non-object HTTP {status_code}: "
            f"{health!r}"
        )

    required_statuses = _as_string_tuple(
        options.get("require_status") or options.get("allowed_statuses"),
        default=("healthy",),
    )
    forbidden_reasons = _as_string_tuple(options.get("forbid_degraded_reasons"))
    degraded_reasons = health.get("degraded_reasons") or []
    if not isinstance(degraded_reasons, list):
        degraded_reasons = [degraded_reasons]
    if "allowed_degraded_reasons" in options:
        allowed_degraded_reasons = _as_string_tuple(options.get("allowed_degraded_reasons"))
    elif forbidden_reasons and {s.lower() for s in required_statuses} != {"healthy"}:
        allowed_degraded_reasons = tuple(str(reason) for reason in degraded_reasons)
    else:
        allowed_degraded_reasons = ()
    violations = _health_ready_violations(
        health,
        allowed_statuses=required_statuses,
        allowed_degraded_reasons=allowed_degraded_reasons,
    )

    blocked_reasons = [
        str(reason)
        for reason in degraded_reasons
        if any(_reason_matches(pattern, str(reason)) for pattern in forbidden_reasons)
    ]
    if blocked_reasons:
        violations.append(f"forbidden_degraded_reasons={blocked_reasons}")

    counter_payloads = [health]
    needs_detail = any(
        key in options for key in ("global_active_max", "global_finalizing_max")
    )
    detail: dict[str, Any] | None = None
    if needs_detail:
        detail = _read_health_detail(base_url)
        counter_payloads.insert(0, detail)

    counter_values: dict[str, int] = {}
    for counter_name, option_name in (
        ("global_active", "global_active_max"),
        ("global_finalizing", "global_finalizing_max"),
    ):
        if option_name not in options:
            continue
        actual, source_key = _counter_from_payloads(counter_name, counter_payloads)
        if source_key is None:
            violations.append(f"{counter_name}=<missing>")
            continue
        if actual is None:
            violations.append(f"{source_key}=<invalid>")
            continue
        counter_values[counter_name] = actual
        if actual < 0:
            violations.append(f"{source_key}={actual} < 0")
        maximum = int(options[option_name])
        if actual > maximum:
            violations.append(f"{source_key}={actual} > {maximum}")

    if status_code < 200 or status_code >= 300:
        violations.append(f"http={status_code}")
    if violations:
        summary_payload = dict(health)
        if detail is not None:
            summary_payload["detail_counters"] = {
                "global_active": detail.get("global_active"),
                "global_finalizing": detail.get("global_finalizing"),
            }
        raise assertions.AssertionError(
            "assert_health failed: "
            f"{_health_summary(http_status=status_code, payload=summary_payload, violations=violations)}"
        )

    return {
        "http": status_code,
        "status": health.get("status"),
        "degraded_reasons": degraded_reasons,
        **counter_values,
    }


def _as_nonnegative_int(value: Any) -> int:
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, int):
        return max(value, 0)
    if isinstance(value, str) and value.isdigit():
        return int(value)
    return 0


def _truthy_identity(value: Any) -> bool:
    if value is None or value is False:
        return False
    if isinstance(value, (int, float)) and value == 0:
        return False
    if isinstance(value, str) and value.strip() in {"", "0", "none", "null"}:
        return False
    return True


def _mailbox_channel_id(mailbox: dict[str, Any]) -> str:
    value = mailbox.get("channel_id") or mailbox.get("channelId")
    return str(value or "")


def _mailbox_provider(mailbox: dict[str, Any]) -> str:
    return str(mailbox.get("provider") or mailbox.get("session_provider") or "").lower()


def _relay_health(mailbox: dict[str, Any]) -> dict[str, Any]:
    relay = mailbox.get("relay_health")
    return relay if isinstance(relay, dict) else {}


def _mailbox_busy_reasons(mailbox: dict[str, Any]) -> list[str]:
    reasons: list[str] = []
    relay = _relay_health(mailbox)

    status = str(mailbox.get("agent_turn_status") or "").lower()
    if status not in IDLE_MAILBOX_STATUSES:
        reasons.append(f"agent_turn_status={status}")
    if mailbox.get("has_cancel_token") is True:
        reasons.append("has_cancel_token=true")
    if mailbox.get("inflight_state_present") is True:
        reasons.append("inflight_state_present=true")
    if _as_nonnegative_int(mailbox.get("queue_depth")) > 0:
        reasons.append(f"queue_depth={mailbox.get('queue_depth')}")
    if mailbox.get("recovery_started") is True:
        reasons.append("recovery_started=true")
    if mailbox.get("active_dispatch_present") is True:
        reasons.append("active_dispatch_present=true")
    if _truthy_identity(mailbox.get("active_user_message_id")):
        reasons.append(f"active_user_message_id={mailbox.get('active_user_message_id')}")

    relay_active_turn = str(relay.get("active_turn") or "").lower()
    if relay_active_turn not in {"", "none"}:
        reasons.append(f"relay_health.active_turn={relay_active_turn}")
    if relay.get("bridge_inflight_present") is True:
        reasons.append("relay_health.bridge_inflight_present=true")
    if relay.get("mailbox_has_cancel_token") is True:
        reasons.append("relay_health.mailbox_has_cancel_token=true")
    if _truthy_identity(relay.get("mailbox_active_user_msg_id")):
        reasons.append(
            f"relay_health.mailbox_active_user_msg_id={relay.get('mailbox_active_user_msg_id')}"
        )
    if _as_nonnegative_int(relay.get("queue_depth")) > 0:
        reasons.append(f"relay_health.queue_depth={relay.get('queue_depth')}")
    if _truthy_identity(relay.get("pending_discord_callback_msg_id")):
        reasons.append(
            "relay_health.pending_discord_callback_msg_id="
            f"{relay.get('pending_discord_callback_msg_id')}"
        )
    if relay.get("pending_thread_proof") is True:
        reasons.append("relay_health.pending_thread_proof=true")
    if relay.get("stale_thread_proof") is True:
        reasons.append("relay_health.stale_thread_proof=true")
    if relay.get("desynced") is True:
        reasons.append("relay_health.desynced=true")

    stall_state = str(mailbox.get("relay_stall_state") or "").lower()
    if stall_state not in IDLE_RELAY_STALL_STATES:
        reasons.append(f"relay_stall_state={stall_state}")
    return reasons


def _runtime_payload_has_entries(payload: Any) -> bool:
    if payload in (None, False, "", [], {}):
        return False
    if isinstance(payload, list):
        return len(payload) > 0
    if isinstance(payload, dict):
        return any(_runtime_payload_has_entries(value) for value in payload.values())
    return True


def _runtime_queue_violations(
    *, runtime_root: Path, provider: str, channel_id: str
) -> list[str]:
    violations: list[str] = []
    for label, subdir in RUNTIME_QUEUE_DIRS:
        provider_dir = runtime_root / subdir / provider
        if not provider_dir.is_dir():
            continue
        for token_dir in provider_dir.iterdir():
            target = token_dir / f"{channel_id}.json"
            if not target.exists():
                continue
            try:
                raw = target.read_text(encoding="utf-8").strip()
                payload = json.loads(raw) if raw else []
            except (OSError, json.JSONDecodeError) as error:
                violations.append(f"{label}:{target}: unreadable:{error}")
                continue
            if _runtime_payload_has_entries(payload):
                violations.append(f"{label}:{target}: nonempty")
    return violations


def _mailbox_label(mailbox: dict[str, Any]) -> str:
    return (
        f"{_mailbox_provider(mailbox) or '<provider?>'}:"
        f"{_mailbox_channel_id(mailbox) or '<channel?>'}"
    )


def assert_cell_idle(
    *,
    base_url: str,
    channel_id: str,
    cell: str,
    runtime_root: Path,
    timeout_s: float = 45.0,
    poll_interval_s: float = 2.0,
) -> dict[str, Any]:
    provider = cell_provider(cell)
    deadline = time.monotonic() + timeout_s
    last_violations: list[str] = []
    last_error: str | None = None
    last_mailbox_count = 0

    while time.monotonic() < deadline:
        try:
            detail = _read_health_detail(base_url)
            mailboxes = detail.get("mailboxes")
            if not isinstance(mailboxes, list):
                raise assertions.AssertionError(
                    "/api/health/detail mailboxes is not a list "
                    f"(payload={_payload_summary(detail)})"
                )
            last_error = None
        except Exception as error:  # noqa: BLE001 - poll through transient health errors
            last_error = f"{type(error).__name__}: {error}"
            time.sleep(poll_interval_s)
            continue

        target_mailboxes = [
            mailbox
            for mailbox in mailboxes
            if isinstance(mailbox, dict)
            and _mailbox_channel_id(mailbox) == str(channel_id)
            and _mailbox_provider(mailbox) == provider
        ]
        last_mailbox_count = len(target_mailboxes)
        last_violations = []
        if not target_mailboxes:
            last_violations.append(
                f"no matching mailbox for provider={provider} channel={channel_id}"
            )
        for mailbox in target_mailboxes:
            for reason in _mailbox_busy_reasons(mailbox):
                last_violations.append(f"{_mailbox_label(mailbox)} {reason}")
        last_violations.extend(
            _runtime_queue_violations(
                runtime_root=runtime_root,
                provider=provider,
                channel_id=str(channel_id),
            )
        )

        if not last_violations:
            return {
                "channel_id": str(channel_id),
                "provider": provider,
                "mailboxes_seen": last_mailbox_count,
                "status": "idle",
            }
        time.sleep(poll_interval_s)

    raise assertions.AssertionError(
        f"post-scenario idle check failed for {cell} channel={channel_id}: "
        f"{last_violations}; mailboxes_seen={last_mailbox_count}; "
        f"last_error={last_error or '<none>'}"
    )


def run_scenario(
    scenario: dict[str, Any],
    *,
    args: argparse.Namespace,
    run_id: str,
    client: discord.DiscordClient,
) -> dict[str, Any]:
    scenario_id = str(scenario.get("id"))
    cell = args.cell
    result: dict[str, Any] = {
        "id": scenario_id,
        "path": scenario.get("__path__"),
        "cell": cell,
        "channel_id": args.channel_id,
        "status": "skipped",
        "reason": None,
        "started_at": dt.datetime.now().isoformat(timespec="seconds"),
        "assertions": [],
    }

    target_channel_id = scenario_channel_id(scenario, args)
    if target_channel_id is None:
        result["reason"] = "requires --thread-channel-id or AGENTDESK_E2E_THREAD_CHANNEL_ID"
        return result
    result["channel_id"] = target_channel_id

    if scenario.get("skip_reason"):
        result["reason"] = str(scenario["skip_reason"])
        result["acceptance_criteria"] = scenario.get("acceptance_criteria")
        return result

    destructive = is_destructive(scenario)
    if destructive and not (
        args.allow_destructive
        and os.environ.get("AGENTDESK_E2E_ALLOW_DESTRUCTIVE") == "1"
    ):
        result["status"] = "skipped"
        result["reason"] = (
            "destructive: requires --allow-destructive AND AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1"
        )
        return result

    if args.reset_before_each and not args.dry_run:
        runtime_root = Path(args.queue_runtime_root)
        result["resets"] = [
            reset_channel_state(
                base_url=args.base_url,
                channel_id=target_channel_id,
                runtime_root=runtime_root,
                provider=cell_provider(cell),
            )
        ]
        if args.hard_reset_session_each:
            result["hard_resets"] = [
                hard_reset_provider_session(
                    cell=cell,
                    channel_id=target_channel_id,
                    runtime_root=runtime_root,
                    thread_channel_id=(
                        target_channel_id if scenario.get("requires_thread_channel") else None
                    ),
                )
            ]
        time.sleep(2.0)

    try:
        window = run_one_cell(
            scenario=scenario,
            cell=cell,
            channel_id=target_channel_id,
            client=client,
            run_id=run_id,
            dry_run=args.dry_run,
            args=args,
        )
        result["assertions"].extend(window["assertions"])
        result["relay_count"] = window.get("relay_count")
        result["raw_count"] = window.get("raw_count")
        result["sample_relay"] = window.get("sample_relay")
        result["status"] = "pass"
    except assertions.AssertionError as error:
        result["status"] = "fail"
        result["reason"] = f"assertion: {error}"
        if not args.dry_run:
            try:
                send_teardown_marker(
                    client=client,
                    channel_id=target_channel_id,
                    scenario_id=scenario_id,
                    cell=cell,
                    run_id=run_id,
                )
            except Exception as teardown_error:  # noqa: BLE001 - report without masking failure
                result["teardown_error"] = (
                    f"{type(teardown_error).__name__}: {teardown_error}"
                )
    except Exception as error:  # noqa: BLE001 — surfaced in report
        result["status"] = "fail"
        result["reason"] = f"{type(error).__name__}: {error}"
        if not args.dry_run:
            try:
                send_teardown_marker(
                    client=client,
                    channel_id=target_channel_id,
                    scenario_id=scenario_id,
                    cell=cell,
                    run_id=run_id,
                )
            except Exception as teardown_error:  # noqa: BLE001 - report without masking failure
                result["teardown_error"] = (
                    f"{type(teardown_error).__name__}: {teardown_error}"
                )

    result["completed_at"] = dt.datetime.now().isoformat(timespec="seconds")
    return result


def run_one_cell(
    *,
    scenario: dict[str, Any],
    cell: str,
    channel_id: str,
    client: discord.DiscordClient,
    run_id: str,
    dry_run: bool,
    args: argparse.Namespace,
) -> dict[str, Any]:
    scenario_id = scenario.get("id")
    setup_marker = f"### E2E SETUP {scenario_id} cell={cell} run={run_id}"
    record: dict[str, Any] = {"assertions": []}

    if dry_run:
        print(f"[dry-run] {scenario_id} ({cell}): would send setup → steps → teardown")
        return record

    setup_resp = client.send_control(channel_id, setup_marker)
    setup_marker_id = str(setup_resp.get("message_id") or setup_resp.get("id") or "")
    after_id = setup_marker_id
    window = assertions.Window(setup_marker_id=setup_marker_id)
    time.sleep(8.0)

    def _ingest_observed(messages: list[dict[str, Any]]) -> None:
        for message in sorted(messages, key=lambda m: int(m.get("id", "0"))):
            if (message.get("content") or "").startswith("### E2E TEARDOWN"):
                window.teardown_marker_id = str(message.get("id"))
                continue
            window.add(message)

    first_send_done = False

    def _advance_window_past_setup_echo() -> None:
        nonlocal after_id
        tail = client.fetch_messages(channel_id, after_id=after_id, limit=100)
        if not tail:
            return
        latest = max(int(m.get("id", "0")) for m in tail)
        after_id = str(latest)
        window.raw_messages = []
        window.messages = []

    def _prepare_first_prompt_window() -> None:
        nonlocal first_send_done
        if not first_send_done:
            _advance_window_past_setup_echo()
            first_send_done = True

    last_turn_identity: dict[str, str] | None = None

    for step in scenario.get("steps") or []:
        if not isinstance(step, dict):
            continue
        if "send_prompt" in step:
            _prepare_first_prompt_window()
            window.mark_prompt_sent()
            response = client.send_prompt(
                channel_id,
                step["send_prompt"],
                channel_kind=cell_channel_kind(cell),
            )
            last_turn_identity = turn_identity_from_send_response(
                response,
                channel_id=channel_id,
            )
            time.sleep(3)
        elif "send_provider_hold_prompt" in step:
            _prepare_first_prompt_window()
            prompt = build_provider_hold_prompt(
                step["send_provider_hold_prompt"],
                scenario_id=str(scenario_id),
            )
            window.mark_prompt_sent()
            response = client.send_prompt(
                channel_id,
                prompt,
                channel_kind=cell_channel_kind(cell),
            )
            last_turn_identity = turn_identity_from_send_response(
                response,
                channel_id=channel_id,
            )
            record.setdefault("provider_hold_prompts", []).append(
                {
                    "hold_seconds": int(
                        (step["send_provider_hold_prompt"] or {}).get(
                            "hold_seconds",
                            DEFAULT_PROVIDER_HOLD_SECONDS,
                        )
                    ),
                    "turn_identity": dict(last_turn_identity),
                }
            )
            time.sleep(3)
        elif "send_prompts_concurrent" in step:
            _prepare_first_prompt_window()
            params = step["send_prompts_concurrent"]
            batch = send_prompts_concurrent(
                client=client,
                channel_id=channel_id,
                cell=cell,
                params=params,
            )
            for _ in batch:
                window.mark_prompt_sent()
            record.setdefault("concurrent_prompt_batches", []).append(batch)
        elif "wait_idle_s" in step:
            time.sleep(float(step["wait_idle_s"]))
        elif "wait_for_discord_text" in step:
            needle = step["wait_for_discord_text"]
            predicate = lambda message: (  # noqa: E731
                assertions.is_relay_response(message)
                and needle in (message.get("content") or "")
            )
            found, observed = client.wait_for_message(
                channel_id,
                predicate=predicate,
                after_id=after_id,
                timeout_s=float(step.get("timeout_s", 240)),
                debug_label=f"{scenario.get('id')}::{cell}::wait_for_text:{needle[:32]}",
            )
            _ingest_observed(observed)
            if not found:
                raise assertions.AssertionError(
                    f"timeout waiting for Discord text {needle!r}"
                )
        elif "wait_for_raw_discord_text" in step:
            needle = step["wait_for_raw_discord_text"]
            predicate = lambda message: (  # noqa: E731
                not assertions.is_our_send(message)
                and needle in (message.get("content") or "")
            )
            found, observed = client.wait_for_message(
                channel_id,
                predicate=predicate,
                after_id=after_id,
                timeout_s=float(step.get("timeout_s", 240)),
                debug_label=f"{scenario.get('id')}::{cell}::wait_for_raw:{needle[:32]}",
            )
            _ingest_observed(observed)
            if not found:
                raise assertions.AssertionError(
                    f"timeout waiting for raw Discord text {needle!r}"
                )
        elif "wait_for_provider_hold_state" in step:
            params = step["wait_for_provider_hold_state"] or {}
            ok_marker = str(params.get("ok_marker") or params.get("marker") or "").strip()
            late_marker = str(params.get("late_marker") or "").strip()
            if not ok_marker:
                raise assertions.AssertionError(
                    "wait_for_provider_hold_state requires ok_marker"
                )
            if not late_marker:
                raise assertions.AssertionError(
                    "wait_for_provider_hold_state requires late_marker"
                )
            if last_turn_identity is None:
                raise assertions.AssertionError(
                    "wait_for_provider_hold_state requires a preceding prompt send "
                    "with a turn identity"
                )
            record.setdefault("provider_hold_states", []).append(
                wait_for_provider_hold_state(
                    runtime_root=Path(args.queue_runtime_root),
                    provider=cell_provider(cell),
                    channel_id=channel_id,
                    expected_identity=last_turn_identity,
                    ok_marker=ok_marker,
                    late_marker=late_marker,
                    timeout_s=float(params.get("timeout_s", 180)),
                    poll_interval_s=float(params.get("poll_interval_s", 1)),
                )
            )
        elif "restart_dcserver" in step:
            target = args.restart_target_override or (step["restart_dcserver"] or {}).get(
                "target", "release"
            )
            restart_dcserver_for_e2e(
                target=target,
                args=args,
                base_url=client.base_url,
                cell=cell,
                channel_id=channel_id,
            )
        elif "poison_claude_tui_relay_offset" in step:
            record.setdefault("poisoned_offsets", []).append(
                poison_claude_tui_relay_offset(
                    cell=cell,
                    channel_id=channel_id,
                    runtime_root=Path(args.queue_runtime_root),
                )
            )
        elif "capture_session_identity" in step:
            params = step["capture_session_identity"] or {}
            label = str(params.get("label") or "default")
            record.setdefault("session_identities", {})[label] = capture_session_identity(
                cell=cell,
                channel_id=channel_id,
                scenario=scenario,
            )
        elif "assert_session_preserved" in step:
            raw_params = step["assert_session_preserved"]
            params = raw_params if isinstance(raw_params, dict) else {}
            label = str(params.get("label") or raw_params or "default")
            before = record.get("session_identities", {}).get(label)
            if not isinstance(before, dict):
                raise assertions.AssertionError(
                    f"assert_session_preserved missing captured label {label!r}"
                )
            record.setdefault("session_preserved", {})[label] = assert_session_preserved(
                before=before,
                cell=cell,
                channel_id=channel_id,
                scenario=scenario,
            )
        elif "cancel_turn" in step:
            params = step["cancel_turn"] or {}
            record.setdefault("cancel_turns", []).append(
                cancel_turn(
                    base_url=client.base_url,
                    channel_id=channel_id,
                    force=bool(params.get("force", True)),
                    timeout_s=float(params.get("timeout_s", 15)),
                )
            )
        elif "assert_health" in step:
            params = step["assert_health"] or {}
            record.setdefault("health_assertions", []).append(
                assert_health(client.base_url, params)
            )
        elif "kill_pane" in step:
            thread_channel_id = channel_id if scenario.get("requires_thread_channel") else None
            session_name = cell_session_name(cell, thread_channel_id=thread_channel_id)
            workspace_substring = cell_workspace_substring(cell)
            panes = tmux.list_panes(session_name)
            reverify = (step["kill_pane"] or {}).get(
                "reverify_session_name_substring"
            ) or workspace_substring
            if reverify not in session_name:
                raise assertions.AssertionError(
                    f"refusing kill_pane: session {session_name!r} does not contain {reverify!r}"
                )
            if not panes:
                raise assertions.AssertionError(f"no panes for session {session_name!r}")
            target_pane = panes[0]
            cwd = target_pane.cwd or ""
            if "e2e" not in cwd and reverify and reverify not in cwd:
                raise assertions.AssertionError(
                    f"refusing kill_pane: pane cwd {cwd!r} does not look like an e2e worktree"
                )
            tmux.kill_pane(target_pane.pane_id)
        elif "send_keys_no_enter" in step:
            thread_channel_id = channel_id if scenario.get("requires_thread_channel") else None
            session_name = cell_session_name(cell, thread_channel_id=thread_channel_id)
            if not tmux.send_keys(session_name, step["send_keys_no_enter"]):
                raise assertions.AssertionError(
                    f"tmux send-keys failed for session {session_name!r}"
                )
        elif "send_keys_sequence" in step:
            thread_channel_id = channel_id if scenario.get("requires_thread_channel") else None
            session_name = cell_session_name(cell, thread_channel_id=thread_channel_id)
            raw_params = step["send_keys_sequence"]
            params = raw_params if isinstance(raw_params, dict) else {"keys": raw_params}
            keys = params.get("keys")
            if not isinstance(keys, list) or not keys:
                raise assertions.AssertionError(
                    f"send_keys_sequence requires a non-empty keys list: {step!r}"
                )
            if bool(params.get("mark_prompt_sent", True)):
                window.mark_prompt_sent()
            key_args = [str(key) for key in keys]
            if not tmux.send_keys(session_name, *key_args):
                raise assertions.AssertionError(
                    f"tmux send-keys sequence failed for session {session_name!r}"
                )
            record.setdefault("tmux_key_sequences", []).append(
                {"session": session_name, "count": len(key_args)}
            )
            time.sleep(float(params.get("sleep_s", 0.2)))
        elif "send_keys" in step:
            thread_channel_id = channel_id if scenario.get("requires_thread_channel") else None
            session_name = cell_session_name(cell, thread_channel_id=thread_channel_id)
            window.mark_prompt_sent()
            if not tmux.send_keys(session_name, step["send_keys"]):
                raise assertions.AssertionError(
                    f"tmux send-keys failed for session {session_name!r}"
                )
            time.sleep(0.2)
            if not tmux.send_keys(session_name, "C-m"):
                raise assertions.AssertionError(
                    f"tmux submit failed for session {session_name!r}"
                )
        else:
            raise assertions.AssertionError(f"unknown step shape: {step!r}")

    final_refetches = max(1, int(os.environ.get("AGENTDESK_E2E_FINAL_REFETCHES", "2")))
    final_refetch_interval_s = float(
        os.environ.get("AGENTDESK_E2E_FINAL_REFETCH_INTERVAL_S", "1")
    )
    for attempt in range(final_refetches):
        if attempt > 0:
            time.sleep(final_refetch_interval_s)
        _ingest_observed(client.fetch_messages(channel_id, after_id=after_id, limit=100))

    record["relay_count"] = len(window.messages)
    record["raw_count"] = len(window.raw_messages)
    record["message_updates"] = len(window.message_updates)
    record["sample_relay"] = [
        (m.get("content") or "")[:120] for m in window.messages[:6]
    ]

    for assertion_spec in scenario.get("assertions") or []:
        run_assertion(assertion_spec, window=window)
        record["assertions"].append({"spec": assertion_spec, "passed": True})

    idle_check = assert_cell_idle(
        base_url=client.base_url,
        channel_id=channel_id,
        cell=cell,
        runtime_root=Path(args.queue_runtime_root),
    )
    record["post_scenario_idle"] = idle_check
    record["assertions"].append(
        {"spec": {"post_scenario_cell_idle": True}, "passed": True, "details": idle_check}
    )

    send_teardown_marker(
        client=client,
        channel_id=channel_id,
        scenario_id=str(scenario_id),
        cell=cell,
        run_id=run_id,
    )
    return record


def wait_for_health(
    base_url: str,
    *,
    timeout_s: float = 90.0,
    poll_interval_s: float = 2.0,
    allowed_statuses: tuple[str, ...] = ("healthy",),
    allowed_degraded_reasons: tuple[str, ...] = (),
) -> None:
    deadline = time.monotonic() + timeout_s
    last_http_status: int | None = None
    last_payload: dict[str, Any] | None = None
    last_violations: list[str] = []
    last_error: str | None = None
    while time.monotonic() < deadline:
        try:
            http_status, payload = _read_api_json(base_url, "/api/health", timeout=5)
            last_http_status = http_status
            if isinstance(payload, dict):
                last_payload = payload
                last_violations = _health_ready_violations(
                    payload,
                    allowed_statuses=allowed_statuses,
                    allowed_degraded_reasons=allowed_degraded_reasons,
                )
                if 200 <= http_status < 300 and not last_violations:
                    return
            else:
                last_payload = None
                last_violations = [f"non-object health payload: {payload!r}"]
            last_error = None
        except Exception as error:  # noqa: BLE001 - preserve last transport/parse failure
            last_error = f"{type(error).__name__}: {error}"
        time.sleep(poll_interval_s)
    raise assertions.AssertionError(
        f"dcserver did not become healthy within {timeout_s}s; last="
        f"{_health_summary(http_status=last_http_status, payload=last_payload, violations=last_violations, last_error=last_error)}"
    )


def _guard_no_foreign_active_turns(
    base_url: str,
    channel_id: str,
    cell: str,
    *,
    finalizing_drain_timeout_s: float = RESTART_GUARD_FINALIZING_DRAIN_TIMEOUT_S,
    poll_interval_s: float = RESTART_GUARD_POLL_INTERVAL_S,
) -> None:
    """Refuse restart when a turn is active on a non-cell channel."""
    e2e_channel_ids = {channel_id} if channel_id else set()
    if not e2e_channel_ids:
        return
    current_provider = cell_provider(cell)
    deadline = time.monotonic() + max(finalizing_drain_timeout_s, 0.0)
    last_global_finalizing = 0
    while True:
        try:
            detail = _read_health_detail(base_url)
        except Exception as error:  # noqa: BLE001 - fail closed before destructive restart
            raise assertions.AssertionError(
                "refusing to restart dcserver: unable to read /api/health/detail "
                f"for foreign mailbox guard (cell={cell}, channel={channel_id}): "
                f"{type(error).__name__}: {error}"
            ) from error
        mailboxes = detail.get("mailboxes")
        if not isinstance(mailboxes, list):
            raise assertions.AssertionError(
                "refusing to restart dcserver: /api/health/detail mailboxes is not a list "
                f"(payload={_payload_summary(detail)})"
            )

        busy: list[str] = []
        for mailbox in mailboxes:
            if not isinstance(mailbox, dict):
                continue
            channel = _mailbox_channel_id(mailbox)
            provider = _mailbox_provider(mailbox)
            if channel in e2e_channel_ids and provider == current_provider:
                continue
            reasons = _mailbox_busy_reasons(mailbox)
            if reasons:
                busy.append(f"{_mailbox_label(mailbox)} [{', '.join(reasons)}]")

        if busy:
            raise assertions.AssertionError(
                f"refusing to restart dcserver: live mailbox state outside cell {cell} "
                f"(channel={channel_id}). Active: {busy}."
            )

        global_finalizing = _as_nonnegative_int(detail.get("global_finalizing"))
        last_global_finalizing = global_finalizing
        if global_finalizing <= 0:
            break
        if time.monotonic() >= deadline:
            raise assertions.AssertionError(
                f"refusing to restart dcserver: live mailbox state outside cell {cell} "
                f"(channel={channel_id}). Active: ['global_finalizing={global_finalizing}']."
            )
        time.sleep(poll_interval_s)

    try:
        with urllib.request.urlopen(f"{base_url}/api/sessions", timeout=5) as response:
            payload = json.loads(response.read())
    except Exception:  # noqa: BLE001
        return

    sessions = (
        payload.get("sessions")
        if isinstance(payload, dict)
        else payload
        if isinstance(payload, list)
        else []
    )
    workspace_substring = cell_workspace_substring(cell)
    for session in sessions or []:
        status = str(session.get("status", "")).lower()
        if status not in {"turn_active", "turn_busy", "active"}:
            continue
        session_key = str(session.get("session_key") or "")
        channel = str(session.get("channel_id") or session.get("channelId") or "")
        if channel in e2e_channel_ids:
            continue
        if workspace_substring in session_key:
            continue
        busy.append(session_key or channel or "<unknown>")
    if busy:
        raise assertions.AssertionError(
            f"refusing to restart dcserver: live turn(s) outside cell {cell} "
            f"(channel={channel_id}, global_finalizing={last_global_finalizing}). "
            f"Active: {busy}."
        )


def restart_dcserver_for_e2e(
    *,
    target: str,
    args: argparse.Namespace,
    base_url: str,
    cell: str,
    channel_id: str,
) -> None:
    if target not in ("dev", "release"):
        raise assertions.AssertionError(f"unsupported restart target: {target!r}")
    _guard_no_foreign_active_turns(base_url, channel_id, cell)

    if args.restart_script:
        script = Path(args.restart_script).expanduser()
        if not script.exists():
            raise assertions.AssertionError(f"restart script not found: {script}")
        proc = subprocess.run(
            [str(script), target],
            check=False,
            capture_output=True,
            text=True,
            timeout=600,
        )
        if proc.returncode != 0:
            raise assertions.AssertionError(
                f"restart script failed for {target} with exit {proc.returncode}\n"
                f"stdout:\n{proc.stdout[-4000:]}\n"
                f"stderr:\n{proc.stderr[-4000:]}"
            )
    else:
        label = "com.agentdesk." + ("release" if target == "release" else "dev")
        subprocess.run(
            ["launchctl", "kickstart", "-k", f"gui/{os.getuid()}/{label}"],
            check=False,
            capture_output=True,
        )
    wait_for_health(base_url, timeout_s=90)


def run_assertion(spec: dict[str, Any], *, window: assertions.Window) -> None:
    if not isinstance(spec, dict):
        raise assertions.AssertionError(f"bad assertion spec: {spec!r}")
    if "message_count_between_markers" in spec:
        params = spec["message_count_between_markers"]
        assertions.message_count_between_markers(
            window, low=int(params.get("min", 0)), high=int(params.get("max", 99))
        )
    elif "raw_message_count_between_markers" in spec:
        params = spec["raw_message_count_between_markers"]
        assertions.raw_message_count_between_markers(
            window,
            low=int(params.get("min", 0)),
            high=int(params.get("max", 999)),
            include_our_send=bool(params.get("include_our_send", False)),
        )
    elif spec.get("no_duplicate_content"):
        assertions.no_duplicate_content(window)
    elif "text_present" in spec:
        assertions.text_present(window, needle=spec["text_present"])
    elif "raw_text_present" in spec:
        assertions.raw_text_present(window, needle=spec["raw_text_present"])
    elif "raw_text_absent" in spec:
        params = spec["raw_text_absent"]
        if isinstance(params, dict):
            needle = params.get("needle") or params.get("text")
            if needle is None:
                raise assertions.AssertionError(
                    f"raw_text_absent requires needle/text: {spec!r}"
                )
            assertions.raw_text_absent(
                window,
                needle=str(needle),
                include_our_send=bool(params.get("include_our_send", False)),
            )
        else:
            assertions.raw_text_absent(window, needle=str(params))
    elif "marker_absent" in spec:
        params = spec["marker_absent"]
        if isinstance(params, dict):
            marker = params.get("marker")
            if marker is None:
                raise assertions.AssertionError(
                    f"marker_absent requires marker: {spec!r}"
                )
            assertions.marker_absent(
                window,
                marker=str(marker),
                surface=str(params.get("surface", "relay")),
                include_our_send=bool(params.get("include_our_send", False)),
            )
        else:
            assertions.marker_absent(window, marker=str(params))
    elif "ordered_text_present" in spec:
        # #2838 (P0-2): completeness + ordering of multiple expected fragments.
        needles = spec["ordered_text_present"]
        if not isinstance(needles, list):
            raise assertions.AssertionError(
                f"ordered_text_present must be a list of needles: {spec!r}"
            )
        assertions.ordered_text_present(window, needles=needles)
    elif "no_duplicate_marker" in spec:
        # #2838 (P0-2): catches duplicate-with-differing-header re-emit (e.g.
        # restart-induced or ACK-timeout re-relay) that no_duplicate_content misses.
        assertions.no_duplicate_marker(window, marker=spec["no_duplicate_marker"])
    elif "body_complete" in spec:
        # #2838 (P0-2): catches a truncated-tail relay on long responses.
        params = spec["body_complete"]
        if not isinstance(params, dict) or "head" not in params or "tail" not in params:
            raise assertions.AssertionError(f"body_complete requires {{head, tail}}: {spec!r}")
        assertions.body_complete(window, head=params["head"], tail=params["tail"])
    elif "relay_latency_within" in spec:
        # #2838 (P0-2): bounds the first→last relay span (catches stalls).
        params = spec["relay_latency_within"]
        max_seconds = params.get("max_seconds") if isinstance(params, dict) else params
        assertions.relay_latency_within(window, max_seconds=float(max_seconds))
    elif "chrome_count" in spec:
        params = spec["chrome_count"]
        if not isinstance(params, dict):
            raise assertions.AssertionError(f"chrome_count requires a mapping: {spec!r}")
        assertions.chrome_count(
            window,
            text=params.get("text"),
            regex=params.get("regex"),
            min_count=int(params.get("min", 0)),
            max_count=(
                int(params["max"])
                if "max" in params and params.get("max") is not None
                else None
            ),
            exact=(
                int(params["exact"])
                if "exact" in params and params.get("exact") is not None
                else None
            ),
            include_our_send=bool(params.get("include_our_send", False)),
        )
    elif "completion_chrome_after_body" in spec:
        params = spec["completion_chrome_after_body"]
        body_marker = params.get("body_marker") if isinstance(params, dict) else params
        required = bool(params.get("required", False)) if isinstance(params, dict) else False
        assertions.completion_chrome_after_body(
            window,
            body_marker=str(body_marker),
            required=required,
        )
    elif "body_not_overwritten" in spec:
        assertions.body_not_overwritten(window, marker=str(spec["body_not_overwritten"]))
    elif spec.get("no_suppressed_label_chrome"):
        assertions.no_suppressed_label_chrome(window)
    elif spec.get("no_control_chars"):
        assertions.no_control_chars(window)
    elif spec.get("no_resume_prompt_chrome"):
        assertions.no_resume_prompt_chrome(window)
    else:
        raise assertions.AssertionError(f"unknown assertion: {spec!r}")


def main() -> int:
    args = parse_args()
    cell = args.cell
    handoff_to = args.handoff_to_agent or cell_default_agent(cell)
    output_dir = resolve_output_dir(args.output, cell)
    run_id = output_dir.name
    print(f"[e2e] cell={cell} run_id={run_id} output={output_dir}")

    scenarios_dir = Path(args.scenarios)
    if not scenarios_dir.is_dir():
        print(f"[e2e] scenarios dir not found: {scenarios_dir}", file=sys.stderr)
        return 2
    scenarios = load_scenarios(scenarios_dir, cell=cell)
    if args.filter:
        wanted = {tok.strip() for tok in args.filter.split(",") if tok.strip()}
        scenarios = [s for s in scenarios if str(s.get("id")) in wanted]
    print(f"[e2e] {len(scenarios)} scenarios applicable to {cell}")
    if not scenarios:
        print(f"[e2e] WARNING: no scenarios matched cell {cell}", file=sys.stderr)

    client = discord.DiscordClient(
        base_url=args.base_url,
        timeout_s=args.turn_start_timeout_s,
        handoff_to_agent=handoff_to,
        handoff_from_agent=args.handoff_from_agent,
    )

    lease_token = f"{cell}-{run_id}"
    with lease.acquire(lease_token, cell=cell) if not args.dry_run else _null_lease(run_id):
        results: list[dict[str, Any]] = []
        for scenario in scenarios:
            print(f"[e2e] running {scenario.get('id')} cell={cell}")
            result = run_scenario(scenario, args=args, run_id=run_id, client=client)
            print(f"[e2e]   → {result['status']} {result.get('reason') or ''}")
            results.append(result)

    # Always cell-tag the report filename so an orchestrator that passes a
    # shared --output dir for all 5 cells never overwrites a sibling report.
    summary_path = output_dir / f"report.{cell}.json"
    summary = {
        "run_id": run_id,
        "cell": cell,
        "channel_id": args.channel_id,
        "scenarios": results,
        "totals": {
            "pass": sum(1 for r in results if r["status"] == "pass"),
            "fail": sum(1 for r in results if r["status"] == "fail"),
            "skipped": sum(1 for r in results if r["status"] == "skipped"),
        },
    }
    summary_path.write_text(json.dumps(summary, indent=2))
    print(f"[e2e] report → {summary_path}")
    return 0 if summary["totals"]["fail"] == 0 else 1


class _null_lease:
    def __init__(self, run_id: str):
        self.run_id = run_id

    def __enter__(self):
        return None

    def __exit__(self, *exc):
        return False


if __name__ == "__main__":
    sys.exit(main())
