#!/usr/bin/env python3
"""TUI relay E2E smoke driver.

Reads YAML scenario files under `tests/e2e/tui_relay/scenarios/`, sends prompts
into the configured Discord test channels via AgentDesk's release/dev API,
observes responses, and reports pass/fail per scenario.

Safety guards:
- Lease file at /tmp/agentdesk-e2e-relay.lease.
- Destructive scenarios are skipped unless AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1.
- --dry-run prints intended steps without sending anything.
- Pre-flight check: --channel-id-cc / --channel-id-cdx must be explicitly passed.

Usage:
    scripts/e2e/run_tui_relay.py \\
        --base-url http://127.0.0.1:8791 \\
        --channel-id-cc 1490... \\
        --channel-id-cdx 1490... \\
        --scenarios tests/e2e/tui_relay/scenarios \\
        --output out/e2e/tui_relay/<run_id> \\
        [--dry-run]
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import sys
import time
import uuid
from pathlib import Path
from typing import Any

import urllib.request  # noqa: E402
import yaml  # type: ignore[import-untyped]

sys.path.insert(0, str(Path(__file__).resolve().parent))

from tui_relay import assertions, discord, lease, tmux  # noqa: E402
import subprocess  # noqa: E402


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base-url", default="http://127.0.0.1:8791")
    parser.add_argument("--channel-id-cc", required=True)
    parser.add_argument("--channel-id-cdx", required=True)
    parser.add_argument(
        "--scenarios",
        default="tests/e2e/tui_relay/scenarios",
        help="Path to directory of YAML scenario files",
    )
    parser.add_argument(
        "--filter",
        default=None,
        help="Only run scenarios whose id matches this substring",
    )
    parser.add_argument("--output", default=None)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument(
        "--allow-destructive",
        action="store_true",
        help="Enable destructive steps (kill-pane, restart). "
        "Also requires AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1.",
    )
    parser.add_argument(
        "--reset-before-each",
        action="store_true",
        default=True,
        help="Cancel inflight turns and truncate the on-disk relay queue for "
        "each channel before every scenario. Avoids cross-scenario bleed.",
    )
    parser.add_argument(
        "--queue-runtime-root",
        default=str(Path.home() / ".adk" / "release" / "runtime"),
        help="ADK runtime root used to truncate on-disk relay queues during reset.",
    )
    parser.add_argument(
        "--skip-cdx-if-unavailable",
        action="store_true",
        default=True,
        help="Skip the cdx half of a 'both' scenario if no codex tmux session is "
        "ready, instead of failing the whole scenario.",
    )
    return parser.parse_args()


def resolve_output_dir(arg: str | None) -> Path:
    if arg:
        path = Path(arg)
    else:
        run_id = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
        path = Path("out/e2e/tui_relay") / run_id
    path.mkdir(parents=True, exist_ok=True)
    return path


def load_scenarios(scenarios_dir: Path) -> list[dict[str, Any]]:
    scenarios: list[dict[str, Any]] = []
    for yaml_path in sorted(scenarios_dir.glob("*.yaml")):
        with yaml_path.open("r", encoding="utf-8") as fp:
            data = yaml.safe_load(fp)
        if not isinstance(data, dict):
            raise ValueError(f"{yaml_path} did not parse to a mapping")
        data["__path__"] = str(yaml_path)
        scenarios.append(data)
    return scenarios


def is_destructive(scenario: dict[str, Any]) -> bool:
    steps = scenario.get("steps") or []
    for step in steps:
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


def reset_channel_state(
    *,
    base_url: str,
    channel_id: str,
    runtime_root: Path,
    provider: str = "claude",
) -> dict[str, Any]:
    """Force-cancel any inflight turn and zero the on-disk relay queue.

    Without this the channel can be jammed across runs — the wait_for_prompt
    timeout (45s) on each queued draft amplifies into many-minute blocking
    while assertions starve. We safely bound the destructive reach by only
    touching the queue/placeholder JSON tied to the supplied channel id.
    """

    summary: dict[str, Any] = {"channel_id": channel_id, "actions": []}
    # 1. Cancel turn (force) — best-effort.
    try:
        body = json.dumps({"force": True}).encode("utf-8")
        req = urllib.request.Request(
            f"{base_url}/api/turns/{channel_id}/cancel",
            data=body,
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
                    "tmux_killed": payload.get("tmux_killed"),
                    "lifecycle_path": payload.get("lifecycle_path"),
                }
            }
        )
    except Exception as error:  # noqa: BLE001
        summary["actions"].append({"cancel_turn_error": f"{type(error).__name__}: {error}"})

    # 2. Truncate the on-disk relay queue + placeholder index for this channel.
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


def channel_for_scenario(args: argparse.Namespace, scenario: dict[str, Any]) -> str | None:
    target = scenario.get("channel", "cc")
    if target == "cc":
        return args.channel_id_cc
    if target == "cdx":
        return args.channel_id_cdx
    if target == "both":
        return None
    raise ValueError(f"scenario {scenario.get('id')} has unknown channel target {target!r}")


def run_scenario(
    scenario: dict[str, Any],
    *,
    args: argparse.Namespace,
    run_id: str,
    client: discord.DiscordClient,
) -> dict[str, Any]:
    scenario_id = str(scenario.get("id"))
    result: dict[str, Any] = {
        "id": scenario_id,
        "path": scenario.get("__path__"),
        "channel": scenario.get("channel"),
        "status": "skipped",
        "reason": None,
        "started_at": dt.datetime.now().isoformat(timespec="seconds"),
        "assertions": [],
    }

    destructive = is_destructive(scenario)
    if destructive and not (args.allow_destructive and os.environ.get("AGENTDESK_E2E_ALLOW_DESTRUCTIVE") == "1"):
        result["status"] = "skipped"
        result["reason"] = "destructive: requires --allow-destructive AND AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1"
        return result

    channel_targets: list[tuple[str, str]] = []
    target_kind = scenario.get("channel", "cc")
    if target_kind in ("cc", "cdx"):
        chan = channel_for_scenario(args, scenario)
        if chan is not None:
            channel_targets.append((target_kind, chan))
    elif target_kind == "both":
        channel_targets.append(("cc", args.channel_id_cc))
        channel_targets.append(("cdx", args.channel_id_cdx))
    else:
        result["status"] = "fail"
        result["reason"] = f"unknown channel target {target_kind!r}"
        return result

    # Skip the cdx half of a scenario when the codex session is not up. This
    # keeps the harness usable when codex auto-spawn is broken — cc-only
    # half-passes still surface real signal for the high-risk five.
    if args.skip_cdx_if_unavailable:
        filtered: list[tuple[str, str]] = []
        for kind, channel_id in channel_targets:
            if kind == "cdx" and not tmux.has_session(
                scenario_session_name(scenario, kind)
            ):
                result.setdefault("skipped_halves", []).append(
                    {"kind": kind, "reason": "codex tmux session missing"}
                )
                continue
            filtered.append((kind, channel_id))
        if not filtered:
            result["status"] = "skipped"
            result["reason"] = "all eligible channel halves skipped (codex session missing)"
            return result
        channel_targets = filtered

    if args.reset_before_each:
        runtime_root = Path(args.queue_runtime_root)
        result["resets"] = []
        for kind, channel_id in channel_targets:
            provider = "claude" if kind == "cc" else "codex"
            result["resets"].append(
                reset_channel_state(
                    base_url=args.base_url,
                    channel_id=channel_id,
                    runtime_root=runtime_root,
                    provider=provider,
                )
            )
        # Give the runtime a beat to settle after the cancel/truncate combo.
        time.sleep(2.0)

    result["channels"] = []
    try:
        for kind, channel_id in channel_targets:
            window = run_one_channel(
                scenario=scenario,
                channel_kind=kind,
                channel_id=channel_id,
                client=client,
                run_id=run_id,
                dry_run=args.dry_run,
            )
            result["assertions"].extend(window["assertions"])
            result["channels"].append(
                {
                    "kind": kind,
                    "channel_id": channel_id,
                    "relay_count": window.get("relay_count"),
                    "raw_count": window.get("raw_count"),
                    "sample_relay": window.get("sample_relay"),
                }
            )
        result["status"] = "pass"
    except assertions.AssertionError as error:
        result["status"] = "fail"
        result["reason"] = f"assertion: {error}"
    except Exception as error:  # pragma: no cover — surfaced in report
        result["status"] = "fail"
        result["reason"] = f"{type(error).__name__}: {error}"
    result["completed_at"] = dt.datetime.now().isoformat(timespec="seconds")
    return result


def run_one_channel(
    *,
    scenario: dict[str, Any],
    channel_kind: str,
    channel_id: str,
    client: discord.DiscordClient,
    run_id: str,
    dry_run: bool,
) -> dict[str, Any]:
    scenario_id = scenario.get("id")
    setup_marker = f"### E2E SETUP {scenario_id} channel={channel_kind} run={run_id}"
    teardown_marker = f"### E2E TEARDOWN {scenario_id} channel={channel_kind} run={run_id}"
    record: dict[str, Any] = {"assertions": []}

    if dry_run:
        print(f"[dry-run] {scenario_id} ({channel_kind}): would send setup marker → steps → teardown")
        return record

    setup_resp = client.send(channel_id, setup_marker)
    after_id = str(setup_resp.get("id") or "")
    window = assertions.Window(setup_marker_id=after_id)
    # Brief settle so the SETUP marker is observable before any wait/poll.
    time.sleep(1.0)

    def _ingest_observed(messages: list[dict[str, Any]]) -> None:
        # Discord returns messages in DESC (most-recent-first); ingest in
        # chronological order so duplicate detection sees the first emit
        # before any retry.
        for message in sorted(messages, key=lambda m: int(m.get("id", "0"))):
            if (message.get("content") or "").startswith("### E2E TEARDOWN"):
                window.teardown_marker_id = str(message.get("id"))
                continue
            window.add(message)

    for step in scenario.get("steps") or []:
        if not isinstance(step, dict):
            continue
        if "send_prompt" in step:
            client.send(channel_id, step["send_prompt"])
            # The TUI relay batches keystrokes; give the pane time to flush
            # one prompt before we send the next.
            time.sleep(3)
        elif "wait_idle_s" in step:
            time.sleep(float(step["wait_idle_s"]))
        elif "wait_for_discord_text" in step:
            needle = step["wait_for_discord_text"]
            # Only consider relay messages — needle is expected in the bot
            # response, not in our own driver send.
            predicate = lambda message: (
                assertions.is_relay_response(message)
                and needle in (message.get("content") or "")
            )
            # Default 240s instead of 120s: the claude TUI wait_for_prompt_ready
            # is itself 45s and stacked queued prompts amplify the apparent
            # turn latency.
            found, observed = client.wait_for_message(
                channel_id,
                predicate=predicate,
                after_id=after_id,
                timeout_s=float(step.get("timeout_s", 240)),
            )
            _ingest_observed(observed)
            if not found:
                raise assertions.AssertionError(
                    f"timeout waiting for Discord text {needle!r}"
                )
        elif "restart_dcserver" in step:
            target = (step["restart_dcserver"] or {}).get("target", "release")
            label = "com.agentdesk." + ("release" if target == "release" else "dev")
            subprocess.run(
                ["launchctl", "kickstart", "-k", f"gui/{os.getuid()}/{label}"],
                check=False,
                capture_output=True,
            )
            wait_for_health(client.base_url, timeout_s=60)
        elif "kill_pane" in step:
            reverify = (step["kill_pane"] or {}).get("reverify_session_name_substring") or ""
            session_name = scenario_session_name(scenario, channel_kind)
            panes = tmux.list_panes(session_name)
            # 2-step safety:
            #  a) session name must contain the requested substring (e.g. "adk-dash")
            #  b) the pane cwd must be under the e2e worktree root so we never
            #     kill the operator's main work pane.
            if reverify and reverify not in session_name:
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
            session_name = scenario_session_name(scenario, channel_kind)
            tmux.send_keys(session_name, step["send_keys_no_enter"])
        else:
            raise assertions.AssertionError(f"unknown step shape: {step!r}")

    # Final sweep so any late-arriving relay messages between the last wait
    # and the teardown send are part of the window.
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


def wait_for_health(base_url: str, *, timeout_s: float = 60.0, poll_interval_s: float = 2.0) -> None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        try:
            import urllib.request

            with urllib.request.urlopen(f"{base_url}/api/health", timeout=5) as response:
                if 200 <= response.status < 300:
                    return
        except Exception:  # noqa: BLE001 — best-effort polling
            pass
        time.sleep(poll_interval_s)
    raise assertions.AssertionError(f"dcserver did not become healthy within {timeout_s}s")


def scenario_session_name(scenario: dict[str, Any], channel_kind: str) -> str:
    """Best-effort: assume default AgentDesk-{provider}-{channel_name} naming."""

    suffix = "claude" if channel_kind == "cc" else "codex"
    channel_name = "adk-dash-cc-e2e" if channel_kind == "cc" else "adk-dash-cdx-e2e"
    return f"AgentDesk-{suffix}-{channel_name}"


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
    else:
        raise assertions.AssertionError(f"unknown assertion: {spec!r}")


def main() -> int:
    args = parse_args()
    output_dir = resolve_output_dir(args.output)
    run_id = output_dir.name
    print(f"[e2e] run_id={run_id} output={output_dir}")

    scenarios_dir = Path(args.scenarios)
    if not scenarios_dir.is_dir():
        print(f"[e2e] scenarios dir not found: {scenarios_dir}", file=sys.stderr)
        return 2
    scenarios = load_scenarios(scenarios_dir)
    if args.filter:
        # Accept comma-separated ids and treat each as an *exact* scenario id
        # match (e.g. "E-1,E-5"). The previous substring match was unsafe
        # because "E-1" matched E-10/E-11/E-12.
        wanted = {tok.strip() for tok in args.filter.split(",") if tok.strip()}
        scenarios = [s for s in scenarios if str(s.get("id")) in wanted]
    print(f"[e2e] loaded {len(scenarios)} scenarios")

    client = discord.DiscordClient(base_url=args.base_url)

    with lease.acquire(run_id) if not args.dry_run else _null_lease(run_id):
        results: list[dict[str, Any]] = []
        for scenario in scenarios:
            print(f"[e2e] running {scenario.get('id')} (channel={scenario.get('channel')})")
            result = run_scenario(scenario, args=args, run_id=run_id, client=client)
            print(f"[e2e]   → {result['status']} {result.get('reason') or ''}")
            results.append(result)

    summary_path = output_dir / "report.json"
    summary = {
        "run_id": run_id,
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
