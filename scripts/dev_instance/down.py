"""Remove a canonical instance only after its existing owners confirm shutdown."""

import json
import errno
import os
from pathlib import Path
import socket
import stat
import subprocess
import time
import uuid

from . import host, keeper, lifecycle
from .files import InstanceError, _open_dir, _open_parent, credential_alias, instance_root, read_regular_text, safe_absolute
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


def _scan(descriptor, sockets=None, prefix=(), root=None):
    result = {}
    for name in os.listdir(descriptor):
        relative = prefix + (name,)
        metadata = os.stat(name, dir_fd=descriptor, follow_symlinks=False)
        if metadata.st_uid != os.geteuid():
            raise InstanceError("cleanup entry has a foreign owner")
        token = (metadata.st_dev, metadata.st_ino)
        if credential_alias(root, relative, descriptor, metadata):
            result[relative] = (token, True)
        elif stat.S_ISDIR(metadata.st_mode):
            child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            try:
                if token != _inode(os.fstat(child)):
                    raise InstanceError("cleanup directory changed")
                result[relative] = (token, False)
                result.update(_scan(child, sockets, relative, root))
            finally:
                os.close(child)
        elif stat.S_ISREG(metadata.st_mode) and metadata.st_nlink == 1:
            result[relative] = (token, False)
        elif stat.S_ISSOCK(metadata.st_mode) and (sockets or {}).get(relative) == token:
            result[relative] = (token, False)
        else:
            raise InstanceError("cleanup refuses links and unowned special entries")
    return result


def _inode(metadata):
    return metadata.st_dev, metadata.st_ino


def _tmux_witness(root):
    if not safe_absolute(root/'runtime/tmux/socket').exists():
        return {}
    parent, name = _open_parent(root, 'runtime/tmux/socket', private=True)
    try:
        try:
            metadata = os.stat(name, dir_fd=parent, follow_symlinks=False)
        except FileNotFoundError:
            return {}
        if not stat.S_ISSOCK(metadata.st_mode) or metadata.st_uid != os.geteuid() or metadata.st_nlink != 1:
            raise InstanceError('invalid instance tmux socket')
        return {str(root/'runtime/tmux/socket'): _inode(metadata)}
    finally:
        os.close(parent)


def _close_tmux(manifest, witness, deadline):
    if not witness:
        return
    root = Path(manifest['root'])
    parent, name = _open_parent(root, 'runtime/tmux/socket', private=True)
    previous = os.open('.', os.O_RDONLY | os.O_DIRECTORY)
    try:
        # Relative addressing keeps the verified directory pinned across short Unix socket paths.
        os.fchdir(parent)
        expected = witness[str(root/'runtime/tmux/socket')]
        def unchanged():
            metadata = os.stat(name, dir_fd=parent, follow_symlinks=False)
            if (not stat.S_ISSOCK(metadata.st_mode) or metadata.st_uid != os.geteuid()
                    or metadata.st_nlink != 1 or _inode(metadata) != expected):
                raise InstanceError('instance tmux socket changed')
        def connected():
            unchanged()
            with socket.socket(socket.AF_UNIX) as channel:
                channel.settimeout(max(.001, min(.1, deadline-time.monotonic())))
                try:
                    channel.connect(name)
                    return True
                except OSError as error:
                    if error.errno == errno.ECONNREFUSED:
                        return False
                    raise lifecycle.CleanupPending(None) from None
        try:
            alive = connected()
        except FileNotFoundError:
            return
        if alive:
            from .manifest import child_environment
            environment = child_environment(manifest, {})
            def control(*args):
                unchanged()
                try:
                    result = subprocess.run(['tmux', '-S', name, *args], env=environment,
                                            stdin=subprocess.DEVNULL, capture_output=True, text=True,
                                            timeout=max(.001, deadline-time.monotonic()))
                except (OSError, subprocess.TimeoutExpired):
                    raise lifecycle.CleanupPending(None) from None
                if result.returncode:
                    raise lifecycle.CleanupPending(None)
                return result.stdout
            sessions = control('list-sessions', '-F', '#{session_name}').splitlines()
            prefix = manifest['hosts']['tmux']['prefix']
            if not sessions or any(name != prefix and not name.startswith(prefix+'-') for name in sessions):
                raise lifecycle.CleanupPending(None)
            pids = control('list-panes', '-a', '-F', '#{pane_pid}').splitlines()
            pids += control('display-message', '-p', '#{pid}').splitlines()
            if not pids or any(not pid.isdecimal() or int(pid) <= 1 or int(pid) == os.getpid() for pid in pids):
                raise lifecycle.CleanupPending(None)
            control('kill-server')
            for pid in set(map(int, pids)):
                _wait_gone(pid, deadline)
            try:
                if connected():
                    raise lifecycle.CleanupPending(None)
            except FileNotFoundError:
                return
        unchanged()
        os.unlink(name, dir_fd=parent)
    finally:
        os.fchdir(previous)
        os.close(previous)
        os.close(parent)


