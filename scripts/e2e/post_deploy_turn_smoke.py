#!/usr/bin/env python3
"""Post-deploy smoke verdicts for the ``!clear`` transition + continuous-turn scenarios.

``cell-reset`` clears configured TUI cells once, before the smoke's first preflight.
``cell-path`` names the session host the configured cell channel really runs on.
``gateway-ready`` waits, up to a deadline, until the gateway can take Discord input.
``preflight`` proves the E2E cell idle before the deploy smoke posts prompts.
``judge`` folds one ``run_tui_relay.py`` report and the dcserver log bytes
written during that run into one smoke line. Only a fully evidenced clean run
passes; missing or unreadable evidence is a FAIL, never a PASS.

Exit codes: 0 pass/idle/ready, 1 fail/busy/not ready, 2 unreadable input, 3 foreign reset lease,
4 failed reset state.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
import urllib.request
from contextlib import ExitStack
from pathlib import Path
from typing import Any, Callable, Iterable

import yaml  # type: ignore[import-untyped]

sys.path.insert(0, str(Path(__file__).resolve().parent))

import run_tui_relay as cell_driver  # noqa: E402
from post_deploy_relay_continuity import (  # noqa: E402
    SmokeConfigError, _channel_id_from_config, channel_is_herdr, load_channel_id_from_config,
)

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
    steps = f"steps: {cell_driver.format_step_timings((result or {}).get('step_timings'))}"
    if findings:
        return False, f"{detail}; " + "; ".join(findings) + f"; {steps}"
    return True, f"{detail} {steps}"


def classify(
    *, report: dict[str, Any] | None, scenario_id: str, cell: str, channel_id: str,
    run_id: str, driver_rc: int, markers: list[str] | tuple[str, ...] = (),
    log_counts: dict[str, int] | None = None, launch_options: Iterable[str] = (),
) -> dict[str, Any]:
    """Bind the report to this run, cell and channel, then judge what the user saw; unbound fails."""
    outcome = {"classification": "FAIL", "observed_reason": "evidence incomplete"}
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
    violations = report.get("coverage_class_violations")
    if (not isinstance(violations, list) or violations or row.get("teardown_error")
            or row.get("dirty_active_residue")):
        return outcome
    passed, detail = judge(report=report, scenario_id=scenario_id, markers=markers,
                           log_counts=log_counts, driver_rc=driver_rc, cell=cell,
                           channel_id=channel_id, launch_options=launch_options)
    outcome.update(classification="PASS" if passed else "FAIL", observed_reason=detail)
    return outcome


def cell_path(config_path: Path, cell: str, channel_id: str) -> str:
    """The session host the configured cell channel runs on: ``herdr`` when Herdr owns it."""
    return "herdr" if channel_is_herdr(config_path, cell, channel_id) else cell_driver.cell_runtime(cell)


def gateway_observation(detail: Any, *, provider: str) -> str | None:
    """Why /api/health/detail does not yet show a recovered, connected gateway; None when it does."""
    if not isinstance(detail, dict):
        return "health detail is not an object"
    if detail.get("fully_recovered") is not True:
        return "startup recovery not finished"
    providers = detail.get("providers")
    if not isinstance(providers, list):
        return "providers missing"
    gateways = [p for p in providers if isinstance(p, dict) and p.get("runtime_role") == "gateway"]
    if not any(p.get("name") == provider for p in gateways):
        return f"no {provider} gateway provider"
    waiting = sorted(str(p.get("name")) for p in gateways if p.get("connected") is not True)
    return f"gateway not connected: {', '.join(waiting)}" if waiting else None


def wait_gateway_ready(
    *, base_url: str, cell: str, channel_id: str, deadline_s: float, interval_s: float = 2.0,
    clock: Callable[[], float] = time.monotonic, sleep: Callable[[float], None] = time.sleep,
) -> tuple[bool, str]:
    """Poll until the gateway is connected and a Discord read of the cell channel answers in time.

    The read is the one every scenario wait polls, so a server that cannot answer it yet is not ready.
    """
    started = clock()
    observation = "not observed"
    while True:
        remaining = deadline_s - (clock() - started)
        if remaining <= 0:
            return False, f"not ready after {clock() - started:.1f}s: {observation}"
        request_s = max(0.1, min(10.0, remaining))
        try:
            _, detail = cell_driver._read_api_json(base_url, "/api/health/detail", timeout=request_s)  # noqa: SLF001
            observation = gateway_observation(detail, provider=cell_driver.cell_provider(cell))
            remaining = deadline_s - (clock() - started)
            if observation is None and remaining > 0:
                read_started = clock()
                client = cell_driver.discord.DiscordClient(base_url=base_url, timeout_s=max(0.1, min(10.0, remaining)))
                client.fetch_messages(channel_id, limit=1)
                # A read that answers only after the deadline does not make the gateway ready in time.
                if clock() - started <= deadline_s:
                    return True, (f"ready after {clock() - started:.1f}s "
                                  f"(gateway connected; Discord read {clock() - read_started:.1f}s)")
                observation = "Discord read answered after the deadline"
        except Exception as error:  # noqa: BLE001 - an unanswered probe is "not ready yet"
            observation = f"{type(error).__name__}: {error}"[:300]
        print(f"gateway-ready waiting {clock() - started:.1f}s: {observation}", flush=True)
        remaining = deadline_s - (clock() - started)
        if remaining <= 0:
            continue
        sleep(min(interval_s, remaining))


def cell_session_busy_reasons(*, cell: str, channel_id: str, sessions_payload: Any) -> list[str]:
    """Use E-1's session status and channel/workspace matching for both idle checks."""
    sessions = sessions_payload.get("sessions") if isinstance(sessions_payload, dict) else sessions_payload
    if not isinstance(sessions, list):
        raise ValueError("sessions payload is not a list")
    workspace = cell_driver.cell_workspace_substring(cell)
    busy = []
    for session in sessions:
        if not isinstance(session, dict):
            continue
        status = str(session.get("status") or "").lower()
        if status not in {"turn_active", "turn_busy", "active"}:
            continue
        key = str(session.get("session_key") or "")
        channel = str(session.get("channel_id") or session.get("channelId") or "")
        if channel == str(channel_id) or workspace in key:
            busy.append(f"session {key or channel or '<unknown>'} status={status}")
    return busy


