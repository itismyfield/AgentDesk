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
import datetime as dt
import json
import os
import subprocess
import sys
import time
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
    return parser.parse_args()


def cell_provider(cell: str) -> str:
    return cell.split("-", 1)[0]


def cell_runtime(cell: str) -> str:
    return cell.split("-", 1)[1]


def cell_session_name(cell: str) -> str:
    """tmux session name owned by the cell's worker agent."""
    return f"AgentDesk-{cell_provider(cell)}-adk-{cell}-e2e"


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
        for key in ("restart_dcserver", "kill_pane", "kill_tui_process", "send_keys_no_enter"):
            if key in step:
                return True
    return False


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
) -> dict[str, Any]:
    """Burn the cell's TUI session so the next prompt starts fresh."""
    session_name = cell_session_name(cell)
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
        url = f"{base_url}/api/turns/{channel_id}/cancel?force=true"
        req = urllib.request.Request(
            url,
            data=b"",
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=15) as resp:
            payload = json.loads(resp.read().decode("utf-8"))
        summary["actions"].append(
            {
                "cancel_turn": {
                    "ok": payload.get("ok"),
                    "queued_remaining": payload.get("queued_remaining"),
                    "queue_purged": payload.get("queue_purged"),
                    "tmux_killed": payload.get("tmux_killed"),
                    "lifecycle_path": payload.get("lifecycle_path"),
                }
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
                channel_id=args.channel_id,
                runtime_root=runtime_root,
                provider=cell_provider(cell),
            )
        ]
        if args.hard_reset_session_each:
            result["hard_resets"] = [
                hard_reset_provider_session(
                    cell=cell,
                    channel_id=args.channel_id,
                    runtime_root=runtime_root,
                )
            ]
        time.sleep(2.0)

    try:
        window = run_one_cell(
            scenario=scenario,
            cell=cell,
            channel_id=args.channel_id,
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
    except Exception as error:  # noqa: BLE001 — surfaced in report
        result["status"] = "fail"
        result["reason"] = f"{type(error).__name__}: {error}"

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
    teardown_marker = f"### E2E TEARDOWN {scenario_id} cell={cell} run={run_id}"
    record: dict[str, Any] = {"assertions": []}

    if dry_run:
        print(f"[dry-run] {scenario_id} ({cell}): would send setup → steps → teardown")
        return record

    setup_resp = client.send(channel_id, setup_marker)
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

    for step in scenario.get("steps") or []:
        if not isinstance(step, dict):
            continue
        if "send_prompt" in step:
            if not first_send_done:
                _advance_window_past_setup_echo()
                first_send_done = True
            client.send_prompt(
                channel_id,
                step["send_prompt"],
                channel_kind=cell_channel_kind(cell),
            )
            time.sleep(3)
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
        elif "restart_dcserver" in step:
            target = args.restart_target_override or (step["restart_dcserver"] or {}).get(
                "target", "release"
            )
            restart_dcserver_for_e2e(
                target=target, args=args, base_url=client.base_url, cell=cell
            )
        elif "kill_pane" in step:
            session_name = cell_session_name(cell)
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
            tmux.send_keys(cell_session_name(cell), step["send_keys_no_enter"])
        else:
            raise assertions.AssertionError(f"unknown step shape: {step!r}")

    _ingest_observed(client.fetch_messages(channel_id, after_id=after_id, limit=100))

    record["relay_count"] = len(window.messages)
    record["raw_count"] = len(window.raw_messages)
    record["sample_relay"] = [
        (m.get("content") or "")[:120] for m in window.messages[:6]
    ]

    for assertion_spec in scenario.get("assertions") or []:
        run_assertion(assertion_spec, window=window)
        record["assertions"].append({"spec": assertion_spec, "passed": True})

    client.send(channel_id, teardown_marker)
    return record


def wait_for_health(base_url: str, *, timeout_s: float = 90.0, poll_interval_s: float = 2.0) -> None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"{base_url}/api/health", timeout=5) as response:
                if 200 <= response.status < 300:
                    return
        except Exception:  # noqa: BLE001
            pass
        time.sleep(poll_interval_s)
    raise assertions.AssertionError(f"dcserver did not become healthy within {timeout_s}s")


def _guard_no_foreign_active_turns(
    base_url: str, channel_id: str, cell: str
) -> None:
    """Refuse restart when a turn is active on a non-cell channel."""
    e2e_channel_ids = {channel_id} if channel_id else set()
    if not e2e_channel_ids:
        return
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
    busy: list[str] = []
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
            f"(channel={channel_id}). Active: {busy}."
        )


def restart_dcserver_for_e2e(
    *,
    target: str,
    args: argparse.Namespace,
    base_url: str,
    cell: str,
) -> None:
    if target not in ("dev", "release"):
        raise assertions.AssertionError(f"unsupported restart target: {target!r}")
    _guard_no_foreign_active_turns(base_url, args.channel_id, cell)

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
    elif spec.get("no_duplicate_content"):
        assertions.no_duplicate_content(window)
    elif "text_present" in spec:
        assertions.text_present(window, needle=spec["text_present"])
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
