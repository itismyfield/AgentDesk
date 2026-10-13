#!/usr/bin/env python3
"""Post-deploy smoke verdicts for the ``!clear`` transition + continuous-turn scenarios.

``preflight`` proves the E2E cell idle before the deploy smoke posts anything.
``judge`` folds one ``run_tui_relay.py`` report and the dcserver log bytes
written during that run into one smoke line. Only a fully evidenced clean run
passes; missing or unreadable evidence is a FAIL, never a PASS.

Exit codes: 0 pass/idle or explicitly classified gap, 1 fail/busy, 2 unevaluable input.
"""

from __future__ import annotations

import argparse
import copy
import json
import re
import sys
from pathlib import Path
from typing import Any, Iterable

import yaml  # type: ignore[import-untyped]

sys.path.insert(0, str(Path(__file__).resolve().parent))

import run_tui_relay as cell_driver  # noqa: E402
from post_deploy_relay_continuity import channel_is_herdr  # noqa: E402

ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*m")
# platform/tmux.rs log_kill_request; the session token ends at the first space.
KILL_REQUEST = re.compile(r"tmux kill requested: session=(\S+) reason=(.*)$")
# codex_tui/input.rs readiness timeout detail, carried by the fallback WARN.
COMPOSER_NOT_DETECTED = "reason=composer_not_detected"
# codex_tui/warm_followup.rs log_fallback message.
COLD_RESUME_FALLBACK = "warm follow-up falling back to one cold resume launch"
# The same WARN names a launch-options mismatch kind and, when known, the changed fields.
LAUNCH_OPTIONS_MISMATCH = re.compile(r'launch_options_mismatch="(\w+)"')
CHANGED_FIELDS = re.compile(r'changed_fields="([^"]*)"')
# Warm follow-up kill reasons: codex CodexWarmFallbackReason::reason_text and
# claude host retire "claude tui follow-up failed, recreating".
FOLLOWUP_KILL_REASON = "follow-up"
AFTER_CLEAR_LABEL = "AFTER_CLEAR"
LOG_COUNTERS = ("composer_not_detected", "warm_followup_kill", "cold_resume")


def scan_log(
    lines: Iterable[str],
    *,
    session: str,
    channel_id: str,
    launch_options: list[str] | None = None,
) -> dict[str, int]:
    """Count warm follow-up failure evidence that names this cell's session or channel.

    Each launch-options fallback is appended to ``launch_options`` as ``kind`` or ``kind(fields)``.
    """

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
            mismatch = LAUNCH_OPTIONS_MISMATCH.search(line)
            if mismatch and launch_options is not None:
                fields = CHANGED_FIELDS.search(line)
                fields_text = fields.group(1) if fields else ""
                kind = mismatch.group(1)
                launch_options.append(f"{kind}({fields_text})" if fields_text else kind)
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
    launch_options: Iterable[str] = (),
    cell: str | None = None,
    channel_id: str | None = None,
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
        elif hits != 1:
            findings.append(f"{label} invalid delivery count ({hits})")

    observed_cell = cell or (result or {}).get("cell") or (report or {}).get("cell", "")
    if observed_cell.endswith("-herdr"):
        log_part = "herdr=detail+CLI"
        if sorted(_label(marker) for marker in markers) != ["AFTER_CLEAR", "T1", "T2"]:
            findings.append("Herdr smoke requires T1, T2 and AFTER_CLEAR delivery counts")
        observations = (result or {}).get("herdr_observations")
        try:
            if not isinstance(observations, list) or len(observations) < 2:
                raise ValueError("preflight/post-turn observations missing")
            expected_channel = channel_id or (report or {}).get("channel_id") or (result or {}).get("channel_id")
            if not expected_channel:
                raise ValueError("target channel missing")
            for evidence in observations:
                if (evidence.get("surface") != "GET /api/health/detail" or evidence.get("validated") is not True
                        or evidence.get("status_surface") != "agentdesk herdr status"
                        or type(evidence.get("current_input_holds")) is not int
                        or evidence["current_input_holds"] != 0):
                    raise ValueError("detail/CLI evidence incomplete")
                cell_driver.herdr.clean_turn(evidence.get("herdr", {}))
            cell_driver.herdr.current_row({"executions": [observations[-1].get("row")], "other_input_holds": []},
                                         channel=expected_channel, provider=cell_driver.cell_provider(observed_cell))
        except (ValueError, TypeError, AttributeError, cell_driver.assertions.AssertionError) as error:
            findings.append(f"Herdr observation unavailable: {error}")
    elif log_counts is None:
        log_part = "log=unavailable"
        findings.append("dcserver log excerpt unavailable")
    else:
        log_part = " ".join(f"{key}={log_counts.get(key, 0)}" for key in LOG_COUNTERS)
        launch_option_parts = list(launch_options)
        if launch_option_parts:
            log_part += " launch_options=" + ";".join(launch_option_parts)
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