def _cell_guard(args: argparse.Namespace) -> int:
    """Preserve E-1's snapshot guard exit codes: 0 busy, 1 idle, 2 unreadable."""
    try:
        detail = json.loads(Path(args.health_detail).read_text(encoding="utf-8"))
        sessions = json.loads(Path(args.sessions).read_text(encoding="utf-8"))
        mailboxes = detail.get("mailboxes") if isinstance(detail, dict) else None
        if not isinstance(mailboxes, list):
            raise ValueError("health/detail mailboxes is not a list")
        provider = cell_driver.cell_provider(args.cell)
        busy = [f"mailbox {provider}:{args.channel_id} [{', '.join(reasons)}]"
                for box in mailboxes if isinstance(box, dict)
                and cell_driver._mailbox_channel_id(box) == str(args.channel_id)  # noqa: SLF001
                and cell_driver._mailbox_provider(box) == provider  # noqa: SLF001
                if (reasons := cell_driver._mailbox_busy_reasons(box))]  # noqa: SLF001
        busy.extend(cell_session_busy_reasons(cell=args.cell, channel_id=args.channel_id, sessions_payload=sessions))
    except Exception as error:  # noqa: BLE001 - unreadable snapshots never prove idle
        print(f"{type(error).__name__}: {error}", file=sys.stderr)
        return 2
    if busy:
        print("; ".join(busy))
        return 0
    return 1


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


class CellResetBusy(RuntimeError):
    """Another E2E driver owns the cell; reset must yield without HTTP traffic."""


