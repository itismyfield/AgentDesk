#!/usr/bin/env python3
"""Post-deploy live TUI relay continuity smoke for AgentDesk.

The live smoke wraps the existing TUI relay E2E driver and runs the two
restart-boundary scenarios that prove the operational invariant:

* E-9: output is already streaming, the release boundary runs, and the tail
  marker must still arrive in Discord after restart.
* E-19: the TUI tmux session identity is unchanged across restart and a
  post-restart prompt still relays to Discord.

Offline modes intentionally avoid Discord credentials so CI can exercise the
argument, config, and evidence validation logic.
"""

from __future__ import annotations

import argparse
import copy
import datetime as dt
import json
import os
import shlex
import stat
import subprocess
import sys
from pathlib import Path
from typing import Any

import yaml  # type: ignore[import-untyped]

sys.path.insert(0, str(Path(__file__).resolve().parent))

import run_tui_relay as cell_driver  # noqa: E402


DEFAULT_CONFIG = Path.home() / ".adk" / "release" / "config" / "agentdesk.yaml"
DEFAULT_SCENARIOS = Path("tests/e2e/tui_relay/scenarios")
DEFAULT_OUTPUT_ROOT = Path("out/e2e/post_deploy_relay_continuity")
DEFAULT_BASE_URL = "http://127.0.0.1:8791"
TUI_CELLS = ("claude-tui", "codex-tui")
REQUIRED_SCENARIO_IDS = ("E-9", "E-19")
DEFAULT_FILTER = ",".join(REQUIRED_SCENARIO_IDS)
IDLE_RELAY_STALL_STATES = {"", "healthy"}
IDLE_AGENT_STATUSES = {"", "idle", "none"}


def _idle_mailbox(channel_id: str = "222", provider: str = "claude") -> dict[str, Any]:
    return {
        "provider": provider,
        "channel_id": channel_id,
        "agent_turn_status": "idle",
        "has_cancel_token": False,
        "queue_depth": 0,
        "recovery_started": False,
        "active_user_message_id": None,
        "inflight_state_present": False,
        "active_dispatch_present": False,
        "relay_stall_state": "healthy",
        "watcher_attached": True,
        "relay_health": {
            "active_turn": "none",
            "bridge_inflight_present": False,
            "mailbox_has_cancel_token": False,
            "mailbox_active_user_msg_id": None,
            "queue_depth": 0,
            "pending_discord_callback_msg_id": None,
            "pending_thread_proof": False,
            "stale_thread_proof": False,
            "desynced": False,
        },
    }


_FIXTURE_PASS = {
    "cell": "claude-tui",
    "channel_id": "222",
    "tmux": {
        "before": {
            "session_name": "AgentDesk-claude-adk-claude-tui-e2e",
            "pane_ids": ["%1"],
            "pane_pids": ["12345"],
            "cwd": "/repo",
        },
        "after": {
            "session_name": "AgentDesk-claude-adk-claude-tui-e2e",
            "pane_ids": ["%1"],
            "pane_pids": ["12345"],
            "cwd": "/repo",
        },
    },
    "watcher": {"reattached": True},
    "discord": {
        "local_output_after_restart_seen": True,
        "post_restart_marker_seen": True,
        "post_restart_message_id": "333",
    },
    "health_detail": {"mailboxes": [_idle_mailbox()]},
}

BUILTIN_FIXTURES: dict[str, dict[str, Any]] = {
    "pass": _FIXTURE_PASS,
    "relay-missing": {
        **copy.deepcopy(_FIXTURE_PASS),
        "discord": {
            "local_output_after_restart_seen": True,
            "post_restart_marker_seen": False,
        },
    },
    "bad-state": {
        **copy.deepcopy(_FIXTURE_PASS),
        "health_detail": {
            "mailboxes": [
                {
                    **_idle_mailbox(),
                    "agent_turn_status": "active",
                    "inflight_state_present": True,
                    "relay_stall_state": "tmux_alive_relay_dead",
                    "relay_owner_kind": "none",
                    "relay_health": {
                        **_idle_mailbox()["relay_health"],
                        "active_turn": "foreground",
                        "bridge_inflight_present": True,
                        "stale_thread_proof": True,
                    },
                }
            ]
        },
    },
}


