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
        temporary_directory = tempfile.TemporaryDirectory
        with patch.object(tempfile, 'TemporaryDirectory', side_effect=lambda **kwargs: temporary_directory(prefix='d-', dir='/private/tmp')):
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
