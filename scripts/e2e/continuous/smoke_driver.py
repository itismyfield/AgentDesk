"""Runner driver for the release host: one deploy-smoke turn scenario on a machine-local test cell.

It runs the same harness and judge as the post-deploy smoke, then reports the continuous driver
schema. The harness runs inside this process, so the input writer lives and dies with the channel
lease holder, and every input passes the turn budget gate first. A lost observation (transport
timeout, rotated server log) is UNEVALUABLE, never FAIL.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys

from . import budget, channel_lock, plan, release_host, runner, summary, turn_gate, usage
from .store import write_json

REPO = Path(__file__).resolve().parents[3]
SCENARIOS = REPO / "tests/e2e/tui_relay/scenarios"
ASSERTION = "turns_delivered_once"
TRANSPORT = re.compile(r"^(TimeoutError|socket\.timeout)\b|URL error|\bHTTP 5\d\d\b|retry budget exhausted")
FINGERPRINT_BYTES = 65536
# Every input of these steps is a channel post that passes the turn gate; nothing else is driven here.
GATED_STEPS = {"send_discord_prompt", "wait_for_discord_text", "wait_for_raw_discord_text", "wait_idle_s",
               "relay_author", "timeout_s"}


def _smoke():
    sys.path.insert(0, str(REPO / "scripts/e2e"))
    import post_deploy_turn_smoke
    from tui_relay import source_compare
    return post_deploy_turn_smoke, source_compare


def observation(outcome, row, only_log_missing, stopped=None):
    """Map the smoke verdict; a seen duplicate or refusal stays FAIL even when an observation was also lost."""
    if outcome["classification"] == "PASS":
        return "PASS", None
    counts = row.get("marker_counts") if isinstance(row.get("marker_counts"), dict) else {}
    steps = row.get("step_timings") if isinstance(row.get("step_timings"), list) else []
    last = steps[-1].get("step") if steps and isinstance(steps[-1], dict) else ""
    seen = any(type(hits) is int and hits > 1 for hits in counts.values()) or row.get("input_refusals")
    if not seen and stopped:
        # The budget gate withheld the rest of the scenario, so the missing turns are not a product failure.
        return "UNEVALUABLE", stopped
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


def _harness(driver, gate, log_path, argv):
    """Run the harness in this process with its output in `log_path`; returns its exit code."""
    saved, command = (os.dup(1), os.dup(2)), sys.argv
    with open(log_path, "w", encoding="utf-8") as log:
        sys.stdout.flush()
        os.dup2(log.fileno(), 1)
        os.dup2(log.fileno(), 2)
        sys.argv, driver.discord.input_gate = [str(REPO / "scripts/e2e/run_tui_relay.py"), *argv], gate.before_send
        try:
            return driver.main()
        except SystemExit as stop:
            return stop.code if type(stop.code) is int else 2
        except Exception as error:  # noqa: BLE001 - the harness report, not this exception, is the evidence
            print(f"[e2e] harness aborted: {type(error).__name__}: {error}")
            return 2
        finally:
            sys.argv, driver.discord.input_gate = command, None
            sys.stdout.flush()
            for fd, target in zip(saved, (1, 2)):
                os.dup2(fd, target)
                os.close(fd)


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
    specs = (smoke.yaml.safe_load(path.read_text(encoding="utf-8")) for path in sorted(SCENARIOS.glob("*.yaml")))
    spec = next((data for data in specs if isinstance(data, dict) and str(data.get("id")) == args.scenario), {})
    steps = [step for step in spec.get("steps") or [] if isinstance(step, dict)]
    if not steps or not all(set(step) <= GATED_STEPS for step in steps):
        return refuse("SCENARIO_NOT_GATED")
    remaining, limit = json.loads(args.budget), cell["turn_tokens"]
    # The reservation must hold the whole worst-case plan before the first prompt; each turn is then gated.
    if (not markers or type(limit) is not int or limit <= 0 or remaining["turns"] < len(markers)
            or remaining["tokens"] < len(markers) * limit or remaining["time_s"] < cell["deadline_s"]):
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
    needs_log = smoke.cell_driver.cell_runtime(cell["smoke_cell"]) != "herdr"
    try:
        log_mark = _log_mark(host["server_log"]) if needs_log else None
        write_json(out / "plan.json", {"run_id": run_id, "markers": markers, "channel_id": channel})
    except (OSError, channel_lock.LeaseError):
        return refuse("EVIDENCE_UNWRITABLE")
    gate = turn_gate.TurnGate(budget.TurnGuard(remaining), provider, markers, limit,
                              lambda: _transcripts(compare, host, provider, channel), cell.get("usage_wait_s", 20))
    driver_rc = _harness(smoke.cell_driver, gate, out / "relay.log", [
        "--base-url", host["base_url"], "--cell", cell["smoke_cell"], "--channel-id", channel,
        "--scenarios", str(SCENARIOS), "--filter", args.scenario, "--run-id", run_id, "--no-reset-before-each",
        "--phase-deadline-s", str(cell["deadline_s"]), "--output", str(out),
        "--queue-runtime-root", host["runtime_root"], "--required-agent-mode", "real_live",
        "--required-coverage-class", "live"])
    gate.settle()
    log_read = needs_log and _log_excerpt(host["server_log"], log_mark, out / "server.log")
    counts, options = None, []
    if log_read:
        with open(out / "server.log", encoding="utf-8", errors="replace") as lines:
            counts = smoke.scan_log(lines, session=smoke.cell_driver.cell_session_name(cell["smoke_cell"]),
                                    channel_id=channel, launch_options=options)
    # The excerpt holds every channel's server lines from the run window; only its counters are kept.
    (out / "server.log").unlink(missing_ok=True)
    result = smoke._load_report(out / f"report.{cell['smoke_cell']}.json")  # noqa: SLF001
    def judged(log_counts):
        return smoke.classify(report=result, scenario_id=args.scenario, cell=cell["smoke_cell"], channel_id=channel,
                              run_id=run_id, driver_rc=driver_rc, markers=markers, log_counts=log_counts,
                              launch_options=options)

    outcome = judged(counts)
    rows = result.get("scenarios") if isinstance(result, dict) else None
    row = rows[0] if isinstance(rows, list) and len(rows) == 1 and isinstance(rows[0], dict) else {}
    # The log alone is missing when the same evidence passes with a clean excerpt of this cell.
    only_log = needs_log and not log_read and judged({"cell_lines": 1})["classification"] == "PASS"
    status, reason = observation(outcome, row, only_log, gate.stopped)
    turns = gate.guard.started
    if None in gate.spent:
        # A turn without its input record or finished usage is unmeasured: the runner keeps the reservation.
        report.update(native_turns=None, tokens=None, usage=dict.fromkeys(summary.USAGE))
        status, reason = ("FAIL", reason) if status == "FAIL" else ("UNEVALUABLE", reason or "USAGE_UNKNOWN")
    else:
        spent = {field: sum(turn[field] for turn in gate.spent) for field in usage.TOKENS}
        report.update(native_turns=turns, tokens=sum(spent.values()), budget_checked=turns,
                      usage={"native_turns": turns, **spent})
    try:
        after = release_host.detail(host["base_url"])
        if release_host.identity(host, after) != release_host.identity(host, before):
            status, reason = ("FAIL" if status == "FAIL" else "UNEVALUABLE"), "SERVER_CHANGED_DURING_RUN"
    except (OSError, ValueError, KeyError, TypeError):
        after = None
    report.update(status=status, real_provider_contacted=row.get("real_provider_contacted") is True,
                  evidence_paths=[str(out / name) for name in (f"report.{cell['smoke_cell']}.json", "relay.log")],
                  assertions=[{"key": ASSERTION, "status": status, "complete": status != "UNEVALUABLE"}],
                  verdict=f"relay {args.scenario} cell={cell['smoke_cell']} result={status} {outcome['observed_reason']}")
    if reason:
        report["reason"] = reason
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
