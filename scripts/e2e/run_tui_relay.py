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

import yaml  # type: ignore[import-untyped]

sys.path.insert(0, str(Path(__file__).resolve().parent))

from tui_relay import assertions, discord, lease  # noqa: E402


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

    for step in scenario.get("steps") or []:
        if not isinstance(step, dict):
            continue
        if "send_prompt" in step:
            client.send(channel_id, step["send_prompt"])
            time.sleep(2)
        elif "wait_idle_s" in step:
            time.sleep(float(step["wait_idle_s"]))
        elif "wait_for_discord_text" in step:
            needle = step["wait_for_discord_text"]
            found = client.wait_for_message(
                channel_id,
                predicate=lambda message: needle in (message.get("content") or ""),
                after_id=after_id,
                timeout_s=float(step.get("timeout_s", 120)),
            )
            if not found:
                raise assertions.AssertionError(f"timeout waiting for Discord text {needle!r}")
            window.add(found)

    messages = client.fetch_messages(channel_id, after_id=after_id)
    for message in messages:
        if (message.get("content") or "").startswith("### E2E TEARDOWN"):
            window.teardown_marker_id = str(message.get("id"))
            break
        window.add(message)

    for assertion_spec in scenario.get("assertions") or []:
        run_assertion(assertion_spec, window=window)
        record["assertions"].append({"spec": assertion_spec, "passed": True})

    client.send(channel_id, teardown_marker)
    return record


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
        scenarios = [s for s in scenarios if args.filter in str(s.get("id"))]
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