class SmokeConfigError(ValueError):
    """Raised when the smoke cannot be configured safely."""


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--base-url", default=DEFAULT_BASE_URL)
    parser.add_argument("--config", default=str(DEFAULT_CONFIG))
    parser.add_argument("--scenarios", default=str(DEFAULT_SCENARIOS))
    parser.add_argument("--cell", default="claude-tui", choices=TUI_CELLS)
    parser.add_argument(
        "--channel-id",
        default=None,
        help="Discord E2E channel id. If omitted, resolved from agentdesk.yaml.",
    )
    parser.add_argument("--output", default=None)
    parser.add_argument("--filter", default=DEFAULT_FILTER)
    parser.add_argument(
        "--queue-runtime-root",
        default=str(Path.home() / ".adk" / "release" / "runtime"),
    )
    parser.add_argument(
        "--turn-start-timeout-s",
        type=float,
        default=float(os.environ.get("AGENTDESK_E2E_TURN_START_TIMEOUT_S", "180")),
    )
    parser.add_argument(
        "--deploy-command",
        default=os.environ.get("AGENTDESK_POST_DEPLOY_RELAY_DEPLOY_COMMAND"),
        help="Command that performs the release deploy/restart boundary.",
    )
    parser.add_argument(
        "--restart-script",
        default=os.environ.get("AGENTDESK_POST_DEPLOY_RELAY_RESTART_SCRIPT"),
        help="Existing run_tui_relay-compatible restart wrapper. Mutually exclusive with --deploy-command.",
    )
    parser.add_argument(
        "--confirm-live",
        action="store_true",
        help="Required for live execution because the smoke drives Discord and restarts release.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Validate config and print the driver command without touching Discord or dcserver.",
    )
    parser.add_argument(
        "--self-check",
        action="store_true",
        help="Validate local script/config prerequisites without running live E2E.",
    )
    parser.add_argument(
        "--strict-live",
        action="store_true",
        help="In --self-check, require live-run config such as channel id and deploy boundary.",
    )
    parser.add_argument(
        "--fixture",
        default=None,
        help="Validate fixture evidence by builtin name or JSON path; no Discord credentials needed.",
    )
    parser.add_argument(
        "--list-fixtures",
        action="store_true",
        help="List builtin fixture names and exit.",
    )
    return parser.parse_args(argv)


def _json_dump(payload: dict[str, Any]) -> None:
    print(json.dumps(payload, indent=2, sort_keys=True))


def _cell_provider(cell: str) -> str:
    return cell_driver.cell_provider(cell)


def _cell_agent_id(cell: str) -> str:
    return cell_driver.cell_default_agent(cell)


def load_channel_id_from_config(config_path: Path, cell: str) -> str:
    path = config_path.expanduser()
    with path.open("r", encoding="utf-8") as handle:
        config = yaml.safe_load(handle)
    agents = config.get("agents") if isinstance(config, dict) else None
    if not isinstance(agents, list):
        raise SmokeConfigError(f"{path} has no agents list")

    agent_id = _cell_agent_id(cell)
    provider = _cell_provider(cell)
    for agent in agents:
        if not isinstance(agent, dict) or agent.get("id") != agent_id:
            continue
        channels = agent.get("channels")
        channel = channels.get(provider) if isinstance(channels, dict) else None
        channel_id = channel.get("id") if isinstance(channel, dict) else None
        if not channel_id:
            raise SmokeConfigError(
                f"{path} agent {agent_id!r} has no {provider} channel id"
            )
        return str(channel_id)
    raise SmokeConfigError(f"{path} agent {agent_id!r} not found")


def resolve_channel_id(args: argparse.Namespace) -> str:
    if args.channel_id:
        return str(args.channel_id)
    return load_channel_id_from_config(Path(args.config), args.cell)