def classify(
    *, report: dict[str, Any] | None, scenario_id: str, cell: str, channel_id: str,
    run_id: str, config_path: Path, driver_rc: int, markers: list[str] | tuple[str, ...] = (),
    log_counts: dict[str, int] | None = None, launch_options: Iterable[str] = (),
) -> dict[str, Any]:
    """Classify only current, configured Herdr refusals; preserve the driver report."""
    outcome = {"classification": "FAIL", "observed_reason": "evidence incomplete", "known_gaps": []}
    try:
        designated = channel_is_herdr(config_path, cell, channel_id)
        rows = report.get("scenarios") if isinstance(report, dict) else None
        if (not run_id or not isinstance(rows, list) or len(rows) != 1
                or report.get("run_id") != run_id or report.get("cell") != cell
                or str(report.get("channel_id")) != str(channel_id)):
            return outcome
        row = rows[0]
        if (not isinstance(row, dict) or row.get("id") != scenario_id
                or row.get("run_id") != run_id or row.get("cell") != cell
                or str(row.get("channel_id")) != str(channel_id)):
            return outcome
        if (report.get("coverage_class_violations") or row.get("teardown_error")
                or row.get("dirty_active_residue")):
            return outcome
        passed, detail = (judge(report=report, scenario_id=scenario_id, markers=markers,
                               log_counts=log_counts, driver_rc=driver_rc, cell=cell,
                               channel_id=channel_id, launch_options=launch_options)
                          if scenario_id != "E-1" else (row.get("status") == "pass" and driver_rc == 0,
                                                       "round-trip"))
        outcome.update(classification="PASS" if passed else "FAIL", observed_reason=detail)
        classification = None
        if designated and scenario_id == "E-50" and passed:
            classification, detail = "UNEXPECTED_PASS", "full judge passed on Herdr-configured channel"
        elif designated and row.get("status") == "fail" and driver_rc == 1:
            if (row.get("teardown_error") or row.get("wait_timeouts") or row.get("dirty_active_residue")
                    or any(a.get("status") in {"fail", "unevaluable"} or a.get("passed") is False for a in row.get("assertions", []))):
                return outcome
            reason = row.get("reason", "")
            attribution = row.get("failure_attribution") or {}
            if attribution and attribution.get("raw_reason") != reason.removeprefix("assertion: "):
                return outcome
            if scenario_id == "E-1":
                match = re.fullmatch(r"RuntimeError: turn/start HTTP 422: (.+)", reason, re.DOTALL)
                body = json.loads(match[1]) if match else None
                if (isinstance(body, dict) and body.get("status") == "invalid_target"
                        and body.get("error") == "herdr turn refused: executor_not_wired"
                        and (not attribution or attribution.get("source") == "exception")):
                    classification, detail = "KNOWN_GAP", "HTTP 422 invalid_target/executor_not_wired"
            elif scenario_id == "E-50":
                refusal = row.get("clear_refusal") or {}
                reply = refusal.get("reply") or {}
                after = str(refusal.get("after_id") or "")
                content = reply.get("content", "")
                match = re.fullmatch(r"세션을 초기화하지 못했어요: herdr clear refused: rotation_unsettled\(([1-9][0-9]*)\)", content)
                if (refusal.get("stage") != "clear_ack" or not after.isdigit() or int(after) <= 0 or not match
                        or cell_driver.assertions.is_our_send(reply)
                        or not cell_driver.herdr.clear_refusal([reply], after=int(after), channel=channel_id)
                        or reason != "assertion: text !clear refused by the server: " + content
                        or (attribution and attribution.get("source") != "assertion")):
                    return outcome
                counts = row.get("marker_counts") or {}
                if (sorted(_label(m) for m in markers) != ["AFTER_CLEAR", "T1", "T2"]
                        or any(type(counts.get(m)) is not int or counts[m] !=
                               (0 if _label(m) == AFTER_CLEAR_LABEL else 1) for m in markers)):
                    return outcome
                # Run the unchanged judge after replacing only the interrupted clear transition.
                complete = copy.deepcopy(report)
                complete["scenarios"][0].update(status="pass", reason=None, marker_counts=dict.fromkeys(markers, 1))
                clean, _ = judge(report=complete, scenario_id=scenario_id, markers=markers,
                                 log_counts=log_counts, driver_rc=0, cell=cell,
                                 channel_id=channel_id, launch_options=launch_options)
                if clean:
                    classification, detail = "KNOWN_GAP", f"herdr clear refused: rotation_unsettled({match[1]})"
        if classification:
            observed = {"reason": detail}
            cell_driver.known_gap.apply_herdr_result(
                observed, {"profile": "post_deploy_configured_herdr", "issue": detail},
                passed=classification == "UNEXPECTED_PASS")
            outcome.update(classification=classification, observed_reason=detail, known_gaps=observed["known_gaps"])
        return outcome
    except (OSError, ValueError, TypeError, AttributeError, KeyError, yaml.YAMLError):
        return {"classification": "FAIL", "observed_reason": "configuration or evidence unreadable", "known_gaps": []}


