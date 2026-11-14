"""Reduced readiness and recorded-PID cleanup, using only disposable child fixtures."""
from contextlib import contextmanager
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
from types import SimpleNamespace
from unittest import mock

from scripts.dev_instance import identity, lifecycle
from scripts.dev_instance.files import InstanceError
from scripts.dev_instance.lease import LeaseBusy


class CompletionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.base.chmod(0o700)
        self.root = self.base / str(uuid.uuid4())
        self.root.mkdir(mode=0o700)
        self.manifest = {'root':str(self.root), 'instance_id':self.root.name,
                         'worktree':'/synthetic/declared-worktree', 'head':'a'*40, 'api':{'port':18791}}
        self.nonce = str(uuid.uuid4())
        dead = subprocess.Popen([sys.executable, '-c', 'pass'], env={'PATH':'/usr/bin:/bin'})
        dead.wait(timeout=5)
        self.supervisor_pid = dead.pid

    def child(self, cwd=None, ignore=False):
        code = 'import signal,time; '
        if ignore:
            code += 'signal.signal(signal.SIGTERM, signal.SIG_IGN); '
        code += 'print("ready",flush=True); time.sleep(60)'
        child = subprocess.Popen([sys.executable, '-u', '-c', code], cwd=cwd or self.root,
                                 env={'PATH':'/usr/bin:/bin'}, stdout=subprocess.PIPE, text=True)
        self.assertEqual(child.stdout.readline().strip(), 'ready')
        def finish():
            if child.poll() is None:
                child.kill()
            child.wait(timeout=5)
            child.stdout.close()
        self.addCleanup(finish)
        return child

    def record(self, child):
        return {'instance_id':self.root.name, 'nonce':self.nonce, 'pid':child.pid,
                'supervisor_pid':self.supervisor_pid}

    def cleanup(self, child, record=None, grace=.1):
        method = getattr(lifecycle, 'cleanup_orphan', None)
        self.assertTrue(callable(method), 'recorded orphan cleanup must be connected to up')
        def alive(pid):
            if pid == child.pid:
                return child.poll() is None
            self.assertEqual(pid, self.supervisor_pid)
            return False
        with mock.patch.object(lifecycle, '_pid_alive', side_effect=alive):
            return method(self.manifest, record or self.record(child), grace)

    def test_recorded_instance_term_reaps_only_its_declared_root(self):
        child = self.child()
        self.cleanup(child)
        self.assertEqual(child.returncode, -signal.SIGTERM)

    def test_term_resistant_recorded_instance_requires_kill_and_exit(self):
        child = self.child(ignore=True)
        try:
            self.cleanup(child)
        except lifecycle.CleanupPending:
            self.fail('SIGKILL stage must finish the TERM-resistant orphan')
        self.assertEqual(child.returncode, -signal.SIGKILL, 'SIGKILL stage must finish the TERM-resistant orphan')

    def test_foreign_instance_process_is_never_signalled(self):
        other = self.base/'foreign';other.mkdir(mode=0o700)
        child = self.child(cwd=other)
        refused = False
        try:
            self.cleanup(child)
        except InstanceError:
            refused = True
        self.assertIsNone(child.poll(), 'foreign instance process was signalled')
        self.assertTrue(refused, 'foreign instance identity must refuse cleanup')

    def test_process_read_failure_is_hold_instead_of_exit(self):
        method = getattr(lifecycle, '_pid_alive', None)
        self.assertTrue(callable(method))
        with mock.patch.object(lifecycle.os, 'kill', side_effect=PermissionError('synthetic probe error')):
            with self.assertRaises(lifecycle.CleanupPending):
                method(123456)

    def test_live_supervisor_is_not_an_orphan(self):
        child = self.child()
        method = getattr(lifecycle, 'cleanup_orphan', None)
        self.assertTrue(callable(method))
        with mock.patch.object(lifecycle, '_pid_alive', return_value=True):
            with self.assertRaises(InstanceError):
                method(self.manifest, self.record(child), .1)
        self.assertIsNone(child.poll())

    def test_health_and_current_instance_are_sufficient_for_ready(self):
        body = {'test_instance':{'instance_id':self.root.name, 'nonce':self.nonce, 'pid':123}}
        with mock.patch.object(identity, 'request_json', return_value=body), \
             mock.patch.object(identity, 'recheck_credentials'):
            try:
                result = identity.wait_health(self.manifest, self.nonce, 123, 'b'*64, [], time.monotonic()+1)
            except InstanceError:
                self.fail('health and the current instance identity must permit Ready without a witness bundle')
        self.assertTrue(result['live_ready'])

    def test_foreign_health_identity_cannot_publish_ready(self):
        for field, value in [('instance_id',str(uuid.uuid4())), ('nonce',str(uuid.uuid4())), ('pid',124)]:
            body = {'test_instance':{'instance_id':self.root.name, 'nonce':self.nonce, 'pid':123}}
            body['test_instance'][field] = value
            with self.subTest(field=field), mock.patch.object(identity, 'request_json', return_value=body):
                with self.assertRaises(InstanceError):
                    identity.wait_health(self.manifest, self.nonce, 123, 'b'*64, [], time.monotonic()+1)

    def test_unavailable_health_never_publishes_ready(self):
        with mock.patch.object(identity, 'request_json', side_effect=ConnectionError()):
            with self.assertRaises(InstanceError):
                identity.wait_health(self.manifest, self.nonce, 123, 'b'*64, [], time.monotonic()+.03)

    @contextmanager
    def recorded_busy_fixture(self, *, previous=None, manifest=None, record=None):
        old = self.base / str(uuid.uuid4())
        old.mkdir(mode=0o700)
        (old / 'runtime').mkdir(mode=0o700)
        nonce = str(uuid.uuid4())
        old_manifest = {**self.manifest, 'root': str(old), 'instance_id': old.name}
        old_record = {'nonce': nonce, 'instance_id': old.name,
                      'pid': self.supervisor_pid + 1, 'supervisor_pid': self.supervisor_pid}
        old_lease = {'root': str(old), 'instance_id': old.name, 'nonce': nonce}
        old_manifest.update(manifest or {})
        old_record.update(record or {})
        old_lease.update(previous or {})
        lease_path = self.base / 'lease'
        for path, value in ((old / 'manifest.json', old_manifest),
                            (old / 'runtime/instance-run.json', old_record), (lease_path, old_lease)):
            path.write_text(json.dumps(value))
            path.chmod(0o600)
        with lease_path.open('rb') as stream:
            candidate = SimpleNamespace(fd=stream.fileno(), close=mock.Mock())
            yield old_manifest, old_record, candidate

    def test_busy_up_uses_the_recorded_same_worktree_before_retrying_acquire(self):
        with self.recorded_busy_fixture() as (manifest, record, candidate):
            with mock.patch.object(lifecycle, 'acquire', side_effect=[LeaseBusy(11), 'owned lease']) as acquire, \
                 mock.patch.object(lifecycle, 'open_candidate', return_value=candidate), \
                 mock.patch.object(lifecycle, 'cleanup_orphan') as cleanup:
                self.assertEqual(lifecycle._acquire_instance(self.manifest, .1), 'owned lease')
                cleanup.assert_called_once_with(manifest, record, .1)
                self.assertEqual(acquire.call_count, 2)
                candidate.close.assert_called_once_with()

    def test_busy_up_refuses_each_unbound_recorded_owner_without_signal_or_retry(self):
        cases = (
            ('record_nonce', {}, {}, {'nonce': str(uuid.uuid4())}),
            ('worktree', {}, {'worktree': '/synthetic/other-worktree'}, {}),
            ('lease_root', {'root': str(self.base / str(uuid.uuid4()))}, {}, {}),
            ('manifest_instance_id', {}, {'instance_id': str(uuid.uuid4())}, {}),
            ('record_instance_id', {}, {}, {'instance_id': str(uuid.uuid4())}),
        )
        for name, previous, manifest, changed_record in cases:
            with self.subTest(boundary=name), self.recorded_busy_fixture(
                    previous=previous, manifest=manifest, record=changed_record) as (old, record, candidate):
                busy, outcome = LeaseBusy(11), None
                with mock.patch.object(lifecycle, 'acquire', side_effect=[busy, 'unexpected lease']) as acquire, \
                     mock.patch.object(lifecycle, 'open_candidate', return_value=candidate), \
                     mock.patch.object(lifecycle, '_pid_alive', side_effect=lambda pid: pid == record['pid']), \
                     mock.patch.object(lifecycle, '_process_root', return_value=old['root']), \
                     mock.patch.object(lifecycle.os, 'kill') as killed, \
                     mock.patch.object(lifecycle, 'cleanup_orphan', wraps=lifecycle.cleanup_orphan) as cleanup:
                    try:
                        lifecycle._acquire_instance(self.manifest, .001)
                    except (LeaseBusy, lifecycle.CleanupPending) as error:
                        outcome = error
                    self.assertIs(outcome, busy, 'unbound recorded owner must preserve the original Busy refusal')
                    acquire.assert_called_once_with(self.root, self.manifest['instance_id'])
                    killed.assert_not_called()
                    if name == 'record_instance_id':
                        cleanup.assert_called_once_with(old, record, .001)
                    else:
                        cleanup.assert_not_called()
                    candidate.close.assert_called_once_with()

    def test_recorded_orphan_alive_after_kill_is_hold(self):
        record = self.record(SimpleNamespace(pid=self.supervisor_pid + 1))
        outcome = None
        with mock.patch.object(lifecycle, '_pid_alive', side_effect=lambda pid: pid == record['pid']), \
             mock.patch.object(lifecycle, '_process_root', return_value=self.manifest['root']), \
             mock.patch.object(lifecycle.os, 'kill') as killed:
            try:
                lifecycle.cleanup_orphan(self.manifest, record, .001)
            except lifecycle.CleanupPending as error:
                outcome = error
            self.assertIsInstance(outcome, lifecycle.CleanupPending,
                                  'TERM and KILL without confirmed exit must remain HOLD')
            self.assertEqual(killed.call_args_list,
                             [mock.call(record['pid'], signal.SIGTERM), mock.call(record['pid'], signal.SIGKILL)])

    def test_recorded_orphan_signal_error_is_hold(self):
        record = self.record(SimpleNamespace(pid=self.supervisor_pid + 1))
        outcome = None
        with mock.patch.object(lifecycle, '_pid_alive', side_effect=lambda pid: pid == record['pid']), \
             mock.patch.object(lifecycle, '_process_root', return_value=self.manifest['root']), \
             mock.patch.object(lifecycle.os, 'kill', side_effect=PermissionError('synthetic signal refusal')) as killed:
            try:
                lifecycle.cleanup_orphan(self.manifest, record, .001)
            except lifecycle.CleanupPending as error:
                outcome = error
            self.assertIsInstance(outcome, lifecycle.CleanupPending, 'a failed signal cannot confirm orphan exit')
            killed.assert_called_once_with(record['pid'], signal.SIGTERM)

    def test_busy_up_preserves_cleanup_hold_without_retrying_acquire(self):
        with self.recorded_busy_fixture() as (manifest, record, candidate):
            busy, pending, outcome = LeaseBusy(11), lifecycle.CleanupPending(None), None
            with mock.patch.object(lifecycle, 'acquire', side_effect=[busy, 'unexpected lease']) as acquire, \
                 mock.patch.object(lifecycle, 'open_candidate', return_value=candidate), \
                 mock.patch.object(lifecycle, 'cleanup_orphan', side_effect=pending) as cleanup, \
                 mock.patch.object(lifecycle.os, 'kill') as killed:
                try:
                    lifecycle._acquire_instance(self.manifest, .1)
                except (LeaseBusy, lifecycle.CleanupPending) as error:
                    outcome = error
                self.assertIs(outcome, pending, 'HOLD must not be recategorized as Busy or success')
                acquire.assert_called_once_with(self.root, self.manifest['instance_id'])
                cleanup.assert_called_once_with(manifest, record, .1)
                killed.assert_not_called()
                candidate.close.assert_called_once_with()


if __name__ == '__main__':
    unittest.main()