def required_scenario_ids(args: argparse.Namespace) -> tuple[str, ...]:
    requested = tuple(token.strip() for token in str(args.filter).split(",") if token.strip())
    missing = [sid for sid in REQUIRED_SCENARIO_IDS if sid not in requested]
    if missing:
        raise SmokeConfigError(
            "--filter must include "
            + ", ".join(REQUIRED_SCENARIO_IDS)
            + f"; missing {', '.join(missing)}"
        )
    return REQUIRED_SCENARIO_IDS


def validate_required_scenarios(scenarios_dir: Path, cell: str) -> dict[str, Any]:
    path = scenarios_dir.expanduser()
    if not path.is_dir():
        raise SmokeConfigError(f"scenarios dir not found: {path}")
    scenarios = cell_driver.load_scenarios(path, cell=cell)
    found = {str(scenario.get("id")) for scenario in scenarios}
    missing = [sid for sid in REQUIRED_SCENARIO_IDS if sid not in found]
    if missing:
        raise SmokeConfigError(
            f"{path} is missing required scenario(s) for {cell}: {', '.join(missing)}"
        )
    return {
        "scenarios_dir": str(path),
        "required_ids": list(REQUIRED_SCENARIO_IDS),
        "found_count": len(found),
    }


def resolve_output_dir(arg: str | None) -> Path:
    if arg:
        path = Path(arg)
    else:
        run_id = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
        path = DEFAULT_OUTPUT_ROOT / run_id
    path.mkdir(parents=True, exist_ok=True)
    return path