def preflight_busy_reasons(
    *, base_url: str, cell: str, channel_id: str, runtime_root: Path | None = None,
    herdr_endpoint: str | None = None, herdr_status_bin: str = "agentdesk",
) -> list[str]:
    """Busy reasons for the cell; an absent mailbox is idle, as the E-1 guard treats it."""

    if cell_driver.cell_runtime(cell) == "herdr":
        args = argparse.Namespace(base_url=base_url, cell=cell, channel_id=channel_id,
                                  herdr_endpoint=herdr_endpoint, herdr_status_bin=herdr_status_bin)
        cell_driver.herdr.observe(cell_driver, args, cold=True)
        return []
    if runtime_root is None:
        raise ValueError("legacy preflight requires --queue-runtime-root")
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
    pre.add_argument("--queue-runtime-root")
    pre.add_argument("--herdr-endpoint")
    pre.add_argument("--herdr-status-bin", default="agentdesk")
    jud = sub.add_parser("judge")
    jud.add_argument("--config")
    jud.add_argument("--run-id")
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
                runtime_root=Path(args.queue_runtime_root) if args.queue_runtime_root else None,
                herdr_endpoint=args.herdr_endpoint, herdr_status_bin=args.herdr_status_bin,
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
    launch_options: list[str] = []
    if args.log_excerpt and cell_driver.cell_runtime(args.cell) != "herdr":
        try:
            with open(args.log_excerpt, encoding="utf-8", errors="replace") as handle:
                log_counts = scan_log(
                    handle,
                    session=cell_driver.cell_session_name(args.cell),
                    channel_id=str(args.channel_id),
                    launch_options=launch_options,
                )
        except OSError:
            log_counts = None
    passed, detail = judge(
        report=report,
        scenario_id=args.scenario,
        markers=markers,
        log_counts=log_counts,
        driver_rc=args.driver_rc,
        launch_options=launch_options,
        cell=args.cell, channel_id=args.channel_id,
    )
    if args.config:
        outcome = classify(report=report, scenario_id=args.scenario, cell=args.cell,
                           channel_id=args.channel_id, run_id=args.run_id or "",
                           config_path=Path(args.config), driver_rc=args.driver_rc, markers=markers,
                           log_counts=log_counts, launch_options=launch_options)
        classification = outcome["classification"]
        if classification in {"KNOWN_GAP", "UNEXPECTED_PASS"}:
            print(f"{classification}: relay {args.scenario} cell={args.cell} reason={outcome['observed_reason']}")
            return 0
        if classification == "FAIL":
            passed = False
        if args.scenario == "E-1":
            detail = outcome["observed_reason"]
            passed = classification == "PASS"
    print(f"relay {args.scenario} cell={args.cell} result={'pass' if passed else 'FAIL'} {detail}")
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