def _erase(descriptor, entries, prefix=(), root=None):
    for name in os.listdir(descriptor):
        relative = prefix + (name,)
        metadata = os.stat(name, dir_fd=descriptor, follow_symlinks=False)
        identity, approved_alias = entries.get(relative, (None, False))
        if _inode(metadata) != identity:
            raise InstanceError("cleanup tree changed before deletion")
        if approved_alias:
            # Scan approves the target; erasure only rechecks the witnessed link itself.
            if (relative != ('config', 'credential') or not stat.S_ISLNK(metadata.st_mode)
                    or metadata.st_uid != os.geteuid() or metadata.st_nlink != 1):
                raise InstanceError("approved cleanup alias changed")
            os.unlink(name, dir_fd=descriptor)
        elif stat.S_ISDIR(metadata.st_mode):
            child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            try:
                if _inode(os.fstat(child)) != identity:
                    raise InstanceError("cleanup directory changed before deletion")
                _erase(child, entries, relative, root)
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


def _retired_owner(owner):
    try:
        if (owner.get('state') not in {'RUNNING', 'HOLD'}
                or str(uuid.UUID(owner['nonce'])) != owner['nonce']
                or type(owner.get('keeper_pid')) is not int or owner['keeper_pid'] <= 1
                or type(owner.get('resources')) is not list or len(owner['resources']) != 2
                or {child['kind'] for child in owner['resources']} != {'postgres', 'herdr'}):
            return False
        pids = [owner['keeper_pid']] + [child['pid'] for child in owner['resources']]
        return all(type(pid) is int and pid > 1 and not lifecycle._pid_alive(pid) for pid in pids)
    except (KeyError, TypeError, ValueError):
        return False


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
            try:
                owner = keeper.request(root, owner['nonce'], 'down', timeout=max(.01, deadline-time.monotonic()))
            except EOFError:
                raise lifecycle.CleanupPending(None) from None
        if owner.get('root') != str(root):
            raise lifecycle.CleanupPending(None)
        if (owner.get('known_resources_closed') is not True
                or owner.get('gateway_exit_confirmed') is not True):
            if not _retired_owner(owner):
                raise lifecycle.CleanupPending(None)
            # A dead keeper cannot publish fresh exits; deletion still requires the kernel lease below.
            owner = {**owner, 'reaped':True, 'resources':[
                {**child, 'state':'STOPPED', 'exit_confirmed':True} for child in owner['resources']]}

        _wait_gone(owner.get('keeper_pid'), deadline)
        if type(owner.get('resources')) is not list:
            raise lifecycle.CleanupPending(None)
        for child in owner['resources']:
            if type(child) is not dict or (not owner.get('reaped') and type(child.get('exitcode')) is not int):
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
        tmux = _tmux_witness(root)
        sockets.update(tmux)
        manifest = validate(base, instance_id, ambient, check_ports=False, live_sockets=sockets, check_head=False, allow_missing_directories=True)
        relative_sockets = {Path(path).relative_to(root).parts: token for path, token in sockets.items()}
    except (KeyError, TypeError, AttributeError):
        raise InstanceError("invalid cleanup ownership witness") from None
    descriptor = _open_dir(root)
    parent = _open_dir(root.parent)
    lease = None
    try:
        root_identity = _inode(os.fstat(descriptor))
        _scan(descriptor, relative_sockets, root=root)
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
        _close_tmux(manifest, tmux, deadline)
        validate(base, instance_id, ambient, check_ports=False, check_head=False, allow_missing_directories=True)
        entries = _scan(descriptor, root=root)
        if (root_identity != _inode(os.stat(root.name, dir_fd=parent, follow_symlinks=False))
                or root_identity != _inode(os.stat(root, follow_symlinks=False))):
            raise InstanceError("cleanup root changed")
        _erase(descriptor, entries, root=root)
        os.rmdir(root.name, dir_fd=parent)
        os.fsync(parent)
        return {'status':'REMOVED', 'instance_id':instance_id, 'root':str(root),
                'known_resources_closed':True, 'credential_files':0,
                'resources':(owner or {}).get('resources', []), 'reaped_owner':(owner or {}).get('reaped', False)}
    finally:
        if lease is not None:
            lease.close()
        os.close(descriptor)
        os.close(parent)