def _restart_boundary_path(args: argparse.Namespace, output_dir: Path) -> Path | None:
    if args.deploy_command and args.restart_script:
        raise SmokeConfigError("--deploy-command and --restart-script are mutually exclusive")
    if args.restart_script:
        script = Path(args.restart_script).expanduser()
        if not script.exists():
            raise SmokeConfigError(f"restart script not found: {script}")
        return script
    if not args.deploy_command:
        return None
    wrapper = output_dir / "restart-via-deploy-command.sh"
    wrapper.write_text(
        "\n".join(
            [
                "#!/usr/bin/env bash",
                "set -euo pipefail",
                'target="${1:-release}"',
                'if [ "$target" != "release" ]; then',
                '  echo "post-deploy smoke only supports release restart target, got: $target" >&2',
                "  exit 64",
                "fi",
                'if [ -z "${AGENTDESK_POST_DEPLOY_RELAY_DEPLOY_COMMAND:-}" ]; then',
                '  echo "AGENTDESK_POST_DEPLOY_RELAY_DEPLOY_COMMAND is required" >&2',
                "  exit 64",
                "fi",
                'exec bash -lc "$AGENTDESK_POST_DEPLOY_RELAY_DEPLOY_COMMAND"',
                "",
            ]
        ),
        encoding="utf-8",
    )
    wrapper.chmod(wrapper.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return wrapper


def build_driver_command(
    *,
    args: argparse.Namespace,
    channel_id: str,
    output_dir: Path,
    restart_script: Path,
) -> list[str]:
    cmd = [
        sys.executable,
        "scripts/e2e/run_tui_relay.py",
        "--base-url",
        args.base_url,
        "--cell",
        args.cell,
        "--channel-id",
        str(channel_id),
        "--scenarios",
        str(args.scenarios),
        "--filter",
        str(args.filter),
        "--output",
        str(output_dir),
        "--queue-runtime-root",
        str(args.queue_runtime_root),
        "--restart-target-override",
        "release",
        "--restart-script",
        str(restart_script),
        "--allow-destructive",
        "--turn-start-timeout-s",
        str(args.turn_start_timeout_s),
    ]
    return cmd


def _check(
    checks: list[dict[str, Any]],
    name: str,
    ok: bool,
    detail: str,
    *,
    fatal: bool,
) -> None:
    checks.append({"name": name, "ok": ok, "fatal": fatal, "detail": detail})


def run_self_check(
    args: argparse.Namespace,
    *,
    strict_live: bool,
) -> dict[str, Any]:
    checks: list[dict[str, Any]] = []

    _check(
        checks,
        "cell",
        args.cell in TUI_CELLS,
        f"cell={args.cell}",
        fatal=True,
    )

    driver_path = Path("scripts/e2e/run_tui_relay.py")
    _check(
        checks,
        "driver",
        driver_path.exists(),
        str(driver_path),
        fatal=True,
    )

    try:
        scenario_detail = validate_required_scenarios(Path(args.scenarios), args.cell)
    except Exception as error:  # noqa: BLE001 - reported as a check
        _check(checks, "scenarios", False, str(error), fatal=True)
    else:
        _check(checks, "scenarios", True, json.dumps(scenario_detail), fatal=True)

    try:
        required_scenario_ids(args)
    except Exception as error:  # noqa: BLE001
        _check(checks, "filter", False, str(error), fatal=True)
    else:
        _check(checks, "filter", True, str(args.filter), fatal=True)

    try:
        channel_id = resolve_channel_id(args)
    except Exception as error:  # noqa: BLE001
        _check(checks, "channel", False, str(error), fatal=strict_live)
    else:
        _check(checks, "channel", True, f"{args.cell} channel={channel_id}", fatal=True)

    boundary_exclusive = not (args.deploy_command and args.restart_script)
    _check(
        checks,
        "restart_boundary_exclusive",
        boundary_exclusive,
        "--deploy-command and --restart-script are mutually exclusive",
        fatal=True,
    )

    boundary_ok = bool(args.deploy_command or args.restart_script) and boundary_exclusive
    _check(
        checks,
        "restart_boundary",
        boundary_ok,
        "--deploy-command or --restart-script"
        if not boundary_ok
        else (
            "--restart-script " + str(args.restart_script)
            if args.restart_script
            else "--deploy-command " + shlex.quote(str(args.deploy_command))
        ),
        fatal=strict_live,
    )

    if args.restart_script:
        restart_path = Path(args.restart_script).expanduser()
        _check(
            checks,
            "restart_script_exists",
            restart_path.exists(),
            str(restart_path),
            fatal=True,
        )

    if args.deploy_command and "deploy-release.sh" not in str(args.deploy_command):
        _check(
            checks,
            "deploy_command_shape",
            True,
            "custom deploy command; verify it preserves tmux sessions",
            fatal=False,
        )
    elif args.deploy_command:
        _check(
            checks,
            "deploy_command_shape",
            True,
            "deploy-release.sh command detected",
            fatal=False,
        )

    ok = all(check["ok"] or not check["fatal"] for check in checks)
    return {
        "mode": "self-check",
        "ok": ok,
        "strict_live": strict_live,
        "checks": checks,
    }


def _load_fixture(name_or_path: str) -> dict[str, Any]:
    if name_or_path in BUILTIN_FIXTURES:
        return copy.deepcopy(BUILTIN_FIXTURES[name_or_path])
    path = Path(name_or_path)
    with path.open("r", encoding="utf-8") as handle:
        payload = json.load(handle)
    if not isinstance(payload, dict):
        raise SmokeConfigError(f"fixture must be a JSON object: {path}")
    return payload


def _truthy_identity(value: Any) -> bool:
    if value is None or value is False:
        return False
    if isinstance(value, (int, float)) and value == 0:
        return False
    if isinstance(value, str) and value.strip().lower() in {"", "0", "none", "null"}:
        return False
    return True


def _as_nonnegative_int(value: Any) -> int:
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, int):
        return max(value, 0)
    if isinstance(value, str) and value.isdigit():
        return int(value)
    return 0


def _mailbox_provider(mailbox: dict[str, Any]) -> str:
    return str(mailbox.get("provider") or mailbox.get("session_provider") or "").lower()


def _mailbox_channel_id(mailbox: dict[str, Any]) -> str:
    return str(mailbox.get("channel_id") or mailbox.get("channelId") or "")