def cell_reset(*, base_url: str, cell: str, channel_id: str, config_path: Path,
               runtime_root: Path | None = None) -> None:
    """Release one captured E2E episode through the server CAS, then acknowledge !clear."""
    target = f"cell={cell} channel={channel_id}"

    def log(action: str) -> None:
        print(f"cell-reset {target} {action}", flush=True)

    if cell not in {"claude-tui", "codex-tui"}:
        log("skipped: reset only supports legacy TUI smoke cells")
        return
    # The smoke resolver binds these fixed E2E agent IDs, never a caller-selected agent.
    config = yaml.safe_load(config_path.read_text(encoding="utf-8"))
    if _channel_id_from_config(config, cell, config_path) != str(channel_id):
        raise ValueError("channel outside configured E2E allowlist")
    if not channel_id.isdecimal() or int(channel_id) <= 0:
        raise ValueError("invalid channel in E2E allowlist")
    log("allowlist accepted: configured E2E cell channel")
    hosts = config.get("session_hosts", {})
    herdr = hosts.get("herdr", {}) if isinstance(hosts, dict) else None
    channels = herdr.get("channels", {}) if isinstance(herdr, dict) else None
    if not isinstance(channels, dict):
        raise ValueError("Herdr channels mapping unreadable")
    if str(channel_id) in {str(key) for key in channels}:
        log("skipped: configured channel is owned by Herdr")
        return
    with ExitStack() as held:
        try:
            held.enter_context(cell_driver.lease.acquire(f"cell-reset-{os.getpid()}", cell=cell))
        except RuntimeError as error:
            if str(error).startswith("existing lease at "):
                raise CellResetBusy(f"skipped: foreign active turn on cell ({error})") from error
            raise
        log("cell lease acquired: exclude concurrent E2E drivers through clear ACK")
        held.callback(log, "cell lease releasing: reset finished or refused")
        _reset_cell_turn(base_url=base_url, cell=cell, channel_id=channel_id, runtime_root=runtime_root, log=log)


def _reset_cell_turn(*, base_url: str, cell: str, channel_id: str, runtime_root: Path | None,
                     log: Callable[[str], None]) -> None:
    provider = cell_driver.cell_provider(cell)
    path = f"/api/turn-lease/{provider}/{channel_id}"
    _, expected = cell_driver._read_api_json(base_url, path)  # noqa: SLF001
    log("inspect: " + json.dumps(expected, sort_keys=True))
    if expected is not None:
        fields = {"provider", "channel_id", "generation", "runtime", "user_message_id", "turn_nonce", "started_at"}
        if (not isinstance(expected, dict) or set(expected) != fields
                or expected["provider"] != provider or expected["channel_id"] != int(channel_id)
                or any(type(expected[key]) is not int for key in ("channel_id", "generation", "user_message_id"))
                or any(not isinstance(expected[key], str) or not expected[key]
                       for key in ("runtime", "turn_nonce", "started_at"))):
            raise ValueError("inspect returned malformed or wrong-target identity")
    _, current = cell_driver._read_api_json(base_url, path)  # noqa: SLF001
    if json.dumps(current, sort_keys=True) != json.dumps(expected, sort_keys=True):
        raise ValueError("identity changed after inspect; no release or !clear attempted")
    log("identity matched: exact inspect snapshot")
    client = cell_driver.discord.DiscordClient(base_url=base_url)
    if expected is not None:
        request = urllib.request.Request(
            f"{base_url.rstrip('/')}/api/turn-lease/release",
            data=json.dumps({"expected": expected, "reason": "post-deploy smoke: reset dedicated E2E cell"}).encode(),
            headers=cell_driver.http_auth.request_headers(base_url, {"Content-Type": "application/json"}),
            method="POST",
        )
        log("release requested: CAS with original inspect JSON")
        result = client._read_json(request, operation="cell-reset release")  # noqa: SLF001
        if result.get("released") is not True:
            raise ValueError("release not confirmed; !clear refused")
        log("release confirmed: captured episode and inflight cleanup settled")
        _, current = cell_driver._read_api_json(base_url, path)  # noqa: SLF001
        if current is not None:
            raise ValueError("identity changed after release; !clear refused")
    else:
        log("release skipped: inspect proved no active lease")
        if not (preflight_busy_reasons(base_url=base_url, cell=cell, channel_id=channel_id, runtime_root=runtime_root)
                or cell_session_busy_reasons(cell=cell, channel_id=channel_id,
                                             sessions_payload=cell_driver._read_api_json(base_url, "/api/sessions")[1])):  # noqa: SLF001
            log("reset not needed: no lease and preflight idle; preserve warm session")
            return
    log("!clear requested: discard dedicated E2E session")
    sent = client.send(channel_id, "!clear")
    after = str(sent.get("message_id") or sent.get("id") or "")
    if not after.isdecimal() or int(after) <= 0:
        raise ValueError("!clear send identity unavailable")
    prefixes = ("세션을 초기화했어요", *cell_driver.herdr.CLEAR_REFUSAL_PREFIXES)
    reply, _ = client.wait_for_message(
        channel_id, after_id=after, timeout_s=30, poll_interval_s=1,
        predicate=lambda row: (not cell_driver.assertions.is_our_send(row)
                               and cell_driver.herdr.clear_reply(row, prefixes, after=int(after), channel=channel_id)),
    )
    if reply is None:
        raise ValueError("!clear ack unavailable; reset unconfirmed")
    if cell_driver.herdr.clear_refusal([reply], after=int(after), channel=channel_id):
        raise ValueError("!clear refused; reset unconfirmed")
    log("!clear ack confirmed: session reset completed")


