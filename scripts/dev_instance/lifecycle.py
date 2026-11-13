"""Bound one declared child without claiming live readiness or orphan authority."""

import hashlib
import json
import math
import os
from pathlib import Path
import re
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


def _stop_child(child, grace):
    # This function receives only this supervisor's sole, unreaped Popen handle.
    if child.poll() is not None:
        return
    child.terminate()
    try:
        child.wait(timeout=grace)
    except subprocess.TimeoutExpired:
        child.kill()
        try:
            child.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            raise CleanupPending(child) from None


def supervise(base, instance_id, ambient, *, binary_sha256, head, wall_timeout,
              stop_grace, max_gateway=1, spawn=subprocess.Popen):
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