def _mailbox_label(mailbox: dict[str, Any]) -> str:
    provider = _mailbox_provider(mailbox) or "<provider?>"
    channel = _mailbox_channel_id(mailbox) or "<channel?>"
    return f"{provider}:{channel}"


def _mailbox_bad_state_reasons(mailbox: dict[str, Any]) -> list[str]:
    reasons: list[str] = []
    relay = mailbox.get("relay_health")
    relay_health = relay if isinstance(relay, dict) else {}

    status = str(mailbox.get("agent_turn_status") or "").lower()
    if status not in IDLE_AGENT_STATUSES:
        reasons.append(f"agent_turn_status={status}")
    if mailbox.get("has_cancel_token") is True:
        reasons.append("has_cancel_token=true")
    if mailbox.get("inflight_state_present") is True:
        reasons.append("inflight_state_present=true")
    if _as_nonnegative_int(mailbox.get("queue_depth")) > 0:
        reasons.append(f"queue_depth={mailbox.get('queue_depth')}")
    if mailbox.get("active_dispatch_present") is True:
        reasons.append("active_dispatch_present=true")
    if mailbox.get("recovery_started") is True:
        reasons.append("recovery_started=true")
    if _truthy_identity(mailbox.get("active_user_message_id")):
        reasons.append(f"active_user_message_id={mailbox.get('active_user_message_id')}")

    relay_owner_kind = str(mailbox.get("relay_owner_kind") or "").lower()
    if mailbox.get("inflight_state_present") is True and relay_owner_kind in {
        "",
        "none",
        "unknown",
    }:
        reasons.append(f"ownerless_inflight relay_owner_kind={relay_owner_kind or '<missing>'}")
    if mailbox.get("watcher_attached") is False and (
        mailbox.get("inflight_state_present") is True or status not in IDLE_AGENT_STATUSES
    ):
        reasons.append("watcher_attached=false while mailbox is live")

    stall_state = str(mailbox.get("relay_stall_state") or "").lower()
    if stall_state not in IDLE_RELAY_STALL_STATES:
        reasons.append(f"relay_stall_state={stall_state}")

    relay_active_turn = str(relay_health.get("active_turn") or "").lower()
    if relay_active_turn not in {"", "none"}:
        reasons.append(f"relay_health.active_turn={relay_active_turn}")
    if relay_health.get("bridge_inflight_present") is True:
        reasons.append("relay_health.bridge_inflight_present=true")
    if relay_health.get("mailbox_has_cancel_token") is True:
        reasons.append("relay_health.mailbox_has_cancel_token=true")
    if _truthy_identity(relay_health.get("mailbox_active_user_msg_id")):
        reasons.append(
            "relay_health.mailbox_active_user_msg_id="
            f"{relay_health.get('mailbox_active_user_msg_id')}"
        )
    if _as_nonnegative_int(relay_health.get("queue_depth")) > 0:
        reasons.append(f"relay_health.queue_depth={relay_health.get('queue_depth')}")
    if _truthy_identity(relay_health.get("pending_discord_callback_msg_id")):
        reasons.append(
            "orphaned_target pending_discord_callback_msg_id="
            f"{relay_health.get('pending_discord_callback_msg_id')}"
        )
    if relay_health.get("pending_thread_proof") is True:
        reasons.append("pending_thread_proof=true")
    if relay_health.get("stale_thread_proof") is True:
        reasons.append("stale_thread_proof=true")
    if relay_health.get("desynced") is True:
        reasons.append("relay_health.desynced=true")
    if relay_health.get("stale_status_panel") is True:
        reasons.append("stale_status_panel=true")
    if relay_health.get("stale_footer_target") is True:
        reasons.append("stale_footer_target=true")
    return reasons


