"""Kernel lease and bounded, non-ready instance supervisor effects."""

import concurrent.futures
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import queue
import resource
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import uuid
from unittest import mock

from scripts.dev_instance import lease
from scripts.dev_instance.files import InstanceError
from scripts.dev_instance import lifecycle
from tests import test_dev_instance as declarations


class InstanceLeaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="instance-lease-")
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name).resolve()
        self.base.chmod(0o700)
        self.instance_id = str(uuid.uuid4())
        self.root = self.base / self.instance_id
        self.root.mkdir(mode=0o700)

    def test_exclusive_lease_is_real_and_close_only(self):
        with lease.acquire(self.root, self.instance_id) as owner:
            path = self.base / lease.LEASE_NAME
            identity = path.stat()
            self.assertEqual(json.loads(path.read_text()), owner.witness())
            self.assertFalse(os.get_inheritable(owner.fd))
            with self.assertRaises(lease.LeaseBusy):
                lease.acquire(self.root, self.instance_id)
        with lease.acquire(self.root, self.instance_id):
            self.assertEqual(path.stat().st_ino, identity.st_ino)

    def test_private_fixed_path_rejects_links_modes_and_foreign_roots(self):
        path = self.base / lease.LEASE_NAME
        foreign = self.base / "foreign"
        foreign.write_text("preserve foreign bytes")
        path.symlink_to(foreign)
        with self.assertRaises((InstanceError, OSError)):
            lease.acquire(self.root, self.instance_id)
        self.assertEqual(foreign.read_text(), "preserve foreign bytes")
        path.unlink()
        os.link(foreign, path)
        with self.assertRaises(InstanceError):
            lease.acquire(self.root, self.instance_id)
        path.unlink()
        path.write_text("existing")
        path.chmod(0o644)
        with self.assertRaises(InstanceError):
            lease.acquire(self.root, self.instance_id)
        path.chmod(0o600)
        self.base.chmod(0o755)
        with self.assertRaises(InstanceError):
            lease.acquire(self.root, self.instance_id)
        self.base.chmod(0o700)
        with self.assertRaises(InstanceError):
            lease.acquire(self.root, str(uuid.uuid4()))

    def test_witness_drift_and_replaced_inode_refuse_without_deletion(self):
        with lease.acquire(self.root, self.instance_id) as owner:
            path = self.base / lease.LEASE_NAME
            record = owner.witness()
            record["nonce"] = str(uuid.uuid4())
            path.write_text(json.dumps(record))
            with self.assertRaises(InstanceError):
                owner.verify()
        with lease.acquire(self.root, self.instance_id) as owner:
            path.rename(self.base / "old-owned-inode")
            path.write_text("foreign replacement remains")
            path.chmod(0o600)
            with self.assertRaises(InstanceError):
                owner.verify()
        self.assertEqual(path.read_text(), "foreign replacement remains")

    def test_publication_failure_releases_only_its_fd_and_keeps_inode(self):
        with mock.patch.object(lease.os, "pwrite", side_effect=OSError("synthetic write failure")):
            with self.assertRaises(OSError):
                lease.acquire(self.root, self.instance_id)
        path = self.base / lease.LEASE_NAME
        inode = path.stat().st_ino
        with lease.acquire(self.root, self.instance_id):
            self.assertEqual(path.stat().st_ino, inode)

    def test_parent_close_does_not_unlock_inherited_child_description(self):
        owner = lease.acquire(self.root, self.instance_id)
        lease_path = self.base / lease.LEASE_NAME
        original_inode = lease_path.stat().st_ino
        child = subprocess.Popen(
            [sys.executable, "-c", "import os,sys;os.fstat(int(sys.argv[1]));print('held',flush=True);sys.stdin.read()", str(owner.fd)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            pass_fds=(owner.fd,), text=True, env={"PATH": "/usr/bin:/bin"})
        try:
            self.assertEqual(child.stdout.readline(), "held\n")
            owner.close()
            with self.assertRaises(lease.LeaseBusy):
                lease.acquire(self.root, self.instance_id)
        finally:
            owner.close()
            child.communicate(input="stop", timeout=5)
        self.assertEqual(child.returncode, 0)
        try:
            next_owner = lease.acquire(self.root, self.instance_id)
        except lease.LeaseBusy:
            self.fail("last inherited child exited but the fixed-inode lease remained busy")
        with next_owner:
            self.assertEqual(lease_path.stat().st_ino, original_inode)
        print("LAST_HOLDER_REACQUIRE " + json.dumps({
            "child_exit": child.returncode, "inode_before": original_inode,
            "inode_after": lease_path.stat().st_ino, "reacquired": True,
            "release": "parent close then child cooperative stdin exit", "lease_unlink": False}), flush=True)

    def test_1000_independent_opens_have_one_held_winner_and_999_busy(self):
        count, workers = 1000, 32
        original_limits = resource.getrlimit(resource.RLIMIT_NOFILE)
        needed = 2 * count + 128
        hard = original_limits[1]
        self.assertTrue(hard == resource.RLIM_INFINITY or hard >= needed,
                        "test process hard FD limit cannot support the explicit 1000-open bound")
        resource.setrlimit(resource.RLIMIT_NOFILE, (max(original_limits[0], needed), hard))
        candidates = []
        gate, release = threading.Event(), threading.Event()
        first_lock = threading.Lock()
        first = []
        results = queue.Queue()
        worker_ready = queue.Queue()

        def attempt(index, candidate):
            if index < workers:
                worker_ready.put((index, threading.get_ident()))
            gate.wait()
            owns_barrier = False
            try:
                candidate.lock()
                with first_lock:
                    if not first:
                        first.append(index)
                        owns_barrier = True
                results.put((index, "winner"))
                if owns_barrier:
                    if not release.wait(30):
                        results.put((index, "barrier_timeout"))
            except lease.LeaseBusy as error:
                results.put((index, "busy" if error.errno == errno.EWOULDBLOCK else f"errno:{error.errno}"))
            except BaseException as error:
                results.put((index, type(error).__name__))
            finally:
                candidate.close()

        try:
            for _ in range(count):
                candidates.append(lease.open_candidate(self.root, self.instance_id))
            self.assertEqual(len({candidate.fd for candidate in candidates}), count)
            self.assertEqual(len({candidate.parent_fd for candidate in candidates}), count)
            self.assertEqual(len(candidates), count)
            original_inode = (self.base / lease.LEASE_NAME).stat().st_ino
            with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as executor:
                futures = []
                try:
                    for index, candidate in enumerate(candidates):
                        futures.append(executor.submit(attempt, index, candidate))
                    ready = [worker_ready.get(timeout=10) for _ in range(workers)]
                    self.assertEqual({index for index, _ in ready}, set(range(workers)))
                    self.assertEqual(len({identity for _, identity in ready}), workers)
                    gate.set()
                    actual = [results.get(timeout=20) for _ in range(count)]
                    self.assertEqual({index for index, _ in actual}, set(range(count)))
                    self.assertEqual(len(actual), count)
                    outcomes = [outcome for _, outcome in actual]
                    print("LEASE_1000 " + json.dumps({
                        "selected": count, "executed": len(actual), "winner": outcomes.count("winner"),
                        "busy": outcomes.count("busy"), "busy_errno": errno.EWOULDBLOCK,
                        "busy_errno_name": "EWOULDBLOCK", "other": sum(value not in {"winner", "busy"} for value in outcomes),
                        "duplicate_attempts": len(actual) - len({index for index, _ in actual}),
                        "independent_opens": len(candidates), "workers": workers, "open_gate_prepared": count,
                        "worker_ready_ids": sorted(index for index, _ in ready), "worker_ready_count": len(ready),
                        "worker_thread_ids": sorted(identity for _, identity in ready),
                        "winner_held_until_results": len(actual) if first and not release.is_set() else 0,
                        "fd_limit_before": original_limits, "fd_limit_applied": resource.getrlimit(resource.RLIMIT_NOFILE),
                        "executed_ids": sorted(index for index, _ in actual)}), flush=True)
                    self.assertEqual(outcomes.count("winner"), 1)
                    self.assertEqual(outcomes.count("busy"), 999)
                    self.assertEqual(set(outcomes), {"winner", "busy"})
                    with self.assertRaises(lease.LeaseBusy):
                        lease.acquire(self.root, self.instance_id)
                finally:
                    gate.set()
                    release.set()
                for future in futures:
                    future.result(timeout=5)
            self.assertTrue(results.empty())
            with lease.acquire(self.root, self.instance_id):
                self.assertEqual((self.base / lease.LEASE_NAME).stat().st_ino, original_inode)
        finally:
            release.set()
            for candidate in candidates:
                candidate.close()
            resource.setrlimit(resource.RLIMIT_NOFILE, original_limits)
        self.assertEqual(resource.getrlimit(resource.RLIMIT_NOFILE), original_limits)


class InstanceSupervisorTests(unittest.TestCase):
    def setUp(self):
        self.fixture = declarations.InstanceDeclarationTests(methodName="runTest")
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.fixture.base.chmod(0o700)
        self.manifest = self.fixture.prepare()
        self.root = Path(self.manifest["root"])
        exclude = self.fixture.worktree / ".git/info/exclude"
        exclude.write_text(exclude.read_text() + "\ntarget/\n")
        self.binary = self.fixture.worktree / "target/debug/agentdesk"
        self.binary.parent.mkdir(parents=True)

    def binary_fixture(self, *, wait=False, ignore_term=False, exit_code=0):
        script = f'''#!{sys.executable}
import fcntl,json,os,signal,sys,time
from pathlib import Path
root=Path(os.environ["AGENTDESK_ROOT_DIR"])
witness=json.loads(os.environ["AGENTDESK_TEST_LEASE"])
fd=witness["fd"]
metadata=os.fstat(fd)
record=json.loads(os.pread(fd,4096,0))
if {ignore_term!r}: signal.signal(signal.SIGTERM,signal.SIG_IGN)
capture={{"pid":os.getpid(),"argv":sys.argv[1:],"environment":dict(os.environ),"cwd":os.getcwd(),"witness":witness,"record":record,"device":metadata.st_dev,"inode":metadata.st_ino,"inherited":os.get_inheritable(fd)}}
(root/"child-capture.json").write_text(json.dumps(capture))
deadline=time.monotonic()+10
while {wait!r} and not (root/"cooperative-stop").exists() and time.monotonic()<deadline: time.sleep(.01)
sys.exit({exit_code})
'''
        self.binary.write_text(script)
        self.binary.chmod(0o700)
        return hashlib.sha256(self.binary.read_bytes()).hexdigest()

    def supervise(self, digest, **kwargs):
        arguments = {"head": self.manifest["head"], "binary_sha256": digest,
                     "wall_timeout": 5, "stop_grace": 0.1}
        arguments.update(kwargs)
        return lifecycle.supervise(self.fixture.base, self.fixture.instance_id,
                                   {"AGENTDESK_TOKEN": declarations.SENTINEL}, **arguments)

    def test_actual_exec_receives_declared_env_argv_and_same_lease_fd(self):
        digest = self.binary_fixture()
        result = self.supervise(digest)
        self.assertEqual(result["status"], "STOPPED")
        self.assertFalse(result["live_ready"])
        self.assertEqual(result["restarts"], 0)
        self.assertEqual(result["child_exit"], 0)
        capture = json.loads((self.root / "child-capture.json").read_text())
        self.assertEqual(capture["argv"], ["dcserver"])
        self.assertEqual(capture["pid"], result["child_pid"])
        self.assertEqual(capture["cwd"], str(self.root))
        self.assertEqual(capture["witness"], capture["record"])
        self.assertEqual((capture["device"], capture["inode"]),
                         (capture["witness"]["device"], capture["witness"]["inode"]))
        self.assertTrue(capture["inherited"])
        for name in ("HOME", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "AGENTDESK_API_URL", "ADK_API_URL"):
            self.assertEqual(capture["environment"][name], declarations.child_environment(self.manifest, {})[name])
        self.assertNotIn(declarations.SENTINEL, json.dumps(capture))
        self.assertNotIn("TMUX", capture["environment"])
        self.assertNotIn("TMUX_PANE", capture["environment"])
        with lease.acquire(self.root, self.fixture.instance_id):
            pass

    def test_bounds_and_source_mismatch_refuse_before_spawn_or_lease(self):
        digest = self.binary_fixture()
        variants = [{"max_gateway": 2}, {"max_gateway": True}, {"wall_timeout": 0},
                    {"wall_timeout": float("inf")}, {"wall_timeout": float("nan")},
                    {"wall_timeout": 601}, {"wall_timeout": 60, "stop_grace": 31}, {"stop_grace": 0},
                    {"wall_timeout": 0.05, "stop_grace": 0.1},
                    {"head": "0" * 40}, {"binary_sha256": "0" * 64}]
        for variant in variants:
            with self.subTest(variant=variant):
                spawn = mock.Mock()
                with self.assertRaises(InstanceError):
                    self.supervise(digest, spawn=spawn, **variant)
                spawn.assert_not_called()
                self.assertFalse((self.fixture.base / lease.LEASE_NAME).exists())

    def test_multiple_gateway_children_refuse_without_spawning(self):
        digest = self.binary_fixture()
        spawn = mock.Mock()
        with self.assertRaises(InstanceError):
            self.supervise(digest, max_gateway=2, spawn=spawn)
        spawn.assert_not_called()
        self.assertFalse((self.fixture.base / lease.LEASE_NAME).exists())

    def test_wall_timeout_stops_only_owned_child_and_releases_lease(self):
        digest = self.binary_fixture(wait=True, ignore_term=True)
        def ready_spawn(*args, **kwargs):
            child = subprocess.Popen(*args, **kwargs)
            try:
                deadline = time.monotonic() + 5
                while not (self.root / "child-capture.json").exists() and time.monotonic() < deadline:
                    self.assertIsNone(child.poll(), "actual child exited before its ready witness")
                    time.sleep(0.01)
                self.assertTrue((self.root / "child-capture.json").exists())
                return child
            except BaseException:
                child.kill()
                child.wait(timeout=5)
                raise
        started = time.monotonic()
        result = self.supervise(digest, wall_timeout=2, stop_grace=0.1, spawn=ready_spawn)
        self.assertEqual(result["status"], "TIMEOUT")
        self.assertEqual(result["child_exit"], -9)
        self.assertLess(time.monotonic() - started, 6)
        with lease.acquire(self.root, self.fixture.instance_id):
            pass

    def test_nonzero_child_exit_is_failed_and_never_restarted(self):
        digest = self.binary_fixture(exit_code=23)
        result = self.supervise(digest)
        self.assertEqual(result["status"], "FAILED")
        self.assertEqual(result["child_exit"], 23)
        self.assertEqual(result["restarts"], 0)

    def test_cleanup_pending_retains_the_exclusive_child_handle(self):
        digest = self.binary_fixture()

        class Child:
            pid, returncode = 12345, None

            def __init__(self):
                self.signals = []

            def poll(self):
                return None

            def wait(self, timeout):
                raise subprocess.TimeoutExpired("owned fixture child", timeout)

            def terminate(self):
                self.signals.append("term")

            def kill(self):
                self.signals.append("kill")

        child = Child()
        with self.assertRaises(lifecycle.CleanupPending) as raised:
            self.supervise(digest, spawn=lambda *_args, **_kwargs: child)
        self.assertIs(raised.exception.child, child)
        self.assertEqual(child.signals, ["term", "kill"])

    def test_wrapper_sigkill_keeps_child_lock_until_cooperative_exit(self):
        digest = self.binary_fixture(wait=True)
        command = [sys.executable, "-E", "-s", "-m", "scripts.dev_instance", "up",
                   "--instance", self.fixture.instance_id, "--head", self.manifest["head"],
                   "--binary-sha256", digest, "--wall-timeout", "5", "--stop-grace", "0.1"]
        wrapper = subprocess.Popen(command, cwd=declarations.REPOSITORY,
                                   env={"PATH": "/usr/bin:/bin", "HOME": str(self.fixture.home)},
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(lambda: (self.root / "cooperative-stop").touch())
        try:
            deadline = time.monotonic() + 5
            while not (self.root / "child-capture.json").exists() and time.monotonic() < deadline:
                self.assertIsNone(wrapper.poll(), "wrapper exited before its actual child exec")
                time.sleep(0.01)
            self.assertTrue((self.root / "child-capture.json").exists())
            capture = json.loads((self.root / "child-capture.json").read_text())
            lease_path = self.fixture.base / lease.LEASE_NAME
            original_inode = lease_path.stat().st_ino
            wrapper.kill()
            wrapper.wait(timeout=5)
            self.assertEqual(wrapper.returncode, -9)
            with self.assertRaises(lease.LeaseBusy):
                lease.acquire(self.root, self.fixture.instance_id)
            self.assertEqual(lease_path.stat().st_ino, original_inode)
            (self.root / "cooperative-stop").touch()
            deadline = time.monotonic() + 5
            acquired = False
            while time.monotonic() < deadline:
                try:
                    with lease.acquire(self.root, self.fixture.instance_id) as next_owner:
                        self.assertEqual(lease_path.stat().st_ino, original_inode)
                        self.assertNotEqual(next_owner.nonce, capture["witness"]["nonce"])
                        acquired = True
                    break
                except lease.LeaseBusy:
                    time.sleep(0.01)
            self.assertTrue(acquired, "last inherited holder did not release after its own exit")
            print("WRAPPER_REACQUIRE " + json.dumps({
                "wrapper_pid": wrapper.pid, "child_pid": capture["pid"], "wrapper_exit": wrapper.returncode,
                "child_alive_acquire": "EWOULDBLOCK", "child_exit": "cooperative stop file then process exit",
                "inode_before": original_inode, "inode_after": lease_path.stat().st_ino,
                "reacquired": acquired, "lease_unlink": False, "lease_ttl": False,
                "release": "close-only after last inherited holder exits"}), flush=True)
        finally:
            (self.root / "cooperative-stop").touch()
            if wrapper.poll() is None:
                wrapper.kill()
            wrapper.communicate(timeout=5)

    def test_cli_busy_is_nonzero_and_never_reports_ready(self):
        digest = self.binary_fixture()
        with lease.acquire(self.root, self.fixture.instance_id):
            result = self.fixture.cli("up", "--instance", self.fixture.instance_id,
                                      "--head", self.manifest["head"], "--binary-sha256", digest,
                                      "--wall-timeout", "1", "--stop-grace", "0.1")
        self.assertEqual(result.returncode, 5, result.stderr)
        self.assertEqual(json.loads(result.stdout), {"status": "DEFERRED_BUSY", "live_ready": False})




"""Keeper authority is an original handle or a current nonce, never a disk PID."""

import importlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
import uuid


class KeeperAdmissionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="keeper-")
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name).resolve()
        self.base.chmod(0o700)
        self.root = self.base / str(uuid.uuid4())
        self.root.mkdir(mode=0o700)
        try:
            self.keeper = importlib.import_module("scripts.dev_instance.keeper")
        except ModuleNotFoundError:
            self.keeper = None

    def api(self):
        self.assertIsNotNone(self.keeper, "normal keeper admission is not implemented")
        return self.keeper

    def test_fixed_resource_lock_is_held_until_confirmed_exit(self):
        keeper = self.api()
        owner = keeper.ResourceLock(self.root, str(uuid.uuid4()))
        original = (self.base / keeper.LOCK_NAME).stat()
        try:
            self.assertFalse(os.get_inheritable(owner.fd))
            # An independent open, not the O_EXCL marker, proves the flock contract.
            import fcntl
            fd = os.open(self.base / keeper.LOCK_NAME, os.O_RDWR)
            try:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            finally:
                os.close(fd)
            with self.assertRaises((ValueError, OSError)):
                keeper.ResourceLock(self.root, str(uuid.uuid4()))
            owner.finish()
        finally:
            owner.close()
        again = keeper.ResourceLock(self.root, str(uuid.uuid4()))
        try:
            self.assertEqual((self.base / keeper.LOCK_NAME).stat().st_ino, original.st_ino)
            again.finish()
        finally:
            again.close()

    def test_lost_keeper_marker_refuses_without_pid_adoption(self):
        keeper = self.api()
        owner = keeper.ResourceLock(self.root, str(uuid.uuid4()))
        marker = (self.base / keeper.MARKER_NAME).read_bytes()
        owner.close()  # Simulates kernel FD loss with unconfirmed resources.
        from unittest import mock
        with mock.patch.object(os, "kill", side_effect=AssertionError("foreign PID signal")):
            with self.assertRaises(ValueError):
                keeper.ResourceLock(self.root, str(uuid.uuid4()))
        self.assertEqual((self.base / keeper.MARKER_NAME).read_bytes(), marker)

    def test_replaced_owner_or_lock_is_not_unlinked(self):
        keeper = self.api()
        owner = keeper.ResourceLock(self.root, str(uuid.uuid4()))
        marker = self.base / keeper.MARKER_NAME
        try:
            marker.rename(self.base / "owned-old")
            marker.write_text("foreign replacement")
            marker.chmod(0o600)
            with self.assertRaises(ValueError):
                owner.finish()
            self.assertEqual(marker.read_text(), "foreign replacement")
        finally:
            owner.close()

    def test_private_pool_and_fixed_lock_reject_foreign_paths(self):
        keeper = self.api()
        foreign = self.base / "foreign"
        foreign.write_text("unchanged")
        (self.base / keeper.LOCK_NAME).symlink_to(foreign)
        with self.assertRaises((ValueError, OSError)):
            keeper.ResourceLock(self.root, str(uuid.uuid4()))
        self.assertEqual(foreign.read_text(), "unchanged")

    def test_protocol_binds_exact_root_nonce_and_action(self):
        keeper = self.api()
        nonce = str(uuid.uuid4())
        valid = {"root": str(self.root), "nonce": nonce, "action": "down"}
        self.assertTrue(keeper.valid_request(valid, self.root, nonce))
        for field, wrong in [("root", str(self.base / str(uuid.uuid4()))),
                             ("nonce", str(uuid.uuid4())), ("action", "kill-pid")]:
            value = valid | {field: wrong}
            self.assertFalse(keeper.valid_request(value, self.root, nonce))
        self.assertFalse(keeper.valid_request(valid | {"pid": os.getpid()}, self.root, nonce))
        for value in (None, [], {}, True):
            self.assertFalse(keeper.valid_request(valid | {"action": value}, self.root, nonce))

    def test_slow_partial_frame_has_one_total_deadline(self):
        keeper = self.api()
        parent, peer = socket.socketpair()
        parent.settimeout(.05)
        def drip():
            try:
                for byte in b'{"long":"partial packet"}\n':
                    peer.sendall(bytes([byte]))
                    time.sleep(.02)
            except OSError:
                pass
        worker = threading.Thread(target=drip)
        started = time.monotonic()
        worker.start()
        try:
            with self.assertRaises(TimeoutError):
                keeper.receive(parent)
            self.assertLess(time.monotonic()-started, .2)
        finally:
            parent.close()
            peer.close()
            worker.join(timeout=1)
        self.assertFalse(worker.is_alive())




