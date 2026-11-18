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


def _abandon_orphans(state, run_id):
    """A RUNNING record seen by the lock holder has no live owner left: mark it, never resume it."""
    for path in sorted((state / "runs").glob("*.json")):
        row = read_json(path, None)
        if isinstance(row, dict) and row.get("result") == "RUNNING":
            row.update(result="ABANDONED", abandoned_by=run_id)
            write_json(path, row)
            if row.get("request", {}).get("kind") == "deploy":
                inbox.record(state, {row["request"]["request_id"]: "ABANDONED"})


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


def _execute(lease, config, record, row):
    started = time.monotonic()
    argv = config["driver"] + ["--scenario", row["scenario"], "--cell", row["cell"], "--run-id", record["run_id"],
                               "--profile", record["profile"]]
    if record["request"].get("deploy_sha"):
        argv += ["--deploy-sha", record["request"]["deploy_sha"]]
    proc = lease.spawn(argv, record["run_id"], stdin=subprocess.DEVNULL, capture_output=True, text=True)
    try:
        report = json.loads((proc.stdout or "").strip().splitlines()[-1])
    except (IndexError, ValueError):
        report = {}
    status = report.get("status") if isinstance(report, dict) else None
    turns = report.get("native_turns") if isinstance(report, dict) else None
    row.update(status=status if status in RESULTS and proc.returncode == 0 else "UNEVALUABLE",
               driver_rc=proc.returncode, elapsed_s=round(time.monotonic() - started, 3),
               native_turns=turns if type(turns) is int else None)


def _run(lease, state, config, request, run_id, now):
    manifest = plan.load(Path(config["manifest"]) if config.get("manifest") else plan.MANIFEST)
    cursor = plan.cursor(state)
    if request["kind"] == "deploy":
        inbox.record(state, {**{rid: "SUPERSEDED" for rid in request["superseded"]},
                             **{stem: "REJECTED" for stem in request["rejected"]}})
        bundle, profile, rows = "P", "default", plan.deploy_occurrences(manifest)
    else:
        steps = plan.rotation(manifest)
        bundle, cell, profile = steps[cursor["index"] % len(steps)]
        rows = plan.step_occurrences(manifest, bundle, cell, profile)
    for row in rows:
        blocked = plan.gate(manifest, row, config["capabilities"])
        if blocked:
            row.update(status=blocked[0], reason=blocked[1])
    runnable = [row for row in rows if "status" not in row]
    amounts = {row["cell"].split("-")[0]: (config.get("reservation") or {}).get(bundle, {}).get(row["cell"].split("-")[0])
               for row in runnable}
    record = {"schema_version": 1, "run_id": run_id, "request": request, "bundle": bundle, "profile": profile,
              "cursor_index": cursor["index"], "started_at": now, "budget_day": budget.day(now),
              "lease_key": lease.key, "occurrences": rows}
    refused = budget.reserve(state, config.get("budget") or {}, amounts, run_id, now) if runnable else None
    if request["kind"] == "scheduled":
        plan.save_cursor(state, {**cursor, "last_scheduled_at": now})
    if refused:
        for row in runnable:
            row.update(status="DEFERRED_BUDGET", reason=refused)
        if request["kind"] == "deploy":
            inbox.record(state, {request["request_id"]: "DEFERRED_BUDGET"})
        return _record(state, record, result="DEFERRED_BUDGET")
    if request["kind"] == "deploy":
        inbox.record(state, {request["request_id"]: "RUNNING"})
    _record(state, record, result="RUNNING")
    spent = {}
    for row in runnable:
        provider = row["cell"].split("-")[0]
        if spent.get(provider, 0) >= amounts[provider]["time_s"]:
            row.update(status="DEFERRED_BUDGET", reason=f"{provider} run reservation used up")
            continue
        _execute(lease, config, record, row)
        spent[provider] = spent.get(provider, 0) + row["elapsed_s"]
    measured = {}
    for provider in amounts:
        mine = [row for row in runnable if row["cell"].startswith(provider + "-") and "elapsed_s" in row]
        turns = [row["native_turns"] for row in mine]
        measured[provider] = {"time_s": round(spent.get(provider, 0), 3),
                              "turns": sum(turns) if None not in turns else None}
    budget.settle(state, record["budget_day"], run_id, measured)
    result = _verdict(rows)
    if request["kind"] == "deploy":
        inbox.record(state, {request["request_id"]: result})
    else:
        plan.save_cursor(state, {"index": cursor["index"] + 1, "last_scheduled_at": now})
    return _record(state, record, result=result, completed_at=time.time())


def tick(config_path, now=None):
    config = load_config(config_path)
    if config.get("enabled") is not True:
        return {"status": "DISABLED"}
    config = settings(config)
    state, now = config["state_dir"], time.time() if now is None else now
    if _due(state, config, now) is None:
        return {"status": "NOT_DUE"}
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
