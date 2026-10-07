"""Herdr cells: Discord stimuli, health/status observations, no tmux or runtime files."""

from __future__ import annotations

import copy
import json
import os
import subprocess
import time
import urllib.error

from . import assertions, known_gap

TMUX_STEPS = {
    "kill_pane", "capture_session_identity", "assert_session_preserved",
    "poison_claude_tui_relay_offset", "send_keys", "send_keys_no_enter",
    "send_keys_sequence", "local_control_then_prompt",
}


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


def published_row(block, status, *, channel, provider):
    counts = block.get("reconnect")
    keys = ("channels", "published", "withheld", "unknown", "pending")
    if not isinstance(counts, dict) or any(type(counts.get(k)) is not int or counts[k] < 0 for k in keys):
        raise assertions.AssertionError("Herdr reconnect counts missing or invalid")
    if counts["withheld"] or counts["unknown"] or counts["pending"] or not counts["published"]:
        raise assertions.AssertionError("Herdr reconnect has unpublished rows")
    if counts["channels"] != counts["published"]:
        raise assertions.AssertionError("Herdr reconnect did not publish every row")
    rows = status.get("executions") if isinstance(status, dict) else None
    if not isinstance(rows, list):
        raise assertions.AssertionError("Herdr status executions missing")
    matches = [r for r in rows if isinstance(r, dict) and str(r.get("channel")) == str(channel)
               and r.get("provider") == provider and r.get("state") != "retired"]
    if len(matches) != 1:
        raise assertions.AssertionError("Herdr target row missing or ambiguous")
    row = matches[0]
    if (row.get("state") != "bound" or row.get("pane") != "provider_running"
            or row.get("launch_evidence") != "recorded" or not row.get("nonce")
            or "input_hold" not in row or row["input_hold"] is not None):
        raise assertions.AssertionError("Herdr target row is not a published running execution")
    return row


def observe(driver, args, *, clean=False, reconnected=False):
    _, payload = driver._read_api_json(args.base_url, "/api/health", timeout=5)
    block = ready(payload, endpoint=getattr(args, "herdr_endpoint", None), channel=args.channel_id)
    if clean:
        clean_turn(block)
    evidence = {"surface": "GET /api/health", "herdr": block}
    if reconnected:
        proc = subprocess.run([getattr(args, "herdr_status_bin", "agentdesk"), "herdr", "status"],
                              capture_output=True, text=True, check=True, timeout=30)
        evidence["row"] = published_row(block, json.loads(proc.stdout), channel=args.channel_id,
                                          provider=driver.cell_provider(args.cell))
    return evidence


def expected_cancel_refusal(error):
    """Only the observed host guard conflict is a product gap; not generic HTTP failure."""
    cause = error.__cause__
    return (isinstance(cause, urllib.error.HTTPError) and cause.code == 409
            and "session host is not legacy tmux" in str(error))


def omissions(scenario):
    items = []
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
    result["status"] = next((s for s in ("fail", "known_gap", "unexpected_pass", "not_applicable", "dry_run")
                             if s in {http["status"], stop["status"]}), "pass")
    result["reason"] = "; ".join(f"{c['id']}: {c['status']} {c.get('reason') or ''}" for c in (http, stop))
    driver._refresh_agent_mode_record(result, scenario=scenario, declared_agent_mode=result["agent_mode"], dry_run=args.dry_run)
    driver._refresh_coverage_class_record(result, scenario=scenario, declared_coverage_class=result["coverage_class"], dry_run=args.dry_run)
    return result


