"""Public removal requires scoped shutdown and refuses linked trees."""
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from tests import test_dev_instance as fixture
from scripts.dev_instance import __main__ as cli, identity, discord_rest
from scripts.dev_instance.files import write_private_json


class InstanceDownTests(unittest.TestCase):
    def setUp(self):
        self.fixture = fixture.InstanceDeclarationTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.manifest = self.fixture.prepare()
        self.root = self.fixture.base / self.fixture.instance_id

    def down(self):
        with patch.object(Path, 'home', return_value=self.fixture.home), patch.dict(os.environ, {}, clear=True):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                code = cli.main(['down', '--instance', self.root.name])
            return code, json.loads(output.getvalue())

    def test_public_down_deletes_credentials_and_root(self):
        credential = self.root/'credential/claude_bot_token'
        credential.write_text('synthetic-private-token')
        credential.chmod(0o600)
        code, result = self.down()
        self.assertEqual(code, 0, result)
        self.assertEqual(result['status'], 'REMOVED')
        self.assertFalse(credential.exists())
        self.assertFalse(self.root.exists())

    def _tmux_socket(self, name='socket'):
        import socket
        listener = socket.socket(socket.AF_UNIX)
        parent = self.root/'runtime/tmux'
        # bind relative to the private directory, including on macOS's short socket limit.
        descriptor = os.open('.', os.O_RDONLY)
        try:
            os.chdir(parent)
            listener.bind(name)
        finally:
            os.fchdir(descriptor)
            os.close(descriptor)
        return listener

    def test_down_removes_declared_stale_native_tmux_socket(self):
        listener = self._tmux_socket()
        listener.close()
        code, result = self.down()
        self.assertEqual(code, 0, result)
        self.assertEqual(result['status'], 'REMOVED')
        self.assertEqual(result['credential_files'], 0)
        self.assertFalse(self.root.exists())

    def test_live_publication_witness_preserves_strictness_and_original_socket_identity(self):
        from scripts.dev_instance.files import publish_runtime, read_regular_text
        listener = self._tmux_socket()
        listener.close()
        leaf = self.root/'runtime/instance-run.json'
        for context in (None, []):
            with self.assertRaises(ValueError):
                publish_runtime(self.root, 'runtime/instance-run.json', {'generation': 1}, live_sockets=context)
            self.assertFalse(leaf.exists())
        identity = publish_runtime(self.root, 'runtime/instance-run.json', {'generation': 1}, live_sockets={})
        original = read_regular_text(leaf)
        metadata = (self.root/'runtime/tmux/socket').lstat()
        wrong = {str(self.root/'runtime/tmux/socket'): (metadata.st_dev, metadata.st_ino+1)}
        with self.assertRaises(ValueError):
            publish_runtime(self.root, 'runtime/instance-run.json', {'generation': 2}, identity, wrong)
        self.assertEqual(read_regular_text(leaf), original)
        unknown = self._tmux_socket('unknown')
        unknown.close()
        with self.assertRaises(ValueError):
            publish_runtime(self.root, 'runtime/instance-run.json', {'generation': 2}, identity, {})
        self.assertEqual(read_regular_text(leaf), original)

    def test_down_rejects_unknown_socket_and_fifo_without_partial_deletion(self):
        for kind in ('socket', 'fifo'):
            leaf = self.root/'runtime/tmux/unknown'
            if kind == 'socket':
                listener = self._tmux_socket('unknown')
                listener.close()
            else:
                os.mkfifo(leaf)
            with self.subTest(kind=kind):
                self.assertEqual(self.down()[0], 2)
                self.assertTrue((self.root/'manifest.json').exists())
                self.assertTrue(leaf.exists())
            leaf.unlink()

    def test_down_holds_unidentified_live_tmux_endpoint(self):
        listener = self._tmux_socket()
        listener.listen(1)
        self.addCleanup(listener.close)
        with patch('scripts.dev_instance.down.subprocess.run', side_effect=OSError):
            code, result = self.down()
        self.assertEqual(code, 6, result)
        self.assertTrue((self.root/'manifest.json').exists())
        self.assertTrue((self.root/'runtime/tmux/socket').exists())

    def test_down_closes_only_the_declared_tmux_server_and_waits_for_its_panes(self):
        from scripts.dev_instance import down
        listener = self._tmux_socket()
        listener.listen(8)
        self.addCleanup(listener.close)
        child = subprocess.Popen([sys.executable, '-c', 'import time;time.sleep(60)'])
        def finish():
            if child.poll() is None:
                child.terminate()
            child.wait(timeout=5)
        self.addCleanup(finish)
        calls = []
        def control(argv, **kwargs):
            self.assertEqual(argv[:3], ['tmux', '-S', 'socket'])
            self.assertEqual(Path.cwd(), self.root/'runtime/tmux')
            self.assertEqual(kwargs['env']['TMUX_TMPDIR'], str(self.root/'runtime/tmux'))
            calls.append(argv[3])
            if argv[3] == 'list-sessions':
                output = self.manifest['hosts']['tmux']['prefix']+'-claude\n'
            elif argv[3] in ('list-panes', 'display-message'):
                output = str(child.pid)+'\n'
            else:
                self.assertEqual(argv[3:], ['kill-server'])
                finish()
                listener.close()
                (self.root/'runtime/tmux/socket').unlink()  # tmux can remove its own endpoint.
                output = ''
            return SimpleNamespace(returncode=0, stdout=output)
        with patch.object(down.subprocess, 'run', side_effect=control):
            code, result = self.down()
        self.assertEqual(code, 0, result)
        self.assertEqual(calls, ['list-sessions', 'list-panes', 'display-message', 'kill-server'])
        self.assertIsNotNone(child.poll())
        self.assertFalse(self.root.exists())

    def test_down_holds_empty_pane_list_even_with_a_valid_server_pid(self):
        from scripts.dev_instance import down
        listener = self._tmux_socket()
        listener.listen(8)
        self.addCleanup(listener.close)
        calls = []
        def control(argv, **kwargs):
            calls.append(argv[3])
            output = {'list-sessions': self.manifest['hosts']['tmux']['prefix']+'-claude\n',
                      'list-panes': '', 'display-message': '99999999\n', 'kill-server': ''}[argv[3]]
            return SimpleNamespace(returncode=0, stdout=output)
        with patch.object(down.subprocess, 'run', side_effect=control), patch.object(down, '_wait_gone') as wait:
            code, result = self.down()
        self.assertEqual(code, 6, result)
        self.assertEqual(result['status'], 'HOLD')
        self.assertEqual(calls, ['list-sessions', 'list-panes', 'display-message'])
        wait.assert_not_called()
        self.assertTrue((self.root/'manifest.json').exists())
        self.assertTrue((self.root/'runtime/tmux/socket').exists())

    def test_down_holds_foreign_tmux_sessions_without_kill_or_deletion(self):
        listener = self._tmux_socket()
        listener.listen(1)
        self.addCleanup(listener.close)
        with patch('scripts.dev_instance.down.subprocess.run', return_value=SimpleNamespace(returncode=0, stdout='AgentDesk-operating\n')) as control:
            code, result = self.down()
        self.assertEqual(code, 6, result)
        self.assertEqual(control.call_count, 1)
        self.assertEqual(control.call_args.args[0][3], 'list-sessions')
        self.assertTrue((self.root/'manifest.json').exists())

    def test_down_refuses_replaced_tmux_socket_before_unlink(self):
        from scripts.dev_instance import down
        listener = self._tmux_socket()
        listener.close()
        original = down._quiesce
        def replace(*args):
            result = original(*args)
            (self.root/'runtime/tmux/socket').rename(self.root/'runtime/tmux/original')
            replacement = self._tmux_socket()
            replacement.close()
            return result
        with patch.object(down, '_quiesce', side_effect=replace):
            self.assertEqual(self.down()[0], 2)
        self.assertTrue((self.root/'runtime/tmux/socket').exists())
        self.assertTrue((self.root/'manifest.json').exists())

    def test_down_does_not_admit_a_symlink_as_the_tmux_socket(self):
        outside = self.fixture.home/'outside'
        outside.write_text('retained')
        (self.root/'runtime/tmux/socket').symlink_to(outside)
        self.assertEqual(self.down()[0], 2)
        self.assertEqual(outside.read_text(), 'retained')
        self.assertTrue((self.root/'manifest.json').exists())

    def test_links_and_external_targets_are_refused_without_deletion(self):
        external = self.fixture.home/'outside'
        external.write_text('retain')
        for kind in ('symlink', 'hardlink'):
            leaf = self.root/'outputs/foreign'
            if kind == 'symlink': leaf.symlink_to(external)
            else: os.link(external, leaf)
            before = sorted(str(path.relative_to(self.root)) for path in self.root.rglob("*"))
            self.assertEqual(self.down()[0], 2)
            self.assertEqual(sorted(str(path.relative_to(self.root)) for path in self.root.rglob("*")), before)
            self.assertTrue(self.root.exists())
            self.assertTrue((self.root/'receipt.json').exists())
            self.assertEqual(external.read_text(), 'retain')
            leaf.unlink()
        for target in (external, self.fixture.home/'.adk/release/runtime'):
            target.parent.mkdir(parents=True, exist_ok=True)
            if not target.exists(): target.mkdir()
            from scripts.dev_instance.down import remove
            with patch.object(Path, 'home', return_value=self.fixture.home):
                with self.assertRaises(ValueError): remove(target.parent, target.name, {})
            self.assertTrue(target.exists())
        from scripts.dev_instance.down import remove
        with patch.object(Path, 'home', return_value=self.fixture.home):
            with self.assertRaises(ValueError): remove(self.fixture.home/'other', self.root.name, {})
        saved = self.root.with_name(self.root.name+'-saved')
        self.root.rename(saved)
        self.root.symlink_to(saved, target_is_directory=True)
        self.assertEqual(self.down()[0], 2)
        self.assertTrue((saved/'receipt.json').exists())
        self.root.unlink()
        saved.rename(self.root)

    def _canonical_alias_is_removed_without_traversal(self, absolute):
        from scripts.dev_instance.files import validate_tree
        marker = self.root/'credential/claude_bot_token'
        marker.write_text('synthetic');marker.chmod(0o600)
        alias = self.root/'config/credential'
        alias.symlink_to(str(self.root/'credential') if absolute else '../credential', target_is_directory=True)
        validate_tree(self.root)
        original = os.unlink
        removed = []
        def capture(name, *args, **kwargs):
            removed.append(name)
            return original(name, *args, **kwargs)
        with patch('scripts.dev_instance.down.os.unlink', side_effect=capture):
            code, result = self.down()
        self.assertEqual(code, 0, result)
        self.assertEqual(result['status'], 'REMOVED')
        self.assertEqual(removed.count('claude_bot_token'), 1)
        self.assertEqual(removed.count('credential'), 1)
        self.assertFalse(self.root.exists())

    def test_exact_internal_credential_alias_is_valid_and_removed_without_traversal(self):
        self._canonical_alias_is_removed_without_traversal(False)

    def test_absolute_canonical_credential_alias_is_removed_without_traversal(self):
        self._canonical_alias_is_removed_without_traversal(True)

    def test_dangling_exact_internal_alias_is_unlinked_without_following_target(self):
        from scripts.dev_instance.files import validate_tree
        alias = self.root/'config/credential'
        missing = self.root/'credential'
        missing.rmdir()
        alias.symlink_to('../credential', target_is_directory=True)
        validate_tree(self.root)
        original = os.unlink
        removed = []
        def capture(name, *args, **kwargs):
            removed.append(name)
            return original(name, *args, **kwargs)
        with patch('scripts.dev_instance.down.os.unlink', side_effect=capture):
            code, result = self.down()
        self.assertEqual(code, 0, result)
        self.assertEqual(removed.count('credential'), 1)
        self.assertFalse(missing.exists())
        self.assertFalse(self.root.exists())

    def test_noncanonical_internal_alias_targets_are_rejected_without_deletion(self):
        from scripts.dev_instance.files import validate_tree
        alias = self.root/'config/credential'
        for target in ['../outputs', '..', '../outputs/missing', str(self.root/'outputs')]:
            alias.symlink_to(target, target_is_directory=True)
            before = sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*'))
            with self.subTest(target=target):
                with self.assertRaises(ValueError):validate_tree(self.root)
                self.assertEqual(self.down()[0], 2)
                self.assertEqual(before, sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*')))
                self.assertTrue((self.root/'manifest.json').exists())
            alias.unlink()

    def test_dangling_external_alias_is_rejected_without_deletion(self):
        from scripts.dev_instance.files import validate_tree
        alias = self.root/'config/credential'
        target = self.fixture.home/'outside-missing'
        alias.symlink_to(target, target_is_directory=True)
        before = sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*'))
        with self.assertRaises(ValueError):validate_tree(self.root)
        self.assertEqual(self.down()[0], 2)
        self.assertEqual(before, sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*')))
        self.assertTrue(alias.is_symlink())
        self.assertFalse(target.exists())

    def test_dangling_wrong_alias_name_is_rejected_without_deletion(self):
        from scripts.dev_instance.files import validate_tree
        alias = self.root/'config/credentials'
        alias.symlink_to('../outputs/missing', target_is_directory=True)
        before = sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*'))
        with self.assertRaises(ValueError):validate_tree(self.root)
        self.assertEqual(self.down()[0], 2)
        self.assertEqual(before, sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*')))
        self.assertTrue(alias.is_symlink())

    def _canonical_alias_down_in_order(self, first):
        from scripts.dev_instance import down
        alias = self.root/'config/credential'
        alias.symlink_to('../credential', target_is_directory=True)
        original_listdir, original_erase, original_alias = os.listdir, down._erase, down.credential_alias
        root_identity = self.root.stat().st_ino
        erasing = False
        def ordered(descriptor):
            names = original_listdir(descriptor)
            if isinstance(descriptor, int) and os.fstat(descriptor).st_ino == root_identity:
                return sorted(names, key=lambda name: (name != first, name))
            return names
        def erase(*args, **kwargs):
            nonlocal erasing
            erasing = True
            return original_erase(*args, **kwargs)
        def validate_alias(*args, **kwargs):
            self.assertFalse(erasing, 'erasure must not revalidate alias targets')
            return original_alias(*args, **kwargs)
        with patch.object(down.os, 'listdir', side_effect=ordered), patch.object(down, '_erase', side_effect=erase), patch.object(down, 'credential_alias', side_effect=validate_alias):
            code, result = self.down()
        self.assertEqual(code, 0, result)
        self.assertEqual(result['status'], 'REMOVED')
        self.assertFalse(self.root.exists())

    def test_alias_removal_survives_target_directory_deleted_first(self):
        self._canonical_alias_down_in_order('credential')

    def test_alias_removal_survives_config_directory_deleted_first(self):
        self._canonical_alias_down_in_order('config')

    def test_down_recovers_missing_required_directories_with_intact_declarations(self):
        from scripts.dev_instance.files import validate_tree
        from scripts.dev_instance.manifest import validate
        alias = self.root/'config/credential'
        alias.symlink_to('../credential', target_is_directory=True)
        (self.root/'credential').rmdir()
        validate_tree(self.root)
        with self.assertRaisesRegex(ValueError, 'declared instance directory is missing'):
            validate(self.fixture.base, self.root.name, {})
        code, result = self.down()
        self.assertEqual(code, 0, result)
        self.assertFalse(self.root.exists())

    def test_partial_root_without_manifest_is_preserved(self):
        (self.root/'credential').rmdir()
        (self.root/'manifest.json').unlink()
        self.assertEqual(self.down()[0], 2)
        self.assertTrue((self.root/'receipt.json').exists())

    def test_cleanup_refuses_existing_wrong_layout_type(self):
        (self.root/'home').rename(self.root/'outputs/saved-home')
        (self.root/'home').write_text('invalid directory replacement')
        self.assertEqual(self.down()[0], 2)
        self.assertTrue((self.root/'manifest.json').exists())
        self.assertEqual((self.root/'home').read_text(), 'invalid directory replacement')

    def test_erase_refuses_alias_replacement_or_new_hardlink_after_scan(self):
        from scripts.dev_instance import down
        alias = self.root/'config/credential'
        alias.symlink_to('../credential', target_is_directory=True)
        original_listdir = os.listdir
        root_inode = self.root.stat().st_ino
        def alias_first(descriptor):
            names = original_listdir(descriptor)
            first = 'config' if os.fstat(descriptor).st_ino == root_inode else 'credential'
            return sorted(names, key=lambda name: (name != first, name))
        for kind in ['replacement', 'hardlink']:
            descriptor = down._open_dir(self.root)
            try:
                entries = down._scan(descriptor, root=self.root)
                link = self.fixture.home/'changed-alias'
                if kind == 'replacement':
                    link.symlink_to('../credential', target_is_directory=True)
                    os.replace(link, alias)
                else:os.link(alias, link, follow_symlinks=False)
                with self.subTest(kind=kind), patch.object(down.os, 'listdir', side_effect=alias_first):
                    with self.assertRaises(ValueError):down._erase(descriptor, entries, root=self.root)
                self.assertTrue(alias.is_symlink())
                self.assertTrue((self.root/'manifest.json').exists())
                if link.is_symlink():link.unlink()
            finally:os.close(descriptor)

    def test_alias_target_parent_cancellation_is_rejected_before_deletion(self):
        from scripts.dev_instance.files import validate_tree
        alias = self.root/'config/credential'
        alias.symlink_to('credential/../../credential', target_is_directory=True)
        with self.assertRaises(ValueError):validate_tree(self.root)
        self.assertEqual(self.down()[0], 2)
        self.assertTrue((self.root/'manifest.json').exists())
        self.assertTrue(alias.is_symlink())

    def test_alias_boundary_rejects_outside_wrong_name_and_hardlink_without_deletion(self):
        from scripts.dev_instance.files import validate_tree
        outside = self.fixture.home/'outside-dir'
        outside.mkdir()
        marker = outside/'marker'
        marker.write_text('unchanged')
        cases = [('config/credential',outside,'symlink'),
                 ('config/credentials',self.root/'credential','symlink'),
                 ('outputs/credential',self.root/'credential','symlink'),
                 ('config/credential',marker,'hardlink')]
        for name,target,kind in cases:
            alias=self.root/name
            if kind=='symlink':alias.symlink_to(target,target_is_directory=True)
            else:os.link(target,alias)
            before=sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*'))
            with self.subTest(name=name,kind=kind):
                with self.assertRaises(ValueError):validate_tree(self.root)
                self.assertEqual(self.down()[0],2)
                self.assertEqual(before,sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*')))
                self.assertEqual(marker.read_text(),'unchanged')
            alias.unlink()

    def test_hardlinked_credential_alias_is_rejected(self):
        from scripts.dev_instance.files import validate_tree
        alias=self.root/'config/credential'
        alias.symlink_to('../credential',target_is_directory=True)
        linked=self.fixture.home/'linked-alias'
        os.link(alias,linked,follow_symlinks=False)
        self.assertEqual(alias.lstat().st_nlink,2)
        with self.assertRaises(ValueError):validate_tree(self.root)
        self.assertEqual(self.down()[0],2)
        self.assertTrue(alias.is_symlink())
        self.assertTrue((self.root/'receipt.json').exists())
        linked.unlink()
        alias.unlink()

    def test_down_removes_unchanged_declaration_after_worktree_head_advances(self):
        self.fixture.git('commit','--allow-empty','-qm','later clean head')
        from scripts.dev_instance.manifest import validate
        with self.assertRaises(ValueError):validate(self.fixture.base,self.root.name,{})
        self.assertEqual(self.down()[0],0)
        self.assertFalse(self.root.exists())

    def test_stale_full_owner_is_removed_only_after_all_pids_and_lease_are_gone(self):
        import uuid
        dead = subprocess.Popen([sys.executable,'-c','pass'])
        dead.wait(timeout=5)
        write_private_json(self.root,'runtime/resources-owner.json',
            {'root':str(self.root),'nonce':str(uuid.uuid4()),'state':'RUNNING',
             'keeper_pid':dead.pid,'known_resources_closed':False,'gateway_exit_confirmed':False,
             'sockets':{},'resources':[{'kind':kind,'pid':dead.pid,'exitcode':None,'state':'LIVE'}
                                      for kind in ['postgres','herdr']]})
        self.assertEqual(self.down()[0],0)
        self.assertFalse(self.root.exists())

    def test_retired_owner_cannot_be_deleted_while_kernel_lease_is_held(self):
        import uuid
        from scripts.dev_instance.lease import acquire
        dead=subprocess.Popen([sys.executable,'-c','pass']);dead.wait(timeout=5)
        write_private_json(self.root,'runtime/resources-owner.json',
            {'root':str(self.root),'nonce':str(uuid.uuid4()),'state':'RUNNING',
             'keeper_pid':dead.pid,'known_resources_closed':False,'gateway_exit_confirmed':False,
             'sockets':{},'resources':[{'kind':kind,'pid':dead.pid,'exitcode':None,'state':'LIVE'}
                                      for kind in ['postgres','herdr']]})
        with acquire(self.root,self.root.name),patch('scripts.dev_instance.lifecycle.MAX_STOP_GRACE_SECONDS',.05):
            self.assertEqual(self.down()[0],6)
        self.assertTrue((self.root/'receipt.json').exists())

    def test_stale_owner_with_live_resource_is_hold_and_never_signals_it(self):
        import uuid
        dead = subprocess.Popen([sys.executable,'-c','pass']);dead.wait(timeout=5)
        write_private_json(self.root,'runtime/resources-owner.json',
            {'root':str(self.root),'nonce':str(uuid.uuid4()),'state':'RUNNING',
             'keeper_pid':dead.pid,'known_resources_closed':False,'gateway_exit_confirmed':False,
             'sockets':{},'resources':[{'kind':kind,'pid':os.getpid(),'exitcode':None,'state':'LIVE'}
                                      for kind in ['postgres','herdr']]})
        self.assertEqual(self.down()[0],6)
        self.assertTrue((self.root/'receipt.json').exists())

    def test_external_instance_pool_is_refused_before_any_deletion(self):
        from scripts.dev_instance.down import remove
        from scripts.dev_instance.manifest import prepare
        other = self.fixture.home/'other'
        manifest = prepare(other, self.root.name, self.fixture.worktree, fixture.profile(), {})
        with patch.object(Path, 'home', return_value=self.fixture.home):
            with self.assertRaises(ValueError): remove(other, self.root.name, {})
        self.assertTrue((Path(manifest['root'])/'receipt.json').exists())

    def test_uncertain_cleanup_preserves_root_and_credential(self):
        credential = self.root/'credential/claude_bot_token'
        credential.write_text('synthetic-private-token')
        credential.chmod(0o600)
        dead = subprocess.Popen([sys.executable, '-c', 'pass'])
        dead.wait(timeout=5)
        write_private_json(self.root, 'runtime/resources-owner.json',
                           {'root':str(self.root), 'nonce':'fixture', 'keeper_pid':dead.pid, 'sockets':{},
                            'known_resources_closed':False, 'gateway_exit_confirmed':True, 'resources':[]})
        with patch('scripts.dev_instance.keeper.request', return_value={'state':'HOLD','known_resources_closed':False}):
            self.assertEqual(self.down()[0], 6)
        self.assertTrue(credential.exists())
        self.assertTrue((self.root/'receipt.json').exists())

    def test_keeper_disconnect_is_hold_without_deletion(self):
        import socket
        import threading
        from scripts.dev_instance import keeper
        from scripts.dev_instance.files import publish_runtime
        with socket.socket(socket.AF_UNIX) as listener:
            listener.bind(str(self.root/keeper.SOCKET_FILE))
            listener.listen(1)
            listener.settimeout(5)
            metadata = (self.root/keeper.SOCKET_FILE).lstat()
            sockets = {str(self.root/keeper.SOCKET_FILE):(metadata.st_dev,metadata.st_ino)}
            publish_runtime(self.root, keeper.OWNER_FILE,
                {'root':str(self.root),'nonce':'fixture','sockets':sockets},live_sockets=sockets)
            def disconnect():
                with listener.accept()[0] as channel:
                    channel.settimeout(5)
                    keeper.receive(channel)
            worker = threading.Thread(target=disconnect)
            worker.start()
            result = self.fixture.cli('down','--instance',self.root.name)
            worker.join(timeout=5)
            self.assertFalse(worker.is_alive())
        self.assertEqual(result.returncode,6,result.stdout+result.stderr)
        self.assertEqual(json.loads(result.stdout)['status'],'HOLD')
        self.assertTrue((self.root/'receipt.json').exists())

    def test_missing_gateway_ack_is_hold_even_with_closed_resources(self):
        dead = subprocess.Popen([sys.executable, '-c', 'pass'])
        dead.wait(timeout=5)
        write_private_json(self.root, 'runtime/resources-owner.json',
            {'root':str(self.root),'nonce':'fixture','keeper_pid':dead.pid,'sockets':{},
             'known_resources_closed':True,'gateway_exit_confirmed':False,'resources':[]})
        self.assertEqual(self.down()[0],6)
        self.assertTrue((self.root/'receipt.json').exists())

    def test_unwitnessed_host_owner_is_hold(self):
        from scripts.dev_instance import host
        write_private_json(self.root, host.OWNER_FILE, {'unconfirmed':True})
        self.assertEqual(self.down()[0], 6)
        self.assertTrue((self.root/'receipt.json').exists())

    def test_recorded_orphan_is_reclaimed_before_removal(self):
        dead = subprocess.Popen([sys.executable, '-c', 'pass'])
        dead.wait(timeout=5)
        child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'],cwd=self.root)
        def finish():
            if child.poll() is None: child.kill()
            child.wait(timeout=5)
        self.addCleanup(finish)
        write_private_json(self.root, 'runtime/instance-run.json',
            {'instance_id':self.root.name,'pid':child.pid,'supervisor_pid':dead.pid,'nonce':'fixture'})
        original = __import__('scripts.dev_instance.lifecycle',fromlist=['_pid_alive'])._pid_alive
        with patch('scripts.dev_instance.lifecycle.MAX_STOP_GRACE_SECONDS', .1), \
                patch('scripts.dev_instance.lifecycle._pid_alive', side_effect=lambda pid: child.poll() is None if pid==child.pid else original(pid)):
            self.assertEqual(self.down()[0],0)
        self.assertIsNotNone(child.poll())
        self.assertFalse(self.root.exists())

    def test_claimed_closed_but_live_pid_keeps_root(self):
        dead = subprocess.Popen([sys.executable, '-c', 'pass'])
        dead.wait(timeout=5)
        child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'],cwd=self.root)
        def finish():
            if child.poll() is None: child.kill()
            child.wait(timeout=5)
        self.addCleanup(finish)
        write_private_json(self.root, 'runtime/resources-owner.json',
            {'root':str(self.root),'nonce':'fixture','keeper_pid':dead.pid,'sockets':{},
             'known_resources_closed':True,'gateway_exit_confirmed':True,'resources':[{'pid':child.pid,'exitcode':0}]})
        with patch('scripts.dev_instance.lifecycle.MAX_STOP_GRACE_SECONDS', .05):
            self.assertEqual(self.down()[0],6)
        self.assertTrue((self.root/'receipt.json').exists())
        self.assertIsNone(child.poll())

    def test_live_keeper_and_child_exit_before_removal(self):
        self._live_keeper_and_child_exit_before_removal(False)

    def test_live_keeper_publication_accepts_late_native_tmux_socket(self):
        self._live_keeper_and_child_exit_before_removal(True)

    def _live_keeper_and_child_exit_before_removal(self, native_tmux):
        # A real keeper uses the production request/cleanup path with an owned fixture child.
        program = r'''
import os, socket, subprocess, sys, time
from pathlib import Path
from scripts.dev_instance import keeper
from scripts.dev_instance.manifest import read_json
class Owner:
    def __init__(self, manifest, spec, *args, **kwargs):
        self.root=Path(manifest['root']); self.sockets=kwargs['live_sockets']; self.closed=False
        self.child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'],cwd=self.root,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    def start(self): return self
    def close(self, deadline):
        self.child.terminate(); self.child.wait(timeout=max(.1,deadline-time.monotonic())); self.closed=True
    def witness(self):
        return dict(sockets={} if self.closed else self.sockets,
            resources=[dict(kind='fixture',pid=self.child.pid,exitcode=self.child.poll())],known_resources_closed=self.closed)
import scripts.dev_instance.resources
scripts.dev_instance.resources.ResourceOwner=Owner
with socket.socket(fileno=int(sys.argv[1])) as lifeline:
    root=Path(sys.argv[2]); keeper.serve(lifeline,read_json(root/'manifest.json'),read_json(Path(sys.argv[3])),'fixture')
'''
        spec = self.fixture.home/'resources.json'
        spec.write_text(json.dumps(dict(postgres_bin=str(Path(sys.executable).resolve()),initdb_bin=str(Path(sys.executable).resolve()),psql_bin=str(Path(sys.executable).resolve()),herdr_bin=str(Path(sys.executable).resolve()),startup_timeout=5,stop_grace=2,max_aux=2,pg_pool=4,pg_reserve=1)))
        import socket
        from scripts.dev_instance import keeper
        parent, child = socket.socketpair()
        process = subprocess.Popen([sys.executable,'-c',program,str(child.fileno()),str(self.root),str(spec)],
                                   cwd=fixture.REPOSITORY, pass_fds=(child.fileno(),),stderr=subprocess.PIPE)
        child.close()
        def finish():
            parent.close()
            try: process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill(); process.wait(timeout=5)
            process.stderr.close()
        self.addCleanup(finish)
        parent.settimeout(10)
        try: ready = keeper.receive(parent)
        except EOFError:
            process.wait(timeout=5)
            self.fail(process.stderr.read().decode())
        child_pid = ready['resources'][0]['pid']
        if native_tmux:
            listener = self._tmux_socket()
            listener.close()
        import threading
        def acknowledge():
            frame = keeper.receive(parent)
            keeper.send(parent, {'nonce':'fixture','exit':0,'deadline':frame['deadline']})
            process.wait(timeout=10)
        worker = threading.Thread(target=acknowledge)
        worker.start()
        self.addCleanup(parent.close)
        code, result = self.down()
        worker.join(timeout=10)
        self.assertFalse(worker.is_alive())
        self.assertEqual(code, 0, result)
        self.assertFalse(self.root.exists())
        with self.assertRaises(ProcessLookupError): os.kill(child_pid,0)
        self.assertNotEqual(process.poll(), None)


class DiscordUserAgentTests(unittest.TestCase):
    def test_identity_and_driver_use_the_shared_user_agent(self):
        expected = "DiscordBot (https://github.com/itismyfield/AgentDesk, 0.1)"
        requests=[]
        class Response:
            status=200
            def __enter__(self): return self
            def __exit__(self,*args): pass
            def read(self,*args): return b'{}'
        class Transport:
            def __enter__(self): return self
            def __exit__(self,*args): pass
            def open(self, request, **kwargs): requests.append(request); return Response()
        with patch.object(identity,'_transport',return_value=Transport()):
            identity.request_json('https://discord.com/api/v10/oauth2/applications/@me',time.monotonic()+5,b'synthetic')
        admission=SimpleNamespace(manifest={'root':'/synthetic'},observed={})
        with patch.object(discord_rest,'_transport',return_value=Transport()), \
                patch.object(discord_rest,'recheck_credentials'), \
                patch.object(discord_rest,'_credential',return_value=(None,b'synthetic')):
            discord_rest.Client(admission,time.monotonic()+5)._http('notify','GET','/synthetic')
        self.assertEqual(len(requests),2)
        self.assertEqual([r.get_header('User-agent') for r in requests],[expected,expected])
