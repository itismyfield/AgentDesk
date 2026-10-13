"""Herdr cells: Discord stimuli, health/status observations, no tmux or runtime files."""

from __future__ import annotations

import copy
import datetime as dt
import json
import os
import re
import subprocess
import time
import urllib.error
import urllib.parse

from . import assertions, http_auth, known_gap

TMUX_STEPS = {
    "kill_pane", "capture_session_identity", "assert_session_preserved",
    "poison_claude_tui_relay_offset", "send_keys", "send_keys_no_enter",
    "send_keys_sequence", "local_control_then_prompt",
}
# Server reasons name a channel only as its raw Discord id; provider and counter aggregates never do.
AGGREGATE_REASON_PREFIXES = ("provider:", "global_active_counter_", "no_providers_registered")
CHANNEL_ID = re.compile(r"\d{17,}")
# Text !clear refusals as worded in commands/control/native.rs and control.rs. A wording change
# only loses the fail-fast: the acknowledgement wait then times out as before.
CLEAR_REFUSAL_PREFIXES = ("세션을 초기화하지 못했어요:", "세션 전환 중이라 초기화하지 못했어요")


def validate_arguments(parser, args):
    allowed = args.herdr_allowed_degraded_reason or []
    if allowed and not args.cell.endswith("-herdr"):
        parser.error("--herdr-allowed-degraded-reason applies only to Herdr cells")
    if any(pattern.strip() in ("", "*") for pattern in allowed):
        parser.error("--herdr-allowed-degraded-reason must name a reason or prefix; empty or bare '*' waives everything")


def detail_ready(driver, http_status, payload, *, allowed, channel):
    """Return the degraded reasons waived by the operator allowlist; raise unless detail health is ready."""
    if not allowed:
        if (http_status != 200 or payload.get("status") != "healthy"
                or payload.get("fully_recovered") is not True
                or ("ok" in payload and payload["ok"] is not True)
                or ("degraded" in payload and payload["degraded"] is not False)
                or payload.get("degraded_reasons") != []):
            raise assertions.AssertionError("Herdr detail health readiness failed or incomplete")
        return []
    state, reasons = payload.get("status"), payload.get("degraded_reasons")
    if not isinstance(reasons, list) or any(not isinstance(r, str) for r in reasons):
        raise assertions.AssertionError("Herdr detail degraded_reasons must be a list of strings")
    # Detail omits ok/degraded on the registry path; standalone sets ok only when healthy.
    healthy = state == "healthy"
    if (http_status != 200 or state not in ("healthy", "degraded")
            or payload.get("fully_recovered") is not True
            or ("ok" in payload and payload["ok"] is not healthy)
            or ("degraded" in payload and payload["degraded"] is not (state == "degraded"))
            or healthy == bool(reasons)):
        raise assertions.AssertionError("Herdr detail health readiness failed or incomplete")
    hard = [r for r in reasons if "herdr" in r.lower() or str(channel) in r]
    unmatched = [r for r in reasons if r not in hard and not driver._health_reason_allowed(r, tuple(allowed))]
    if hard or unmatched:
        raise assertions.AssertionError(
            f"Herdr detail degraded_reasons not waived: never_waived={hard} unmatched={unmatched}")
    return reasons


def unattributed(reasons):
    """Waived reasons whose text cannot show they are not the target's; the operator checks those."""
    return [r for r in reasons if r.startswith(AGGREGATE_REASON_PREFIXES) or not CHANNEL_ID.search(r)]


def waiver_summary(args, observations):
    allowed = list(getattr(args, "herdr_allowed_degraded_reason", None) or [])
    readiness = [o.get("readiness") or {} for o in observations if isinstance(o, dict)]
    ignored = sorted({r for item in readiness for r in item.get("ignored_degraded_reasons", [])})
    # Derived from the union, so observations recorded before the marker existed are still marked.
    return {"degraded_reason_waiver": "applied" if ignored else ("unused" if allowed else "none"),
            "allowed_degraded_reasons": allowed, "ignored_degraded_reasons": ignored,
            "ignored_degraded_reasons_unattributed": unattributed(ignored)}


def report_waiver(args, result):
    result["herdr_readiness_waiver"] = summary = waiver_summary(args, result.get("herdr_observations", []))
    if summary["allowed_degraded_reasons"]:
        print(f"[e2e]   herdr readiness waiver: {json.dumps(summary, ensure_ascii=False)}")


