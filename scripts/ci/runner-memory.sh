#!/usr/bin/env bash
# Samples memory and peak process names; the workflow keeps observation failures separate from build results.
exec python3 - "$0" "$@" <<'PY'
import base64
import contextlib
import datetime
import fcntl
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import urllib.parse
import urllib.request
import zipfile

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
CRATE_TYPES = {"bin", "lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}
COPIED = ("metadata.json", "phase.json", "samples.jsonl")
ARTIFACT_API = "/twirp/github.actions.results.api.v1.ArtifactService/"


def write_json(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, sort_keys=True) + "\n")
    temporary.replace(path)


@contextlib.contextmanager
def locked():
    with (STATE / "lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def mib(value):
    return "unavailable" if value is None else f"{value / 1048576:.1f}"


def read_number(path):
    try:
        value = path.read_text().strip()
        return int(value) if value.isdigit() else None
    except OSError:
        return None


def rustc_details(status, identity):
    try:
        executable, *args = os.fsdecode(status.with_name("cmdline").read_bytes()).split("\0")
        # A reused PID must not attach another process's arguments to this sample.
        if os.path.basename(executable) != "rustc" or not os.path.samestat(identity, status.stat()):
            return {}
    except OSError:
        return {}

    details = {"test": False}
    crate_types = []
    for index, arg in enumerate(args):
        if arg == "--":
            break
        if arg == "--test":
            details["test"] = True
        option, separator, value = arg.partition("=")
        if option not in ("--crate-name", "--crate-type"):
            continue
        if not separator and index + 1 < len(args):
            value = args[index + 1]
        if option == "--crate-name" and re.fullmatch(r"\w+", value):
            details["crate_name"] = value
        if option == "--crate-type" and all(kind in CRATE_TYPES for kind in value.split(",")):
            crate_types.append(value)
    if crate_types:
        details["crate_type"] = ",".join(crate_types)
    return details


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
    rss_sum, peaks, names = 0, {}, []
    for status in PROC.glob("[0-9]*/status"):
        try:
            with status.open(errors="replace") as stream:
                fields = dict(line.split(":", 1) for line in stream.read().splitlines() if ":" in line)
                comm = fields.get("Name", "").strip()
                identity = os.fstat(stream.fileno()) if comm == "rustc" else None
        except FileNotFoundError:
            continue
        except OSError:
            errors.append("process_status_unreadable")
            continue
        if "Name" in fields:
            names.append(comm)
        for field, metric in (("VmRSS", "rss"), ("VmHWM", "hwm")):
            parts = fields.get(field, "").split()
            if parts and parts[0].isdigit():
                size = int(parts[0]) * 1024
                if metric == "rss":
                    rss_sum += size
                if metric not in peaks or size > peaks[metric][0]:
                    peaks[metric] = (size, comm, status, identity)
    if names:
        values["rustc_processes"] = names.count("rustc")
    if "rss" in peaks:
        values["rss_sum_bytes"] = rss_sum
    details = {}
    for metric, (size, comm, status, identity) in peaks.items():
        values[f"largest_{metric}_bytes"] = size
        if comm:
            if status not in details:
                details[status] = {"comm": comm}
                if identity is not None:
                    details[status].update(rustc_details(status, identity))
            values[f"largest_{metric}_process"] = details[status]
    if not names or len(peaks) != 2:
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
    return row


def upload(name):
    # The calls actions/upload-artifact@v4 makes; no workflow step can run while the build step does.
    token = os.environ["ACTIONS_RUNTIME_TOKEN"]
    claims = token.split(".")[1]
    scopes = json.loads(base64.urlsafe_b64decode(claims + "=" * (-len(claims) % 4)))["scp"].split()
    run, job = next(scope.split(":")[1:] for scope in scopes if scope.startswith("Actions.Results:"))
    archive = io.BytesIO()
    with locked(), zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as bundle:
        # export.log carries the earlier results, so the last copy a lost runner left shows them.
        for item in (*COPIED, "export.log"):
            bundle.write(STATE / item, item)
    body = archive.getvalue()

    def call(method, **fields):
        fields.update(workflow_run_backend_id=run, workflow_job_run_backend_id=job, name=name)
        request = urllib.request.Request(
            urllib.parse.urljoin(os.environ["ACTIONS_RESULTS_URL"], ARTIFACT_API + method), json.dumps(fields).encode(),
            {"Authorization": "Bearer " + token, "Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=30) as response:
            reply = json.load(response)
        if not reply.get("ok"):
            raise RuntimeError(method)
        return reply

    expires = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(days=7)
    target = call("CreateArtifact", version=4, expires_at=expires.strftime("%Y-%m-%dT%H:%M:%SZ"))["signed_upload_url"]
    blob = urllib.request.Request(target, body, {"x-ms-blob-type": "BlockBlob", "x-ms-blob-content-type": "zip"}, method="PUT")
    urllib.request.urlopen(blob, timeout=60).close()
    call("FinalizeArtifact", size=str(len(body)), hash="sha256:" + hashlib.sha256(body).hexdigest())


def export(phase):
    # A lost runner never reaches the always() upload, so copies leave while the phase still runs.
    started = time.monotonic()
    for minutes in sorted(float(value) for value in os.environ["RUNNER_MEMORY_EXPORT_MINUTES"].split()):
        while time.monotonic() - started < minutes * 60:
            if (STATE / "stop").exists() or json.loads((STATE / "phase.json").read_text())["phase"] != phase:
                return
            time.sleep(1)
        name = f"{os.environ.get('RUNNER_MEMORY_EXPORT_NAME', 'runner-memory')}-at-{minutes:g}m"
        outcome = "exported " + name
        try:
            upload(name)
        except Exception as error:
            # Only the error type and HTTP status are logged; messages can carry a signed URL.
            outcome = f"failed {name} ({type(error).__name__} {getattr(error, 'code', '')})"
        try:
            with locked():
                latest = json.loads((STATE / "samples.jsonl").read_text().splitlines()[-1])
        except (OSError, ValueError, IndexError):
            latest = {}
        print(f"runner memory: {outcome} scheduled={minutes:g} elapsed={(time.monotonic() - started) / 60:.1f}"
              f" host_used={mib(latest.get('host_used_bytes'))} swap_used={mib(latest.get('swap_used_bytes'))}", flush=True)


def detach(command, log_name):
    with (STATE / log_name).open("a") as log:
        subprocess.Popen(["bash", str(SCRIPT), *command], stdin=subprocess.DEVNULL,
                         stdout=log, stderr=log, start_new_session=True, close_fds=True)


def runner_scope():
    environment = os.environ.get("RUNNER_ENVIRONMENT")
    if environment not in {"github-hosted", "self-hosted"}:
        return {"runner_environment": None, "shared_host": None}
    return {"runner_environment": environment, "shared_host": environment == "self-hosted"}


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
        scope = {key: metadata.get(key, value) for key, value in runner_scope().items()}
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
            **scope,
            "status": "unavailable" if not rows else ("partial" if errors or sampler_status != "stopped" else "complete"),
            "sampler_status": sampler_status, "samples": len(rows), "errors": errors,
            "interval_seconds": metadata.get("interval_seconds"), "cpu_count": metadata.get("cpu_count"),
            "host_total_bytes": maximum("host_total_bytes"),
            "cgroup_limit_bytes": min((row["cgroup_limit_bytes"] for row in rows if row.get("cgroup_limit_bytes") is not None), default=None),
            "cgroup_limit_state": "finite" if any(row.get("cgroup_limit_bytes") is not None for row in rows) else ("unlimited" if any(row.get("cgroup_limit_state") == "unlimited" for row in rows) else "unavailable"),
            "lifetime_cgroup_peak_bytes": maximum("cgroup_peak_bytes"), "phases": phases,
        }
        write_json(STATE / "summary.json", summary)
    limit = "unlimited" if summary["cgroup_limit_state"] == "unlimited" else mib(summary["cgroup_limit_bytes"]) + " MiB"
    lines = ["### Runner memory observation", "", f"Status: {summary['status']}; sampler: {sampler_status}; samples: {len(rows)}.",
             f"Runner environment: {scope['runner_environment'] or 'unavailable'}."
             + (" Counters can include activity from other workloads on this shared host." if scope["shared_host"] else ""),
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
            first = sample()
            write_json(STATE / "metadata.json", {"interval_seconds": interval, "cpu_count": os.cpu_count(),
                                                "cargo_build_jobs": os.environ.get("CARGO_BUILD_JOBS"), **runner_scope(),
                                                "image_os": os.environ.get("ImageOS"), "image_version": os.environ.get("ImageVersion"),
                                                "runner_name": os.environ.get("RUNNER_NAME"),
                                                "swap_total_bytes_at_start": first["swap_total_bytes"]})
        detach(["_sample"], "sampler.log")
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
    elif COMMAND == "checkpoint":
        # Freeze a pre-build copy for upload before a later runner loss can stop cleanup.
        with locked():
            sample()
            checkpoint = STATE.parent / "runner-memory-checkpoint"
            checkpoint.mkdir()
            for name in COPIED:
                shutil.copy2(STATE / name, checkpoint / name)
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
        if COMMAND == "begin" and os.environ.get("RUNNER_MEMORY_EXPORT_MINUTES"):
            detach(["_export", sys.argv[3]], "export.log")
    elif COMMAND == "_export":
        export(sys.argv[3])
    elif COMMAND == "finish":
        finish()
    else:
        raise ValueError("expected_start_checkpoint_begin_end_finish")


try:
    main()
except Exception as error:
    print("runner memory: observation failed (" + type(error).__name__ + ")", file=sys.stderr)
    sys.exit(1)
PY