def _reset_smoke_cells(args: argparse.Namespace) -> int:
    results: dict[str, Any] = {}
    for cell in dict.fromkeys(args.cell):
        channel = None
        try:
            if cell not in {"claude-tui", "codex-tui"}:
                print(f"cell-reset cell={cell} skipped: not a legacy TUI cell", flush=True)
            else:
                channel = load_channel_id_from_config(Path(args.config), cell)
                cell_reset(base_url=args.base_url, cell=cell, channel_id=channel, config_path=Path(args.config),
                           runtime_root=Path(args.queue_runtime_root) if args.queue_runtime_root else None)
            results[cell] = {"status": "ready", "channel_id": channel, "reason": "reset complete or unnecessary"}
        except CellResetBusy as error:
            results[cell] = {"status": "skipped", "channel_id": channel, "reason": str(error)}
        except SmokeConfigError as error:
            results[cell] = {"status": "failed" if cell == args.cell[0] else "skipped",
                             "channel_id": channel, "reason": f"E2E allowlist unavailable: {error}"}
        except Exception as error:  # noqa: BLE001 - isolate reset failures to their target cell
            results[cell] = {"status": "failed", "channel_id": channel, "reason": f"{type(error).__name__}: {error}"}
        results[cell]["relay"] = cell == args.cell[0]
        print(f"cell-reset cell={cell} {results[cell]['status']}: {results[cell]['reason']}", flush=True)
    relay = results[args.cell[0]]
    try:
        # E-1 must read observations made after !clear, rather than its earlier API probes.
        for output, path in ((args.health_detail_output, "/api/health/detail"), (args.sessions_output, "/api/sessions")):
            if output and relay["status"] == "ready":
                _, payload = cell_driver._read_api_json(args.base_url, path)  # noqa: SLF001
                Path(output).write_text(json.dumps(payload), encoding="utf-8")
                print(f"cell-reset snapshot refreshed: {path}", flush=True)
    except Exception as error:  # noqa: BLE001 - unreadable relay observations block only the relay cell
        relay.update(status="failed", reason=f"snapshot refresh failed: {error}")
        print(f"cell-reset cell={args.cell[0]} failed: {relay['reason']}", flush=True)
    if args.state_output:
        try:
            Path(args.state_output).write_text(json.dumps(results), encoding="utf-8")
        except OSError as error:
            print(f"cell-reset failed: state output unavailable: {error}", flush=True)
            return 2
    return {"ready": 0, "skipped": 3, "failed": 2}[relay["status"]]