def ready(payload, *, endpoint, channel):
    block = payload.get("herdr") if isinstance(payload, dict) else None
    if not isinstance(block, dict):
        raise assertions.AssertionError("health missing herdr block")
    if block.get("admission") != "open" or block.get("restart_required") is not False:
        raise assertions.AssertionError("Herdr admission closed or restart required/unknown")
    endpoints = block.get("endpoints")
    local = endpoints.get(endpoint) if isinstance(endpoints, dict) else None
    if not endpoint or not isinstance(local, dict) or local.get("local") is not True:
        raise assertions.AssertionError("Herdr channel endpoint must be explicitly selected and local")
    # Health does not expose the channel -> endpoint join. Fail closed unless every
    # configured endpoint is local; an arbitrary local key must not bless a remote channel.
    if any(not isinstance(e, dict) or e.get("local") is not True for e in endpoints.values()):
        raise assertions.AssertionError("mixed/remote Herdr endpoints cannot prove channel locality")
    channels = block.get("configured_channels")
    if not isinstance(channels, list) or str(channel) not in channels:
        raise assertions.AssertionError("Herdr channel absent from boot configuration")
    return block


def clean_turn(block):
    if type(block.get("input_holds")) is not int or block["input_holds"] != 0:
        raise assertions.AssertionError("Herdr input_holds must be measured zero")


def reconnect_counts(block):
    counts = block.get("reconnect")
    keys = ("channels", "published", "withheld", "unknown", "pending")
    if not isinstance(counts, dict) or any(type(counts.get(k)) is not int or counts[k] < 0 for k in keys):
        raise assertions.AssertionError("Herdr reconnect counts missing or invalid")
    return counts


def published_row(block, status, *, channel, provider):
    counts = reconnect_counts(block)
    if counts["withheld"] or counts["unknown"] or counts["pending"] or not counts["published"]:
        raise assertions.AssertionError("Herdr reconnect has unpublished rows")
    if counts["channels"] != counts["published"]:
        raise assertions.AssertionError("Herdr reconnect did not publish every row")
    return current_row(status, channel=channel, provider=provider)


def current_row(status, *, channel, provider, cold=False):
    rows = status.get("executions") if isinstance(status, dict) else None
    holds = status.get("other_input_holds") if isinstance(status, dict) else None
    if not isinstance(rows, list) or not isinstance(holds, list):
        raise assertions.AssertionError("Herdr status executions/other_input_holds missing")
    if holds:
        raise assertions.AssertionError("Herdr current CLI status has other input holds")
    for row in rows:
        if (not isinstance(row, dict) or row.get("record") == "unreadable"
                or any(key not in row for key in ("channel", "provider", "state", "input_hold"))
                or not isinstance(row.get("channel"), (str, int)) or isinstance(row.get("channel"), bool)
                or not str(row["channel"]) or not row.get("provider") or not row.get("state")):
            raise assertions.AssertionError("Herdr status row unreadable or incomplete")
        if row["input_hold"] is not None:
            raise assertions.AssertionError("Herdr current CLI status has an input hold")
    matches = [r for r in rows if str(r["channel"]) == str(channel) and r["state"] != "retired"]
    if not matches and cold:
        return None
    if len(matches) != 1:
        raise assertions.AssertionError("Herdr target row missing or ambiguous")
    row = matches[0]
    if (row["provider"] != provider or row["state"] != "bound" or row.get("pane") != "provider_running"
            or row.get("launch_evidence") != "recorded"
            or not isinstance(row.get("nonce"), str) or not row["nonce"]):
        raise assertions.AssertionError("Herdr target row is not a verified running execution")
    return row


