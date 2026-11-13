"""Actual private PG/Herdr and native production listener; synthetic identity REST."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
import unittest
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from scripts.dev_instance import files, identity, keeper, lease
from tests.dev_instance_keeper_real import RealKeeperTests
from tests.test_dev_instance import profile
from scripts.dev_instance.manifest import prepare

NAMES = ('announce_bot_token', 'claude_bot_token', 'codex_bot_token', 'notify_bot_token')
SELECTOR = 'config::test_instance::bound_health_tests::b2a_native_bound_health_child'
REPO = Path(__file__).resolve().parents[1]


def wrapper():
    from scripts.dev_instance import __main__ as entry, lifecycle
    mode, endpoint = sys.argv[2:4]
    original_request = identity.request_json
    changed = False
    children, canceled = [], False
    def stop_fixture(signum, frame):
        nonlocal canceled
        canceled = True
        # This test wrapper still owns these unreaped Popen handles; it does not adopt a PID.
        for child in children:
            if child.poll() is None:
                child.terminate()
    signal.signal(signal.SIGTERM, stop_fixture)
    def request(url, deadline, token=None):
        nonlocal changed
        if token is not None:
            result = original_request(endpoint, deadline, token)
            if mode == 'wrong-id' and result['id'] == '900001': result['id'] = '999999'
            return result
        if mode == 'replace-path' and not changed:
            changed = True
            binary = Path(os.environ['B2A_FIXTURE_BINARY'])
            backup=binary.with_name('b2a-image-'+uuid.uuid4().hex)
            binary.rename(backup)
            shutil.copyfile(backup,binary)
            record=Path(os.environ['HOME'])/'image-backup.json'
            record.write_text(json.dumps({'path':str(backup),'inode':backup.stat().st_ino}))
            binary.chmod(0o700)
        result = original_request(url, deadline)
        if mode == 'wrong-nonce': result['test_instance']['nonce'] = 'foreign-run'
        if mode == 'wrong-head': result['test_instance']['head'] = '0'*40
        if mode == 'missing-app': result['test_instance']['identities'].pop()
        return result
    identity.request_json = request
    def native_spawn(argv, **kwargs):
        if canceled: raise files.InstanceError('fixture cancellation before child spawn')
        if len(argv) != 2 or argv[1] != 'dcserver':
            raise AssertionError('unexpected production child argv')
        kwargs['env'] = {**kwargs['env'], 'I3A_B2A_NATIVE_CHILD':'1'}
        if mode == 'registry': kwargs['env']['I3A_B2A_NATIVE_REGISTRY'] = '1'
        native_log = Path(kwargs['cwd'])/'logs/native-child.log'
        with native_log.open('wb') as output:
            kwargs['stdout'] = kwargs['stderr'] = output
            child = subprocess.Popen([argv[0], '--exact', SELECTOR, '--nocapture'], **kwargs)
            children.append(child)
            launch=Path(kwargs['cwd'])/'runtime/native-launch.json'
            launch.write_text(json.dumps({'pid':child.pid,'argv':[argv[0],'--exact',SELECTOR,'--nocapture'],'env':kwargs['env']}))
            launch.chmod(0o600)
            return child
    def supervise(*args, **kwargs):
        return lifecycle.supervise(*args, **kwargs, spawn=native_spawn)
    if mode == 'publication-io-error':
        original_publish=lifecycle.publish_runtime
        def publication(root,relative,*args,**kwargs):
            if str(relative)=='runtime/instance-run.json': raise OSError('synthetic owned publication failure')
            return original_publish(root,relative,*args,**kwargs)
        lifecycle.publish_runtime=publication
    entry.supervise = supervise
    return entry.main(sys.argv[4:])


class BoundHealthRealTests(RealKeeperTests):
    # The inherited fixture supplies only original-handle cleanup, not its test selection.
    test_actual_caller_down_reaps_in_reverse_order_and_refuses_foreign_nonce = None
    test_actual_caller_partial_start_failure_rolls_back_before_gateway = None
    test_actual_caller_reap_uses_same_current_run_and_waits_for_exit = None
    test_actual_live_resources_and_lost_lock_marker_refuse_another_keeper = None
    test_foreign_tcp_listener_prevents_sql_and_resource_adoption = None
    test_replaced_global_marker_never_reports_successful_down = None
    test_wrapper_sigkill_is_hold_without_gateway_guess_or_credential_delete = None

    def setUp(self):
        super().setUp()
        native = os.environ.get('ADK_B2A_NATIVE_BINARY')
        self.assertTrue(native and Path(native).is_file(), 'fresh native test binary is required')
        self.instance_id = str(uuid.uuid4())
        self.worktree = REPO
        self.manifest = prepare(self.base,self.instance_id,REPO,profile(),{})
        self.root = Path(self.manifest['root'])
        os.utime(self.root/'herdr-home/config.toml',(time.time()-3,time.time()-3))
        self.binary = REPO/'target/debug/agentdesk'
        self.assertEqual(self.binary.stat().st_ino,Path(native).stat().st_ino,
                         'the actual lane artifact must be the explicitly pinned native fixture')
        self.digest = hashlib.sha256(self.binary.read_bytes()).hexdigest()
        self.frame = {'kind':'admission','schema':1,
                      'identities':dict(zip(NAMES, ('900003','900001','900002','900006')))}
        self.tokens = {}
        for index, name in enumerate(NAMES):
            value = 'SYNTHETIC_B2A_PRIVATE_'+str(index)
            self.tokens[value] = self.frame['identities'][name]
            path = self.root/'credential'/name
            path.write_text(value); path.chmod(0o600)
        requests = self.requests = []
        values = self.tokens
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args): pass
            def do_GET(self):
                app = values.get(self.headers.get('Authorization','').removeprefix('Bot '))
                requests.append(app)
                body = json.dumps({'id':app}).encode()
                self.send_response(200); self.send_header('Content-Length',str(len(body)))
                self.end_headers(); self.wfile.write(body)
        self.api = ThreadingHTTPServer(('127.0.0.1',0),Handler)
        thread = threading.Thread(target=self.api.serve_forever,daemon=True);thread.start()
        self.addCleanup(self.api.server_close); self.addCleanup(self.api.shutdown)
        self.gateway_nonce = None

    def start(self, mode='positive', digest=None):
        self.specpath.write_text(json.dumps(self.spec))
        read, write = os.pipe()
        os.write(write,(json.dumps(self.frame)+'\n').encode())
        environment = {'PATH':'/usr/bin:/bin','HOME':str(self.home),'LC_ALL':'C',
                       'B2A_FIXTURE_BINARY':str(self.binary),'DISCORD_TOKEN':'SYNTHETIC_NEVER_INHERIT'}
        try:
            self.wrapper = subprocess.Popen([sys.executable,'-E','-s','-m',__name__,
                '--wrapper',mode,f'http://127.0.0.1:{self.api.server_port}/identity',
                'up','--instance',self.instance_id,'--head',self.manifest['head'],
                '--binary-sha256',digest or self.digest,'--wall-timeout','120','--stop-grace','5',
                '--resources',str(self.specpath)],cwd=str(REPO),env=environment,
                stdin=read,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        finally:
            os.close(read)
        self.control = os.fdopen(write,'w')
        self.addCleanup(self.control.close)
        (REPO/'reports/i3a-b2a/logs'/('progress-'+self._testMethodName+'.json')).write_text(
            json.dumps({'root':str(self.root),'mode':mode,'wrapper_pid':self.wrapper.pid,'head':self.manifest['head']}))
        return self.wrapper

    def finish_fixture(self):
        # Every subprocess remains this fixture's original handle. Missing witness stays a failure.
        if getattr(self,'control',None) and self.gateway_nonce and self.wrapper.poll() is None:
            try:
                self.control.write(json.dumps({'root':str(self.root),'nonce':self.gateway_nonce,'action':'down'})+'\n')
                self.control.flush()
            except (OSError,ValueError): pass
        if self.wrapper is not None:
            try: out,err = self.wrapper.communicate(timeout=12)
            except subprocess.TimeoutExpired:
                self.wrapper.terminate(); out,err = self.wrapper.communicate(timeout=15)
            artifact = REPO/'reports/i3a-b2a/logs'/self._testMethodName
            artifact.mkdir(exist_ok=True)
            (artifact/'wrapper.stdout').write_text(out)
            (artifact/'wrapper.stderr').write_text(err)
            for relative in ('logs/native-child.log',keeper.OWNER_FILE,'runtime/b2a-child-capture.json','runtime/instance-run.json','runtime/native-launch.json'):
                source=self.root/relative
                if source.exists(): shutil.copyfile(source,artifact/source.name)
        if (self.root/keeper.OWNER_FILE).exists():
            self.wait_for(lambda:self.record().get('known_resources_closed'),'owned resource exit',timeout=15)

    def receipt(self):
        path = self.root/'runtime/instance-run.json'
        if self.wrapper.poll() is not None and not path.exists():
            out,err = self.wrapper.communicate(timeout=5)
            log = self.root/'logs/native-child.log'
            self.fail(f'native wrapper exited {self.wrapper.returncode}: {out} {err}; '+
                      (log.read_text()[-6000:] if log.exists() else 'no native log'))
        return json.loads(path.read_text()) if path.exists() else None

    def assert_closed(self):
        record = self.wait_for(lambda:self.record() if self.record().get('known_resources_closed') else None,
                               'actual PG/Herdr rollback')
        self.assertEqual(record['close_order'],['herdr','postgres'])
        self.assertTrue(all(item['state']=='STOPPED' for item in record['resources']))
        with lease.acquire(self.root,self.instance_id): pass
        for path in [*self.root.joinpath('runtime').rglob('*.json'), *self.root.joinpath('logs').glob('*')]:
            if path.is_file():
                data = path.read_bytes()
                self.assertNotIn(b'SYNTHETIC_B2A_PRIVATE_',data,str(path))
                self.assertNotIn(b'SYNTHETIC_NEVER_INHERIT',data,str(path))
        return record

    def acceptance(self,mode):
        self.start(mode)
        receipt = self.wait_for(self.receipt,'actual native bound health',timeout=100)
        self.gateway_nonce = receipt['nonce']
        launch=json.loads((self.root/'runtime/native-launch.json').read_text())
        self.assertEqual(launch['pid'],receipt['pid'])
        self.assertEqual(launch['argv'][1:],['--exact',SELECTOR,'--nocapture'])
        self.assertNotIn('SYNTHETIC_B2A_PRIVATE_',json.dumps(launch))
        self.assertNotIn('SYNTHETIC_NEVER_INHERIT',json.dumps(launch))
        capture=self.wait_for(lambda:json.loads((self.root/'runtime/b2a-child-capture.json').read_text()),'actual child capture')
        self.assertEqual(capture['pid'],receipt['pid']); self.assertEqual(capture['bound_pid'],receipt['pid'])
        self.assertEqual(capture['port'],receipt['port'])
        self.assertTrue(capture['binary_fd_cloexec'] and capture['lease_fd_cloexec'])
        self.assertEqual(capture['registry'],mode=='registry')
        self.assertNotIn('SYNTHETIC_B2A_PRIVATE_',json.dumps(capture))
        self.assertNotIn('SYNTHETIC_NEVER_INHERIT',json.dumps(capture))
        self.assertTrue(receipt['verified']); self.assertFalse(receipt['live_ready'])
        self.assertEqual(receipt['head'],self.manifest['head'])
        self.assertEqual(receipt['binary_sha256'],self.digest)
        self.assertEqual(receipt['port'],self.manifest['api']['port'])
        self.assertEqual({x['name']:x['application_id'] for x in receipt['identities']},self.frame['identities'])
        self.assertEqual(len(self.requests),4)
        self.control.write(json.dumps({'root':str(self.root),'nonce':receipt['nonce'],'action':'down'})+'\n');self.control.flush()
        out,err = self.wrapper.communicate(timeout=15)
        self.assertEqual(self.wrapper.returncode,0,out+err)
        result=json.loads(out)
        self.assertTrue(result['verified']);self.assertFalse(result['live_ready'])
        self.assert_closed()
        print('B2A_ACTUAL '+json.dumps({'pid':receipt['pid'],'port':receipt['port'],'nonce':receipt['nonce'],
             'binary_sha256':self.digest,'identities':len(receipt['identities']),'live_ready':False,'resources_remaining':0}),flush=True)

    def test_actual_native_same_run_four_identities_health_and_down(self): self.acceptance('positive')
    def test_actual_native_registry_arm_preserves_same_run_witness(self): self.acceptance('registry')

    def refusal(self, mode, *, digest=None, started=True):
        self.start(mode,digest)
        out,err=self.wrapper.communicate(timeout=110)
        self.assertEqual(self.wrapper.returncode,2,out+err)
        self.assertEqual(json.loads(out)['status'],'REJECTED')
        self.assertFalse((self.root/'runtime/instance-run.json').exists())
        if started: self.assert_closed()
        else: self.assertFalse((self.root/keeper.OWNER_FILE).exists())

    def test_publication_io_error_reuses_original_handle_rollback(self): self.refusal('publication-io-error')
    def test_wrong_identity_refuses_before_any_runtime_resource(self): self.refusal('wrong-id',started=False)
    def test_foreign_run_nonce_rolls_back_actual_resources(self): self.refusal('wrong-nonce')
    def test_foreign_head_rolls_back_actual_resources(self): self.refusal('wrong-head')
    def test_missing_identity_in_actual_health_rolls_back_resources(self): self.refusal('missing-app')
    def test_executable_path_replacement_rolls_back_actual_resources(self): self.refusal('replace-path')
    def test_old_artifact_sha_refuses_before_any_runtime_resource(self): self.refusal('positive',digest='0'*64,started=False)


if __name__ == '__main__':
    if len(sys.argv)>1 and sys.argv[1]=='--wrapper': sys.exit(wrapper())
    unittest.main()
