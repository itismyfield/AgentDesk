"""Bound one declared child without claiming live readiness or orphan authority."""

import hashlib
import json
import math
import threading
import os
from pathlib import Path
import re
import select
import math
import threading
import subprocess
import time

from .files import InstanceError, safe_absolute
from .lease import LEASE_ENV, acquire
from .manifest import child_environment, validate, worktree_identity

MAX_WALL_SECONDS = 600
MAX_STOP_GRACE_SECONDS = 30


class CleanupPending(InstanceError):
    def __init__(self, child):
        super().__init__("owned child cleanup pending")
        self.child = child


def _bounds(wall_timeout, stop_grace, max_gateway):
    valid = lambda value: type(value) in (int, float) and math.isfinite(value) and value > 0
    if (type(max_gateway) is not int or max_gateway != 1 or not valid(wall_timeout)
            or not valid(stop_grace) or wall_timeout > MAX_WALL_SECONDS
            or stop_grace > min(wall_timeout, MAX_STOP_GRACE_SECONDS)):
        raise InstanceError("unsupported lifecycle resource bounds")


def _stop_child(child, grace, deadline=None):
    # This function receives only this supervisor's sole, unreaped Popen handle.
    if child.poll() is not None:
        return
    remaining = lambda: grace if deadline is None else max(0, deadline-time.monotonic())
    child.terminate()
    try:
        child.wait(timeout=remaining())
    except subprocess.TimeoutExpired:
        child.kill()
        try:
            child.wait(timeout=remaining())
        except subprocess.TimeoutExpired:
            raise CleanupPending(child) from None


def supervise(base, instance_id, ambient, *, binary_sha256, head, wall_timeout,
              stop_grace, max_gateway=1, spawn=subprocess.Popen, resources=None, control_stream=None):
    _bounds(wall_timeout, stop_grace, max_gateway)
    started = time.monotonic()
    manifest = validate(base, instance_id, ambient)
    if (not isinstance(head, str) or manifest["head"] != head
            or not isinstance(binary_sha256, str) or not re.fullmatch(r"[0-9a-f]{64}", binary_sha256)):
        raise InstanceError("explicit lifecycle source identity mismatch")
    binary = safe_absolute(Path(manifest["worktree"]) / "target/debug/agentdesk")
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise InstanceError("declared lane binary is unavailable")
    with binary.open("rb") as source:
        if hashlib.file_digest(source, "sha256").hexdigest() != binary_sha256:
            raise InstanceError("declared lane binary SHA mismatch")
    environment = child_environment(manifest, ambient)
    root = Path(manifest["root"])
    with acquire(root, instance_id) as owner:
        owner.verify()
        if worktree_identity(manifest["worktree"]) != head:
            raise InstanceError("lifecycle source HEAD changed")
        remaining = wall_timeout - (time.monotonic() - started)
        if remaining <= 0:
            raise InstanceError("lifecycle startup budget exhausted")
        environment[LEASE_ENV] = json.dumps(owner.witness(), sort_keys=True, separators=(",", ":"))
        if resources is not None:
            return _supervise_resources(manifest, resources, binary, environment, owner,
                                        started + wall_timeout, stop_grace, spawn, control_stream)
        child = spawn([str(binary), "dcserver"], cwd=str(root), env=environment,
                      stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                      stderr=subprocess.DEVNULL, close_fds=True, pass_fds=(owner.fd,))
        status = "STOPPED"
        try:
            owner.verify()
            remaining = max(0, wall_timeout - (time.monotonic() - started))
            child.wait(timeout=remaining)
        except subprocess.TimeoutExpired:
            status = "TIMEOUT"
            _stop_child(child, stop_grace)
        except BaseException:
            _stop_child(child, stop_grace)
            raise
        if status == "STOPPED" and child.returncode != 0:
            status = "FAILED"
        return {"status": status, "live_ready": False, "instance_id": instance_id,
                "head": head, "binary_sha256": binary_sha256, "child_pid": child.pid,
                "child_exit": child.returncode, "nonce": owner.nonce,
                "max_gateway": 1, "restarts": 0}


