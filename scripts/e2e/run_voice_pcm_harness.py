#!/usr/bin/env python3
"""Run the deterministic unattended voice PCM harness.

The harness feeds generated PCM into the AgentDesk voice receiver boundary and
exercises the real receive/STT/routing/TTS-observability path with local command
shims. It does not join Discord voice and does not cover live Discord media
transport.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any


DEFAULT_REPORT = Path("target/voice-pcm-harness-report.json")
TEST_FILTER = "voice_pcm_harness_unattended_e2e"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--report",
        default=str(DEFAULT_REPORT),
        help="Machine-readable report path written by the Rust harness.",
    )
    parser.add_argument(
        "--cargo",
        default=os.environ.get("CARGO", "cargo"),
        help="Cargo executable to run.",
    )
    parser.add_argument(
        "--quiet",
        action="store_true",
        help="Suppress cargo's passing test output where possible.",
    )
    return parser.parse_args()


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def load_report(path: Path) -> dict[str, Any]:
    if not path.exists():
        raise FileNotFoundError(f"voice PCM harness report was not written: {path}")
    with path.open("r", encoding="utf-8") as fp:
        report = json.load(fp)
    if not isinstance(report, dict):
        raise ValueError(f"voice PCM harness report is not a JSON object: {path}")
    return report


def validate_report(report: dict[str, Any]) -> None:
    if report.get("agent_mode") != "controlled":
        raise ValueError(f"unexpected agent_mode: {report.get('agent_mode')!r}")
    if report.get("live_discord_media_transport_covered") is not False:
        raise ValueError("report must explicitly state live Discord media transport is not covered")

    failures: list[str] = []
    scenarios = list(report.get("scenarios") or [])
    scenarios.extend(report.get("negative_dependency_scenarios") or [])
    if not scenarios:
        raise ValueError("voice PCM harness report has no scenarios")
    for scenario in scenarios:
        if not isinstance(scenario, dict):
            failures.append(f"non-object scenario: {scenario!r}")
            continue
        if scenario.get("status") != "passed":
            raw = scenario.get("raw_failure_reasons") or []
            failures.append(f"{scenario.get('scenario_id')}: {raw}")
    if failures:
        raise ValueError("voice PCM harness failures:\n" + "\n".join(failures))


def main() -> int:
    args = parse_args()
    root = repo_root()
    report_path = Path(args.report)
    if not report_path.is_absolute():
        report_path = root / report_path

    env = os.environ.copy()
    env["ADK_VOICE_PCM_HARNESS_REPORT"] = str(report_path)

    command = [
        args.cargo,
        "test",
        "--lib",
        TEST_FILTER,
        "--",
        "--nocapture",
    ]
    if args.quiet:
        command.insert(2, "--quiet")

    print("Running deterministic voice PCM harness", flush=True)
    print(f"Report: {report_path}", flush=True)
    result = subprocess.run(command, cwd=root, env=env, check=False)
    if result.returncode != 0:
        return result.returncode

    try:
        report = load_report(report_path)
        validate_report(report)
    except Exception as error:
        print(f"Report validation failed: {error}", file=sys.stderr)
        return 1

    print("Voice PCM harness passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
