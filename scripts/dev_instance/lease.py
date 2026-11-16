"""One fixed, close-only kernel lease for the instance pool."""

import fcntl
import json
import os
from pathlib import Path
import stat
import uuid

from .files import InstanceError, _open_dir, instance_root

LEASE_NAME = ".gateway.lease"
LEASE_ENV = "AGENTDESK_TEST_LEASE"


class LeaseBusy(InstanceError):
    def __init__(self, number):
        super().__init__("instance pool is busy")
        self.errno = number


def _private_file(metadata):
    return (stat.S_ISREG(metadata.st_mode) and stat.S_IMODE(metadata.st_mode) == 0o600
            and metadata.st_uid == os.geteuid() and metadata.st_nlink == 1)


class LeaseHandle:
    def __init__(self, root, instance_id, parent_fd, fd):
        self.root, self.instance_id = root, instance_id
        self.parent_fd, self.fd = parent_fd, fd
        self.parent_identity = os.fstat(parent_fd)
        self.identity = os.fstat(fd)
        self.nonce = str(uuid.uuid4())
        self.held = False

    def witness(self):
        return {"schema_version": 1, "fd": self.fd, "device": self.identity.st_dev,
                "inode": self.identity.st_ino, "instance_id": self.instance_id,
                "root": str(self.root), "nonce": self.nonce}

    def _verify_path(self):
        if self.fd is None or self.parent_fd is None:
            raise InstanceError("lease is closed")
        parent = os.stat(self.root.parent, follow_symlinks=False)
        descriptor = os.fstat(self.fd)
        leaf = os.stat(LEASE_NAME, dir_fd=self.parent_fd, follow_symlinks=False)
        if (not stat.S_ISDIR(parent.st_mode) or stat.S_IMODE(parent.st_mode) != 0o700
                or parent.st_uid != os.geteuid()
                or (parent.st_dev, parent.st_ino) != (self.parent_identity.st_dev, self.parent_identity.st_ino)
                or not _private_file(descriptor) or not _private_file(leaf)
                or (descriptor.st_dev, descriptor.st_ino) != (self.identity.st_dev, self.identity.st_ino)
                or (leaf.st_dev, leaf.st_ino) != (self.identity.st_dev, self.identity.st_ino)):
            raise InstanceError("fixed lease identity changed")

    def verify(self):
        self._verify_path()
        if not self.held or os.pread(self.fd, 4097, 0) != self._record():
            raise InstanceError("lease witness changed")

    def _record(self):
        return (json.dumps(self.witness(), sort_keys=True, separators=(",", ":")) + "\n").encode()

    def lock(self):
        self._verify_path()
        try:
            fcntl.flock(self.fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise LeaseBusy(error.errno) from None
        self._verify_path()
        record = self._record()
        if len(record) > 4096:
            raise InstanceError("lease witness is too large")
        os.ftruncate(self.fd, 0)
        written = os.pwrite(self.fd, record, 0)
        if written != len(record):
            raise InstanceError("lease witness publication is incomplete")
        os.fsync(self.fd)
        self.held = True
        self.verify()
        return self

    def close(self):
        fd, parent_fd = self.fd, self.parent_fd
        self.fd = self.parent_fd = None
        try:
            if fd is not None:
                os.close(fd)
        finally:
            if parent_fd is not None:
                os.close(parent_fd)

    def __enter__(self):
        self.verify()
        return self

    def __exit__(self, *_error):
        self.close()


def open_candidate(root, instance_id):
    root = Path(root)
    if root != instance_root(root.parent, instance_id):
        raise InstanceError("lease instance root mismatch")
    parent_fd = _open_dir(root.parent)
    fd = None
    try:
        parent = os.fstat(parent_fd)
        if stat.S_IMODE(parent.st_mode) != 0o700 or parent.st_uid != os.geteuid():
            raise InstanceError("instance pool must be private")
        fd = os.open(LEASE_NAME, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK,
                     0o600, dir_fd=parent_fd)
        if fd < 3 or not _private_file(os.fstat(fd)):
            raise InstanceError("invalid fixed lease file")
        handle = LeaseHandle(root, instance_id, parent_fd, fd)
        handle._verify_path()
        return handle
    except BaseException:
        if fd is not None:
            os.close(fd)
        os.close(parent_fd)
        raise


def acquire(root, instance_id):
    handle = open_candidate(root, instance_id)
    try:
        return handle.lock()
    except BaseException:
        handle.close()
        raise