def observe(driver, args, *, reconnected=False, cold=False, waivable=True):
    # Scenario-declared health steps keep their strict meaning; the waiver is withheld there.
    operator = list(getattr(args, "herdr_allowed_degraded_reason", None) or [])
    allowed = operator if waivable else []
    # Waiver metadata precedes the request so refused or failed requests still record it.
    evidence = {"surface": "GET /api/health/detail", "status_surface": "agentdesk herdr status",
                "observed_at": dt.datetime.now(dt.timezone.utc).isoformat(),
                "readiness": {"degraded_reason_waiver": ("unused" if waivable else "withheld") if operator else "none",
                              "allowed_degraded_reasons": allowed, "ignored_degraded_reasons": [],
                              "ignored_degraded_reasons_unattributed": []}}
    try:
        if not http_auth.loopback_origin(args.base_url):
            raise assertions.AssertionError("Herdr observations require target-node loopback API and local CLI")
        status, payload = driver._read_api_json(args.base_url, "/api/health/detail", timeout=5)
        evidence["http_status"] = status
        if not isinstance(payload, dict):
            raise assertions.AssertionError("Herdr detail health must be an object")
        evidence["readiness"].update({k: payload[k] for k in
                                      ("status", "ok", "fully_recovered", "degraded", "degraded_reasons") if k in payload})
        evidence["server_values"] = {k: payload[k] for k in
                                     ("uptime_secs", "started_at", "process_started_at_unix", "incarnation", "boot_id") if k in payload}
        ignored = detail_ready(driver, status, payload, allowed=allowed, channel=args.channel_id)
        if ignored:
            evidence["readiness"].update(degraded_reason_waiver="applied", ignored_degraded_reasons=list(ignored),
                                         ignored_degraded_reasons_unattributed=unattributed(ignored))
        block = ready(payload, endpoint=getattr(args, "herdr_endpoint", None), channel=args.channel_id)
        evidence["herdr"] = {k: block[k] for k in
                             ("admission", "restart_required", "configured_channels", "endpoints", "input_holds", "reconnect") if k in block}
        clean_turn(block)
        reconnect_counts(block)
        proc = subprocess.run([getattr(args, "herdr_status_bin", "agentdesk"), "herdr", "status"],
                              capture_output=True, text=True, check=True, timeout=30)
        current = json.loads(proc.stdout)
        evidence["row"] = current_row(current, channel=args.channel_id,
                                      provider=driver.cell_provider(args.cell), cold=cold and not reconnected)
        evidence["current_input_holds"] = 0
        if reconnected:
            published_row(block, current, channel=args.channel_id, provider=driver.cell_provider(args.cell))
        evidence["validated"] = True
        return evidence
    except Exception as error:
        error.herdr_observation = evidence
        raise


def clear_reply(row, prefixes, *, after, channel):
    """A bot message in this channel, after the !clear message `after`, starting with one of `prefixes`."""
    content = row.get("content") or ""
    return bool((row.get("author") or {}).get("bot") and str(row.get("id", "")).isdigit() and int(row["id"]) > after
                and str(row.get("channel_id", channel)) == str(channel) and content.startswith(prefixes))


def clear_refusal(rows, *, after, channel):
    """The bot reply refusing the pending text !clear; refusal and completion share one boundary."""
    return next((r["content"] for r in rows if clear_reply(r, CLEAR_REFUSAL_PREFIXES, after=after, channel=channel)), None)


def expected_cancel_refusal(error):
    """Only the observed host guard conflict is a product gap; not generic HTTP failure."""
    cause = error.__cause__
    return (isinstance(cause, urllib.error.HTTPError) and cause.code == 409
            and "session host is not legacy tmux" in str(error))


def validate_restart_binding(scenario, args):
    port = urllib.parse.urlparse(getattr(args, "base_url", "")).port
    expected = {8797: "dev", 8791: "release"}.get(port)
    for step in scenario.get("steps", []):
        if "restart_dcserver" in step:
            target = getattr(args, "restart_target_override", None) or (step["restart_dcserver"] or {}).get("target", "release")
            if target not in ("dev", "release") or (expected and target != expected):
                raise ValueError(f"restart target {target!r} does not match base URL {args.base_url}")


