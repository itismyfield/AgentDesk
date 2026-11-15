"""Remove a canonical instance only after its existing owners confirm shutdown."""

import json
import os
from pathlib import Path
import stat
import time

from . import host, keeper, lifecycle
from .files import InstanceError, _open_dir, instance_root, read_regular_text
from .lease import LeaseBusy, acquire
from .manifest import validate


def _record(root, relative):
    try:
        value = json.loads(read_regular_text(root / relative))
    except FileNotFoundError:
        return None
    if type(value) is not dict:
        raise InstanceError("invalid cleanup record")
    return value


def _scan(descriptor, sockets=None, prefix=()):
    result = {}
    for name in os.listdir(descriptor):
        relative = prefix + (name,)
        metadata = os.stat(name, dir_fd=descriptor, follow_symlinks=False)
        if metadata.st_uid != os.geteuid():
            raise InstanceError("cleanup entry has a foreign owner")
        token = (metadata.st_dev, metadata.st_ino)
        if stat.S_ISDIR(metadata.st_mode):
            child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            try:
                if token != _inode(os.fstat(child)):
                    raise InstanceError("cleanup directory changed")
                result[relative] = token
                result.update(_scan(child, sockets, relative))
            finally:
                os.close(child)
        elif stat.S_ISREG(metadata.st_mode) and metadata.st_nlink == 1:
            result[relative] = token
        elif stat.S_ISSOCK(metadata.st_mode) and (sockets or {}).get(relative) == token:
            result[relative] = token
        else:
            raise InstanceError("cleanup refuses links and unowned special entries")
    return result


def _inode(metadata):
    return metadata.st_dev, metadata.st_ino


def _erase(descriptor, entries, prefix=()):
    for name in os.listdir(descriptor):
        relative = prefix + (name,)
        metadata = os.stat(name, dir_fd=descriptor, follow_symlinks=False)
        if _inode(metadata) != entries.get(relative):
            raise InstanceError("cleanup tree changed before deletion")
        if stat.S_ISDIR(metadata.st_mode):
            child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            try:
                if _inode(os.fstat(child)) != entries[relative]:
                    raise InstanceError("cleanup directory changed before deletion")
                _erase(child, entries, relative)
            finally:
                os.close(child)
            os.rmdir(name, dir_fd=descriptor)
        elif stat.S_ISREG(metadata.st_mode) and metadata.st_nlink == 1:
            os.unlink(name, dir_fd=descriptor)
        else:
            raise InstanceError("cleanup entry changed before deletion")


def _wait_gone(pid, deadline):
    if type(pid) is not int or pid <= 1 or pid == os.getpid():
        raise lifecycle.CleanupPending(None)
    while lifecycle._pid_alive(pid):
        if time.monotonic() >= deadline:
            raise lifecycle.CleanupPending(None)
        time.sleep(.02)


def _quiesce(manifest, owner, run, deadline):
    root = Path(manifest['root'])
    if owner is None and (root / host.OWNER_FILE).exists():
        raise lifecycle.CleanupPending(None)
    if run is not None:
        if (run.get('instance_id') != root.name or type(run.get('supervisor_pid')) is not int
                or run['supervisor_pid'] <= 1):
            raise InstanceError("cleanup run identity mismatch")
        if not lifecycle._pid_alive(run['supervisor_pid']):
            lifecycle.cleanup_orphan(manifest, run, min(5, max(.01, deadline-time.monotonic())))
    if owner is not None:
        if owner.get('root') != str(root) or not isinstance(owner.get('nonce'), str) or not owner['nonce']:
            raise InstanceError("cleanup owner identity mismatch")
        if (root / keeper.SOCKET_FILE).exists():
            owner = keeper.request(root, owner['nonce'], 'down', timeout=max(.01, deadline-time.monotonic()))
        if (owner.get('root') != str(root) or owner.get('known_resources_closed') is not True
                or owner.get('gateway_exit_confirmed') is not True):
            raise lifecycle.CleanupPending(None)
        _wait_gone(owner.get('keeper_pid'), deadline)
        if type(owner.get('resources')) is not list:
            raise lifecycle.CleanupPending(None)
        for child in owner['resources']:
            if type(child) is not dict or type(child.get('exitcode')) is not int:
                raise lifecycle.CleanupPending(None)
            _wait_gone(child.get('pid'), deadline)
    if run is not None:
        _wait_gone(run.get('pid'), deadline)
    return owner


def remove(base, instance_id, ambient):
    canonical = Path.home() / '.adk/dev/instances'
    if Path(base) != canonical:
        raise InstanceError("cleanup base must be the canonical instance pool")
    root = instance_root(base, instance_id)
    owner = _record(root, keeper.OWNER_FILE)
    run = _record(root, 'runtime/instance-run.json')
    try:
        sockets = {path: tuple(token) for path, token in (owner or {}).get('sockets', {}).items()
                   if Path(path).exists()}
        manifest = validate(base, instance_id, ambient, check_ports=False, live_sockets=sockets)
        relative_sockets = {Path(path).relative_to(root).parts: token for path, token in sockets.items()}
    except (KeyError, TypeError, AttributeError):
        raise InstanceError("invalid cleanup ownership witness") from None
    descriptor = _open_dir(root)
    parent = _open_dir(root.parent)
    lease = None
    try:
        root_identity = _inode(os.fstat(descriptor))
        _scan(descriptor, relative_sockets)
        deadline = time.monotonic() + lifecycle.MAX_STOP_GRACE_SECONDS
        owner = _quiesce(manifest, owner, run, deadline)
        while lease is None:
            try:
                lease = acquire(root, instance_id)
            except LeaseBusy:
                if time.monotonic() >= deadline:
                    raise lifecycle.CleanupPending(None) from None
                time.sleep(.02)
        # The shared close-only lease excludes new up while every existing owner is gone.
        if (root.parent / keeper.MARKER_NAME).exists() or (root.parent / keeper.MARKER_NAME).is_symlink():
            raise lifecycle.CleanupPending(None)
        validate(base, instance_id, ambient, check_ports=False)
        entries = _scan(descriptor)
        if (root_identity != _inode(os.stat(root.name, dir_fd=parent, follow_symlinks=False))
                or root_identity != _inode(os.stat(root, follow_symlinks=False))):
            raise InstanceError("cleanup root changed")
        _erase(descriptor, entries)
        os.rmdir(root.name, dir_fd=parent)
        os.fsync(parent)
        return {'status':'REMOVED', 'instance_id':instance_id, 'root':str(root),
                'known_resources_closed':True, 'credential_files':0,
                'resources':(owner or {}).get('resources', [])}
    finally:
        if lease is not None:
            lease.close()
        os.close(descriptor)
        os.close(parent)
