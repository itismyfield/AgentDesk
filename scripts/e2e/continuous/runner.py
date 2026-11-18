"""One runner tick: off check first, then deploy inbox or due rotation, channel lock, budget, run.

Off (the default) returns DISABLED before any lock, directory or state IO. Under the lock a run
reserves budget, executes runnable occurrences through the configured driver with the inherited
lease FD, settles budget and advances the cursor once. A crashed run keeps its reservation, does
not advance the cursor and is marked ABANDONED by the next lock holder.
"""

import json
from pathlib import Path
import os
import subprocess
import time
import uuid

from . import budget, channel_lock, inbox, plan
from .store import read_json, write_json

RESULTS = {"PASS", "FAIL", "UNEVALUABLE"}


def load_config(path):
    config = read_json(path, {"enabled": False})
    if not isinstance(config, dict):
        raise ValueError("runner config must be a JSON object")
    return config


def settings(config):
    """Validate the fields a run needs; periodic and full-run choices stay null until measured."""
    state = config.get("state_dir")
    if not isinstance(state, str) or not os.path.isabs(state):
        raise ValueError("state_dir must be an absolute path")
    channel_lock.resource_key(config.get("resource"))
    interval = config.get("rotation_interval_s")
    if interval is not None and (type(interval) is not int or interval <= 0):
        raise ValueError("rotation_interval_s must be null or a positive integer")
    if config.get("daily_full_at") is not None:
        raise ValueError("daily_full_at stays null: the full census run is not part of this runner")
    driver = config.get("driver")
    if not isinstance(driver, list) or not driver or not all(isinstance(arg, str) for arg in driver):
        raise ValueError("driver must be an argv list")
    capabilities = config.get("capabilities", [])
    if not isinstance(capabilities, list) or not all(isinstance(item, str) for item in capabilities):
        raise ValueError("capabilities must list capability names")
    return {**config, "state_dir": Path(state), "capabilities": capabilities}


def _due(state, config, now):
    newest, older, invalid = inbox.pending(state)
    if newest is not None:
        return {"kind": "deploy", "request_id": newest["request_id"], "deploy_sha": newest["deploy_sha"],
                "superseded": older, "rejected": invalid}
    interval, last = config.get("rotation_interval_s"), plan.cursor(state)["last_scheduled_at"]
    if interval is not None and (last is None or now >= last + interval):
        return {"kind": "scheduled"}
    return None


def _run_path(state, run_id):
    return state / "runs" / f"{run_id}.json"


def _record(state, record, **changes):
    record.update(changes)
    write_json(_run_path(state, record["run_id"]), record)
    return record


def _apply_completed(state, row):
    budget.settle(state, row["budget_day"], row["run_id"], row.get("measured", {}))
    if row["request"]["kind"] == "deploy":
        inbox.record(state, {row["request"]["request_id"]: row["result"]})
    else:
        cursor = plan.cursor(state)
        if cursor["index"] <= row["cursor_index"]:
            plan.save_cursor(state, {"index": row["cursor_index"] + 1, "last_scheduled_at": row["started_at"]})
    _record(state, row, applied=True)


def _abandon_orphans(state, run_id):
    """Replay durable completions; an unfinished run is abandoned without re-executing it."""
    rows = [read_json(path, {}) for path in (state / "runs").glob("*.json")]
    for row in sorted(rows, key=lambda row: (row.get("started_at", 0), row.get("run_id", ""))):
        if row.get("completed_at") is not None and not row.get("applied"):
            _apply_completed(state, row)
        elif row.get("result") == "RUNNING":
            result = "ABANDONED" if budget.reserved(state, row["budget_day"], row["run_id"]) else "DEFERRED_BUDGET"
            if result == "ABANDONED":
                charged = read_json(state / "budget" / f"{row['budget_day']}.json", {})[row["run_id"]]
                budget.settle(state, row["budget_day"], row["run_id"],
                              {provider: {field: None for field in budget.FIELDS} for provider in charged})
            _record(state, row, result=result, abandoned_by=run_id)
            if row.get("request", {}).get("kind") == "deploy":
                if inbox.ledger(state).get(row["request"]["request_id"]) in (None, "RUNNING"):
                    inbox.record(state, {row["request"]["request_id"]: result})