def omissions(scenario):
    items = []
    if any("restart_dcserver" in step for step in scenario.get("steps", [])):
        items.append({"check": "server_incarnation_changed", "status": "not_applicable",
                      "reason": "public health exposes no per-restart incarnation; nonce continuity does not prove dcserver restarted"})
    for step in scenario.get("steps", []):
        for key in TMUX_STEPS.intersection(step):
            items.append({"check": key, "status": "not_applicable", "reason": "tmux-only step; Herdr has no tmux session"})
        if "wait_for_provider_hold_state" in step:
            items.append({"check": "durable_provider_hold", "status": "not_applicable",
                          "reason": "durable inflight file is not a Herdr health/status observation"})
    if scenario.get("durable_delivery_probe"):
        items.append({"check": "durable_receipt_frontier", "status": "not_applicable",
                      "reason": "legacy disk receipt/frontier probe has no Herdr health/status equivalent"})
    if any("provider_hold_marker_seen" in a for a in scenario.get("assertions", [])):
        items.append({"check": "provider_hold_marker_seen", "status": "not_applicable",
                      "reason": "durable provider hold witness is not exposed by health/status"})
    if scenario.get("e36_normal_intake"):
        items.append({"check": "native_intake_queue_join", "status": "not_applicable",
                      "reason": "Claude JSONL, intake log and tmux watcher join is not a Herdr observation"})
        items.append({"check": "queued_followup_commit_order", "status": "not_applicable",
                      "reason": "Discord publication cannot prove queued intake commit/native execution order"})
    if any("assert_health" in s for s in scenario.get("steps", [])):
        items.append({"check": "legacy_global_mailbox_idle", "status": "not_applicable",
                      "reason": "legacy global/detail mailbox counters are not Herdr row health"})
    return items


def run(driver, scenario, args, client, run_id, result):
    if not scenario.get("herdr_text_stop"):
        return _run(driver, scenario, args, client, run_id, result)
    # Distinct setup windows and issue scopes: HTTP HostOwned is NOT the P10-3 command path.
    http = _run(driver, scenario, args, client, run_id, copy.deepcopy(result))
    if http["status"] in {"fail", "skipped"}:
        return http
    if not args.dry_run and http["status"] == "known_gap":
        time.sleep(70)  # Let the refused HTTP hold finish before the next independent prompt.
    text = {"id": "E-18-stop", "agent_mode": "real_live", "coverage_class": "live", "destructive": True,
            "steps": [
                {"send_provider_hold_prompt": {"ok_marker": "[E2E:E18S:OK]", "late_marker": "[E2E:E18S:LATE]", "hold_seconds": 60}},
                {"wait_for_provider_hold_state": {"ok_marker": "[E2E:E18S:OK]", "late_marker": "[E2E:E18S:LATE]"}},
                {"send_text_stop": True}, {"wait_idle_s": 70},
                {"send_prompt": "Reply with exactly [E2E:E18S:NEXT]"}, {"wait_for_discord_text": "[E2E:E18S:NEXT]"}],
            "assertions": [{"marker_absent": {"marker": "[E2E:E18S:LATE]", "surface": "relay"}},
                           {"no_duplicate_marker": "[E2E:E18S:OK]"}, {"no_duplicate_marker": "[E2E:E18S:NEXT]"},
                           {"text_present": "[E2E:E18S:NEXT]"}, {"no_duplicate_content": True}]}
    text_result = copy.deepcopy(result)
    text_result["id"] = text["id"]
    stop = _run(driver, text, args, client, run_id, text_result)
    result["subcases"] = [http, stop]
    for key in ("not_applicable", "known_gaps", "herdr_observations", "assertions"):
        result[key] = [item for case in (http, stop) for item in case.get(key, [])]
    result["real_provider_contacted"] = http["real_provider_contacted"] or stop["real_provider_contacted"]
    result["herdr_readiness_waiver"] = waiver_summary(args, result["herdr_observations"])
    result["status"] = next((s for s in ("fail", "unexpected_pass", "known_gap", "not_applicable", "dry_run")
                             if s in {http["status"], stop["status"]}), "pass")
    result["reason"] = "; ".join(f"{c['id']}: {c['status']} {c.get('reason') or ''}" for c in (http, stop))
    driver._refresh_agent_mode_record(result, scenario=scenario, declared_agent_mode=result["agent_mode"], dry_run=args.dry_run)
    driver._refresh_coverage_class_record(result, scenario=scenario, declared_coverage_class=result["coverage_class"], dry_run=args.dry_run)
    return result


def _run(driver, scenario, args, client, run_id, result):
    """Every exit, early refusals and skips included, records the waiver summary."""
    try:
        return _run_checked(driver, scenario, args, client, run_id, result)
    finally:
        report_waiver(args, result)