def _supervise_resources(manifest, specification, binary, environment, lease, deadline, grace, spawn, control_stream):
    from . import keeper
    from .manifest import resource_spec
    specification = resource_spec(specification)
    if specification["stop_grace"] != grace or specification["startup_timeout"] > deadline-time.monotonic():
        raise InstanceError("resource bounds exceed supervisor admission")
    process, control = keeper.launch(manifest, specification, lease.nonce)
    child, requested, timed_out, stop_end = None, False, False, None
    try:
        ready = keeper.receive(control)
        if ready.get("state") != "RUNNING":
            return {**ready, "status": "HOLD", "live_ready": False,
                    "max_gateway": 1, "restarts": 0}
        lease.verify()
        if time.monotonic() >= deadline:
            raise InstanceError("resource startup exhausted wall budget")
        child = spawn([str(binary), "dcserver"], cwd=manifest["root"], env=environment,
                      stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                      stderr=subprocess.DEVNULL, close_fds=True, pass_fds=(lease.fd,))
        if control_stream is not None:
            def controls():
                try:
                    for line in iter(lambda: control_stream.readline(32770), ""):
                        frame = json.loads(line)
                        if len(line) <= 32769 and keeper.valid_request(frame, Path(manifest["root"]), lease.nonce):
                            keeper.request(manifest["root"], lease.nonce, frame["action"], timeout=grace+1)
                except (EOFError, OSError, ValueError):
                    return
            threading.Thread(target=controls, daemon=True).start()
        while child.poll() is None:
            remaining = deadline-time.monotonic()
            if remaining <= 0:
                requested, timed_out = True, True
                stop_end = time.monotonic() + grace
                _stop_child(child, grace, stop_end)
                break
            ready, _, _ = select.select([control], [], [], min(remaining, .05))
            if ready:
                frame = keeper.receive(control)
                stop_end = frame.get("deadline")
                if (set(frame) != {"action", "nonce", "deadline"} or frame["action"] != "stop"
                        or frame["nonce"] != lease.nonce or type(stop_end) not in (int, float)
                        or not math.isfinite(stop_end) or stop_end > time.monotonic()+grace):
                    raise InstanceError("keeper control authority changed")
                requested = True
                _stop_child(child, grace, stop_end)
                break
        stop_end = stop_end or time.monotonic()+grace
        keeper.send(control, {"nonce": lease.nonce, "exit": child.wait(timeout=max(0, stop_end-time.monotonic())),
                              "deadline": stop_end})
        control.settimeout(grace + 1)
        result = keeper.receive(control)
        if result.get("state") != "STOPPED" or not result.get("known_resources_closed"):
            raise CleanupPending(process)
        if process.wait(timeout=grace) != 0:
            raise CleanupPending(process)
        return {**result, "status": "TIMEOUT" if timed_out else
                "STOPPED" if requested or child.returncode == 0 else "FAILED",
                "child_pid": child.pid, "child_exit": child.returncode,
                "max_gateway": 1, "restarts": 0, "head": manifest["head"]}
    except (EOFError, OSError, ValueError) as error:
        if child is not None:
            _stop_child(child, grace, stop_end or time.monotonic()+grace)
        raise CleanupPending(process) from error
    except BaseException:
        if child is not None:
            stop_end = stop_end or time.monotonic()+grace
            _stop_child(child, grace, stop_end)
            try:
                keeper.send(control, {"nonce": lease.nonce, "exit": child.returncode, "deadline": stop_end})
            except OSError:
                pass
        raise
    finally:
        control.close()
        try:
            process.wait(timeout=grace + 1)
        except subprocess.TimeoutExpired:
            # The keeper retains pending original handles; killing it would orphan them.
            raise CleanupPending(process) from None