def _selection(state, config, request):
    manifest = plan.load(Path(config["manifest"]) if config.get("manifest") else plan.MANIFEST)
    cursor = plan.cursor(state)
    if request["kind"] == "deploy":
        bundle, profile, rows = "P", "default", plan.deploy_occurrences(manifest)
    else:
        steps = plan.rotation(manifest)
        bundle, cell, profile = steps[cursor["index"] % len(steps)]
        rows = plan.step_occurrences(manifest, bundle, cell, profile)
    for row in rows:
        blocked = plan.gate(manifest, row, config["capabilities"])
        if blocked:
            row.update(status=blocked[0], reason=blocked[1])
    amounts = {row["cell"].split("-")[0]: (config.get("reservation") or {}).get(bundle, {}).get(row["cell"].split("-")[0])
               for row in rows if "status" not in row}
    return cursor, bundle, profile, rows, amounts


def _preflight(state, config, request, recovering=False):
    amounts = _selection(state, config, request)[-1]
    if refused := budget.configured(config.get("budget") or {}, amounts):
        return refused
    paused = read_json(state / "budget" / "paused.json", {})
    for provider in amounts:
        if (provider in paused and not recovering) or not {f"usage:{provider}", f"turn_budget:{provider}"} <= set(config["capabilities"]):
            return f"{provider} usage or native-turn budget capability unavailable"
    return None


def _verdict(rows):
    statuses = {row["status"] for row in rows}
    if "FAIL" in statuses:
        return "FAIL"
    if "UNEVALUABLE" in statuses:
        return "UNEVALUABLE"
    if statuses == {"PASS"}:
        return "PASS"
    if "PASS" in statuses:
        return "PARTIAL"
    return "BLOCKED_CAPABILITY" if "BLOCKED_CAPABILITY" in statuses else "PARTIAL"


def _execute(lease, config, record, row, remaining):
    started = time.monotonic()
    remaining = {**remaining, "deadline": started + remaining["time_s"]}
    argv = config["driver"] + ["--scenario", row["scenario"], "--cell", row["cell"], "--run-id", record["run_id"],
                               "--profile", record["profile"], "--budget", json.dumps(remaining)]
    if record["request"].get("deploy_sha"):
        argv += ["--deploy-sha", record["request"]["deploy_sha"]]
    proc = lease.spawn(argv, record["run_id"], stdin=subprocess.DEVNULL, capture_output=True, text=True)
    try:
        report = json.loads((proc.stdout or "").strip().splitlines()[-1])
    except (IndexError, ValueError):
        report = {}
    status = report.get("status") if isinstance(report, dict) else None
    turns = report.get("native_turns") if isinstance(report, dict) else None
    row.update(status=status if status in RESULTS and (proc.returncode == 0 or status == "FAIL") else "UNEVALUABLE",
               driver_rc=proc.returncode, elapsed_s=round(time.monotonic() - started, 3),
               native_turns=turns if type(turns) is int and turns >= 0 else None,
               tokens=report.get("tokens") if isinstance(report, dict) and type(report.get("tokens")) is int and report["tokens"] >= 0 else None)
    if isinstance(report, dict) and row["native_turns"] is not None and (report.get("budget_checked") != row["native_turns"]
            or row["native_turns"] > remaining["turns"] or (row["tokens"] is not None and row["tokens"] > remaining["tokens"])):
        row.update(status="FAIL" if row["status"] == "FAIL" else "UNEVALUABLE", native_turns=None,
                   reason="native-turn budget witness missing")