def validate_fixture_evidence(evidence: dict[str, Any]) -> list[str]:
    violations: list[str] = []

    tmux = evidence.get("tmux") if isinstance(evidence.get("tmux"), dict) else {}
    if tmux.get("session_preserved") is False:
        violations.append("tmux session_preserved=false")
    before = tmux.get("before")
    after = tmux.get("after")
    if isinstance(before, dict) and isinstance(after, dict):
        if before != after:
            violations.append("tmux identity changed across deploy boundary")
    elif tmux.get("session_preserved") is not True:
        violations.append("tmux before/after identity evidence missing")

    watcher = evidence.get("watcher") if isinstance(evidence.get("watcher"), dict) else {}
    if watcher.get("reattached") is not True:
        violations.append("watcher reattach evidence missing or false")

    discord = evidence.get("discord") if isinstance(evidence.get("discord"), dict) else {}
    post_marker_seen = discord.get("post_restart_marker_seen") is True
    local_output_seen = discord.get("local_output_after_restart_seen") is True
    if not post_marker_seen:
        if local_output_seen:
            violations.append(
                "local output exists after restart but Discord post-restart marker is missing"
            )
        else:
            violations.append("Discord post-restart relay marker missing")

    health_detail = evidence.get("health_detail")
    mailboxes = health_detail.get("mailboxes") if isinstance(health_detail, dict) else None
    if not isinstance(mailboxes, list):
        violations.append("health_detail.mailboxes missing")
    else:
        target_channel = str(evidence.get("channel_id") or "")
        target_provider = _cell_provider(str(evidence.get("cell") or "claude-tui"))
        target_seen = False
        for mailbox in mailboxes:
            if not isinstance(mailbox, dict):
                continue
            if target_channel and _mailbox_channel_id(mailbox) != target_channel:
                continue
            if target_provider and _mailbox_provider(mailbox) != target_provider:
                continue
            target_seen = True
            for reason in _mailbox_bad_state_reasons(mailbox):
                violations.append(f"{_mailbox_label(mailbox)} {reason}")
        if target_channel and not target_seen:
            violations.append(
                f"health_detail has no target mailbox provider={target_provider} channel={target_channel}"
            )
    return violations


def _scenario_by_id(report: dict[str, Any]) -> dict[str, dict[str, Any]]:
    scenarios = report.get("scenarios")
    if not isinstance(scenarios, list):
        return {}
    return {
        str(scenario.get("id")): scenario
        for scenario in scenarios
        if isinstance(scenario, dict)
    }


def validate_driver_report(report: dict[str, Any]) -> list[str]:
    violations: list[str] = []
    totals = report.get("totals") if isinstance(report.get("totals"), dict) else {}
    if int(totals.get("fail") or 0) > 0:
        violations.append(f"driver reported failed scenario count={totals.get('fail')}")

    scenarios = _scenario_by_id(report)
    for scenario_id in REQUIRED_SCENARIO_IDS:
        scenario = scenarios.get(scenario_id)
        if not scenario:
            violations.append(f"driver report missing required scenario {scenario_id}")
            continue
        if scenario.get("status") != "pass":
            violations.append(
                f"{scenario_id} status={scenario.get('status')} reason={scenario.get('reason')}"
            )
            continue
        if scenario.get("wait_timeouts"):
            violations.append(f"{scenario_id} has wait timeout diagnostics")
        if not scenario.get("post_scenario_idle"):
            violations.append(f"{scenario_id} missing post-scenario idle evidence")
        relay_count = int(scenario.get("relay_count") or 0)
        if scenario_id == "E-9" and relay_count <= 0:
            violations.append("E-9 did not record any relay messages")
        if scenario_id == "E-19" and not scenario.get("session_preserved"):
            violations.append("E-19 missing tmux session preservation evidence")
    return violations


def _report_path(output_dir: Path, cell: str) -> Path:
    return output_dir / f"report.{cell}.json"


def _load_driver_report(output_dir: Path, cell: str) -> dict[str, Any]:
    path = _report_path(output_dir, cell)
    with path.open("r", encoding="utf-8") as handle:
        report = json.load(handle)
    if not isinstance(report, dict):
        raise SmokeConfigError(f"driver report is not a JSON object: {path}")
    return report


