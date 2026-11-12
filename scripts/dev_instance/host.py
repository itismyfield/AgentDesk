"""Manual dedicated Herdr ownership; no discovery or adoption of another daemon."""

import ctypes
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import time

from .config import HERDR_CONFIG
from .files import InstanceError, _open_parent, read_regular_text, safe_absolute
from .manifest import child_environment, read_json, validate
from .profile import Unevaluable

OWNER_FILE = "runtime/herdr-owner.json"
OWNER_LOCK = "runtime/herdr-owner.lock"


@contextmanager
def _owner_lock(root):
    parent, name = _open_parent(root, OWNER_LOCK)
    descriptor = None
    try:
        descriptor = os.open(name, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK,
                             0o600, dir_fd=parent)
        meta = os.fstat(descriptor)
        if not stat.S_ISREG(meta.st_mode) or meta.st_nlink != 1 or meta.st_uid != os.geteuid():
            raise InstanceError("invalid Herdr owner lock")
        os.fchmod(descriptor, 0o600)
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise InstanceError("Herdr owner is busy") from None
        yield
    finally:
        if descriptor is not None:
            os.close(descriptor)
        os.close(parent)


def _publish_owner(root, record):
    parent, name = _open_parent(root, OWNER_FILE)
    descriptor = None
    try:
        descriptor = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                             0o600, dir_fd=parent)
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "w", closefd=False) as output:
            json.dump(record, output, sort_keys=True)
            output.flush()
            os.fsync(descriptor)
        os.fsync(parent)
    finally:
        if descriptor is not None:
            os.close(descriptor)
        os.close(parent)


def _identifier(value):
    return (isinstance(value, str) and 0 < len(value) <= 256
            and all(ch.isascii() and (ch.isalnum() or ch in "-_:.") for ch in value))


def _same_owner(current, original):
    if not isinstance(current, dict) or set(current) != set(original):
        return False
    if any(current[key] != value for key, value in original.items() if key != "workspaces"):
        return False
    workspaces = current["workspaces"]
    if not isinstance(workspaces, list):
        return False
    seen_workspaces, seen_panes = set(), set()
    for pair in workspaces:
        if not isinstance(pair, dict) or set(pair) != {"workspace_id", "pane_id"}:
            return False
        workspace, pane = pair["workspace_id"], pair["pane_id"]
        if (not _identifier(workspace) or not _identifier(pane)
                or workspace in seen_workspaces or pane in seen_panes):
            return False
        seen_workspaces.add(workspace)
        seen_panes.add(pane)
    return True


class HerdrCleanupPending(InstanceError):
    def __init__(self, child):
        super().__init__("owned daemon cleanup pending")
        self.child = child


def _stop_owned(child):
    child.terminate()
    try:
        child.wait(timeout=5)
    except subprocess.TimeoutExpired:
        child.kill()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            raise HerdrCleanupPending(child) from None


def process_start(pid):
    """Read only the child PID returned by this wrapper's spawn handle."""
    if sys.platform == "darwin":
        buffer = ctypes.create_string_buffer(136)
        libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        size = libproc.proc_pidinfo(pid, 3, 0, buffer, len(buffer))
        if size != len(buffer):
            raise InstanceError("owned daemon start identity unavailable")
        seconds = int.from_bytes(buffer.raw[120:128], sys.byteorder)
        micros = int.from_bytes(buffer.raw[128:136], sys.byteorder)
        identity = {"platform": "darwin", "seconds": seconds, "micros": micros}
        if seconds and micros < 1_000_000:
            return identity
    elif sys.platform.startswith("linux"):
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        ticks = int(fields[19])
        if ticks:
            return {"platform": "linux", "ticks": ticks}
    raise InstanceError("owned daemon start identity unavailable")


class HerdrOwner:
    """Keep the original child handle; a receipt alone never authorizes a signal."""

    def __init__(self, manifest, child, identity, record):
        self.manifest = manifest
        self.child = child
        self.identity = identity
        self.record = record

    def close(self):
        root = Path(self.manifest["root"])
        with _owner_lock(root):
            if not _same_owner(read_json(root / OWNER_FILE), self.record):
                raise InstanceError("daemon receipt changed; cleanup refused")
            if self.child.poll() is not None:
                return
            if self.identity(self.child.pid) != self.record["daemon"]["start_identity"]:
                raise InstanceError("daemon identity changed; cleanup refused")
            _stop_owned(self.child)


def start_herdr(base, instance_id, binary, ambient, *, spawn=subprocess.Popen,
                identity=process_start):
    manifest = validate(base, instance_id, ambient, check_ports=False)
    root = Path(manifest["root"])
    declared = manifest["hosts"]["herdr"]
    binary = safe_absolute(binary)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise InstanceError("explicit Herdr binary unavailable")
    home = Path(declared["home"])
    config = home / "config.toml"
    if read_regular_text(config) != HERDR_CONFIG:
        raise InstanceError("Herdr config is not canonical")
    if config.stat().st_mtime_ns + 1_000_000_000 >= time.time_ns():
        raise Unevaluable("Herdr config preparation margin is not met")
    environment = child_environment(manifest, ambient)
    environment.update({
        "HOME": str(home), "XDG_CONFIG_HOME": str(home / "xdg"),
        "XDG_STATE_HOME": str(home / "state"), "XDG_DATA_HOME": str(home / "data"),
        "XDG_CACHE_HOME": str(home / "cache"), "XDG_RUNTIME_DIR": str(root / "runtime/herdr"),
        "HERDR_CONFIG_PATH": str(home / "config.toml"),
        "HERDR_SOCKET_PATH": declared["socket"], "HERDR_SESSION": declared["session"],
        "SHELL": "/bin/sh", "TERM": "xterm-256color",
    })
    binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
    with _owner_lock(root):
        for path in (root / OWNER_FILE, Path(declared["socket"])):
            if path.exists() or path.is_symlink():
                raise InstanceError("existing Herdr resource cannot be adopted")
        child = spawn([str(binary), "server"], env=environment, cwd=str(home),
                      stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                      stderr=subprocess.DEVNULL, close_fds=True, start_new_session=True)
        try:
            record = {
                "schema_version": 1, "instance_id": instance_id, "endpoint": declared,
                "binary_sha256": binary_hash,
                "daemon": {"pid": child.pid, "start_identity": identity(child.pid)},
                "workspaces": [],
            }
            if child.poll() is not None:
                raise InstanceError("owned Herdr exited before receipt publication")
            _publish_owner(root, record)
        except Exception:
            # Only this spawn handle is available; a disk PID never authorizes cleanup.
            if child.poll() is None:
                _stop_owned(child)
            raise
    return HerdrOwner(manifest, child, identity, record)