def _run(lease, state, config, request, run_id, now):
    cursor, bundle, profile, rows, amounts = _selection(state, config, request)
    runnable = [row for row in rows if "status" not in row]
    record = {"schema_version": 1, "run_id": run_id, "request": request, "bundle": bundle, "profile": profile,
              "cursor_index": cursor["index"], "started_at": now, "budget_day": budget.day(now),
              "lease_key": lease.key, "occurrences": rows}
    _record(state, record, result="RUNNING")  # Before reserving, so a crash leaves a record to abandon.
    refused = budget.reserve(state, config.get("budget") or {}, amounts, run_id, now) if runnable else None
    if refused:
        for row in runnable:
            row.update(status="DEFERRED_BUDGET", reason=refused)
        if request["kind"] == "deploy":
            inbox.record(state, {request["request_id"]: "DEFERRED_BUDGET"})
        return _record(state, record, result="DEFERRED_BUDGET")
    if request["kind"] == "deploy":
        inbox.record(state, {**{rid: "SUPERSEDED" for rid in request["superseded"]},
                             **{stem: "REJECTED" for stem in request["rejected"]}, request["request_id"]: "RUNNING"})
    spent, turned, tokens, unknown = {}, {}, {}, set()
    for row in runnable:
        provider = row["cell"].split("-")[0]
        if (provider in unknown or spent.get(provider, 0) >= amounts[provider]["time_s"]
                or turned.get(provider, 0) >= amounts[provider]["turns"] or tokens.get(provider, 0) >= amounts[provider]["tokens"]):
            row.update(status="DEFERRED_BUDGET", reason=f"{provider} run reservation used up")
            continue
        remaining = {"turns": amounts[provider]["turns"] - turned.get(provider, 0),
                     "tokens": amounts[provider]["tokens"] - tokens.get(provider, 0),
                     "time_s": amounts[provider]["time_s"] - spent.get(provider, 0)}
        _execute(lease, config, record, row, remaining)
        spent[provider] = spent.get(provider, 0) + row["elapsed_s"]
        if row["native_turns"] is None or row["tokens"] is None:
            unknown.add(provider)
        else:
            turned[provider] = turned.get(provider, 0) + row["native_turns"]
            tokens[provider] = tokens.get(provider, 0) + row["tokens"]
    measured = {}
    for provider in amounts:
        mine = [row for row in runnable if row["cell"].startswith(provider + "-") and "elapsed_s" in row]
        turns = [row["native_turns"] for row in mine]
        measured[provider] = {"time_s": round(spent.get(provider, 0), 3),
                              "tokens": None if provider in unknown else tokens.get(provider, 0),
                              "turns": sum(turns) if None not in turns else None}
    result = _verdict(rows)
    _record(state, record, result=result, completed_at=time.time(), measured=measured)
    _apply_completed(state, record)
    return record


def tick(config_path, now=None):
    config = load_config(config_path)
    if config.get("enabled") is not True:
        return {"status": "DISABLED"}
    config = settings(config)
    state, now = config["state_dir"], time.time() if now is None else now
    request = _due(state, config, now)
    recovery = any(row.get("result") == "RUNNING" or (row.get("completed_at") is not None and not row.get("applied"))
                   for row in (read_json(p, {}) for p in (state / "runs").glob("*.json")))
    if request is None and not recovery:
        return {"status": "NOT_DUE"}
    if refused := _preflight(state, config, request or {"kind": "scheduled"}, recovering=recovery):
        return {"status": "DEFERRED_BUDGET", "reason": refused}
    run_id = str(uuid.uuid4())
    try:
        lease = channel_lock.acquire(state, config["resource"])
    except channel_lock.LeaseBusy:
        return {"status": "DEFERRED_LEASE", "run_id": run_id}
    with lease:
        lease.publish_owner({"kind": "runner", "run_id": run_id, "pid": os.getpid(), "since": now})
        _abandon_orphans(state, run_id)
        request = _due(state, config, now)
        if request is None:
            return {"status": "NOT_DUE"}
        if refused := _preflight(state, config, request):
            return {"status": "DEFERRED_BUDGET", "reason": refused, "run_id": run_id}
        record = _run(lease, state, config, request, run_id, now)
        return {"status": record["result"], "run_id": run_id}


def hold(config_path, argv):
    """Manual instance E2E under the same channel lock; no enabled flag or budget is involved."""
    config = load_config(config_path)
    state = config.get("state_dir")
    if not isinstance(state, str) or not os.path.isabs(state):
        raise ValueError("state_dir must be an absolute path")
    run_id = str(uuid.uuid4())
    with channel_lock.acquire(Path(state), config.get("resource")) as lease:
        lease.publish_owner({"kind": "manual", "run_id": run_id, "pid": os.getpid(), "since": time.time()})
        return lease.spawn(argv, run_id).returncode
