"""Manual dedicated Herdr ownership; no discovery or adoption of another daemon."""

import ctypes
import hashlib
import os
from pathlib import Path
import subprocess
import sys

from .files import InstanceError, safe_absolute, write_private_json, write_private_text
from .manifest import child_environment, read_json, validate

OWNER_FILE = "runtime/herdr-owner.json"
HERDR_CONFIG = "[session]\nresume_agents_on_restore = false\n"


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
        if self.child.poll() is not None:
            return
        root = Path(self.manifest["root"])
        if read_json(root / OWNER_FILE) != self.record:
            raise InstanceError("daemon receipt changed; cleanup refused")
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
    for path in (root / OWNER_FILE, Path(declared["socket"])):
        if path.exists() or path.is_symlink():
            raise InstanceError("existing Herdr resource cannot be adopted")
    home = Path(declared["home"])
    write_private_text(root, home.relative_to(root) / "config.toml", HERDR_CONFIG)
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
        write_private_json(root, OWNER_FILE, record)
    except Exception:
        # Only the handle just spawned is available here; no PID is recovered from disk.
        if child.poll() is None:
            _stop_owned(child)
        raise
    return HerdrOwner(manifest, child, identity, record)