def reset_status(path: Path, cell: str, channel_id: str) -> tuple[int, str]:
    """A failed/foreign reset blocks only its cell, including after a partial cleanup."""
    try:
        record = json.loads(path.read_text(encoding="utf-8"))[cell]
        status, reason = record["status"], record["reason"]
        if status not in {"ready", "skipped", "failed"} or not isinstance(reason, str):
            raise ValueError("invalid reset outcome")
        if record["channel_id"] is not None and record["channel_id"] != channel_id:
            raise ValueError("reset channel changed")
        if status == "failed" and record.get("relay") is True:
            return 1, f"skipped: blocked by E-1 FAIL ({reason})"
        return {"ready": 0, "skipped": 1, "failed": 4}[status], f"cell-reset cell={cell} {status}: {reason}"
    except (OSError, ValueError, KeyError, TypeError) as error:
        return 4, f"cell-reset state unavailable for {cell}: {error}"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    guard = sub.add_parser("cell-guard")
    guard.add_argument("--health-detail", required=True)
    guard.add_argument("--sessions", required=True)
    guard.add_argument("--cell", required=True, choices=cell_driver.SUPPORTED_CELLS)
    guard.add_argument("--channel-id", required=True)
    reset = sub.add_parser("cell-reset")
    reset.add_argument("--base-url", required=True)
    reset.add_argument("--config", required=True)
    reset.add_argument("--cell", required=True, action="append", choices=cell_driver.SUPPORTED_CELLS)
    reset.add_argument("--health-detail-output")
    reset.add_argument("--sessions-output")
    reset.add_argument("--queue-runtime-root")
    reset.add_argument("--state-output")
    status = sub.add_parser("reset-status")
    status.add_argument("--reset-state", required=True)
    status.add_argument("--cell", required=True, choices=cell_driver.SUPPORTED_CELLS)
    status.add_argument("--channel-id", required=True)
    path = sub.add_parser("cell-path")
    path.add_argument("--config", required=True)
    path.add_argument("--cell", required=True, choices=cell_driver.SUPPORTED_CELLS)
    path.add_argument("--channel-id", required=True)
    ready = sub.add_parser("gateway-ready")
    ready.add_argument("--base-url", required=True)
    ready.add_argument("--cell", required=True, choices=cell_driver.SUPPORTED_CELLS)
    ready.add_argument("--channel-id", required=True)
    ready.add_argument("--deadline-s", type=float, required=True)
    pre = sub.add_parser("preflight")
    pre.add_argument("--base-url", required=True)
    pre.add_argument("--cell", required=True, choices=cell_driver.SUPPORTED_CELLS)
    pre.add_argument("--channel-id", required=True)
    pre.add_argument("--queue-runtime-root")
    pre.add_argument("--herdr-endpoint")
    pre.add_argument("--herdr-status-bin", default="agentdesk")
    pre.add_argument("--reset-state")
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

    if args.command == "cell-guard":
        return _cell_guard(args)

    if args.command == "cell-reset":
        return _reset_smoke_cells(args)

    if args.command == "cell-path":
        try:
            print(cell_path(Path(args.config), args.cell, args.channel_id))
        except (OSError, ValueError, yaml.YAMLError) as error:
            print(f"{type(error).__name__}: {error}", file=sys.stderr)
            return 2
        return 0

    if args.command == "gateway-ready":
        ready_now, detail = wait_gateway_ready(base_url=args.base_url, cell=args.cell,
                                               channel_id=args.channel_id, deadline_s=args.deadline_s)
        print(f"gateway-ready {'ok' if ready_now else 'FAIL'}: {detail}", flush=True)
        return 0 if ready_now else 1

    if args.command == "reset-status" or (args.command == "preflight" and args.reset_state):
        rc, detail = reset_status(Path(args.reset_state), args.cell, args.channel_id)
        if rc or args.command == "reset-status":
            print(detail)
            return rc

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
    path_part = ""
    if args.config:
        outcome = classify(report=report, scenario_id=args.scenario, cell=args.cell,
                           channel_id=args.channel_id, run_id=args.run_id or "",
                           driver_rc=args.driver_rc, markers=markers,
                           log_counts=log_counts, launch_options=launch_options)
        passed, detail = outcome["classification"] == "PASS", outcome["observed_reason"]
        try:
            path_part = f" path={cell_path(Path(args.config), args.cell, args.channel_id)}"
        except (OSError, ValueError, yaml.YAMLError) as error:
            # A verdict the current configuration cannot be bound to is not a pass.
            path_part, passed = " path=unreadable", False
            detail += f"; configured session host unreadable: {type(error).__name__}: {error}"
    print(f"relay {args.scenario} cell={args.cell} result={'pass' if passed else 'FAIL'}{path_part} {detail}")
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
