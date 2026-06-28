#!/usr/bin/env python3
"""Run the AgentDesk Discord relay E2E matrix.

This is the operator/orchestrator entry point. It drives worker channels from
outside those workers, so a worker never runs a driver against its own mailbox.
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
from pathlib import Path
from typing import Any

import yaml  # type: ignore[import-untyped]

sys.path.insert(0, str(Path(__file__).resolve().parent))

import run_tui_relay as cell_driver  # noqa: E402
from tui_relay import assertions  # noqa: E402


DEFAULT_CONFIG = Path.home() / ".adk" / "release" / "config" / "agentdesk.yaml"
DEFAULT_SCENARIOS = Path("tests/e2e/tui_relay/scenarios")
DEFAULT_CELLS = cell_driver.SUPPORTED_CELLS


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default="http://127.0.0.1:8791")
    parser.add_argument("--config", default=str(DEFAULT_CONFIG))
    parser.add_argument("--scenarios", default=str(DEFAULT_SCENARIOS))
    parser.add_argument("--cells", default=",".join(DEFAULT_CELLS))
    parser.add_argument("--filter", default=None)
    parser.add_argument("--output", default=None)
    parser.add_argument("--twice", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--allow-destructive", action="store_true")
    if hasattr(argparse, "BooleanOptionalAction"):
        parser.add_argument(
            "--reset-before-each",
            action=argparse.BooleanOptionalAction,
            default=True,
            help="Reset each configured e2e channel before every scenario (default: true).",
        )
    else:
        parser.set_defaults(reset_before_each=True)
        reset_group = parser.add_mutually_exclusive_group()
        reset_group.add_argument(
            "--reset-before-each",
            dest="reset_before_each",
            action="store_true",
            help="Reset each configured e2e channel before every scenario (default).",
        )
        reset_group.add_argument(
            "--no-reset-before-each",
            dest="reset_before_each",
            action="store_false",
            help="Skip cancel/reset before each scenario.",
        )
    parser.add_argument(
        "--queue-runtime-root",
        default=str(Path.home() / ".adk" / "release" / "runtime"),
        help="ADK runtime root used to truncate on-disk relay queues during reset.",
    )
    parser.add_argument(
        "--hard-reset-session-each",
        action="store_true",
        help="Pass through to cell driver; only use for dedicated E2E cells.",
    )
    parser.add_argument(
        "--turn-start-timeout-s",
        type=float,
        default=float(os.environ.get("AGENTDESK_E2E_TURN_START_TIMEOUT_S", "180")),
    )
    parser.add_argument(
        "--required-agent-mode",
        choices=cell_driver.AGENT_MODES,
        default=os.environ.get("AGENTDESK_E2E_REQUIRED_AGENT_MODE"),
        help="Fail selected scenarios whose declared agent_mode is below this gate.",
    )
    parser.add_argument(
        "--required-coverage-class",
        choices=cell_driver.COVERAGE_CLASSES,
        default=os.environ.get("AGENTDESK_E2E_REQUIRED_COVERAGE_CLASS"),
        help="Fail selected scenarios whose declared coverage_class is below this gate.",
    )
    return parser.parse_args()


def load_channel_ids(config_path: Path) -> dict[str, str]:
    with config_path.open("r", encoding="utf-8") as fp:
        config = yaml.safe_load(fp)
    agents = config.get("agents") if isinstance(config, dict) else None
    if not isinstance(agents, list):
        raise ValueError(f"{config_path} has no agents list")

    resolved: dict[str, str] = {}
    for cell in DEFAULT_CELLS:
        agent_id = cell_driver.cell_default_agent(cell)
        provider = cell_driver.cell_provider(cell)
        agent = next(
            (
                item
                for item in agents
                if isinstance(item, dict) and item.get("id") == agent_id
            ),
            None,
        )
        if not isinstance(agent, dict):
            raise ValueError(f"agent {agent_id!r} not found in {config_path}")
        channels = agent.get("channels")
        channel = channels.get(provider) if isinstance(channels, dict) else None
        channel_id = channel.get("id") if isinstance(channel, dict) else None
        if not channel_id:
            raise ValueError(f"agent {agent_id!r} has no {provider} channel id")
        resolved[cell] = str(channel_id)
    return resolved


def load_cross_channel_scenarios(scenarios_dir: Path) -> list[dict[str, Any]]:
    scenarios: list[dict[str, Any]] = []
    for yaml_path in sorted(scenarios_dir.glob("*.yaml")):
        with yaml_path.open("r", encoding="utf-8") as fp:
            data = yaml.safe_load(fp)
        if not isinstance(data, dict):
            raise ValueError(f"{yaml_path} did not parse to a mapping")
        if data.get("orchestration") != "cross_channel":
            continue
        if not isinstance(data.get("cross_channel"), dict):
            raise ValueError(f"{yaml_path} declares cross_channel without config")
        data["__path__"] = str(yaml_path)
        cell_driver.validate_scenario_agent_mode(data)
        cell_driver.validate_scenario_coverage_class(data)
        scenarios.append(data)
    return scenarios


def parse_cells(raw: str) -> list[str]:
    cells = [cell.strip() for cell in raw.split(",") if cell.strip()]
    unknown = [cell for cell in cells if cell not in DEFAULT_CELLS]
    if unknown:
        raise ValueError(f"unsupported cell(s): {', '.join(unknown)}")
    return cells


def parse_filter(raw: str | None) -> set[str]:
    if raw is None:
        return set()
    return {token.strip() for token in raw.split(",") if token.strip()}


def resolve_output_dir(arg: str | None) -> Path:
    if arg:
        out = Path(arg)
    else:
        run_id = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
        out = Path("out/e2e/tui_relay") / f"matrix-{run_id}"
    out.mkdir(parents=True, exist_ok=True)
    return out


def run_cell(
    *,
    cell: str,
    channel_id: str,
    args: argparse.Namespace,
    output_dir: Path,
    pass_index: int,
) -> dict[str, Any]:
    cell_output = output_dir / f"pass-{pass_index}" / cell
    cmd = [
        sys.executable,
        "scripts/e2e/run_tui_relay.py",
        "--base-url",
        args.base_url,
        "--cell",
        cell,
        "--channel-id",
        channel_id,
        "--scenarios",
        str(args.scenarios),
        "--output",
        str(cell_output),
        "--queue-runtime-root",
        str(args.queue_runtime_root),
        "--turn-start-timeout-s",
        str(args.turn_start_timeout_s),
    ]
    if args.filter:
        cmd.extend(["--filter", args.filter])
    if args.dry_run:
        cmd.append("--dry-run")
    if args.required_agent_mode:
        cmd.extend(["--required-agent-mode", str(args.required_agent_mode)])
    if args.required_coverage_class:
        cmd.extend(["--required-coverage-class", str(args.required_coverage_class)])
    if args.allow_destructive:
        cmd.append("--allow-destructive")
    if not args.reset_before_each:
        cmd.append("--no-reset-before-each")
    if args.hard_reset_session_each:
        cmd.append("--hard-reset-session-each")

    print(f"[matrix] pass={pass_index} cell={cell} channel={channel_id}")
    print("[matrix] command=" + " ".join(cmd))
    proc = subprocess.run(cmd, check=False, text=True)
    report_path = cell_output / f"report.{cell}.json"
    report: dict[str, Any] | None = None
    if report_path.exists():
        report = json.loads(report_path.read_text(encoding="utf-8"))
    return {
        "kind": "cell",
        "pass_index": pass_index,
        "cell": cell,
        "provider": cell_driver.cell_provider(cell),
        "runtime": cell_driver.cell_runtime(cell),
        "channel_id": channel_id,
        "provider_identity": cell_driver.provider_identity(cell, channel_id),
        "returncode": proc.returncode,
        "report": str(report_path),
        "totals": (report or {}).get("totals"),
        "agent_mode_totals": (report or {}).get("agent_mode_totals"),
        "coverage_class_totals": (report or {}).get("coverage_class_totals"),
        "coverage_class_violations": (report or {}).get("coverage_class_violations"),
        "real_provider_contacted": bool((report or {}).get("real_provider_contacted")),
        "ok": proc.returncode == 0 and bool(report),
    }


def _cross_participants_required_cells(scenario: dict[str, Any]) -> set[str]:
    config = scenario.get("cross_channel") or {}
    raw_participants = config.get("participants")
    if not isinstance(raw_participants, list) or len(raw_participants) < 2:
        raise ValueError(
            f"{scenario.get('__path__') or scenario.get('id')} cross_channel.participants "
            "must contain at least two entries"
        )
    cells: set[str] = set()
    for idx, raw_participant in enumerate(raw_participants):
        if not isinstance(raw_participant, dict):
            raise ValueError(
                f"{scenario.get('id')} participant {idx} must be a mapping"
            )
        cell = str(raw_participant.get("cell") or "")
        if cell not in DEFAULT_CELLS:
            raise ValueError(f"{scenario.get('id')} participant {idx} has bad cell {cell!r}")
        cells.add(cell)
    return cells


def resolve_cross_channel_participants(
    scenario: dict[str, Any],
    *,
    channel_ids: dict[str, str],
    selected_cells: list[str],
) -> list[dict[str, Any]]:
    required_cells = _cross_participants_required_cells(scenario)
    missing_selected = sorted(required_cells.difference(selected_cells))
    if missing_selected:
        raise ValueError(
            "cross-channel scenario requires selected cells: "
            + ", ".join(missing_selected)
        )

    config = scenario["cross_channel"]
    participants: list[dict[str, Any]] = []
    seen_cells: set[str] = set()
    for idx, raw_participant in enumerate(config["participants"]):
        cell = str(raw_participant["cell"])
        channel_id = channel_ids.get(cell)
        if not channel_id:
            raise ValueError(f"configured e2e channel id missing for cell {cell!r}")
        marker = raw_participant.get("marker")
        prompt = raw_participant.get("prompt")
        if not marker or not prompt:
            raise ValueError(f"{scenario.get('id')} participant {cell} needs marker and prompt")
        if cell in seen_cells:
            raise ValueError(f"{scenario.get('id')} repeats participant cell {cell!r}")
        seen_cells.add(cell)
        participants.append(
            {
                "index": idx,
                "name": str(raw_participant.get("name") or cell),
                "cell": cell,
                "provider": cell_driver.cell_provider(cell),
                "runtime": cell_driver.cell_runtime(cell),
                "channel_id": str(channel_id),
                "channel_kind": str(
                    raw_participant.get("channel_kind")
                    or cell_driver.cell_channel_kind(cell)
                ),
                "handoff_to_agent": cell_driver.cell_default_agent(cell),
                "provider_identity": cell_driver.provider_identity(cell, str(channel_id)),
                "workspace_substring": cell_driver.cell_workspace_substring(cell),
                "marker": str(marker),
                "prompt": str(prompt),
            }
        )
    return participants


def _participant_key(participant: dict[str, Any]) -> str:
    return f"{participant['cell']}:{participant['channel_id']}"


def _active_sessions_payload(base_url: str) -> list[dict[str, Any]]:
    try:
        with cell_driver.urllib.request.urlopen(f"{base_url}/api/sessions", timeout=5) as response:
            payload = json.loads(response.read())
    except Exception:  # noqa: BLE001 - health/detail is the fail-closed source of truth.
        return []
    sessions = (
        payload.get("sessions")
        if isinstance(payload, dict)
        else payload
        if isinstance(payload, list)
        else []
    )
    return [session for session in sessions if isinstance(session, dict)]


def guard_no_foreign_live_state(
    *,
    base_url: str,
    participants: list[dict[str, Any]],
) -> dict[str, Any]:
    """Fail closed if an active turn exists outside the configured E-11 cells."""

    allowed_pairs = {
        (str(participant["provider"]), str(participant["channel_id"]))
        for participant in participants
    }
    allowed_channels = {str(participant["channel_id"]) for participant in participants}
    allowed_workspaces = {
        str(participant["workspace_substring"]) for participant in participants
    }
    try:
        detail = cell_driver._read_health_detail(base_url)  # noqa: SLF001
    except Exception as error:  # noqa: BLE001
        raise assertions.AssertionError(
            "refusing cross-channel E-11: unable to read /api/health/detail "
            f"for foreign live-state guard: {type(error).__name__}: {error}"
        ) from error

    mailboxes = detail.get("mailboxes")
    if not isinstance(mailboxes, list):
        raise assertions.AssertionError(
            "refusing cross-channel E-11: /api/health/detail mailboxes is not a list "
            f"(payload={cell_driver._payload_summary(detail)})"  # noqa: SLF001
        )

    busy: list[str] = []
    for mailbox in mailboxes:
        if not isinstance(mailbox, dict):
            continue
        channel = cell_driver._mailbox_channel_id(mailbox)  # noqa: SLF001
        provider = cell_driver._mailbox_provider(mailbox)  # noqa: SLF001
        if (provider, channel) in allowed_pairs:
            continue
        reasons = cell_driver._mailbox_busy_reasons(mailbox)  # noqa: SLF001
        if reasons:
            busy.append(f"{provider or '<provider?>'}:{channel or '<channel?>'} [{', '.join(reasons)}]")

    global_finalizing = cell_driver._as_nonnegative_int(  # noqa: SLF001
        detail.get("global_finalizing")
    )
    if global_finalizing > 0:
        busy.append(f"global_finalizing={global_finalizing}")

    for session in _active_sessions_payload(base_url):
        status = str(session.get("status", "")).lower()
        if status not in {"turn_active", "turn_busy", "active"}:
            continue
        channel = str(session.get("channel_id") or session.get("channelId") or "")
        session_key = str(session.get("session_key") or "")
        if channel in allowed_channels:
            continue
        if any(workspace in session_key for workspace in allowed_workspaces):
            continue
        busy.append(session_key or channel or "<unknown-session>")

    if busy:
        raise assertions.AssertionError(
            "refusing cross-channel E-11: live state exists outside the selected "
            f"configured e2e cells. Active: {busy}."
        )
    return {
        "status": "isolated",
        "allowed": [
            {
                "cell": participant["cell"],
                "provider": participant["provider"],
                "channel_id": participant["channel_id"],
            }
            for participant in participants
        ],
    }


def _message_sort_id(message: dict[str, Any]) -> int:
    try:
        return int(message.get("id", "0"))
    except ValueError:
        return 0


def _ingest_observed(
    window: assertions.Window,
    messages: list[dict[str, Any]],
) -> None:
    for message in sorted(messages, key=_message_sort_id):
        if (message.get("content") or "").startswith("### E2E TEARDOWN"):
            window.teardown_marker_id = str(message.get("id") or "")
            continue
        window.add(message)


def _advance_window_past_setup_echo(
    *,
    client: cell_driver.discord.DiscordClient,
    channel_id: str,
    after_id: str,
    window: assertions.Window,
) -> str:
    tail = client.fetch_messages(channel_id, after_id=after_id, limit=100)
    if not tail:
        return after_id
    latest = max(_message_sort_id(message) for message in tail)
    window.raw_messages = []
    window.messages = []
    return str(latest)


def _send_cross_channel_teardown(
    *,
    clients: dict[str, cell_driver.discord.DiscordClient],
    participants: list[dict[str, Any]],
    scenario_id: str,
    run_id: str,
) -> list[str]:
    errors: list[str] = []
    for participant in participants:
        key = _participant_key(participant)
        client = clients.get(key)
        if client is None:
            continue
        try:
            client.send_control(
                participant["channel_id"],
                cell_driver.scenario_teardown_marker(
                    scenario_id,
                    cell=participant["cell"],
                    run_id=run_id,
                ),
            )
        except Exception as error:  # noqa: BLE001 - preserve original scenario result.
            errors.append(
                f"{participant['cell']}:{type(error).__name__}: {error}"
            )
    return errors


class CrossChannelAssertionError(assertions.AssertionError):
    def __init__(self, message: str, records: dict[str, list[dict[str, Any]]]):
        super().__init__(message)
        self.records = records


def _assert_cross_channel_non_leak(
    *,
    participants: list[dict[str, Any]],
    windows: dict[str, assertions.Window],
) -> dict[str, list[dict[str, Any]]]:
    assertion_records: dict[str, list[dict[str, Any]]] = {}
    failures: list[str] = []
    for participant in participants:
        key = _participant_key(participant)
        window = windows[key]
        marker = participant["marker"]
        specs: list[tuple[dict[str, Any], Any]] = [
            ({"text_present": marker}, lambda: assertions.text_present(window, needle=marker)),
            (
                {"no_duplicate_marker": marker},
                lambda: assertions.no_duplicate_marker(window, marker=marker),
            ),
            ({"no_duplicate_content": True}, lambda: assertions.no_duplicate_content(window)),
        ]
        for sibling in participants:
            if sibling is participant:
                continue
            sibling_marker = sibling["marker"]
            specs.extend(
                [
                    (
                        {
                            "marker_absent": {
                                "marker": sibling_marker,
                                "surface": "relay",
                            }
                        },
                        lambda sibling_marker=sibling_marker: assertions.marker_absent(
                            window,
                            marker=sibling_marker,
                            surface="relay",
                        ),
                    ),
                    (
                        {
                            "marker_absent": {
                                "marker": sibling_marker,
                                "surface": "raw",
                            }
                        },
                        lambda sibling_marker=sibling_marker: assertions.marker_absent(
                            window,
                            marker=sibling_marker,
                            surface="raw",
                        ),
                    ),
                ]
            )
        records: list[dict[str, Any]] = []
        for spec, check in specs:
            try:
                check()
            except assertions.AssertionError as error:
                message = str(error)
                records.append({"spec": spec, "passed": False, "error": message})
                failures.append(f"{key} {spec}: {message}")
                continue
            records.append({"spec": spec, "passed": True})
        assertion_records[key] = records
    if failures:
        raise CrossChannelAssertionError(
            "cross-channel non-leak assertion(s) failed: " + "; ".join(failures),
            assertion_records,
        )
    return assertion_records


def _cross_channel_result_channels(
    *,
    participants: list[dict[str, Any]],
    windows: dict[str, assertions.Window],
    assertion_records: dict[str, list[dict[str, Any]]],
) -> list[dict[str, Any]]:
    channels: list[dict[str, Any]] = []
    for participant in participants:
        key = _participant_key(participant)
        window = windows[key]
        channels.append(
            {
                "cell": participant["cell"],
                "channel_id": participant["channel_id"],
                "relay_count": len(window.messages),
                "raw_count": len(window.raw_messages),
                "message_updates": len(window.message_updates),
                "sample_relay": [
                    (message.get("content") or "")[:120]
                    for message in window.messages[:6]
                ],
                "assertions": assertion_records.get(key, []),
            }
        )
    return channels


def run_cross_channel_scenario(
    scenario: dict[str, Any],
    *,
    args: argparse.Namespace,
    run_id: str,
    channel_ids: dict[str, str],
    selected_cells: list[str],
    pass_index: int,
) -> dict[str, Any]:
    scenario_id = str(scenario.get("id"))
    declared_agent_mode = cell_driver.scenario_agent_mode(scenario)
    declared_coverage_class = cell_driver.scenario_coverage_class(scenario)
    initial_coverage_actual = cell_driver._initial_coverage_class_actual(  # noqa: SLF001
        scenario,
        declared=declared_coverage_class,
        dry_run=bool(args.dry_run),
    )
    result: dict[str, Any] = {
        "kind": "cross_channel",
        "pass_index": pass_index,
        "id": scenario_id,
        "path": scenario.get("__path__"),
        "run_id": run_id,
        "status": "skipped",
        "reason": None,
        "agent_mode": declared_agent_mode,
        "agent_mode_planned": cell_driver.infer_planned_agent_mode(
            scenario,
            declared=declared_agent_mode,
        ),
        "agent_mode_actual": "none",
        "agent_mode_contract": cell_driver._agent_mode_contract(  # noqa: SLF001
            declared=declared_agent_mode,
            actual="none",
            dry_run=bool(args.dry_run),
            real_provider_contacted=False,
        ),
        "coverage_class": declared_coverage_class,
        "coverage_class_planned": cell_driver.infer_planned_coverage_class(
            scenario,
            declared=declared_coverage_class,
        ),
        "coverage_class_actual": initial_coverage_actual,
        "coverage_class_contract": cell_driver._coverage_class_contract(  # noqa: SLF001
            declared=declared_coverage_class,
            actual=initial_coverage_actual,
            dry_run=bool(args.dry_run),
        ),
        "real_provider_contacted": False,
        "failure_attribution": None,
        "participants": [],
        "ok": False,
        "started_at": dt.datetime.now().isoformat(timespec="seconds"),
    }

    gate_violation = cell_driver.required_agent_mode_violation(
        declared=declared_agent_mode,
        required=getattr(args, "required_agent_mode", None),
        scenario_id=scenario_id,
    )
    if gate_violation:
        result["status"] = "fail"
        result["reason"] = gate_violation
        result["failure_attribution"] = cell_driver._failure_attribution(  # noqa: SLF001
            "agent_mode_gate",
            gate_violation,
        )
        return result

    coverage_gate_violation = cell_driver.required_coverage_class_violation(
        declared=declared_coverage_class,
        required=getattr(args, "required_coverage_class", None),
        scenario_id=scenario_id,
    )
    if coverage_gate_violation:
        result["status"] = "fail"
        result["reason"] = coverage_gate_violation
        result["failure_attribution"] = cell_driver._failure_attribution(  # noqa: SLF001
            "coverage_class_gate",
            coverage_gate_violation,
        )
        return result

    try:
        required_cells = _cross_participants_required_cells(scenario)
    except ValueError as error:
        result["status"] = "fail"
        result["reason"] = str(error)
        result["failure_attribution"] = cell_driver._failure_attribution(  # noqa: SLF001
            "config",
            str(error),
        )
        return result
    missing_selected = sorted(required_cells.difference(selected_cells))
    if missing_selected:
        result["reason"] = "requires selected cells: " + ", ".join(missing_selected)
        observed_gate_violation = cell_driver.required_agent_mode_violation(
            declared=declared_agent_mode,
            actual=str(result["agent_mode_actual"]),
            required=getattr(args, "required_agent_mode", None),
            scenario_id=scenario_id,
        )
        if observed_gate_violation and not args.dry_run:
            result["status"] = "fail"
            result["reason"] = observed_gate_violation
            result["failure_attribution"] = cell_driver._failure_attribution(  # noqa: SLF001
                "agent_mode_gate",
                observed_gate_violation,
            )
            return result
        observed_coverage_gate_violation = cell_driver.required_coverage_class_violation(
            declared=declared_coverage_class,
            actual=str(result["coverage_class_actual"]),
            required=getattr(args, "required_coverage_class", None),
            scenario_id=scenario_id,
        )
        if observed_coverage_gate_violation and not args.dry_run:
            result["status"] = "fail"
            result["reason"] = observed_coverage_gate_violation
            result["failure_attribution"] = cell_driver._failure_attribution(  # noqa: SLF001
                "coverage_class_gate",
                observed_coverage_gate_violation,
            )
            return result
        result["ok"] = True
        return result

    try:
        participants = resolve_cross_channel_participants(
            scenario,
            channel_ids=channel_ids,
            selected_cells=selected_cells,
        )
    except ValueError as error:
        result["status"] = "fail"
        result["reason"] = str(error)
        result["failure_attribution"] = cell_driver._failure_attribution(  # noqa: SLF001
            "config",
            str(error),
        )
        return result
    result["participants"] = [
        {
            "cell": participant["cell"],
            "provider": participant["provider"],
            "runtime": participant["runtime"],
            "channel_id": participant["channel_id"],
            "worker_agent": participant["handoff_to_agent"],
            "provider_identity": participant["provider_identity"],
            "marker": participant["marker"],
        }
        for participant in participants
    ]

    if args.dry_run:
        print(
            f"[matrix] dry-run cross-channel {scenario_id}: "
            + ", ".join(
                f"{participant['cell']}->{participant['channel_id']}"
                for participant in participants
            )
        )
        result["status"] = "pass"
        result["coverage_class_actual"] = "live"
        result["coverage_class_contract"] = cell_driver._coverage_class_contract(  # noqa: SLF001
            declared=declared_coverage_class,
            actual="live",
            dry_run=bool(args.dry_run),
        )
        result["ok"] = True
        result["completed_at"] = dt.datetime.now().isoformat(timespec="seconds")
        return result

    runtime_root = Path(args.queue_runtime_root)
    clients = {
        _participant_key(participant): cell_driver.discord.DiscordClient(
            base_url=args.base_url,
            timeout_s=args.turn_start_timeout_s,
            handoff_to_agent=participant["handoff_to_agent"],
            handoff_from_agent="adk-e2e-orchestrator",
        )
        for participant in participants
    }

    try:
        if args.reset_before_each:
            result["resets"] = [
                cell_driver.reset_channel_state(
                    base_url=args.base_url,
                    channel_id=participant["channel_id"],
                    runtime_root=runtime_root,
                    provider=participant["provider"],
                )
                for participant in participants
            ]
            time.sleep(2.0)

        result["isolation_guard"] = guard_no_foreign_live_state(
            base_url=args.base_url,
            participants=participants,
        )

        windows: dict[str, assertions.Window] = {}
        after_ids: dict[str, str] = {}
        for participant in participants:
            key = _participant_key(participant)
            setup_marker = (
                f"### E2E SETUP {scenario_id} cell={participant['cell']} run={run_id}"
            )
            setup_resp = clients[key].send_control(participant["channel_id"], setup_marker)
            setup_marker_id = str(
                setup_resp.get("message_id") or setup_resp.get("id") or ""
            )
            windows[key] = assertions.Window(setup_marker_id=setup_marker_id)
            after_ids[key] = setup_marker_id
        time.sleep(8.0)

        for participant in participants:
            key = _participant_key(participant)
            after_ids[key] = _advance_window_past_setup_echo(
                client=clients[key],
                channel_id=participant["channel_id"],
                after_id=after_ids[key],
                window=windows[key],
            )

        dispatch_results: list[dict[str, Any]] = []

        def _dispatch(participant: dict[str, Any]) -> dict[str, Any]:
            key = _participant_key(participant)
            windows[key].mark_prompt_sent()
            response = clients[key].send_prompt(
                participant["channel_id"],
                participant["prompt"],
                channel_kind=participant["channel_kind"],
            )
            return {
                "cell": participant["cell"],
                "channel_id": participant["channel_id"],
                "message_id": response.get("message_id") or response.get("id"),
            }

        dispatch_errors: list[str] = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=len(participants)) as executor:
            futures = {
                executor.submit(_dispatch, participant): participant
                for participant in participants
            }
            for future in concurrent.futures.as_completed(futures):
                participant = futures[future]
                try:
                    dispatch_results.append(future.result())
                    cell_driver._mark_real_provider_contacted(  # noqa: SLF001
                        result,
                        declared_agent_mode=declared_agent_mode,
                        dry_run=False,
                    )
                except Exception as error:  # noqa: BLE001
                    dispatch_errors.append(
                        f"cell={participant['cell']} channel={participant['channel_id']}: "
                        f"{type(error).__name__}: {error}"
                    )
        result["dispatches"] = sorted(dispatch_results, key=lambda item: item["cell"])
        if dispatch_errors:
            raise assertions.AssertionError(
                "cross-channel dispatch failed " + "; ".join(dispatch_errors)
            )

        wait_timeout_s = float(
            (scenario.get("cross_channel") or {}).get("wait_timeout_s", 240)
        )

        def _wait_for_marker(participant: dict[str, Any]) -> tuple[str, dict[str, Any] | None, list[dict[str, Any]]]:
            key = _participant_key(participant)
            marker = participant["marker"]
            predicate = lambda message: (  # noqa: E731
                assertions.is_relay_response(message)
                and marker in (message.get("content") or "")
            )
            found, observed = clients[key].wait_for_message(
                participant["channel_id"],
                predicate=predicate,
                after_id=after_ids[key],
                timeout_s=wait_timeout_s,
                debug_label=(
                    f"{scenario_id}::{participant['cell']}::wait_for_text:"
                    f"{marker[:32]}"
                ),
            )
            return key, found, observed

        with concurrent.futures.ThreadPoolExecutor(max_workers=len(participants)) as executor:
            futures = {
                executor.submit(_wait_for_marker, participant): participant
                for participant in participants
            }
            for future in concurrent.futures.as_completed(futures):
                participant = futures[future]
                key, found, observed = future.result()
                _ingest_observed(windows[key], observed)
                if not found:
                    raise assertions.AssertionError(
                        "timeout waiting for cross-channel Discord text "
                        f"{participant['marker']!r} in cell={participant['cell']}"
                    )

        final_refetches = max(
            1, int(os.environ.get("AGENTDESK_E2E_FINAL_REFETCHES", "2"))
        )
        final_refetch_interval_s = float(
            os.environ.get("AGENTDESK_E2E_FINAL_REFETCH_INTERVAL_S", "1")
        )
        for attempt in range(final_refetches):
            if attempt > 0:
                time.sleep(final_refetch_interval_s)
            for participant in participants:
                key = _participant_key(participant)
                _ingest_observed(
                    windows[key],
                    clients[key].fetch_messages(
                        participant["channel_id"],
                        after_id=after_ids[key],
                        limit=100,
                    ),
                )

        assertion_records: dict[str, list[dict[str, Any]]] = {}
        try:
            assertion_records = _assert_cross_channel_non_leak(
                participants=participants,
                windows=windows,
            )
        except CrossChannelAssertionError as error:
            result["channels"] = _cross_channel_result_channels(
                participants=participants,
                windows=windows,
                assertion_records=error.records,
            )
            raise

        result["channels"] = []
        for participant in participants:
            key = _participant_key(participant)
            window = windows[key]
            idle_check = cell_driver.assert_cell_idle(
                base_url=args.base_url,
                channel_id=participant["channel_id"],
                cell=participant["cell"],
                runtime_root=runtime_root,
            )
            channel_record = {
                "cell": participant["cell"],
                "provider": participant["provider"],
                "runtime": participant["runtime"],
                "channel_id": participant["channel_id"],
                "provider_identity": participant["provider_identity"],
                "real_provider_contacted": True,
                "relay_count": len(window.messages),
                "raw_count": len(window.raw_messages),
                "message_updates": len(window.message_updates),
                "sample_relay": [
                    (message.get("content") or "")[:120]
                    for message in window.messages[:6]
                ],
                "assertions": assertion_records[key]
                + [
                    {
                        "spec": {"post_scenario_cell_idle": True},
                        "passed": True,
                        "details": idle_check,
                    }
                ],
            }
            result["channels"].append(channel_record)

        teardown_errors = _send_cross_channel_teardown(
            clients=clients,
            participants=participants,
            scenario_id=scenario_id,
            run_id=run_id,
        )
        if teardown_errors:
            result["teardown_errors"] = teardown_errors
        result["status"] = "pass"
        result["coverage_class_actual"] = "live"
        result["coverage_class_contract"] = cell_driver._coverage_class_contract(  # noqa: SLF001
            declared=declared_coverage_class,
            actual="live",
            dry_run=False,
        )
        result["ok"] = True
    except assertions.AssertionError as error:
        result["status"] = "fail"
        result["reason"] = f"assertion: {error}"
        result["failure_attribution"] = cell_driver._failure_attribution(  # noqa: SLF001
            "assertion",
            str(error),
        )
        teardown_errors = _send_cross_channel_teardown(
            clients=clients,
            participants=participants,
            scenario_id=scenario_id,
            run_id=run_id,
        )
        if teardown_errors:
            result["teardown_errors"] = teardown_errors
    except Exception as error:  # noqa: BLE001
        result["status"] = "fail"
        result["reason"] = f"{type(error).__name__}: {error}"
        result["failure_attribution"] = cell_driver._failure_attribution(  # noqa: SLF001
            "exception",
            str(result["reason"]),
        )
        teardown_errors = _send_cross_channel_teardown(
            clients=clients,
            participants=participants,
            scenario_id=scenario_id,
            run_id=run_id,
        )
        if teardown_errors:
            result["teardown_errors"] = teardown_errors

    result["completed_at"] = dt.datetime.now().isoformat(timespec="seconds")
    return result


def _matrix_agent_mode_totals(results: list[dict[str, Any]]) -> dict[str, int]:
    totals = {mode: 0 for mode in cell_driver.AGENT_MODES}
    for result in results:
        nested = result.get("agent_mode_totals")
        if isinstance(nested, dict):
            for mode in totals:
                totals[mode] += int(nested.get(mode) or 0)
            continue
        mode = str(result.get("agent_mode") or "")
        if mode in totals:
            totals[mode] += 1
    return totals


def _matrix_coverage_class_totals(results: list[dict[str, Any]]) -> dict[str, int]:
    totals = {coverage: 0 for coverage in cell_driver.COVERAGE_CLASSES}
    for result in results:
        nested = result.get("coverage_class_totals")
        if isinstance(nested, dict):
            for coverage in totals:
                totals[coverage] += int(nested.get(coverage) or 0)
            continue
        coverage = str(result.get("coverage_class") or "")
        if coverage in totals:
            totals[coverage] += 1
    return totals


def _matrix_coverage_class_violations(
    results: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    violations: list[dict[str, Any]] = []
    for result in results:
        nested = result.get("coverage_class_violations")
        if isinstance(nested, list):
            violations.extend(item for item in nested if isinstance(item, dict))
        attribution = result.get("failure_attribution")
        if not isinstance(attribution, dict):
            continue
        if attribution.get("source") != "coverage_class_gate":
            continue
        violations.append(
            {
                "kind": result.get("kind"),
                "id": result.get("id"),
                "cell": result.get("cell"),
                "provider": result.get("provider"),
                "runtime": result.get("runtime"),
                "coverage_class": result.get("coverage_class"),
                "coverage_class_actual": result.get("coverage_class_actual"),
                "reason": result.get("reason"),
            }
        )
    return violations


def main() -> int:
    args = parse_args()
    cells = parse_cells(args.cells)
    channel_ids = load_channel_ids(Path(args.config).expanduser())
    cross_scenarios = load_cross_channel_scenarios(Path(args.scenarios))
    wanted = parse_filter(args.filter)
    if wanted:
        cross_scenarios = [
            scenario for scenario in cross_scenarios if str(scenario.get("id")) in wanted
        ]
    output_dir = resolve_output_dir(args.output)
    pass_count = 2 if args.twice else 1

    results: list[dict[str, Any]] = []
    for pass_index in range(1, pass_count + 1):
        for cell in cells:
            result = run_cell(
                cell=cell,
                channel_id=channel_ids[cell],
                args=args,
                output_dir=output_dir,
                pass_index=pass_index,
            )
            results.append(result)
            totals = result.get("totals") or {}
            print(
                "[matrix] result "
                f"pass={pass_index} cell={cell} ok={result['ok']} "
                f"pass_count={totals.get('pass')} fail={totals.get('fail')} "
                f"skip={totals.get('skipped')} report={result['report']}"
            )
        for scenario in cross_scenarios:
            print(f"[matrix] cross-channel pass={pass_index} scenario={scenario.get('id')}")
            result = run_cross_channel_scenario(
                scenario,
                args=args,
                run_id=output_dir.name,
                channel_ids=channel_ids,
                selected_cells=cells,
                pass_index=pass_index,
            )
            results.append(result)
            print(
                "[matrix] cross-channel result "
                f"pass={pass_index} scenario={scenario.get('id')} "
                f"status={result['status']} ok={result['ok']} "
                f"reason={result.get('reason') or ''}"
            )

    summary = {
        "output": str(output_dir),
        "cells": cells,
        "passes": pass_count,
        "cross_channel_scenarios": [str(scenario.get("id")) for scenario in cross_scenarios],
        "agent_mode_totals": _matrix_agent_mode_totals(results),
        "coverage_class_totals": _matrix_coverage_class_totals(results),
        "coverage_class_violations": _matrix_coverage_class_violations(results),
        "real_provider_contacted": any(
            result.get("real_provider_contacted") is True for result in results
        ),
        "results": results,
        "ok": all(result["ok"] for result in results),
    }
    summary_path = output_dir / "matrix.json"
    summary_path.write_text(json.dumps(summary, indent=2), encoding="utf-8")
    print(f"[matrix] report -> {summary_path}")
    return 0 if summary["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
