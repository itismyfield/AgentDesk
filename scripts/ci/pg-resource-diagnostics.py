#!/usr/bin/env python3
"""Bounded foreground before/EXIT snapshots; intentionally no mid-Cargo coverage."""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import math
import os
from pathlib import Path
import re
import selectors
import shutil
import signal
import subprocess
import sys
import time

OUTPUT_CAP = 4096
FILE_CAP = 32768
MEMORY_KEYS = ("mem_total_mib", "mem_available_mib", "swap_total_mib", "swap_free_mib")
MEMINFO_KEYS = ("MemTotal", "MemAvailable", "SwapTotal", "SwapFree")
STATE_FORMAT = ('{"Status":{{json .State.Status}},"ExitCode":{{.State.ExitCode}},'
                '"OOMKilled":{{.State.OOMKilled}},"Error":{{json .State.Error}}}')
STATS_PATTERN = re.compile(r"\d+(?:\.\d+)?\s*[KMGTPE]?i?B\s*/\s*"
                           r"\d+(?:\.\d+)?\s*[KMGTPE]?i?B;\d+(?:\.\d+)?%")


def container_name() -> str:
    return os.environ.get("POSTGRES_SERVICE_CONTAINER") or "agentdesk-postgres"


def bounded_probe(argv: list[str], *, timeout: float = 2.0) -> dict:
    """Cap allocation while reading; kill the saved owned group even if its leader exited."""
    try:
        process = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
    except OSError as error:
        return {"error": f"spawn-failed: {error}"}
    buffers = {process.stdout: bytearray(), process.stderr: bytearray()}
    failure = None
    deadline = time.monotonic() + timeout
    try:
        with selectors.DefaultSelector() as selector:
            for stream in buffers:
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_READ)
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError
                for key, _ in selector.select(remaining):
                    chunk = os.read(key.fileobj.fileno(), OUTPUT_CAP)
                    if not chunk:
                        selector.unregister(key.fileobj)
                    elif len(buffers[key.fileobj]) + len(chunk) > OUTPUT_CAP:
                        raise OverflowError
                    else:
                        buffers[key.fileobj].extend(chunk)
            process.wait(timeout=max(0.001, deadline - time.monotonic()))
    except (TimeoutError, subprocess.TimeoutExpired):
        failure = "timeout"
    except OverflowError:
        failure = "output-limit"
    except OSError as error:
        failure = f"read-failed: {error}"
    finally:
        # start_new_session makes pid the owned pgid; do not rediscover it via a dead leader.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        for stream in buffers:
            stream.close()
        try:
            process.wait(timeout=1.0)
        except subprocess.TimeoutExpired:
            failure = "cleanup-timeout"
    if failure:
        return {"error": failure}
    if process.returncode != 0:
        return {"error": f"exit-{process.returncode}"}
    return {"stdout": buffers[process.stdout].decode("utf-8", errors="replace").strip()}


def memory() -> dict:
    try:
        rows = [line.split() for line in Path("/proc/meminfo").read_text().splitlines()]
        values = {row[0].rstrip(":"): int(row[1]) // 1024 for row in rows
                  if len(row) == 3 and row[2] == "kB" and row[0].rstrip(":") in MEMINFO_KEYS}
        return {field: values[key] for key, field in zip(MEMINFO_KEYS, MEMORY_KEYS)}
    except (OSError, ValueError, KeyError) as error:
        return {"error": f"meminfo-unavailable: {error}"}