def run_fixture_mode(args: argparse.Namespace) -> int:
    evidence = _load_fixture(str(args.fixture))
    violations = validate_fixture_evidence(evidence)
    result = {
        "mode": "fixture",
        "fixture": args.fixture,
        "ok": not violations,
        "violations": violations,
    }
    _json_dump(result)
    return 0 if result["ok"] else 1


def run_dry_run(args: argparse.Namespace) -> int:
    checks = run_self_check(args, strict_live=True)
    if not checks["ok"]:
        _json_dump(
            {
                "mode": "dry-run",
                "ok": False,
                "self_check": checks,
                "driver_command": None,
            }
        )
        return 2
    output_dir = resolve_output_dir(args.output)
    restart_script = _restart_boundary_path(args, output_dir)
    channel_id = resolve_channel_id(args)
    command = (
        build_driver_command(
            args=args,
            channel_id=channel_id,
            output_dir=output_dir,
            restart_script=restart_script,
        )
        if restart_script
        else []
    )
    result = {
        "mode": "dry-run",
        "ok": bool(checks["ok"] and command),
        "self_check": checks,
        "driver_command": command,
        "driver_command_shell": shlex.join(command) if command else None,
        "output": str(output_dir),
    }
    _json_dump(result)
    return 0 if result["ok"] else 2


def run_live(args: argparse.Namespace) -> int:
    if not args.confirm_live:
        _json_dump(
            {
                "mode": "live",
                "ok": False,
                "violations": ["live execution requires --confirm-live"],
            }
        )
        return 2
    checks = run_self_check(args, strict_live=True)
    if not checks["ok"]:
        _json_dump({"mode": "live", "ok": False, "self_check": checks})
        return 2

    output_dir = resolve_output_dir(args.output)
    restart_script = _restart_boundary_path(args, output_dir)
    if restart_script is None:
        raise SmokeConfigError("live smoke requires --deploy-command or --restart-script")
    channel_id = resolve_channel_id(args)
    cmd = build_driver_command(
        args=args,
        channel_id=channel_id,
        output_dir=output_dir,
        restart_script=restart_script,
    )

    env = dict(os.environ)
    env["AGENTDESK_E2E_ALLOW_DESTRUCTIVE"] = "1"
    if args.deploy_command:
        env["AGENTDESK_POST_DEPLOY_RELAY_DEPLOY_COMMAND"] = str(args.deploy_command)

    print("[post-deploy-smoke] command=" + shlex.join(cmd))
    proc = subprocess.run(cmd, check=False, text=True, env=env)
    report_path = _report_path(output_dir, args.cell)
    violations: list[str] = []
    report: dict[str, Any] | None = None
    if not report_path.exists():
        violations.append(f"driver report missing: {report_path}")
    else:
        report = _load_driver_report(output_dir, args.cell)
        violations.extend(validate_driver_report(report))
    if proc.returncode != 0:
        violations.append(f"driver exited with {proc.returncode}")

    summary = {
        "mode": "live",
        "ok": not violations,
        "cell": args.cell,
        "channel_id": channel_id,
        "output": str(output_dir),
        "driver_report": str(report_path),
        "driver_returncode": proc.returncode,
        "violations": violations,
        "totals": (report or {}).get("totals"),
    }
    summary_path = output_dir / "post_deploy_relay_continuity.summary.json"
    summary_path.write_text(json.dumps(summary, indent=2, sort_keys=True), encoding="utf-8")
    _json_dump(summary)
    return 0 if summary["ok"] else 1


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        if args.list_fixtures:
            _json_dump({"fixtures": sorted(BUILTIN_FIXTURES)})
            return 0
        if args.fixture:
            return run_fixture_mode(args)
        if args.self_check:
            result = run_self_check(args, strict_live=bool(args.strict_live))
            _json_dump(result)
            return 0 if result["ok"] else 2
        if args.dry_run:
            return run_dry_run(args)
        return run_live(args)
    except SmokeConfigError as error:
        _json_dump({"ok": False, "error": str(error)})
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