"""Resource authority and failure contracts without installed services."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest
import uuid
from unittest import mock

from scripts.dev_instance import resources
from scripts.dev_instance.files import InstanceError
from scripts.dev_instance.manifest import prepare
from tests.test_dev_instance import profile


class ResourceFixture(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="b1r-", dir="/private/tmp")
        self.addCleanup(self.temporary.cleanup)
        self.scratch = Path(self.temporary.name).resolve()
        self.scratch.chmod(0o700)
        self.base, self.worktree = self.scratch / "i", self.scratch / "w"
        self.base.mkdir(mode=0o700)
        self.worktree.mkdir()
        environment = {"PATH": "/usr/bin:/bin", "HOME": str(self.scratch), "LC_ALL": "C"}
        def git(*arguments):
            subprocess.run(["git", "-C", str(self.worktree), *arguments], env=environment,
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        git("init", "-q")
        git("config", "user.name", "Resource Fixture")
        git("config", "user.email", "resource@example.invalid")
        git("config", "commit.gpgsign", "false")
        (self.worktree / "fixture").write_text("synthetic resource declaration\n")
        git("add", "fixture")
        git("commit", "-qm", "resource fixture")
        self.instance_id = str(uuid.uuid4())
        self.manifest = prepare(self.base, self.instance_id, self.worktree, profile(), {})
        self.root = Path(self.manifest["root"])
        config = self.root / "herdr-home/config.toml"
        os.utime(config, (time.time() - 3, time.time() - 3))
        fake = self.scratch / "fake-resource"
        fake.write_text("#!/bin/sh\nexit 71\n")
        fake.chmod(0o700)
        self.spec = dict(postgres_bin=str(fake), initdb_bin=str(fake), psql_bin=str(fake),
                         herdr_bin=str(fake), startup_timeout=20, stop_grace=5,
                         max_aux=2, pg_pool=4, pg_reserve=1)

    def owner(self, **kwargs):
        return resources.ResourceOwner(self.manifest, self.spec, time.monotonic() + 40, **kwargs)

    def real_spec(self):
        result = dict(self.spec)
        for field in ["postgres_bin", "initdb_bin", "psql_bin", "herdr_bin"]:
            value = os.environ.get("ADK_TEST_" + field.upper())
            self.assertIsNotNone(value, "explicit installed binary required: " + field)
            path = Path(value)
            self.assertTrue(path.is_absolute() and path.is_file() and os.access(path, os.X_OK))
            result[field] = str(path)
        return result


class ResourceAuthorityTests(ResourceFixture):
    def test_ambient_database_or_foreign_binary_rejects_before_spawn(self):
        with mock.patch.object(subprocess, "Popen") as spawn:
            with self.assertRaises(InstanceError):
                self.owner(ambient={"DATABASE_URL": "postgresql://synthetic.invalid/foreign"}).start()
            self.spec["postgres_bin"] = "/synthetic/nonexistent/postgres"
            with self.assertRaises(InstanceError):
                self.owner().start()
        spawn.assert_not_called()

    def test_disk_pid_is_never_adopted_or_signalled(self):
        marker = self.root / "postgres/data/postmaster.pid"
        marker.write_text("424242\nforeign sentinel remains\n")
        original = marker.read_bytes()
        owner = self.owner()
        with mock.patch.object(subprocess, "Popen") as spawn, mock.patch.object(os, "kill") as signal:
            with self.assertRaises(InstanceError):
                owner.start()
            with self.assertRaises(resources.CleanupPending):
                owner.close(time.monotonic() + 1)
        spawn.assert_not_called()
        signal.assert_not_called()
        self.assertEqual(marker.read_bytes(), original)
        self.assertFalse(owner.witness()["known_resources_closed"])

    def test_expired_startup_budget_performs_no_spawn(self):
        owner = resources.ResourceOwner(self.manifest, self.spec, time.monotonic() - 1)
        with mock.patch.object(subprocess, "Popen") as spawn:
            with self.assertRaises(InstanceError):
                owner.start()
        spawn.assert_not_called()

    def test_unreaped_handle_is_retained_as_pending(self):
        owner = self.owner()
        child = mock.Mock()
        child.pid = 313131
        child.returncode = None
        child.poll.return_value = None
        child.wait.side_effect = subprocess.TimeoutExpired("owned fake", 0)
        owner.items.append(child)
        owner._kinds.append("postgres")
        with self.assertRaises(resources.CleanupPending) as raised:
            owner.close(time.monotonic() + 0.01)
        self.assertIs(raised.exception.owner, owner)
        self.assertIs(owner.items[0], child)
        child.kill.assert_not_called()
        self.assertFalse(owner.witness()["known_resources_closed"])




    def test_authority_peer_probe_error_holds_before_any_signal(self):
        import socket
        owner = self.owner()
        path = self.root / "postgres/socket/.s.PGSQL.25432"
        with socket.socket(socket.AF_UNIX) as listener:
            listener.bind(str(path))
            listener.listen(1)
            child = mock.Mock()
            child.pid, child.poll.return_value = 313131, None
            owner.items.append(child)
            owner._kinds.append("postgres")
            owner._startup = time.monotonic()+1
            with mock.patch.object(resources, "_peer_pid", side_effect=PermissionError("synthetic authority failure")):
                with self.assertRaises(PermissionError):
                    owner._listener(path, child)
            with self.assertRaises(resources.CleanupPending):
                owner.close(time.monotonic()+1)
            child.send_signal.assert_not_called()
            child.kill.assert_not_called()
            self.assertFalse(owner.witness()["known_resources_closed"])

    def test_keeper_lifeline_eof_is_visible_hold_not_uncaught_eof(self):
        import socket
        from scripts.dev_instance import keeper
        fixture = InstanceSupervisorTests(methodName="runTest")
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        digest = fixture.binary_fixture()
        fake = str(fixture.binary)
        spec = dict(postgres_bin=fake, initdb_bin=fake, psql_bin=fake, herdr_bin=fake,
                    startup_timeout=.1, stop_grace=.1, max_aux=2, pg_pool=4, pg_reserve=1)
        parent, peer = socket.socketpair()
        peer.close()
        process = mock.Mock()
        process.wait.return_value = 1
        with mock.patch.object(keeper, "launch", return_value=(process, parent)):
            with self.assertRaises(lifecycle.CleanupPending):
                fixture.supervise(digest, resources=spec, wall_timeout=1, stop_grace=.1)


if __name__ == "__main__":
    unittest.main()
