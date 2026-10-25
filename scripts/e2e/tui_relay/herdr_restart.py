"""Operator-managed restart observations; this module never restarts a server."""

import datetime as dt
import json
import math
import sys
import time
from pathlib import Path

from . import herdr

LIMITATION = "dcserver incarnation change is not proven; operator restart evidence is required"
UPTIME_NOTE = "restart inferred from server uptime below time since mark; no boot id"
INCARNATION_NOTE = "restart proven by a changed dcserver boot_incarnation"
# Whole-second uptime truncation plus 1 s spare for clock skew; elapsed starts after the mark
# response, so a restart during the mark request is excluded rather than covered by margin.
UPTIME_MARGIN_S = 2
# The clock behind the server's uptime (Rust Instant): host-wide since boot, paused while asleep.
BOOT_CLOCKS = {"darwin": "CLOCK_UPTIME_RAW", "linux": "CLOCK_MONOTONIC"}


def add_arguments(parser):
    parser.add_argument("--herdr-restart-observe-only", choices=("mark", "verify"),
                        help="Record before, or verify after an operator-managed restart; never restart.")
    parser.add_argument("--herdr-restart-mark", help="Observation JSON file shared by mark and verify.")


def validate_arguments(parser, args):
    mode = args.herdr_restart_observe_only
    if mode and (args.herdr_isolated_server or not args.cell.endswith("-herdr")):
        parser.error("observe-only requires a Herdr cell and forbids --herdr-isolated-server")
    if bool(mode) != bool(args.herdr_restart_mark):
        parser.error("--herdr-restart-observe-only and --herdr-restart-mark are required together")
    if mode and args.restart_script:
        parser.error("observe-only forbids --restart-script")


def target(args):
    return {"cell": args.cell, "channel": str(args.channel_id), "endpoint": args.herdr_endpoint,
            "base_url": args.base_url.rstrip("/")}


def boot_clock():
    """Seconds on the host's server-uptime clock, or None where the platform does not offer it."""
    name = BOOT_CLOCKS.get(sys.platform)
    clock = getattr(time, name, None) if name else None
    return None if clock is None else {"clock": name, "seconds": time.clock_gettime(clock)}


def parse_time(value):
    moment = dt.datetime.fromisoformat(value)
    if moment.tzinfo is None or moment > dt.datetime.now(dt.timezone.utc):
        raise ValueError("restart mark time invalid")
    return moment


def load_mark(path, args):
    mark = json.loads(Path(path).read_text(encoding="utf-8"))
    if not isinstance(mark, dict) or mark.get("version") != 1 or mark.get("target") != target(args):
        raise ValueError("restart mark target/version mismatch")
    before = mark.get("observation")
    if not isinstance(before, dict) or before.get("validated") is not True:
        raise ValueError("restart mark observation missing")
    if before.get("surface") != "GET /api/health/detail" or not mark.get("run_id"):
        raise ValueError("restart mark surface/run identity missing")
    observed = parse_time(before["observed_at"])
    # Older marks without completed_at still load; restart_evidence refuses to confirm with them.
    if "completed_at" in mark and parse_time(mark["completed_at"]) < observed:
        raise ValueError("restart mark time invalid")
    herdr.clean_turn(herdr.ready({"herdr": before.get("herdr")}, endpoint=args.herdr_endpoint, channel=args.channel_id))
    herdr.current_row({"executions": [before.get("row")], "other_input_holds": []},
                      channel=args.channel_id, provider=args.cell.split("-", 1)[0])
    return mark


def usable_clock(clock):
    """A supported boot-clock reading with finite, non-negative seconds."""
    return (isinstance(clock, dict) and clock.get("clock") in BOOT_CLOCKS.values()
            and type(clock.get("seconds")) in (int, float) and math.isfinite(clock["seconds"]) and clock["seconds"] >= 0)


def incarnation_evidence(after, mark):
    """Compare boot incarnations when both sides carry one; None falls back to uptime."""
    before = mark["observation"].get("server_values", {}).get("boot_incarnation")
    now = after.get("server_values", {}).get("boot_incarnation")
    if not all(isinstance(value, str) and value for value in (before, now)):
        return None
    return {"restart_evidence": "boot_incarnation" if before != now else "not_observed",
            "boot_incarnation_before": before, "boot_incarnation_after": now}


def restart_evidence(after, mark, verify_clock):
    """Prove a restart only when the server started after the mark; otherwise report not_proven."""
    # Wall time alone cannot rule out a clock step, so no shared boot clock means no confirmation.
    before_clock = mark.get("boot_clock")
    problem = ("the mark has no completion time" if "completed_at" not in mark
               else "the mark has no usable boot clock" if not usable_clock(before_clock)
               else "this host gives no usable boot clock" if not usable_clock(verify_clock)
               else "mark and verify boot clocks differ" if before_clock["clock"] != verify_clock["clock"] else None)
    if problem:
        raise ValueError(f"restart cannot be confirmed: {problem}; take a new mark")
    uptime = after.get("server_values", {}).get("uptime_secs")
    if type(uptime) is not int or uptime < 0:
        return {"restart_evidence": "not_proven"}
    # From mark completion to the verify request start; the smaller clock wins, so a wall-clock
    # step forward or a host sleep cannot inflate elapsed time.
    wall = (dt.datetime.fromisoformat(after["observed_at"]) - dt.datetime.fromisoformat(mark["completed_at"])).total_seconds()
    boot = verify_clock["seconds"] - before_clock["seconds"]
    elapsed = min(wall, boot)
    evidence = {"restart_evidence": "server_uptime", "uptime_secs": uptime, "wall_elapsed_secs": wall,
                "boot_clock": verify_clock["clock"], "boot_clock_elapsed_secs": boot, "elapsed_secs": elapsed}
    if elapsed <= 0 or not uptime < elapsed - UPTIME_MARGIN_S:
        evidence["restart_evidence"] = "not_observed"
    return evidence