def _run_checked(driver, scenario, args, client, run_id, result):
    result["not_applicable"] = omissions(scenario)
    result["coverage_scope"] = "Discord relay + Herdr health/status; not legacy disk/native evidence"
    profile = known_gap.herdr_profile(scenario["id"], args.cell)
    if profile:
        result["expected_gap"] = profile
    for gate in (driver.required_agent_mode_violation(declared=result["agent_mode"], required=getattr(args, "required_agent_mode", None), scenario_id=scenario["id"]),
                 driver.required_coverage_class_violation(declared=result["coverage_class"], required=getattr(args, "required_coverage_class", None), scenario_id=scenario["id"])):
        if gate:
            result.update(status="fail", reason=gate)
            return result
    try:
        validate_restart_binding(scenario, args)
    except ValueError as error:
        result.update(status="fail", reason=str(error))
        return result
    if args.dry_run:
        print(f"[dry-run] {scenario['id']} ({args.cell}): GET /api/health/detail + local CLI status preflight; setup")
        for step in scenario.get("steps", []):
            state = "not_applicable" if TMUX_STEPS.intersection(step) else "planned"
            print(f"[dry-run] {state}: {json.dumps(step, ensure_ascii=False)}")
        if scenario["id"] == "E-12":
            print("[dry-run] planned: force cancel via turn API (expected-fail #5340 P11)")
        if profile:
            print(f"[dry-run] expected-fail (execute, do not skip): {profile['issue']}")
        for item in result["not_applicable"]:
            print(f"[dry-run] not_applicable: {item['check']}: {item['reason']}")
        print("[dry-run] planned: assertions; input_holds=0; teardown (no actions executed)")
        if scenario["id"] == "E-18":
            print("[dry-run] Herdr override: HTTP cancel force=false (HostOwned follow-up, not P10-3); P11 force=true is E-12")
        if any("restart_dcserver" in step for step in scenario.get("steps", [])):
            print("[dry-run] restart: isolated-server attestation; read-only status before/after; same nonce; all rows published")
        result.update(status="dry_run", reason="plan only; no observed passes")
        return result
    if args.hard_reset_session_each:
        result.update(status="fail", reason="Herdr forbids --hard-reset-session-each (tmux)")
        return result
    if driver.is_destructive(scenario) and not (
            args.allow_destructive and os.environ.get("AGENTDESK_E2E_ALLOW_DESTRUCTIVE") == "1"):
        result.update(status="skipped", reason="destructive: requires --allow-destructive AND AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1")
        return result
    enabled = frozenset(filter(None, os.environ.get("AGENTDESK_E2E_FEATURES", "").split(",")))
    missing = set(scenario.get("requires_features", [])) - enabled
    if missing:
        result.update(status="skipped", reason=f"missing E2E features: {sorted(missing)}")
        return result
    if scenario.get("e36_normal_intake") and (args.reset_before_each or args.phase_deadline_s != 3540):
        result.update(status="fail", reason="Herdr E36 requires --no-reset-before-each --phase-deadline-s 3540")
        return result
    window = assertions.Window("")
    gap_token = None
    gap_trials = []
    gap_failure = False
    prompt_count = 0
    first_response = False
    phase = "preflight"
    setup_id = None
    clear_after = None
    result["herdr_observations"] = []
    # Reset/cancel and queue truncation are deliberately never used as Herdr setup.
    result["reset_policy"] = "preserve Herdr session; reset-before-each not executed"
    try:
        result["herdr_observations"].append(observe(driver, args, cold=True))
        phase = "setup"
        sent = client.send_control(args.channel_id, f"### E2E SETUP {scenario['id']} cell={args.cell} run={run_id}")
        setup_id = str(sent.get("message_id") or sent.get("id") or "")
        if not setup_id.isdigit():
            raise ValueError("setup response missing numeric message id")
        window = assertions.Window(setup_id)
        after_id = setup_id

        def ingest(rows):
            for row in sorted(rows, key=lambda r: int(r["id"])):
                window.add(row)
            window.reconcile_snapshot(rows, after_id=after_id)

        def wait(marker, timeout, *, raw=False):
            nonlocal first_response, gap_failure, gap_token
            found, rows = client.wait_for_message(
                args.channel_id, predicate=lambda row: marker in ((row.get("content", "") if raw and (row.get("author") or {}).get("bot") else assertions.relay_body(row)) or ""),
                after_id=after_id, timeout_s=timeout)
            ingest(rows)
            if not found:
                gap_failure = bool(gap_token and gap_token["marker"] == marker and scenario["id"] != "E-18")
                raise assertions.AssertionError(f"Herdr Discord marker timeout: {marker}")
            first_response = True
            if gap_token and gap_token["marker"] == marker:
                gap_token = None
            return found

        def open_gap(index, marker=None):
            nonlocal gap_token
            remaining = scenario.get("steps", [])[index + 1:]
            if marker is None:
                marker = next((str(s["wait_for_discord_text"]).replace("{run_id}", run_id)
                               for s in remaining if "wait_for_discord_text" in s), None)
            gap_token = {"request_index": index, "marker": marker}
            gap_trials.append(dict(gap_token))

        for step_index, step in enumerate(scenario.get("steps", [])):
            if "kill_pane" in step and scenario["id"] == "E-12" and not first_response:
                raise assertions.AssertionError("force termination requires observed running prompt")
            if TMUX_STEPS.intersection(step):
                if "kill_pane" in step and scenario["id"] == "E-12":
                    phase = "force_cancel"
                    open_gap(step_index)
                    try:
                        driver.cancel_turn(base_url=args.base_url, channel_id=args.channel_id, force=True)
                    except assertions.AssertionError as error:
                        gap_failure = expected_cancel_refusal(error)
                        raise
                continue
            phase = next(iter(step))
            if "deliver_prompt" in step:
                gap_token = None
                window.mark_prompt_sent()
                driver.deliver_step(client, step, cell=args.cell, run_id=run_id, channel_id=args.channel_id, record=result)
                result["real_provider_contacted"] = True
                prompt_count += 1
                driver.post_send_sleep(step)
            elif "send_prompt" in step or "send_discord_prompt" in step or "send_provider_hold_prompt" in step:
                if scenario["id"] == "E-12":
                    prompt = driver.build_provider_hold_prompt({"ok_marker": "[E2E:E12:OK]", "late_marker": "[E2E:E12:LATE]", "hold_seconds": 60}, scenario_id="E-12")
                elif "send_provider_hold_prompt" in step:
                    prompt = driver.build_provider_hold_prompt(step["send_provider_hold_prompt"], scenario_id=scenario["id"])
                else:
                    prompt = str(step.get("send_prompt", step.get("send_discord_prompt"))).replace("{run_id}", run_id)
                gap_token = None
                is_clear = prompt.strip().split(maxsplit=1)[0:1] == ["!clear"]
                if is_clear:
                    first_response, prompt_count = False, -1
                elif profile and profile["issue"] == "#5340 P10-2" and prompt_count and first_response:
                    marker = step.get("hold_marker") if step.get("request_key") == "QA" else step.get("body_marker")
                    open_gap(step_index, marker.replace("{run_id}", run_id) if marker else None)
                window.mark_prompt_sent()
                prompt_count += 1
                result["real_provider_contacted"] = True
                result["agent_mode_actual"] = "real_live"
                try:
                    sent = client.send(args.channel_id, prompt)
                    driver.post_send_sleep(step)
                except Exception:
                    # Transport/auth errors are never an expected product gap.
                    gap_failure = False
                    raise
                if is_clear:
                    # Replies to this clear follow its own message id; 0 means no id, so none can be attributed.
                    sent_id = str(sent.get("message_id") or sent.get("id") or "") if isinstance(sent, dict) else ""
                    clear_after = int(sent_id) if sent_id.isdigit() else 0
                if scenario.get("e36_normal_intake"):
                    marker = step.get("hold_marker") if step.get("request_key") == "QA" else step["body_marker"]
                    wait(marker.replace("{run_id}", run_id), 240)
                    if step.get("request_key") == "QA":
                        driver.run_assertion({"marker_absent": {"marker": step["body_marker"].replace("{run_id}", run_id), "surface": "relay"}}, window=window, record=result)
                    if step.get("request_key") == "QB":
                        qa = next(s for s in scenario["steps"] if s.get("request_key") == "QA")
                        wait(qa["body_marker"].replace("{run_id}", run_id), 240)
                    result["herdr_observations"].append(observe(driver, args))
            elif "wait_for_discord_text" in step:
                wait(str(step["wait_for_discord_text"]).replace("{run_id}", run_id), float(step.get("timeout_s", 240)),
                     raw=scenario["id"] == "E-12" and step["wait_for_discord_text"] == "session ended")
            elif "wait_for_raw_discord_text" in step:
                marker = step["wait_for_raw_discord_text"]
                deadline = time.monotonic() + float(step.get("timeout_s", 30))
                while True:
                    rows = client.fetch_messages(args.channel_id, after_id=after_id, limit=100)
                    ingest(rows)
                    refusal = clear_after is not None and clear_refusal(rows, after=clear_after, channel=args.channel_id)
                    if refusal and not clear_after:
                        raise assertions.AssertionError(
                            f"text !clear send returned no message id, so a refusal reply cannot be attributed: {refusal}")
                    if refusal:
                        raise assertions.AssertionError(f"text !clear refused by the server: {refusal}")
                    if any(clear_reply(r, marker, after=clear_after, channel=args.channel_id) if clear_after is not None
                           else marker in r.get("content", "") for r in rows):
                        clear_after = None
                        break
                    if time.monotonic() >= deadline:
                        raise assertions.AssertionError(f"Herdr control acknowledgement missing: {marker}")
                    time.sleep(1)
            elif "wait_for_provider_hold_state" in step:
                params = step["wait_for_provider_hold_state"]
                wait(params["ok_marker"], float(params.get("timeout_s", 180)))
                if params["late_marker"] in "\n".join(r.get("content", "") for r in window.raw_messages):
                    raise assertions.AssertionError("Herdr hold already ended before operation")
                result["herdr_observations"].append(observe(driver, args))
            elif "send_text_stop" in step:
                if not first_response:
                    raise assertions.AssertionError("text stop requires running hold witness")
                client.send(args.channel_id, "!stop")  # The same normal Discord transport as prompts.
                driver.post_send_sleep(step)
                open_gap(step_index)
                ack = "중지하고 있어요..."
                refused = "이 세션의 호스트를 확인하지 못해 중지하지 않았어요. 턴은 계속 진행돼요."
                found, rows = client.wait_for_message(args.channel_id,
                    predicate=lambda r: bool((r.get("author") or {}).get("bot")) and any(t in r.get("content", "") for t in (ack, refused)),
                    after_id=after_id, timeout_s=30)
                ingest(rows)
                if found and refused in found.get("content", ""):
                    gap_failure = True
                    raise assertions.AssertionError("text !stop explicitly refused by Herdr host guard")
                if not found:
                    raise assertions.AssertionError("text !stop acknowledgement missing (not a proven product gap)")
                result["text_stop_ack"] = ack
            elif "cancel_turn" in step:
                if not first_response:
                    raise assertions.AssertionError("cancellation requires observed prompt")
                phase = "cancel_turn"
                open_gap(step_index)
                try:
                    driver.cancel_turn(base_url=args.base_url, channel_id=args.channel_id,
                                       force=False if scenario["id"] == "E-18" else bool((step["cancel_turn"] or {}).get("force", True)))
                except assertions.AssertionError as error:
                    gap_failure = expected_cancel_refusal(error)
                    raise
            elif "wait_idle_s" in step:
                time.sleep(float(step["wait_idle_s"]))
            elif "restart_dcserver" in step:
                if not getattr(args, "herdr_isolated_server", False):
                    raise ValueError("Herdr restart requires --herdr-isolated-server; never use a shared production server")
                if args.restart_script:
                    raise ValueError("Herdr restart forbids custom scripts that may touch tmux")
                _, health = driver._read_api_json(args.base_url, "/api/health", timeout=5)
                if health.get("status") != "healthy":
                    raise assertions.AssertionError("Herdr restart requires healthy server")
                proc = subprocess.run([getattr(args, "herdr_status_bin", "agentdesk"), "herdr", "status"],
                                      capture_output=True, text=True, check=True, timeout=30)
                executions = json.loads(proc.stdout).get("executions")
                if not isinstance(executions, list) or not executions or any(not isinstance(r, dict) or str(r.get("channel")) != str(args.channel_id) for r in executions):
                    raise assertions.AssertionError("Herdr restart requires an isolated test server with only the target row")
                before = [r for r in executions if r.get("provider") == driver.cell_provider(args.cell) and r.get("state") == "bound"]
                if len(before) != 1 or not before[0].get("nonce") or before[0].get("pane") != "provider_running":
                    raise assertions.AssertionError("restart requires one running target row")
                target = args.restart_target_override or (step["restart_dcserver"] or {}).get("target", "release")
                if target not in ("dev", "release"):
                    raise ValueError("invalid dcserver restart target")
                subprocess.run(["launchctl", "kickstart", "-k", f"gui/{os.getuid()}/com.agentdesk.{target}"], check=True, timeout=90)
                deadline = time.monotonic() + 90
                while True:
                    try:
                        evidence = observe(driver, args, reconnected=True, waivable=False)
                        if evidence["row"]["nonce"] != before[0]["nonce"]:
                            raise assertions.AssertionError("Herdr restart replaced execution nonce")
                        break
                    except (assertions.AssertionError, OSError, ValueError, subprocess.SubprocessError):
                        if time.monotonic() >= deadline:
                            raise
                        time.sleep(2)
                result["herdr_observations"].append(evidence)
            elif "assert_health" in step:
                result["herdr_observations"].append(observe(driver, args, waivable=False))
            else:
                raise ValueError(f"unsupported Herdr step: {step}")
        phase = "assertions"
        for attempt in range(int(getattr(args, "final_refetches", 2))):
            ingest(client.fetch_messages(args.channel_id, after_id=after_id, limit=100))
            if attempt + 1 < int(getattr(args, "final_refetches", 2)):
                time.sleep(float(getattr(args, "final_refetch_interval_s", 1)))
        for spec in scenario.get("assertions", []):
            if "provider_hold_marker_seen" in spec:
                result["assertions"].append({"spec": spec, "status": "not_applicable",
                                             "reason": "durable provider hold not exposed by health/status"})
                continue
            if "no_duplicate_marker_with_known_gap" in spec:
                spec = {"no_duplicate_marker": spec["no_duplicate_marker_with_known_gap"]["marker"]}
            try:
                if spec.get("requires_feature") and spec["requires_feature"] not in enabled:
                    result["assertions"].append({"spec": spec, "status": "skipped", "reason": "feature not enabled"})
                    continue
                driver.run_assertion(spec, window=window, record=result, enabled_features=enabled, run_id=run_id)
            except assertions.AssertionError:
                gap_failure = bool(result.get("text_stop_ack")) and "marker_absent" in spec and scenario["id"] == "E-18-stop"
                raise
            result["assertions"].append({"spec": spec, "passed": True})
        if result.get("text_stop_ack"):
            count = sum(result["text_stop_ack"] in r.get("content", "") for r in window.raw_messages if (r.get("author") or {}).get("bot"))
            if count != 1:
                raise assertions.AssertionError(f"text !stop acknowledgement count={count}, expected one")
            result["assertions"].append({"check": "one_stop_ack_no_late_marker_next_turn", "passed": True})
        if scenario.get("e36_normal_intake"):
            for step in scenario["steps"]:
                marker = step["body_marker"].replace("{run_id}", run_id)
                driver.run_assertion({"no_duplicate_marker": marker}, window=window, record=result)
                driver.run_assertion({"text_present": marker}, window=window, record=result)
            result["e36_acceptance"] = {"discord_markers": "pass", "native_intake_queue_join": "not_applicable"}
        phase = "clean_turn"
        result["herdr_observations"].append(observe(driver, args))
        result.update(status="not_applicable" if result["not_applicable"] else "pass",
                      reason="partial coverage: see not_applicable checks" if result["not_applicable"] else None)
        if profile and gap_trials:
            known_gap.apply_herdr_result(result, profile, passed=True)
    except Exception as error:
        if hasattr(error, "herdr_observation"):
            result["herdr_observations"].append(error.herdr_observation)
        result.update(status="fail", reason=f"{type(error).__name__}: {error}")
        result["failure_attribution"] = {"source": phase, "raw_reason": str(error)}
        if profile and gap_failure:
            known_gap.apply_herdr_result(result, profile, passed=False)
    finally:
        result["completed_at"] = driver.dt.datetime.now().isoformat(timespec="seconds")
        if setup_id:
            try:
                driver.send_teardown_marker(client=client, channel_id=args.channel_id, scenario_id=scenario["id"], cell=args.cell, run_id=run_id)
            except Exception as error:
                result.update(status="fail", reason=f"Herdr teardown failed: {error}")
        driver._update_record_window_snapshot(result, window)
        driver._record_marker_counts(result, window, [m.replace("{run_id}", run_id) for m in scenario.get("report_marker_counts", [])])
        driver._refresh_agent_mode_record(result, scenario=scenario, declared_agent_mode=result["agent_mode"], dry_run=False)
        driver._refresh_coverage_class_record(result, scenario=scenario, declared_coverage_class=result["coverage_class"], dry_run=False)
    return result
