#!/usr/bin/env python3
"""Replay existing lease defects and proposed FD assumptions in temporary files."""
import argparse
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import selectors
import subprocess
import sys
import tempfile
import threading

DRIVER = r'''
import json, os, sys
fd, ready, stop = map(int, sys.argv[1:4])
path = sys.argv[4]
try:
    stat = os.fstat(fd)
    inherited = True
    expected = os.stat(path)
    identity_matches = (stat.st_dev, stat.st_ino) == (expected.st_dev, expected.st_ino)
except OSError:
    inherited = False
    identity_matches = False
os.write(ready, (json.dumps({"inherited": inherited, "identity_matches": identity_matches, "pid": os.getpid()})+"\n").encode())
os.read(stop, 1)
if inherited:
    os.close(fd)
os.write(ready, b"closed\n")
'''
WRAPPER = r'''
import fcntl, os, subprocess, sys
path, ready, stop, inherit, driver = sys.argv[1:]
ready, stop = int(ready), int(stop)
fd = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
childfd = fd if inherit == "yes" else -1
fds = (ready, stop, fd) if inherit == "yes" else (ready, stop)
subprocess.Popen([sys.executable, "-c", driver, str(childfd), str(ready), str(stop), path], pass_fds=fds)
command = sys.stdin.buffer.read(1)
if command == b"u":
    fcntl.flock(fd, fcntl.LOCK_UN)
'''


def read_line(fd):
    data = bytearray()
    with selectors.DefaultSelector() as selector:
        selector.register(fd, selectors.EVENT_READ)
        while not data.endswith(b'\n'):
            if not selector.select(5):
                raise TimeoutError('local model child did not reply')
            part = os.read(fd, 1)
            if not part:
                raise RuntimeError('local model child exited before reply')
            data.extend(part)
    return data.decode().strip()


def try_lock(path):
    fd = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return False
        return True
    finally:
        os.close(fd)


def fd_case(path, inherit=True, unlock=False):
    read_ready, write_ready = os.pipe()
    read_stop, write_stop = os.pipe()
    wrapper = subprocess.Popen(
        [sys.executable, '-c', WRAPPER, str(path), str(write_ready), str(read_stop),
         'yes' if inherit else 'no', DRIVER], stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        pass_fds=(write_ready, read_stop))
    os.close(write_ready)
    os.close(read_stop)
    try:
        ready = json.loads(read_line(read_ready))
        before = try_lock(path)
        if unlock:
            wrapper.stdin.write(b'u')
            wrapper.stdin.flush()
        else:
            wrapper.kill()
        wrapper.wait(timeout=5)
        after = try_lock(path)
        os.write(write_stop, b'x')
        assert read_line(read_ready) == 'closed'
        final = try_lock(path)
        return {'inherited': ready['inherited'], 'identity_matches': ready['identity_matches'], 'competing_before_parent_exit': before,
                'competing_after_parent_exit': after, 'competing_after_child_close': final,
                'parent_exit': wrapper.returncode}
    finally:
        if wrapper.poll() is None:
            wrapper.kill()
            wrapper.wait(timeout=5)
        wrapper.stdin.close()
        wrapper.stderr.close()
        os.close(write_stop)
        os.close(read_ready)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    base = Path(__file__).resolve().parents[3]
    source = base / 'scripts/e2e/tui_relay/lease.py'
    before_hash = hashlib.sha256(source.read_bytes()).hexdigest()
    spec = importlib.util.spec_from_file_location('existing_lease', source)
    lease = importlib.util.module_from_spec(spec)
    sys.dont_write_bytecode = True
    spec.loader.exec_module(lease)
    results = []

    def result(name, observed, expected):
        passed = observed == expected
        results.append({'name': name, 'observed': observed, 'expected': expected,
                        'passed': passed})
        print(json.dumps(results[-1]), flush=True)

    with tempfile.TemporaryDirectory(prefix='i8120-lease-replay-') as temp:
        root = Path(temp)
        ttl_path = root / 'existing-ttl.lease'
        with lease.acquire('A', path=ttl_path, ttl_s=0):
            with lease.acquire('B', path=ttl_path, ttl_s=0):
                result('current_TTL_allows_two_live_contexts', True, True)
        barrier = threading.Barrier(2)
        entered = threading.Barrier(2)
        original_read = lease._read_lease
        errors, writers = [], []
        race_path = root / 'existing-race.lease'

        def read_together(path):
            value = original_read(path)
            barrier.wait(timeout=5)
            return value

        def race_writer(run):
            try:
                with lease.acquire(run, path=race_path):
                    writers.append(run)
                    entered.wait(timeout=5)
            except BaseException as error:
                errors.append(type(error).__name__)

        lease._read_lease = read_together
        threads = [threading.Thread(target=race_writer, args=(r,)) for r in ('A', 'B')]
        try:
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join(timeout=10)
            result('current_read_then_truncate_has_two_writers', len(writers), 2)
            result('current_race_replay_errors', errors, [])
        finally:
            lease._read_lease = original_read
        stale_path = root / 'existing-stale-unlink.lease'
        old = lease.acquire('A', path=stale_path, ttl_s=0)
        new = lease.acquire('B', path=stale_path, ttl_s=0)
        old.__enter__()
        new.__enter__()
        old.__exit__(None, None, None)
        result('current_old_owner_unlinks_successor', stale_path.exists(), False)
        new.__exit__(None, None, None)
        held = os.open(root / 'fixed-single.lease', os.O_CREAT | os.O_RDWR, 0o600)
        try:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result('proposed_nonblocking_flock_refuses_second_writer',
                   try_lock(root / 'fixed-single.lease'), False)
        finally:
            os.close(held)
        result('proposed_close_only_reacquires_after_last_holder',
               try_lock(root / 'fixed-single.lease'), True)
        normal = fd_case(root / 'inherited.lease')
        result('proposed_parent_SIGKILL_preserves_child_lock', normal,
               {'inherited': True, 'identity_matches': True, 'competing_before_parent_exit': False,
                'competing_after_parent_exit': False, 'competing_after_child_close': True,
                'parent_exit': -9})
        missing = fd_case(root / 'missing-fd.lease', inherit=False)
        result('mutant_missing_inheritance_releases_on_parent_crash', missing,
               {'inherited': False, 'identity_matches': False, 'competing_before_parent_exit': False,
                'competing_after_parent_exit': True, 'competing_after_child_close': True,
                'parent_exit': -9})
        unlock = fd_case(root / 'explicit-unlock.lease', unlock=True)
        result('mutant_LOCK_UN_releases_while_child_has_FD', unlock,
               {'inherited': True, 'identity_matches': True, 'competing_before_parent_exit': False,
                'competing_after_parent_exit': True, 'competing_after_child_close': True,
                'parent_exit': 0})
    result('production_source_bytes_unchanged', hashlib.sha256(source.read_bytes()).hexdigest(), before_hash)
    args.out.write_text(json.dumps({'source_sha256': before_hash, 'results': results}, indent=2) + '\n')
    return 0 if all(row['passed'] for row in results) else 1


if __name__ == '__main__':
    raise SystemExit(main())