def followup_scenarios(driver, args):
    if not args.filter or args.filter == "E-19":
        marker = "[E2E:E19:{run_id}:POST]"
        return [{"id": "E-19-observe-only", "agent_mode": "real_live", "coverage_class": "live",
                 "steps": [{"send_prompt": f"Reply with exactly {marker}"}, {"wait_for_discord_text": marker}],
                 "report_marker_counts": [marker],
                 "assertions": [{"text_present": marker}, {"no_duplicate_marker": marker}]}]
    wanted = driver.validate_scenario_filter(args.filter, Path(args.scenarios))
    scenarios = [s for s in driver.load_scenarios(Path(args.scenarios), cell=args.cell) if s["id"] in wanted]
    if not scenarios or len(scenarios) != len(wanted):
        raise ValueError("observe-only follow-up scenario unavailable for cell")
    if any("restart_dcserver" in step for s in scenarios for step in s.get("steps", [])):
        raise ValueError("observe-only follow-up cannot contain restart_dcserver")
    return scenarios


def run(driver, args):
    output = driver.resolve_output_dir(args.output, args.cell)
    run_id = getattr(args, "run_id", output.name)
    report = {"run_id": run_id, "cell": args.cell, "channel_id": args.channel_id, "scenarios": [],
              "restart_observation": {"mode": args.herdr_restart_observe_only, "limitation": LIMITATION,
                                      "mark_file": str(args.herdr_restart_mark)},
              "gate_notice": "First gate requires status=pass for each scenario; exit code alone is insufficient."}
    restart = report["restart_observation"]
    try:
        if args.herdr_isolated_server or args.restart_script:
            raise ValueError("observe-only forbids isolated-server and restart scripts")
        scenarios = followup_scenarios(driver, args) if args.herdr_restart_observe_only == "verify" else []
        if args.dry_run:
            print("[dry-run] observe-only: detail + local CLI status; no mark read/write or restart")
            restart["status"] = "dry_run"
        elif args.herdr_restart_observe_only == "mark":
            before = herdr.observe(driver, args)
            completed = {"completed_at": dt.datetime.now(dt.timezone.utc).isoformat(), "boot_clock": boot_clock()}
            restart["before"] = before
            restart["mark_completed"] = completed
            mark = {"version": 1, "run_id": run_id, "target": target(args), "observation": before, **completed}
            with Path(args.herdr_restart_mark).open("x", encoding="utf-8") as handle:
                json.dump(mark, handle, indent=2)
            restart["status"] = "marked"
        else:
            mark = load_mark(args.herdr_restart_mark, args)
            restart["before"] = mark["observation"]
            restart["mark_run_id"] = mark["run_id"]
            # Same cell lease key as the driver's normal sweep, so the two never share a channel.
            with driver.lease.acquire(f"{args.cell}-{run_id}", cell=args.cell.replace("-herdr", "-tui")):
                verify_clock = boot_clock()
                after = herdr.observe(driver, args, reconnected=True)
                restart["after"] = after
                if any(after["row"].get(k) != mark["observation"]["row"].get(k) for k in ("channel", "provider", "nonce")):
                    raise ValueError("restart target row continuity failed")
                restart.update(incarnation_evidence(after, mark) or restart_evidence(after, mark, verify_clock))
                if restart["restart_evidence"] == "not_observed" and "boot_incarnation_before" in restart:
                    raise ValueError(f"boot_incarnation stayed {restart['boot_incarnation_before']}; restart not observed")
                if restart["restart_evidence"] == "not_observed":
                    raise ValueError(f"server uptime {restart['uptime_secs']}s is not below {restart['elapsed_secs']:.3f}s "
                                     f"since mark minus {UPTIME_MARGIN_S}s; restart not observed")
                if restart["restart_evidence"] == "server_uptime":
                    restart["limitation"] = UPTIME_NOTE
                if restart["restart_evidence"] == "boot_incarnation":
                    restart["limitation"] = INCARNATION_NOTE
                restart["status"] = "verified"
                restart["followup_scope"] = "Post-restart turn only; original E-8/E-19 acceptance is not claimed."
                client = driver.discord.DiscordClient(base_url=args.base_url, timeout_s=args.turn_start_timeout_s)
                for scenario in scenarios:
                    result = driver.run_scenario(scenario, args=args, client=client, run_id=run_id)
                    report["scenarios"].append(result)
                    if result["status"] != "pass":
                        break
    except Exception as error:
        restart.update(status="fail", reason=f"{type(error).__name__}: {error}")
        if hasattr(error, "herdr_observation"):
            restart["failed_observation"] = error.herdr_observation
    # The whole run: mark/verify observations plus every follow-up turn's own observations.
    observations = [restart.get(k) for k in ("before", "after", "failed_observation")]
    observations += [o for r in report["scenarios"] if isinstance(r, dict) for o in r.get("herdr_observations") or []]
    report["herdr_readiness_waiver"] = herdr.waiver_summary(args, observations)
    path = output / f"report.{args.cell}.json"
    path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(f"[e2e] observe-only {restart['status']}; {restart['limitation']}; report → {path}")
    if getattr(args, "herdr_allowed_degraded_reason", None):
        print(f"[e2e] herdr readiness waiver: {json.dumps(report['herdr_readiness_waiver'], ensure_ascii=False)}")
    return int(restart["status"] == "fail" or any(r["status"] != "pass" for r in report["scenarios"]))
