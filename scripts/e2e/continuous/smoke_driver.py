"""Runner driver for the release host: one deploy-smoke turn scenario on a machine-local test cell.

It runs the same harness and judge as the post-deploy smoke, then reports the continuous driver
schema. A lost observation (transport timeout, rotated server log) is UNEVALUABLE, never FAIL.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

from . import budget, channel_lock, plan, release_host, runner, summary, usage
from .store import write_json

REPO = Path(__file__).resolve().parents[3]
SCENARIOS = REPO / "tests/e2e/tui_relay/scenarios"
ASSERTION = "turns_delivered_once"
TRANSPORT = re.compile(r"^(TimeoutError|socket\.timeout)\b|URL error|\bHTTP 5\d\d\b|retry budget exhausted")
FINGERPRINT_BYTES = 65536


def _smoke():
    sys.path.insert(0, str(REPO / "scripts/e2e"))
    import post_deploy_turn_smoke
    from tui_relay import source_compare
    return post_deploy_turn_smoke, source_compare


def observation(outcome, row, only_log_missing):
    """Map the smoke verdict; a seen duplicate or refusal stays FAIL even when an observation was also lost."""
    if outcome["classification"] == "PASS":
        return "PASS", None
    counts = row.get("marker_counts") if isinstance(row.get("marker_counts"), dict) else {}
    steps = row.get("step_timings") if isinstance(row.get("step_timings"), list) else []
    last = steps[-1].get("step") if steps and isinstance(steps[-1], dict) else ""
    seen = any(type(hits) is int and hits > 1 for hits in counts.values()) or row.get("input_refusals")
    if not seen and only_log_missing:
        return "UNEVALUABLE", "UNOBSERVED_SERVER_LOG"
    # Only a lookup that could not be sent is unobserved; a prompt that could not be sent is not.
    if not seen and str(last).startswith("wait_for_") and TRANSPORT.search(str(row.get("reason") or "")):
        return "UNEVALUABLE", "UNOBSERVED_TRANSPORT"
    return "FAIL", outcome["observed_reason"]


def _log_mark(path):
    with open(path, "rb") as stream:
        size = stream.seek(0, 2)
        stream.seek(0)
        return size, hashlib.sha256(stream.read(min(size, FINGERPRINT_BYTES))).hexdigest()


def _log_excerpt(path, mark, target):
    """Bytes the server appended during the run; a rotated log has no excerpt."""
    try:
        with open(path, "rb") as stream:
            if hashlib.sha256(stream.read(min(mark[0], FINGERPRINT_BYTES))).hexdigest() != mark[1]:
                return False
            stream.seek(mark[0])
            target.write_bytes(stream.read())
        return True
    except OSError:
        return False


def _transcripts(compare, host, provider, channel_id):
    binding = compare.resolve_binding(host["base_url"], channel_id=channel_id,
                                      transcript_root=(host.get("transcript_roots") or {}).get(provider))
    return binding["transcript_path"]


def run(args, config, spawn=subprocess.run):
    host = config["release_host"]
    cell = host["cells"].get(args.cell)
    report = {"schema_version": 2, "status": "UNEVALUABLE", "native_turns": 0, "tokens": 0, "budget_checked": 0,
              "usage": dict.fromkeys(summary.USAGE, 0), "real_provider_contacted": False, "evidence_paths": []}

    def refuse(reason):
        return {**report, "reason": reason}

    if not isinstance(cell, dict):
        return refuse("CELL_NOT_ENABLED")
    try:
        channel_lock.inherited()
    except (KeyError, ValueError, OSError, channel_lock.LeaseError):
        return refuse("CHANNEL_LEASE_MISSING")
    smoke, compare = _smoke()
    provider, channel = args.cell.split("-")[0], str(cell["channel_id"])
    run_id = f"{args.run_id}-{args.scenario}-{cell['smoke_cell']}"
    markers = smoke.expected_markers(SCENARIOS, args.scenario, run_id)
    remaining = json.loads(args.budget)
    guard = budget.TurnGuard(remaining)
    # The harness sends every prompt itself, so the whole worst-case plan must fit before the first one.
    for _ in markers:
        if not guard.start(cell["turn_tokens"]):
            return refuse("BUDGET_TOO_SMALL_FOR_SCENARIO")
        guard.finish(cell["turn_tokens"])
    if not markers or remaining["time_s"] < cell["deadline_s"]:
        return refuse("BUDGET_TOO_SMALL_FOR_SCENARIO")
    try:
        before = release_host.detail(host["base_url"])
        report.update(release_host.identity(host, before))
    except (OSError, ValueError, KeyError, TypeError):
        return refuse("SERVER_UNOBSERVABLE")
    relay = [sys.executable, str(REPO / "scripts/e2e/post_deploy_turn_smoke.py"), "preflight",
             "--base-url", host["base_url"], "--cell", cell["smoke_cell"], "--channel-id", channel,
             "--queue-runtime-root", host["runtime_root"]]
    idle = spawn(relay, cwd=REPO, capture_output=True, text=True)
    if idle.returncode != 0:
        return refuse("CELL_NOT_IDLE: " + (idle.stdout or "").strip()[:200])
    out = Path(config["state_dir"]) / "outputs" / "e2e" / args.run_id / f"{args.scenario}.{args.cell}"
    marks = []
    try:
        marks.append(usage.mark(_transcripts(compare, host, provider, channel)))
    except (OSError, ValueError, KeyError):
        pass  # A cell without a session yet starts one; its transcript is found after the run.
    needs_log = smoke.cell_driver.cell_runtime(cell["smoke_cell"]) != "herdr"
    try:
        log_mark = _log_mark(host["server_log"]) if needs_log else None
        write_json(out / "plan.json", {"run_id": run_id, "markers": markers, "channel_id": channel})
    except (OSError, channel_lock.LeaseError):
        return refuse("EVIDENCE_UNWRITABLE")
    with open(out / "relay.log", "w", encoding="utf-8") as log:
        driver = spawn([sys.executable, str(REPO / "scripts/e2e/run_tui_relay.py"), "--base-url", host["base_url"],
                        "--cell", cell["smoke_cell"], "--channel-id", channel, "--scenarios", str(SCENARIOS),
                        "--filter", args.scenario, "--run-id", run_id, "--no-reset-before-each",
                        "--phase-deadline-s", str(cell["deadline_s"]), "--output", str(out),
                        "--queue-runtime-root", host["runtime_root"], "--required-agent-mode", "real_live",
                        "--required-coverage-class", "live"], cwd=REPO, stdout=log, stderr=subprocess.STDOUT)
    log_read = needs_log and _log_excerpt(host["server_log"], log_mark, out / "server.log")
    counts, options = None, []
    if log_read:
        with open(out / "server.log", encoding="utf-8", errors="replace") as lines:
            counts = smoke.scan_log(lines, session=smoke.cell_driver.cell_session_name(cell["smoke_cell"]),
                                    channel_id=channel, launch_options=options)
    result = smoke._load_report(out / f"report.{cell['smoke_cell']}.json")  # noqa: SLF001
    def judged(log_counts):
        return smoke.classify(report=result, scenario_id=args.scenario, cell=cell["smoke_cell"], channel_id=channel,
                              run_id=run_id, driver_rc=driver.returncode, markers=markers, log_counts=log_counts,
                              launch_options=options)

    outcome = judged(counts)
    rows = result.get("scenarios") if isinstance(result, dict) else None
    row = rows[0] if isinstance(rows, list) and len(rows) == 1 and isinstance(rows[0], dict) else {}
    # The log alone is missing when the same evidence passes with a clean excerpt of this cell.
    only_log = needs_log and not log_read and judged({"cell_lines": 1})["classification"] == "PASS"
    status, reason = observation(outcome, row, only_log)
    report.update(status=status, real_provider_contacted=row.get("real_provider_contacted") is True,
                  evidence_paths=[str(out / name) for name in (f"report.{cell['smoke_cell']}.json", "relay.log")],
                  assertions=[{"key": ASSERTION, "status": status, "complete": status != "UNEVALUABLE"}],
                  verdict=f"relay {args.scenario} cell={cell['smoke_cell']} result={status} {outcome['observed_reason']}")
    if reason:
        report["reason"] = reason
    try:
        after = release_host.detail(host["base_url"])
        if release_host.identity(host, after) != release_host.identity(host, before):
            report.update(status="FAIL" if status == "FAIL" else "UNEVALUABLE", reason="SERVER_CHANGED_DURING_RUN")
        latest = _transcripts(compare, host, provider, channel)
        if latest not in {item["path"] for item in marks}:
            marks.append({"path": latest, "offset": 0})
        pattern = compare.marker_pattern(run_id=run_id)
        # One native input per prompt of this run; a prompt may carry several of its markers.
        inputs = [text for item in marks for text in compare.native_inputs(item["path"], pattern)
                  if compare.markers(text, pattern)]
        spent = usage.measure(provider, marks)
    except (OSError, ValueError, KeyError, TypeError):
        after, inputs, spent = None, [], None
    seen = sum(1 for hits in (row.get("marker_counts") or {}).values() if type(hits) is int and hits > 0)
    if spent is None or len(inputs) < seen:
        # A delivered prompt without its native input record means a transcript was missed.
        report.update(native_turns=None, tokens=None, usage=dict.fromkeys(summary.USAGE))
    else:
        report.update(native_turns=len(inputs), tokens=sum(spent.values()), budget_checked=min(len(inputs), len(markers)),
                      usage={"native_turns": len(inputs), **spent})
    planned = next((item for item in plan.deploy_occurrences(plan.load(
        Path(config["manifest"]) if config.get("manifest") else plan.MANIFEST))
        if item["scenario"] == args.scenario and item["cell"] == args.cell), {"scenario": args.scenario})
    for key, value in (config.get("occurrence_contracts") or {}).get(args.scenario, {}).items():
        planned.setdefault(key, value)
    planned.update(provider=provider, host=release_host.session_host(after or {}, channel))
    report["actual_occurrence_key"] = summary.occurrence_key(planned, args.profile)
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("--config", "--scenario", "--cell", "--run-id", "--profile", "--budget"):
        parser.add_argument(name, required=True)
    parser.add_argument("--deploy-sha")
    args = parser.parse_args(argv)
    print(json.dumps(run(args, runner.load_config(args.config)), sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
