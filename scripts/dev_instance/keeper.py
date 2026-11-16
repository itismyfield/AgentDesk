"""Keep original resource handles; uncertain ownership remains a visible HOLD."""

import fcntl
import json
import math
import os
from pathlib import Path
import select
import socket
import stat
import subprocess
import sys
import time

from .files import InstanceError, _open_dir, instance_root, read_regular_text

LOCK_NAME = ".resources.lease"
MARKER_NAME = ".resources.owner"
OWNER_FILE = "runtime/resources-owner.json"
SOCKET_FILE = "runtime/keeper.sock"


def _identity(metadata):
    return metadata.st_dev, metadata.st_ino


def _private(metadata):
    return (stat.S_ISREG(metadata.st_mode) and stat.S_IMODE(metadata.st_mode) == 0o600
            and metadata.st_uid == os.geteuid() and metadata.st_nlink == 1)


class ResourceLock:
    def __init__(self, root, nonce):
        self.root = instance_root(Path(root).parent, Path(root).name)
        self.parent = _open_dir(self.root.parent)
        self.fd = self.marker = None
        self.finished = False
        try:
            metadata = os.fstat(self.parent)
            if stat.S_IMODE(metadata.st_mode) != 0o700 or metadata.st_uid != os.geteuid():
                raise InstanceError("resource pool must be private")
            self.parent_identity = _identity(metadata)
            self.fd = os.open(LOCK_NAME, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK,
                              0o600, dir_fd=self.parent)
            if not _private(os.fstat(self.fd)):
                raise InstanceError("invalid fixed resource lock")
            self.lock_identity = _identity(os.fstat(self.fd))
            fcntl.flock(self.fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.verify()
            # A crash may close the lock while children survive; never adopt that marker.
            self.marker = os.open(MARKER_NAME, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                                  0o600, dir_fd=self.parent)
            self.marker_identity = _identity(os.fstat(self.marker))
            self.record = json.dumps({"root": str(self.root), "nonce": nonce}).encode()
            if os.write(self.marker, self.record) != len(self.record):
                raise InstanceError("resource marker publication incomplete")
            os.fsync(self.marker)
            os.fsync(self.parent)
        except OSError:
            self.close()
            raise InstanceError("resource authority unavailable; HOLD") from None
        except BaseException:
            self.close()
            raise

    def verify(self):
        parent = os.stat(self.root.parent, follow_symlinks=False)
        leaf = os.stat(LOCK_NAME, dir_fd=self.parent, follow_symlinks=False)
        if (_identity(parent) != self.parent_identity or not stat.S_ISDIR(parent.st_mode)
                or stat.S_IMODE(parent.st_mode) != 0o700 or parent.st_uid != os.geteuid()
                or not _private(leaf) or _identity(leaf) != self.lock_identity):
            raise InstanceError("fixed resource lock changed")

    def finish(self):
        if self.finished:
            return
        self.verify()
        leaf = os.stat(MARKER_NAME, dir_fd=self.parent, follow_symlinks=False)
        if (not _private(leaf) or _identity(leaf) != self.marker_identity
                or os.pread(self.marker, 4097, 0) != self.record):
            raise InstanceError("resource marker changed; cleanup held")
        os.unlink(MARKER_NAME, dir_fd=self.parent)
        self.finished = True
        os.fsync(self.parent)

    def close(self):
        for key in ("marker", "fd", "parent"):
            descriptor = getattr(self, key, None)
            setattr(self, key, None)
            if descriptor is not None:
                os.close(descriptor)


def send(channel, value):
    channel.sendall(json.dumps(value, separators=(",", ":")).encode() + b"\n")


def receive(channel):
    packet = bytearray()
    deadline = time.monotonic() + (channel.gettimeout() or 5)
    while len(packet) <= 32768:
        remaining = deadline-time.monotonic()
        if remaining <= 0:
            raise TimeoutError("keeper frame deadline expired")
        channel.settimeout(remaining)
        byte = channel.recv(1)
        if not byte:
            raise EOFError("keeper lifeline closed")
        if byte == b"\n":
            value = json.loads(packet)
            if not isinstance(value, dict):
                raise InstanceError("invalid keeper frame")
            return value
        packet.extend(byte)
    raise InstanceError("keeper frame too large")


def valid_request(value, root, nonce):
    return (isinstance(value, dict) and set(value) == {"root", "nonce", "action"}
            and value["root"] == str(root) and value["nonce"] == nonce
            and isinstance(value["action"], str) and value["action"] in {"down", "reap"})


def launch(manifest, spec, nonce):
    parent, child = socket.socketpair()
    root = Path(manifest["root"])
    environment = {"PATH": "/usr/bin:/bin", "HOME": str(root / "home"),
                   "TMPDIR": str(root / "tmp"), "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8"}
    try:
        parent.settimeout(spec["startup_timeout"] + spec["stop_grace"])
        send(parent, {"manifest": manifest, "spec": spec, "nonce": nonce})
        process = subprocess.Popen([sys.executable, "-E", "-s", "-m", __name__, str(child.fileno())],
            cwd=str(Path(__file__).resolve().parents[2]), env=environment, close_fds=True,
            pass_fds=(child.fileno(),), stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return process, parent
    except BaseException:
        parent.close()
        raise
    finally:
        child.close()


def request(root, nonce, action="down", timeout=30):
    root = instance_root(Path(root).parent, Path(root).name)
    record = json.loads(read_regular_text(root / OWNER_FILE))
    frame = {"root": str(root), "nonce": nonce, "action": action}
    if not valid_request(frame, root, record.get("nonce")):
        raise InstanceError("keeper request identity mismatch")
    path = root / SOCKET_FILE
    metadata = path.lstat()
    if (not stat.S_ISSOCK(metadata.st_mode) or metadata.st_uid != os.geteuid()
            or list(_identity(metadata)) != record.get("sockets", {}).get(str(path))):
        raise InstanceError("keeper endpoint changed")
    with socket.socket(socket.AF_UNIX) as channel:
        channel.settimeout(timeout)
        channel.connect(str(path))
        send(channel, frame)
        return receive(channel)


def serve(lifeline, manifest, spec, nonce):
    from .files import _open_parent, publish_runtime
    from .manifest import resource_spec, validate
    from .resources import ResourceOwner
    root = Path(manifest["root"])
    spec = resource_spec(spec)
    validate(root.parent, root.name, {}, check_ports=True)
    lock = ResourceLock(root, nonce)
    listener, owner, endpoint_parent = socket.socket(socket.AF_UNIX), None, None
    endpoint_identity, publication = None, None
    state, gateway_done, waiter = "STARTING", False, None
    stop_end = None

    def publish():
        nonlocal publication
        value = {"root": str(root), "nonce": nonce, "state": state, "live_ready": False,
                 "keeper_pid": os.getpid(), "gateway_exit_confirmed": gateway_done,
                 **owner.witness()}
        publication = publish_runtime(root, OWNER_FILE, value, expected_identity=publication,
                                      live_sockets={path: tuple(identity) for path, identity in value["sockets"].items()})
        return value

    def cleanup():
        nonlocal state
        try:
            owner.close(stop_end or time.monotonic() + spec["stop_grace"])
            state = "STOPPED" if gateway_done else "HOLD"
            return True
        except (InstanceError, OSError):
            state = "HOLD"
            return False

    def finalize():
        nonlocal endpoint_identity
        if endpoint_identity is not None:
            leaf = SOCKET_FILE.rsplit("/", 1)[1]
            metadata = os.stat(leaf, dir_fd=endpoint_parent, follow_symlinks=False)
            if (not stat.S_ISSOCK(metadata.st_mode) or metadata.st_uid != os.geteuid()
                    or _identity(metadata) != endpoint_identity):
                raise InstanceError("keeper endpoint changed; finalization held")
            os.unlink(leaf, dir_fd=endpoint_parent)
            endpoint_identity = None
        listener.close()
        lock.finish()

    try:
        endpoint_parent, name = _open_parent(root, SOCKET_FILE)
        os.fchdir(endpoint_parent)
        listener.bind(name)
        os.chmod(name, 0o600, dir_fd=endpoint_parent, follow_symlinks=False)
        endpoint_identity = _identity(os.stat(name, dir_fd=endpoint_parent, follow_symlinks=False))
        listener.listen(4)
        live = {str(root / SOCKET_FILE): endpoint_identity}
        owner = ResourceOwner(manifest, spec, time.monotonic() + spec["startup_timeout"], live_sockets=live)
        try:
            owner.start()
            state = "RUNNING"
        except (InstanceError, OSError, subprocess.SubprocessError):
            # No Gateway has been spawned; failure remains HOLD without an exit claim.
            if cleanup():
                state = "HOLD"
                finalize()
                result = publish()
                send(lifeline, result)
                return
        send(lifeline, publish())
        lifeline.settimeout(spec["stop_grace"])
        end = time.monotonic() + 600  # The parent enforces the narrower wall bound.
        while True:
            ready, _, _ = select.select([listener, *([lifeline] if lifeline else [])], [], [], max(0, end-time.monotonic()))
            if not ready:
                state = "HOLD"
                if lifeline:
                    send(lifeline, {"action": "stop", "nonce": nonce, "deadline": stop_end or time.monotonic()+spec["stop_grace"]})
                end = time.monotonic() + spec["stop_grace"]
                continue
            if lifeline in ready:
                try:
                    message = receive(lifeline)
                    if message.get("nonce") != nonce:
                        raise InstanceError("lifeline nonce mismatch")
                    candidate = message.get("deadline")
                    if (set(message) == {"nonce", "exit", "deadline"} and type(message["exit"]) is int
                            and type(candidate) in (int, float) and math.isfinite(candidate)
                            and candidate <= time.monotonic()+spec["stop_grace"]):
                        gateway_done, stop_end = True, candidate
                    else:
                        raise InstanceError("invalid Gateway exit acknowledgement")
                except (EOFError, ValueError, OSError):
                    lifeline.close()
                    lifeline = None
                    gateway_done = False
                complete = cleanup()
                if complete:
                    finalize()
                result = publish()
                if waiter:
                    send(waiter, result)
                    waiter.close()
                    waiter = None
                if complete:
                    if lifeline:
                        send(lifeline, result)
                    return
            if listener in ready:
                channel, _ = listener.accept()
                channel.settimeout(spec["stop_grace"])
                try:
                    frame = receive(channel)
                    if not valid_request(frame, root, nonce) or waiter:
                        raise InstanceError("foreign or concurrent keeper request")
                    stop_end = time.monotonic()+spec["stop_grace"]
                    if lifeline:
                        send(lifeline, {"action": "stop", "nonce": nonce, "deadline": stop_end})
                        waiter = channel
                    else:
                        complete = cleanup()
                        if complete:
                            finalize()
                        send(channel, publish())
                        channel.close()
                        if complete:
                            return
                except (ValueError, OSError, EOFError):
                    try:
                        send(channel, {"state": "HOLD", "live_ready": False})
                    except OSError:
                        pass
                    channel.close()
    except BaseException:
        if owner is None:
            finalize()  # No resource constructor has returned, so no child was spawned.
        else:
            complete = cleanup()
            state = "HOLD"
            try:
                result = publish()
                if lifeline:
                    send(lifeline, result)
            except (OSError, ValueError):
                pass
            while not complete:
                # Preserve sole handles and the crash marker while cleanup is uncertain.
                time.sleep(.1)
                complete = cleanup()
            finalize()
        raise
    finally:
        if waiter:
            waiter.close()
        listener.close()
        if endpoint_parent is not None:
            os.close(endpoint_parent)
        lock.close()


if __name__ == "__main__":
    with socket.socket(fileno=int(sys.argv[1])) as lifeline:
        try:
            packet = receive(lifeline)
            serve(lifeline, packet["manifest"], packet["spec"], packet["nonce"])
        except (InstanceError, OSError, ValueError, KeyError):
            send(lifeline, {"state": "HOLD", "live_ready": False})
