#!/usr/bin/env python3
"""Post-deploy smoke verdicts for the ``!clear`` transition + continuous-turn scenarios.

``preflight`` proves the E2E cell idle before the deploy smoke posts anything.
``judge`` folds one ``run_tui_relay.py`` report and the dcserver log bytes
written during that run into one smoke line. Only a fully evidenced clean run
passes; missing or unreadable evidence is a FAIL, never a PASS.

Exit codes: 0 pass/idle, 1 fail/busy, 2 unevaluable input.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any, Iterable

import yaml  # type: ignore[import-untyped]

sys.path.insert(0, str(Path(__file__).resolve().parent))

import run_tui_relay as cell_driver  # noqa: E402

ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*m")
# platform/tmux.rs log_kill_request; the session token ends at the first space.
KILL_REQUEST = re.compile(r"tmux kill requested: session=(\S+) reason=(.*)$")
# codex_tui/input.rs readiness timeout detail, carried by the fallback WARN.
COMPOSER_NOT_DETECTED = "reason=composer_not_detected"
# codex_tui/warm_followup.rs log_fallback message.
COLD_RESUME_FALLBACK = "warm follow-up falling back to one cold resume launch"
# Warm follow-up kill reasons: codex CodexWarmFallbackReason::reason_text and
# claude host retire "claude tui follow-up failed, recreating".
FOLLOWUP_KILL_REASON = "follow-up"
AFTER_CLEAR_LABEL = "AFTER_CLEAR"
LOG_COUNTERS = ("composer_not_detected", "warm_followup_kill", "cold_resume")


def scan_log(lines: Iterable[str], *, session: str, channel_id: str) -> dict[str, int]:
    """Count warm follow-up failure evidence that names this cell's session or channel."""

    # The cell session or one of its thread sessions, never a longer sibling name.
    own_session = re.compile(rf"(?<![\w-]){re.escape(session)}(?:-t\d+)?(?![\w-])")
    own_channel = re.compile(rf"(?<!\d){re.escape(channel_id)}(?!\d)")
    counts = dict.fromkeys(LOG_COUNTERS, 0)
    counts["cell_lines"] = 0
    for raw in lines:
        line = ANSI_ESCAPE.sub("", raw).rstrip("\n")
        kill = KILL_REQUEST.search(line)
        if kill:
            if own_session.fullmatch(kill.group(1)) and FOLLOWUP_KILL_REASON in kill.group(2).lower():
                counts["warm_followup_kill"] += 1
            continue
        if not own_session.search(line) and not own_channel.search(line):
            continue
        counts["cell_lines"] += 1
        if COMPOSER_NOT_DETECTED in line:
            counts["composer_not_detected"] += 1
        if COLD_RESUME_FALLBACK in line:
            counts["cold_resume"] += 1
    return counts


def expected_markers(scenarios_dir: Path, scenario_id: str, run_id: str) -> list[str]:
    for path in sorted(scenarios_dir.glob("*.yaml")):
        data = yaml.safe_load(path.read_text(encoding="utf-8"))
        if isinstance(data, dict) and str(data.get("id")) == scenario_id:
            return [
                str(marker).replace("{run_id}", run_id)
                for marker in data.get("report_marker_counts") or []
            ]
    return []


def _label(marker: str) -> str:
    return marker.rstrip("]").rsplit(":", 1)[-1]


def judge(
    *,
    report: dict[str, Any] | None,
    scenario_id: str,
    markers: list[str],
    log_counts: dict[str, int] | None,
    driver_rc: int,
) -> tuple[bool, str]:
    """Return (passed, detail) for one scenario run; every missing input fails closed."""

    findings: list[str] = []
    result = None
    if report is None:
        findings.append("report unreadable")
    else:
        rows = [row for row in report.get("scenarios") or [] if isinstance(row, dict)]
        result = next((row for row in rows if str(row.get("id")) == scenario_id), None)
        if result is None:
            findings.append("scenario result missing from report")
    if not markers:
        findings.append("scenario declares no report_marker_counts")

    counts = (result or {}).get("marker_counts")
    counts = counts if isinstance(counts, dict) else {}
    marker_parts = []
    for marker in markers:
        label = _label(marker)
        hits = counts.get(marker)
        if not isinstance(hits, int) or isinstance(hits, bool):
            marker_parts.append(f"{label}=?")
            findings.append(f"{label} delivery count unobserved")
            continue
        marker_parts.append(f"{label}={hits}")
        if hits == 0 and label == AFTER_CLEAR_LABEL:
            findings.append(f"{label} body withheld after !clear (0 deliveries)")
        elif hits == 0:
            findings.append(f"{label} body not delivered")
        elif hits > 1:
            findings.append(f"{label} body duplicated ({hits} deliveries)")

    if log_counts is None:
        log_part = "log=unavailable"
        findings.append("dcserver log excerpt unavailable")
    else:
        log_part = " ".join(f"{key}={log_counts.get(key, 0)}" for key in LOG_COUNTERS)
        if any(log_counts.get(key, 0) for key in LOG_COUNTERS):
            findings.append("warm follow-up readiness timeout/kill/cold resume in dcserver log")
        # Every turn logs its channel; none means the excerpt is not this server's run.
        if not log_counts.get("cell_lines", 0):
            findings.append("dcserver log excerpt has no line for this cell")

    status = (result or {}).get("status")
    if status != "pass":
        reason = str((result or {}).get("reason") or "").replace("\n", " ")[:300]
        findings.append(f"scenario status={status or 'missing'}" + (f": {reason}" if reason else ""))
    if driver_rc != 0:
        findings.append(f"driver rc={driver_rc}")

    detail = f"deliveries {' '.join(marker_parts) or 'none'} {log_part}"
    if findings:
        return False, f"{detail}; " + "; ".join(findings)
    return True, detail