def _run(driver, scenario, args, client, run_id, result):
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
    if args.dry_run:
        print(f"[dry-run] {scenario['id']} ({args.cell}): GET /api/health preflight; setup")
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
    gap_attempted = False
    gap_failure = False
    prompt_count = 0
    first_response = False
    phase = "preflight"
    setup_id = None
    result["herdr_observations"] = []
    # Reset/cancel and queue truncation are deliberately never used as Herdr setup.
    result["reset_policy"] = "preserve Herdr session; reset-before-each not executed"
    try:
        result["herdr_observations"].append(observe(driver, args, clean=True))
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
            nonlocal first_response, gap_failure
            found, rows = client.wait_for_message(
                args.channel_id, predicate=lambda row: marker in ((row.get("content", "") if raw and (row.get("author") or {}).get("bot") else assertions.relay_body(row)) or ""),
                after_id=after_id, timeout_s=timeout)
            ingest(rows)
            if not found:
                gap_failure = gap_attempted and scenario["id"] != "E-18"
                raise assertions.AssertionError(f"Herdr Discord marker timeout: {marker}")
            first_response = True
            return found

        for step in scenario.get("steps", []):
            if "kill_pane" in step and scenario["id"] == "E-12" and not first_response:
                raise assertions.AssertionError("force termination requires observed running prompt")
            if TMUX_STEPS.intersection(step):
                if "kill_pane" in step and scenario["id"] == "E-12":
                    phase, gap_attempted = "force_cancel", True
                    try:
                        driver.cancel_turn(base_url=args.base_url, channel_id=args.channel_id, force=True)
                    except assertions.AssertionError as error:
                        gap_failure = expected_cancel_refusal(error)
                        raise
                continue
            phase = next(iter(step))
            if "deliver_prompt" in step:
                window.mark_prompt_sent()
                driver.deliver_step(client, step, cell=args.cell, run_id=run_id, record=result)
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
                if profile and profile["issue"] == "#5340 P10-2" and prompt_count and first_response:
                    gap_attempted = True
                window.mark_prompt_sent()
                prompt_count += 1
                result["real_provider_contacted"] = True
                result["agent_mode_actual"] = "real_live"
                try:
                    client.send(args.channel_id, prompt)
                    driver.post_send_sleep(step)
                except Exception:
                    # Transport/auth errors are never an expected product gap.
                    gap_failure = False
                    raise
                if scenario.get("e36_normal_intake"):
                    marker = step.get("hold_marker") if step.get("request_key") == "QA" else step["body_marker"]
                    wait(marker.replace("{run_id}", run_id), 240)
                    if step.get("request_key") == "QA":
                        driver.run_assertion({"marker_absent": {"marker": step["body_marker"].replace("{run_id}", run_id), "surface": "relay"}}, window=window, record=result)
                    if step.get("request_key") == "QB":
                        qa = next(s for s in scenario["steps"] if s.get("request_key") == "QA")
                        wait(qa["body_marker"].replace("{run_id}", run_id), 240)
                    result["herdr_observations"].append(observe(driver, args, clean=True))
            elif "wait_for_discord_text" in step:
                wait(str(step["wait_for_discord_text"]).replace("{run_id}", run_id), float(step.get("timeout_s", 240)),
                     raw=scenario["id"] == "E-12" and step["wait_for_discord_text"] == "session ended")
            elif "wait_for_raw_discord_text" in step:
                marker = step["wait_for_raw_discord_text"]
                deadline = time.monotonic() + float(step.get("timeout_s", 30))
                while True:
                    rows = client.fetch_messages(args.channel_id, after_id=after_id, limit=100)
                    ingest(rows)
                    if any(marker in r.get("content", "") for r in rows):
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
                gap_attempted = True
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
                phase, gap_attempted = "cancel_turn", True
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
                        evidence = observe(driver, args, clean=True, reconnected=True)
                        if evidence["row"]["nonce"] != before[0]["nonce"]:
                            raise assertions.AssertionError("Herdr restart replaced execution nonce")
                        break
                    except (assertions.AssertionError, OSError, ValueError, subprocess.SubprocessError):
                        if time.monotonic() >= deadline:
                            raise
                        time.sleep(2)
                result["herdr_observations"].append(evidence)
            elif "assert_health" in step:
                result["herdr_observations"].append(observe(driver, args, clean=True))
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
                gap_failure = gap_attempted and "marker_absent" in spec and scenario["id"] == "E-18-stop"
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
        result["herdr_observations"].append(observe(driver, args, clean=True))
        result.update(status="not_applicable" if result["not_applicable"] else "pass",
                      reason="partial coverage: see not_applicable checks" if result["not_applicable"] else None)
        if profile and gap_attempted:
            known_gap.apply_herdr_result(result, profile, passed=True)
    except Exception as error:
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