def filesystems() -> dict:
    target = os.environ.get("CARGO_TARGET_DIR") or str(Path(os.environ.get("GITHUB_WORKSPACE", ".")) / "target")
    result = {}
    for label, raw in (("workspace_target", target), ("runner_temp", os.environ.get("RUNNER_TEMP", "."))):
        entry = {"requested_path": raw}
        try:
            path = Path(raw)
            ancestor = next(p for p in (path, *path.parents) if p.exists())
            usage = shutil.disk_usage(ancestor)
            entry.update(measured_path=str(ancestor), total_mib=usage.total // 1048576,
                         free_mib=usage.free // 1048576)
        except (OSError, StopIteration) as error:
            entry["error"] = f"filesystem-unavailable: {error}"
        result[label] = entry
    return result


def metadata(options) -> dict:
    return {"run_id": options.run_id, "run_attempt": options.run_attempt,
            "commit_sha": options.commit_sha, "container": container_name()}


def emit(record: dict, directory: str) -> int:
    line = json.dumps(record, sort_keys=True)
    failed = False
    try:
        print(line, flush=True)
    except OSError:
        failed = True
    try:
        with (Path(directory) / "samples.jsonl").open("a", encoding="utf-8") as stream:
            stream.write(line + "\n")
            stream.flush()
    except OSError:
        failed = True
    return int(failed)


def snapshot(options) -> int:
    probe = bounded_probe(["docker", "stats", "--no-stream", "--format",
                           "{{.MemUsage}};{{.MemPerc}}", container_name()])
    pg = {"name": container_name(), "mem_usage": probe.get("stdout", "")}
    if "error" in probe or not STATS_PATTERN.fullmatch(pg["mem_usage"]):
        pg["error"] = probe.get("error", "invalid-stats")
    state = None
    status = options.status if options.phase == "exit" else None
    if status not in (None, 0, 130, 143):
        inspected = bounded_probe(["docker", "inspect", "--format", STATE_FORMAT, container_name()])
        try:
            state = json.loads(inspected.get("stdout", ""))
            if not isinstance(state, dict):
                raise ValueError("state is not an object")
            state["scope"] = "postgres-container-only"
        except ValueError:
            state = {"error": inspected.get("error", "invalid-state")}
    record = {"kind": "snapshot", "coverage": "boundary-only", "phase": options.phase,
              "sequence": 1 if options.phase == "before" else 2, "owner_status": status,
              "metadata": metadata(options), "memory": memory(), "filesystems": filesystems(),
              "pg_container": pg, "pg_container_state": state,
              "utc": datetime.now(timezone.utc).isoformat(), "monotonic": time.monotonic()}
    return emit(record, options.directory)


def nonnegative_int(value) -> bool:
    return type(value) is int and value >= 0


def valid_fields(record: dict) -> bool:
    mem, fs, pg = (record.get(key) for key in ("memory", "filesystems", "pg_container"))
    if not isinstance(mem, dict) or "error" in mem:
        return False
    if not all(nonnegative_int(mem.get(key)) for key in MEMORY_KEYS):
        return False
    if not 0 <= mem["mem_available_mib"] <= mem["mem_total_mib"] or mem["mem_total_mib"] == 0:
        return False
    if mem["swap_free_mib"] > mem["swap_total_mib"] or not isinstance(fs, dict):
        return False
    for label in ("workspace_target", "runner_temp"):
        entry = fs.get(label)
        if not isinstance(entry, dict) or "error" in entry:
            return False
        if not all(isinstance(entry.get(key), str) and entry[key] for key in ("requested_path", "measured_path")):
            return False
        if not all(nonnegative_int(entry.get(key)) for key in ("total_mib", "free_mib")):
            return False
        if not 0 <= entry["free_mib"] <= entry["total_mib"] or entry["total_mib"] == 0:
            return False
    if not isinstance(pg, dict) or "error" in pg or pg.get("name") != container_name():
        return False
    return isinstance(pg.get("mem_usage"), str) and bool(STATS_PATTERN.fullmatch(pg["mem_usage"]))


def verify(options) -> int:
    problems = []
    try:
        if not options.directory:
            raise ValueError("no diagnostics directory")
        with (Path(options.directory) / "samples.jsonl").open("rb") as stream:
            data = stream.read(FILE_CAP + 1)
        if len(data) > FILE_CAP:
            raise ValueError("evidence exceeds inspection cap")
        records = [json.loads(line) for line in data.decode("utf-8").splitlines()]
        if len(records) != 2 or not all(isinstance(r, dict) for r in records):
            raise ValueError("exactly two boundary records are required")
        previous = None
        for sequence, (record, phase) in enumerate(zip(records, ("before", "exit")), 1):
            if "error" in record or record.get("kind") != "snapshot" or record.get("coverage") != "boundary-only":
                problems.append("wrong record kind/coverage")
            if record.get("phase") != phase or type(record.get("sequence")) is not int or record["sequence"] != sequence:
                problems.append("wrong boundary order/sequence")
            if record.get("metadata") != metadata(options) or not all((options.run_id, options.run_attempt, options.commit_sha)):
                problems.append("stale or missing run/attempt/SHA/container metadata")
            stamp = record.get("monotonic")
            utc = datetime.fromisoformat(record["utc"])
            if type(stamp) not in (int, float) or not math.isfinite(stamp) or stamp < 0 or utc.utcoffset() is None:
                raise ValueError("invalid monotonic/UTC timestamp")
            if previous is not None:
                elapsed = stamp - previous[0]
                wall_elapsed = (utc - previous[1]).total_seconds()
                if not 0 <= elapsed <= 1830 or wall_elapsed < 0 or abs(elapsed - wall_elapsed) > 5:
                    problems.append("inconsistent timestamps or exceeded step budget")
            previous = (stamp, utc)
            if not valid_fields(record):
                problems.append(f"{phase}: missing, invalid or unavailable requested resource fields")
            status = record.get("owner_status")
            if phase == "before" and status is not None:
                problems.append("before snapshot carries an exit status")
            if phase == "exit" and (not nonnegative_int(status) or status > 255):
                problems.append("exit snapshot has no valid owner status")
            state = record.get("pg_container_state")
            if phase == "exit" and nonnegative_int(status) and status not in (0, 130, 143):
                if not isinstance(state, dict) or "error" in state:
                    problems.append("failure container state is unavailable")
                elif not (isinstance(state.get("Status"), str) and state["Status"]
                          and nonnegative_int(state.get("ExitCode")) and type(state.get("OOMKilled")) is bool
                          and isinstance(state.get("Error"), str) and state.get("scope") == "postgres-container-only"):
                    problems.append("invalid container state fields")
            elif state is not None:
                problems.append("unexpected postmortem for before/success/cancellation")
    except (OSError, ValueError, KeyError, TypeError) as error:
        problems.append(f"unavailable or malformed evidence: {error}")
    for problem in problems:
        print(f"FAIL: {problem}", file=sys.stderr)
    if problems:
        return 1
    print("Two valid boundary snapshots; the entire interval between them is unobserved.")
    for record in records:
        print(json.dumps(record, sort_keys=True)[:OUTPUT_CAP])
    return 0


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("snapshot", "verify"))
    parser.add_argument("--directory", default="")
    parser.add_argument("--phase", choices=("before", "exit"), default="before")
    parser.add_argument("--status", type=int, default=0)
    for field in ("--run-id", "--run-attempt", "--commit-sha"):
        parser.add_argument(field, default="")
    options = parser.parse_args(argv)
    for received in (signal.SIGINT, signal.SIGTERM):
        signal.signal(received, lambda number, _: sys.exit(128 + number))
    return snapshot(options) if options.mode == "snapshot" else verify(options)


if __name__ == "__main__":
    sys.exit(main())
