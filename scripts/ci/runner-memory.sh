#!/usr/bin/env bash
# Samples resource counters only; the workflow keeps observation failures separate from build results.
exec python3 - "$0" "$@" <<'PY'
import contextlib
import datetime
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import time

SCRIPT = Path(sys.argv[1]).resolve()
COMMAND = sys.argv[2] if len(sys.argv) > 2 else ""
STATE = Path(os.environ.get("RUNNER_TEMP", "/tmp")) / "runner-memory"
PROC = Path(os.environ.get("RUNNER_MEMORY_PROC_ROOT", "/proc"))
CGROUP = Path(os.environ.get("RUNNER_MEMORY_CGROUP_ROOT", "/sys/fs/cgroup"))
IDLE = "Setup / other steps"
METRICS = (
    "host_total_bytes", "host_used_bytes", "swap_total_bytes", "swap_used_bytes",
    "cgroup_current_bytes", "cgroup_peak_bytes", "rss_sum_bytes",
    "largest_rss_bytes", "largest_hwm_bytes", "rustc_processes",
)


def write_json(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, sort_keys=True) + "\n")
    temporary.replace(path)


@contextlib.contextmanager
def locked():
    with (STATE / "lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def read_number(path):
    try:
        value = path.read_text().strip()
        return int(value) if value.isdigit() else None
    except OSError:
        return None


def snapshot():
    values = dict.fromkeys(METRICS)
    errors = []
    try:
        memory = {}
        for line in (PROC / "meminfo").read_text().splitlines():
            key, _, value = line.partition(":")
            fields = value.split()
            if fields and fields[0].isdigit():
                memory[key] = int(fields[0]) * 1024
        for prefix, total, available in (
            ("host", "MemTotal", "MemAvailable"), ("swap", "SwapTotal", "SwapFree"),
        ):
            values[prefix + "_total_bytes"] = memory.get(total)
            if total in memory and available in memory:
                values[prefix + "_used_bytes"] = memory[total] - memory[available]
            else:
                errors.append(prefix + "_memory_unavailable")
    except OSError:
        errors.append("meminfo_unavailable")
    scope = CGROUP
    try:
        membership = (PROC / "self/cgroup").read_text().splitlines()
        path = next(line[3:] for line in membership if line.startswith("0::"))
        scope = CGROUP / path.lstrip("/")
    except (OSError, StopIteration):
        errors.append("cgroup_membership_unavailable")
    for name in ("current", "peak"):
        values["cgroup_" + name + "_bytes"] = read_number(scope / ("memory." + name))
        if values["cgroup_" + name + "_bytes"] is None:
            errors.append("cgroup_" + name + "_unavailable")
    limits = []
    limit_state = "unavailable"
    for parent in (scope, *scope.parents):
        if not parent.is_relative_to(CGROUP):
            break
        limit = read_number(parent / "memory.max")
        if limit is not None:
            limits.append(limit)
        else:
            try:
                if (parent / "memory.max").read_text().strip() == "max":
                    limit_state = "unlimited"
            except OSError:
                pass
    values["cgroup_limit_bytes"] = min(limits) if limits else None
    values["cgroup_limit_state"] = "finite" if limits else limit_state
    if limit_state == "unavailable" and not limits:
        errors.append("cgroup_limit_unavailable")
    rss, hwm, names = [], [], []
    for status in PROC.glob("[0-9]*/status"):
        try:
            fields = dict(line.split(":", 1) for line in status.read_text().splitlines() if ":" in line)
        except FileNotFoundError:
            continue
        except OSError:
            errors.append("process_status_unreadable")
            continue
        if "Name" in fields:
            names.append(fields["Name"].strip())
        for field, collection in (("VmRSS", rss), ("VmHWM", hwm)):
            parts = fields.get(field, "").split()
            if parts and parts[0].isdigit():
                collection.append(int(parts[0]) * 1024)
    if names:
        values["rustc_processes"] = names.count("rustc")
    if rss:
        values["rss_sum_bytes"], values["largest_rss_bytes"] = sum(rss), max(rss)
    if hwm:
        values["largest_hwm_bytes"] = max(hwm)
    if not names or not rss or not hwm:
        errors.append("process_memory_unavailable")
    values["errors"] = sorted(set(errors))
    return values


def sample(phase=None):
    if phase is None:
        phase = json.loads((STATE / "phase.json").read_text())["phase"]
    row = snapshot()
    row.update(phase=phase, timestamp=datetime.datetime.now(datetime.timezone.utc).isoformat())
    with (STATE / "samples.jsonl").open("a") as stream:
        stream.write(json.dumps(row, sort_keys=True) + "\n")


def finish():
    started = (STATE / "metadata.json").exists()
    (STATE / "stop").touch()
    deadline = time.monotonic() + 2
    while started and not (STATE / "done.json").exists() and time.monotonic() < deadline:
        time.sleep(0.05)
    sampler_status = "not_started"
    if started:
        sampler_status = "stop_timeout"
        if (STATE / "done.json").exists():
            sampler_status = json.loads((STATE / "done.json").read_text())["status"]
    with locked():
        rows = []
        if (STATE / "samples.jsonl").exists():
            rows = [json.loads(line) for line in (STATE / "samples.jsonl").read_text().splitlines()]
        metadata = json.loads((STATE / "metadata.json").read_text()) if started else {}
        errors = sorted({error for row in rows for error in row["errors"]})
        phases = {}
        for row in rows:
            phase = phases.setdefault(row["phase"], {"samples": 0, "max": dict.fromkeys(METRICS)})
            phase["samples"] += 1
            for key in METRICS:
                value = row.get(key)
                if value is not None:
                    previous = phase["max"][key]
                    phase["max"][key] = value if previous is None else max(previous, value)
        def maximum(key):
            available = [row[key] for row in rows if row.get(key) is not None]
            return max(available) if available else None
        summary = {
            "status": "unavailable" if not rows else ("partial" if errors or sampler_status != "stopped" else "complete"),
            "sampler_status": sampler_status, "samples": len(rows), "errors": errors,
            "interval_seconds": metadata.get("interval_seconds"), "cpu_count": metadata.get("cpu_count"),
            "host_total_bytes": maximum("host_total_bytes"),
            "cgroup_limit_bytes": min((row["cgroup_limit_bytes"] for row in rows if row.get("cgroup_limit_bytes") is not None), default=None),
            "cgroup_limit_state": "finite" if any(row.get("cgroup_limit_bytes") is not None for row in rows) else ("unlimited" if any(row.get("cgroup_limit_state") == "unlimited" for row in rows) else "unavailable"),
            "lifetime_cgroup_peak_bytes": maximum("cgroup_peak_bytes"), "phases": phases,
        }
        write_json(STATE / "summary.json", summary)
    def mib(value):
        return "unavailable" if value is None else f"{value / 1048576:.1f}"
    limit = "unlimited" if summary["cgroup_limit_state"] == "unlimited" else mib(summary["cgroup_limit_bytes"]) + " MiB"
    lines = ["### Runner memory observation", "", f"Status: {summary['status']}; sampler: {sampler_status}; samples: {len(rows)}.",
             f"Interval: {summary['interval_seconds']} s; CPUs: {summary['cpu_count']}; host RAM: {mib(summary['host_total_bytes'])} MiB; cgroup limit: {limit}.",
             f"Cgroup lifetime peak: {mib(summary['lifetime_cgroup_peak_bytes'])} MiB (not a per-step peak).", "",
             "| Phase | Samples | Host used MiB | Swap used MiB | RSS sum MiB | Largest RSS MiB | rustc count | Cgroup current MiB |",
             "|---|---:|---:|---:|---:|---:|---:|---:|"]
    for label, phase in phases.items():
        peak = phase["max"]
        safe_label = label.replace("|", "\\|").replace("\n", " ")
        count = peak["rustc_processes"]
        lines.append(f"| {safe_label} | {phase['samples']} | {mib(peak['host_used_bytes'])} | {mib(peak['swap_used_bytes'])} | {mib(peak['rss_sum_bytes'])} | {mib(peak['largest_rss_bytes'])} | {count if count is not None else 'unavailable'} | {mib(peak['cgroup_current_bytes'])} |")
    lines += ["", "Host used = MemTotal - MemAvailable. RSS sums include shared pages; process HWM and cgroup peak are lifetime counters. Sampled maxima can miss spikes shorter than the interval.",
              "Unavailable counters are null in summary.json; zero is used only for an observed zero.", ""]
    if errors:
        lines += ["Unavailable/partial counters: " + ", ".join(errors), ""]
    report = "\n".join(lines)
    (STATE / "summary.md").write_text(report)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as stream:
            stream.write(report)
    print(f"runner memory: {summary['status']} ({len(rows)} samples; {sampler_status})")


def main():
    if "RUNNER_TEMP" not in os.environ:
        raise ValueError("runner_temp_required")
    STATE.mkdir(parents=True, exist_ok=True)
    if COMMAND == "start":
        interval = float(os.environ.get("RUNNER_MEMORY_INTERVAL_SECONDS", "5"))
        if not 0 < interval <= 60:
            raise ValueError("invalid_interval")
        with locked():
            if (STATE / "metadata.json").exists():
                raise ValueError("already_started")
            write_json(STATE / "phase.json", {"phase": IDLE})
            write_json(STATE / "metadata.json", {"interval_seconds": interval, "cpu_count": os.cpu_count()})
            sample()
        with (STATE / "sampler.log").open("a") as log:
            subprocess.Popen(["bash", str(SCRIPT), "_sample"], stdin=subprocess.DEVNULL,
                             stdout=log, stderr=log, start_new_session=True, close_fds=True)
    elif COMMAND == "_sample":
        status = "error"
        try:
            interval = json.loads((STATE / "metadata.json").read_text())["interval_seconds"]
            deadline = time.monotonic()
            expires = deadline + 21600
            while not (STATE / "stop").exists() and time.monotonic() < expires:
                if time.monotonic() >= deadline:
                    with locked():
                        sample()
                    deadline = time.monotonic() + interval
                time.sleep(0.05)
            status = "stopped" if (STATE / "stop").exists() else "expired"
        finally:
            write_json(STATE / "done.json", {"status": status})
    elif COMMAND in ("begin", "end"):
        with locked():
            if COMMAND == "begin":
                if len(sys.argv) != 4 or not sys.argv[3].strip():
                    raise ValueError("phase_required")
                write_json(STATE / "phase.json", {"phase": sys.argv[3]})
                sample()
            else:
                sample()
                write_json(STATE / "phase.json", {"phase": IDLE})
    elif COMMAND == "finish":
        finish()
    else:
        raise ValueError("expected_start_begin_end_finish")


try:
    main()
except Exception as error:
    print("runner memory: observation failed (" + type(error).__name__ + ")", file=sys.stderr)
    sys.exit(1)
PY
