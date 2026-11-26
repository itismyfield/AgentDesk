"""Channel lease for continuous E2E: one fixed-inode, close-only flock per test parent pair.

The key is guild + sorted parent ids + bot set, so cells, profiles and child threads share one
lock. The owner JSON written into the file is diagnostic; only the kernel lock decides ownership.
"""

import fcntl
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess

LEASE_ENV = "AGENTDESK_E2E_CHANNEL_LEASE"


class LeaseError(Exception):
    pass


class LeaseBusy(LeaseError):
    pass


def resource_key(resource):
    guild, parents, bots = (resource.get(key) if isinstance(resource, dict) else None
                            for key in ("guild", "parents", "bot_set"))
    ids = lambda values: isinstance(values, list) and values and all(  # noqa: E731
        isinstance(value, str) and value.isdigit() for value in values)
    if not (ids([guild]) and ids(parents) and len(set(parents)) == 2 == len(parents) and ids(bots)):
        raise LeaseError("resource needs a guild, two distinct parent ids and the bot set")
    return hashlib.sha256(json.dumps([guild, sorted(parents), sorted(set(bots))]).encode()).hexdigest()[:32]


def _check_private_dir(path):
    info = os.stat(path, follow_symlinks=False)
    if not stat.S_ISDIR(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o700 or info.st_uid != os.geteuid():
        raise LeaseError(f"{path} must be a private directory")


def private_dir(path):
    path = Path(path)
    for level in reversed([path, *path.parents]):
        if not level.exists():
            level.mkdir(mode=0o700, exist_ok=True)  # Directories this creates are private; a racer may win.
    _check_private_dir(path)
    return path


class ChannelLease:
    def __init__(self, path, key, fd):
        self.path, self.key, self.fd = path, key, fd
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o600
                or info.st_uid != os.geteuid() or info.st_nlink != 1):
            raise LeaseError("invalid channel lock file")
        self.identity = info

    def same_file(self):
        leaf = os.stat(self.path, follow_symlinks=False)
        return (leaf.st_dev, leaf.st_ino) == (self.identity.st_dev, self.identity.st_ino)

    def publish_owner(self, owner):
        record = (json.dumps(owner, sort_keys=True) + "\n").encode()
        os.ftruncate(self.fd, 0)
        os.pwrite(self.fd, record, 0)

    def spawn(self, argv, run_id, **kwargs):
        """Run one driver that inherits only this lease FD, named by a device/inode witness."""
        witness = {"fd": self.fd, "device": self.identity.st_dev, "inode": self.identity.st_ino,
                   "key": self.key, "run_id": run_id}
        env = {**os.environ, LEASE_ENV: json.dumps(witness)}
        return subprocess.run(argv, env=env, close_fds=True, pass_fds=(self.fd,), **kwargs)

    def close(self):
        fd, self.fd = self.fd, None
        if fd is not None:
            os.close(fd)

    def __enter__(self):
        return self

    def __exit__(self, *_error):
        self.close()


def acquire(state_dir, resource):
    """Take the lock without waiting; a holder is never preempted, expired or unlinked."""
    key = resource_key(resource)
    path = private_dir(Path(private_dir(state_dir)) / "locks") / f"{key}.lock"
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        lease = ChannelLease(path, key, fd)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise LeaseBusy(key) from None
        if not lease.same_file():
            raise LeaseError("channel lock file was replaced")
        return lease
    except BaseException:
        os.close(fd)
        raise


def is_held(state_dir, resource):
    """Probe current kernel ownership without creating files or reading the last-owner record."""
    key = resource_key(resource)
    root = Path(state_dir)
    path = root / "locks" / f"{key}.lock"
    try:
        _check_private_dir(root)
        _check_private_dir(path.parent)
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    except FileNotFoundError:
        return False
    try:
        lease = ChannelLease(path, key, fd)
        try:
            fcntl.flock(fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
            held = False
        except BlockingIOError:
            held = True
        if not lease.same_file():
            raise LeaseError("channel lock file was replaced")
        return held
    finally:
        os.close(fd)


def inherited(environ=os.environ):
    """Driver side: the witnessed FD is the lock file the runner locked."""
    witness = json.loads(environ[LEASE_ENV])
    info = os.fstat(witness["fd"])
    if (info.st_dev, info.st_ino) != (witness["device"], witness["inode"]):
        raise LeaseError("inherited channel lease does not match its witness")
    return witness