def preflight_busy_reasons(
    *, base_url: str, cell: str, channel_id: str, runtime_root: Path
) -> list[str]:
    """Busy reasons for the cell; an absent mailbox is idle, as the E-1 guard treats it."""

    provider = cell_driver.cell_provider(cell)
    mailboxes = cell_driver._read_health_detail(base_url).get("mailboxes")  # noqa: SLF001
    if not isinstance(mailboxes, list):
        raise ValueError("/api/health/detail mailboxes is not a list")
    busy = [
        f"mailbox {provider}:{channel_id} {reason}"
        for mailbox in mailboxes
        if isinstance(mailbox, dict)
        and cell_driver._mailbox_channel_id(mailbox) == str(channel_id)  # noqa: SLF001
        and cell_driver._mailbox_provider(mailbox) == provider  # noqa: SLF001
        for reason in cell_driver._mailbox_busy_reasons(mailbox)  # noqa: SLF001
    ]
    busy.extend(
        cell_driver._runtime_queue_violations(  # noqa: SLF001
            runtime_root=runtime_root, provider=provider, channel_id=str(channel_id)
        )
    )
    return busy


def _load_report(path: Path) -> dict[str, Any] | None:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return payload if isinstance(payload, dict) else None


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    pre = sub.add_parser("preflight")
    pre.add_argument("--base-url", required=True)
    pre.add_argument("--cell", required=True, choices=cell_driver.SUPPORTED_CELLS)
    pre.add_argument("--channel-id", required=True)
    pre.add_argument("--queue-runtime-root", required=True)
    jud = sub.add_parser("judge")
    jud.add_argument("--report", required=True)
    jud.add_argument("--scenarios", required=True)
    jud.add_argument("--scenario", required=True)
    jud.add_argument("--cell", required=True, choices=cell_driver.SUPPORTED_CELLS)
    jud.add_argument("--channel-id", required=True)
    jud.add_argument("--log-excerpt", default="")
    jud.add_argument("--driver-rc", type=int, required=True)
    args = parser.parse_args(argv)

    if args.command == "preflight":
        try:
            busy = preflight_busy_reasons(
                base_url=args.base_url,
                cell=args.cell,
                channel_id=args.channel_id,
                runtime_root=Path(args.queue_runtime_root),
            )
        except Exception as error:  # noqa: BLE001 - any unreadable snapshot is "not proven idle"
            print(f"unreadable: {type(error).__name__}: {error}")
            return 2
        if busy:
            print("busy: " + "; ".join(busy))
            return 1
        print("idle")
        return 0

    report = _load_report(Path(args.report))
    run_id = str((report or {}).get("run_id") or "")
    markers = expected_markers(Path(args.scenarios), args.scenario, run_id) if run_id else []
    log_counts = None
    if args.log_excerpt:
        try:
            with open(args.log_excerpt, encoding="utf-8", errors="replace") as handle:
                log_counts = scan_log(
                    handle,
                    session=cell_driver.cell_session_name(args.cell),
                    channel_id=str(args.channel_id),
                )
        except OSError:
            log_counts = None
    passed, detail = judge(
        report=report,
        scenario_id=args.scenario,
        markers=markers,
        log_counts=log_counts,
        driver_rc=args.driver_rc,
    )
    print(f"relay {args.scenario} cell={args.cell} result={'pass' if passed else 'FAIL'} {detail}")
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
