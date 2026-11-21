"""Instance-only Discord adapter; credentials and remote effects are synthetic."""
import copy
import importlib
import importlib.util
import json
from pathlib import Path
import time
import unittest
import uuid
from unittest import mock

from scripts.dev_instance import identity
from tests import test_dev_instance as declarations


def fixture_json(root,relative,value):
    path=Path(root)/relative
    path.write_text(json.dumps(value));path.chmod(0o600)


class DriverTests(unittest.TestCase):
    def setUp(self):
        self.fixture = declarations.InstanceDeclarationTests(methodName='runTest')
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.manifest = self.fixture.prepare()
        self.root = Path(self.manifest['root'])
        self.ids = {'claude_bot_token':'900001', 'codex_bot_token':'900002',
                    'notify_bot_token':'900003', 'announce_bot_token':'900006'}
        self.tokens = {name: ('FAKE_SENTINEL_'+name).encode() for name in self.ids}
        for name, token in self.tokens.items():
            self.credential(name, token.decode())
        self.observed = [{'name':name,'application_id':self.ids[name],
                          **identity._credential(self.root,name)[0]} for name in sorted(self.ids)]
        self.record = {'schema':1,'instance_id':self.root.name,'nonce':str(uuid.uuid4()),
                       'pid':43210,'supervisor_pid':43211,'head':self.manifest['head'],
                       'port':self.manifest['api']['port'],'binary_sha256':'a'*64,
                       'live_ready':True,'verified':True,'identities':self.observed}
        self.write_record()
        fixture_json(self.root, 'runtime/resources-owner.json', {'sockets':{}})
        self.reads = []
        self.card_id, self.dispatch_id = str(uuid.uuid4()), str(uuid.uuid4())
        self.registrations = []

    def credential(self,name,value):
        path=self.root/'credential'/name
        path.write_text(value);path.chmod(0o600)

    def write_record(self):
        fixture_json(self.root, 'runtime/instance-run.json', self.record)

    def request(self,url,deadline,token=None):
        self.reads.append(url)
        if url.endswith('/users/@me'):
            return {'id':next(self.ids[name] for name in self.ids if self.tokens[name]==token)}
        return {'test_instance':{k:v for k,v in self.record.items()
                                if k not in ('verified','supervisor_pid')}}

    def dispatch_request(self, url, body, deadline):
        self.registrations.append((url, body))
        if url.endswith('/api/kanban-cards'):return {'card': {'id': self.card_id}}
        return {'dispatch': {'id': self.dispatch_id, 'kanban_card_id': self.card_id,
                             'to_agent_id': body['to_agent_id'], 'dispatch_type': 'scope-assessment',
                             'status': 'pending'}}

    def modules(self):
        self.assertIsNotNone(importlib.util.find_spec('scripts.dev_instance.driver'),
                             'instance Driver and pre-import identity gate must exist')
        return (importlib.import_module('scripts.dev_instance.driver'),
                importlib.import_module('scripts.dev_instance.driver_admission'),
                importlib.import_module('scripts.dev_instance.discord_rest'))

    def execute(self, **kwargs):
        driver, _, _ = self.modules()
        return driver.run(self.fixture.base,self.root.name,'claude',10,
                          identity_request=self.request, dispatch_request=kwargs.pop('dispatch_request', self.dispatch_request), **kwargs)

    def test_id_gate_precedes_assertion_import_and_first_post(self):
        driver, admission, _ = self.modules()
        cases = [('missing',None),('duplicate','900002'),('zero','0'),
                 ('overflow',str(2**64)),('production','1474932782395293736')]
        original = copy.deepcopy(self.manifest)
        for name,value in cases:
            with self.subTest(case=name):
                declaration = copy.deepcopy(original)
                if value is None:declaration['profile']['bots'].pop('driver')
                else:declaration['profile']['bots']['driver']=value
                fixture_json(self.root,'manifest.json',declaration)
                factory=mock.Mock()
                with mock.patch.object(driver,'load_assertions') as imported:
                    result=self.execute(rest_factory=factory)
                self.assertEqual(result,{'status':'UNEVALUABLE(identity)','transmissions':0})
                factory.assert_not_called();imported.assert_not_called()
        fixture_json(self.root,'manifest.json',original)

    def test_receipt_mismatch_cannot_import_or_send(self):
        driver,_,_=self.modules()
        fixture_json(self.root,'receipt.json',{'instance_id':'foreign'})
        factory=mock.Mock()
        with mock.patch.object(driver,'load_assertions') as imported:
            self.assertEqual(self.execute(rest_factory=factory)['status'],'UNEVALUABLE(identity)')
        factory.assert_not_called();imported.assert_not_called();self.assertEqual(self.reads,[])

    def test_current_health_nonce_mismatch_cannot_send(self):
        driver,_,_=self.modules();factory=mock.Mock();request=self.request
        def changed(url,end,token=None):
            result=request(url,end,token)
            if '/api/health' in url:result['test_instance']['nonce']=str(uuid.uuid4())
            return result
        with mock.patch.object(driver,'load_assertions') as imported:
            result=driver.run(self.fixture.base,self.root.name,'claude',10,
                              identity_request=changed,rest_factory=factory)
        self.assertEqual(result['status'],'UNEVALUABLE(identity)')
        factory.assert_not_called();imported.assert_not_called()

    def test_four_credentials_and_notify_driver_role_are_bound(self):
        driver,admission,_=self.modules()
        with self.assertRaises(admission.IdentityRefused):
            admission.admit(self.fixture.base,self.root.name,time.monotonic()+10,
                            request=lambda *args:{'id':'900099'})
        self.record['identities'][2]['application_id']='900007';self.write_record()
        factory=mock.Mock();self.assertEqual(self.execute(rest_factory=factory)['status'],'UNEVALUABLE(identity)')
        factory.assert_not_called()

    def test_replaced_credential_and_operating_announce_identity_are_refused(self):
        _,admission,_=self.modules()
        self.credential('announce_bot_token','REPLACED_SENTINEL')
        with self.assertRaises(admission.IdentityRefused):
            admission.admit(self.fixture.base,self.root.name,time.monotonic()+10,request=self.request)
        self.credential('announce_bot_token',self.tokens['announce_bot_token'].decode())
        self.ids['announce_bot_token']='1479017284805722200'
        self.observed[0]['application_id']=self.ids['announce_bot_token'];self.write_record()
        with self.assertRaises(admission.IdentityRefused):
            admission.admit(self.fixture.base,self.root.name,time.monotonic()+10,request=self.request)

    def client(self,admitted,deadline):
        parent=admitted.manifest['profile']['channels'][0]
        root=self.root;ids=self.ids
        class Fake:
            sent=0
            def create_thread(self,channel,name):
                self.parent=channel;return '900008'
            def send_prompt(self,channel,text):
                self.sent+=1;self.marker=text.split('Reply with exactly: ',1)[1].split()[0]
                path=root/'runtime/discord_inflight/claude';path.mkdir(parents=True,mode=0o700,exist_ok=True)
                fixture_json(root,'runtime/discord_inflight/claude/'+channel+'.json',{
                    'provider':'claude','channel_id':int(channel),'logical_channel_id':int(parent),
                    'thread_id':int(channel),'request_owner_user_id':int(ids['notify_bot_token']),
                    'user_msg_id':900009,'source_message_ids':[900009],'turn_nonce':'native-turn',
                    'session_id':'native-session','output_path':str(root/'outputs/owned.jsonl')})
                return '900009'
            def messages(self,channel,after):
                return [{'id':'900010','channel_id':channel,'type':0,
                         'author':{'id':ids['claude_bot_token'],'bot':True},'content':self.marker}]
        return Fake()

    def test_one_allowed_bot_turn_uses_current_instance_and_author_adapter(self):
        driver,_,_=self.modules()
        result=self.execute(rest_factory=self.client)
        self.assertEqual(result['status'],'OBSERVED')
        self.assertEqual(result['input_source'],'allowed_bot')
        self.assertEqual(result['admission']['source_message_ids'],[900009])
        self.assertEqual(result['transmissions'],2)
        encoded=json.dumps(result)
        for token in self.tokens.values():self.assertNotIn(token.decode(),encoded)

    def test_run_uses_manifest_observation_window_and_shorter_caller_limit(self):
        from types import SimpleNamespace
        driver, _, _ = self.modules()
        for opt_in, timeout, expected in [(False, 600, 120), (True, 600, 300), (True, 7, 7)]:
            with self.subTest(opt_in=opt_in, timeout=timeout):
                now, deadlines = [1000.0], []
                manifest = copy.deepcopy(self.manifest)
                if opt_in:manifest['claude_login'] = {}
                def factory(admitted, deadline):
                    deadlines.append(deadline)
                    child = self.client(admitted, deadline)
                    child.messages = lambda *args: []
                    return child
                with mock.patch.object(driver, 'admit', return_value=SimpleNamespace(manifest=manifest, run=self.record)), \
                     mock.patch.object(driver.time, 'monotonic', side_effect=lambda: now[0]), \
                     mock.patch.object(driver.time, 'sleep', side_effect=lambda seconds: now.__setitem__(0, now[0]+seconds)), \
                     mock.patch.object(driver, 'preserve_instance_log', return_value={'status': 'SAVED'}):
                    result = driver.run(self.fixture.base, self.root.name, 'claude', timeout,
                                        rest_factory=factory, dispatch_request=self.dispatch_request)
                self.assertEqual(deadlines, [1000+expected])
                self.assertEqual(now[0], 1000+expected)
                self.assertEqual(result['status'], 'UNEVALUABLE(admission)')
                self.assertEqual(result['transmissions'], 2)

    def test_default_cli_accepts_opt_in_response_after_180_seconds(self):
        import contextlib, io
        from types import SimpleNamespace
        driver, _, _ = self.modules()
        now, deadlines = [1000.0], []
        manifest = {**self.manifest, 'claude_login': {}}
        def factory(admitted, deadline):
            deadlines.append(deadline)
            child = self.client(admitted, deadline)
            messages = child.messages
            child.messages = lambda *args: messages(*args) if now[0] >= 1210 else []
            return child
        output = io.StringIO()
        with mock.patch.object(driver, 'admit', return_value=SimpleNamespace(manifest=manifest, run=self.record)), \
             mock.patch.object(driver.time, 'monotonic', side_effect=lambda: now[0]), \
             mock.patch.object(driver.time, 'sleep', side_effect=lambda seconds: now.__setitem__(0, now[0]+seconds)), \
             mock.patch.object(driver.driver_dispatch.Client, '_http', side_effect=self.dispatch_request), \
             mock.patch.object(driver.discord_rest, 'Client', side_effect=factory), \
             mock.patch.object(driver, 'preserve_instance_log', return_value={'status': 'SAVED'}), \
             contextlib.redirect_stdout(output):
            code = driver.main(['--instance', self.root.name, '--provider', 'claude'])
        self.assertEqual(code, 0)
        self.assertEqual(deadlines, [1300])
        self.assertEqual(now[0], 1210)
        self.assertEqual(json.loads(output.getvalue())['response_ids'], ['900010'])

    def test_non_opt_in_observed_output_golden_is_unchanged(self):
        import hashlib
        result = self.execute(rest_factory=self.client)
        encoded = json.dumps(result, sort_keys=True)
        for value, label in [(str(self.root), '<root>'), (self.manifest['head'], '<head>'),
                             (self.root.name, '<uuid>'), (self.record['nonce'], '<nonce>'),
                             (self.card_id, '<card>'), (self.dispatch_id, '<dispatch>')]:
            encoded = encoded.replace(value, label)
        self.assertEqual(hashlib.sha256(encoded.encode()).hexdigest(), '678bb6d1e516baa2d0fe661044e687cd05edea198fb50ff6bbb402366613692e')

    def test_driver_preserves_log_only_after_an_explicit_login_turn(self):
        from dataclasses import replace
        driver, admission, _ = self.modules()
        with mock.patch.object(driver, 'preserve_instance_log') as capture:
            result = self.execute(rest_factory=self.client)
        self.assertEqual(result['status'], 'OBSERVED')
        self.assertNotIn('log_capture', result)
        capture.assert_not_called()
        admitted = admission.admit(self.fixture.base, self.root.name, time.monotonic()+10, request=self.request)
        manifest = copy.deepcopy(admitted.manifest)
        manifest['claude_login'] = {'mode': 'default_active', 'account_slot': 3,
            'login_home': str(self.fixture.home), 'config_dir': str(self.root/'home/.adk/profiles/claude/test-claude'),
            'cli': str(self.fixture.home/'.local/share/claude/fixture')}
        (self.root/'logs/dcserver.stdout.log').write_text('EARLY_ADMISSION_EVIDENCE\n')
        with mock.patch.object(driver, 'admit', return_value=replace(admitted, manifest=manifest)):
            result = self.execute(rest_factory=self.client)
        self.assertEqual(result['status'], 'OBSERVED')
        self.assertEqual(result['log_capture']['status'], 'SAVED')
        self.assertEqual((self.root/result['log_capture']['path']).read_text(), 'EARLY_ADMISSION_EVIDENCE\n')

    def test_dispatch_is_registered_before_discord_send_with_same_id(self):
        events = []
        def register(url, body, deadline):
            events.append(('register', url))
            return self.dispatch_request(url, body, deadline)
        def factory(admitted, deadline):
            child = self.client(admitted, deadline);original = child.send_prompt
            def send(channel, text):
                self.assertEqual(len(events), 2)
                self.assertTrue(text.startswith('DISPATCH:'+self.dispatch_id+'\n'))
                events.append(('send', text.splitlines()[0]))
                return original(channel, text)
            child.send_prompt = send;return child
        result = self.execute(dispatch_request=register, rest_factory=factory)
        self.assertEqual(result['status'], 'OBSERVED')
        self.assertEqual(result['dispatch_id'], self.dispatch_id)
        self.assertEqual([row[0] for row in events], ['register','register','send'])
        base = 'http://127.0.0.1:'+str(self.manifest['api']['port'])
        self.assertEqual([url for url, _ in self.registrations], [base+'/api/kanban-cards', base+'/api/dispatches'])
        self.assertTrue(self.registrations[1][1]['skip_outbox'])
        self.assertEqual(self.registrations[1][1]['dispatch_type'], 'scope-assessment')

    def test_registration_failure_or_bad_reply_prevents_all_discord_send(self):
        for mode in ['card-failure','dispatch-failure','bad-card-shape','bad-dispatch-shape','bad-id','wrong-card','wrong-agent','terminal']:
            calls = []
            def register(url, body, deadline):
                calls.append(url)
                if mode == 'card-failure' or (mode == 'dispatch-failure' and url.endswith('/dispatches')):
                    raise ValueError('synthetic registration failure')
                row = self.dispatch_request(url, body, deadline)
                if mode == 'bad-card-shape' and 'card' in row:return {'card': None}
                if mode == 'bad-dispatch-shape' and 'dispatch' in row:return {'dispatch': None}
                if 'dispatch' in row:
                    if mode == 'bad-id':row['dispatch']['id'] = 'invalid'
                    if mode == 'wrong-card':row['dispatch']['kanban_card_id'] = str(uuid.uuid4())
                    if mode == 'wrong-agent':row['dispatch']['to_agent_id'] = 'foreign'
                    if mode == 'terminal':row['dispatch']['status'] = 'completed'
                return row
            factory = mock.Mock()
            with self.subTest(mode=mode):
                result = self.execute(dispatch_request=register, rest_factory=factory)
                self.assertEqual(result['status'], 'UNEVALUABLE(registration)')
                self.assertEqual(result['transmissions'], 0)
                self.assertFalse(result['registration_confirmed'])
                factory.assert_not_called()

    def test_registration_client_refuses_operating_or_foreign_endpoint_before_request(self):
        from scripts.dev_instance import driver_dispatch
        from types import SimpleNamespace
        admitted = SimpleNamespace(manifest=copy.deepcopy(self.manifest), run=copy.deepcopy(self.record))
        original = copy.deepcopy(admitted.manifest['api'])
        for api in [{'host':'127.0.0.1','port':8791,'self_url':'http://127.0.0.1:8791'},
                    {**original, 'self_url':'http://127.0.0.1:8791'},
                    {**original, 'host':'example.invalid'},
                    {**original, 'self_url':'http://example.invalid:'+str(original['port'])}]:
            admitted.manifest['api'] = api
            admitted.run['port'] = api['port']
            request = mock.Mock(side_effect=self.dispatch_request)
            with self.subTest(api=api), self.assertRaises(ValueError):
                driver_dispatch.Client(admitted, time.monotonic()+1, request=request).create('claude')
            request.assert_not_called()

    def test_registration_transport_posts_only_to_instance_without_credentials(self):
        from contextlib import nullcontext
        from scripts.dev_instance import driver_dispatch
        from types import SimpleNamespace
        calls = []
        parent = self
        class Response:
            status = 201
            def __init__(self, value):self.value = value
            def read(self, bound):return json.dumps(self.value).encode()
            def __enter__(self):return self
            def __exit__(self, *args):pass
        class Transport:
            def open(self, request, timeout):
                calls.append((request.full_url, request.get_method(), request.headers))
                return Response(parent.dispatch_request(request.full_url, json.loads(request.data), time.monotonic()+1))
        admitted = SimpleNamespace(manifest=self.manifest, run=self.record)
        with mock.patch.object(driver_dispatch, '_transport', side_effect=lambda end: nullcontext(Transport())):
            result = driver_dispatch.Client(admitted, time.monotonic()+1).create('claude')
        self.assertEqual(result['dispatch_id'], self.dispatch_id)
        base = 'http://127.0.0.1:'+str(self.manifest['api']['port'])
        self.assertEqual([row[:2] for row in calls], [(base+'/api/kanban-cards','POST'),(base+'/api/dispatches','POST')])
        self.assertTrue(all(not any(key.lower()=='authorization' for key in row[2]) for row in calls))
        self.assertTrue(all(row[2]['User-agent'] for row in calls))

    def test_actual_direct_message_receipt_preserves_empty_source_lineage(self):
        driver,_,_=self.modules()
        def factory(admitted,deadline):
            child=self.client(admitted,deadline);original=child.send_prompt
            def send(channel,text):
                result=original(channel,text)
                path=self.root/'runtime/discord_inflight/claude'/(channel+'.json')
                row=json.loads(path.read_text());row['source_message_ids']=[]
                fixture_json(self.root,path.relative_to(self.root),row)
                return result
            child.send_prompt=send;return child
        with mock.patch.object(driver,'MAX_OBSERVE_SECONDS',.05):
            result=self.execute(rest_factory=factory)
        self.assertEqual(result['status'],'OBSERVED','a real direct-message receipt is admitted without invented queue lineage')
        self.assertEqual(result['admission']['lineage'],'direct_message')
        self.assertEqual(result['admission']['source_message_ids'],[])

    def test_response_without_matching_admission_is_unevaluable(self):
        def factory(admitted,deadline):
            child=self.client(admitted,deadline);original=child.send_prompt
            def send(channel,text):
                result=original(channel,text)
                fixture_json(self.root,'runtime/discord_inflight/claude/'+channel+'.json',{
                    'user_msg_id':7,'source_message_ids':[7]})
                return result
            child.send_prompt=send;return child
        driver,_,_=self.modules()
        with mock.patch.object(driver,'MAX_OBSERVE_SECONDS',.03):
            result=self.execute(rest_factory=factory)
        self.assertEqual(result['status'],'UNEVALUABLE(admission)')

    def test_uncertain_thread_mutation_is_not_reported_as_zero_transmissions(self):
        _,_,rest=self.modules()
        child=mock.Mock();child.create_thread.side_effect=TimeoutError('synthetic response loss')
        result=self.execute(rest_factory=lambda *args:child)
        self.assertEqual(result['status'],'UNEVALUABLE(observation)')
        self.assertIsNone(result['transmissions'])
        self.assertEqual(result['transmission_attempts'],1)

    def test_foreign_inflight_axes_cannot_stand_in_for_admission(self):
        driver,_,_=self.modules()
        for key,value in [('provider','codex'),('channel_id',77),('logical_channel_id',77),
                          ('thread_id',77),('request_owner_user_id',77)]:
            with self.subTest(axis=key):
                def factory(admitted,deadline):
                    child=self.client(admitted,deadline);original=child.send_prompt
                    def send(channel,text):
                        result=original(channel,text)
                        path=self.root/'runtime/discord_inflight/claude'/(channel+'.json')
                        row=json.loads(path.read_text());row[key]=value;fixture_json(self.root,path.relative_to(self.root),row)
                        return result
                    child.send_prompt=send;return child
                with mock.patch.object(driver,'MAX_OBSERVE_SECONDS',.02):
                    self.assertEqual(self.execute(rest_factory=factory)['status'],'UNEVALUABLE(admission)')

    def test_coherent_operating_id_and_notify_role_mismatch_are_refused(self):
        driver,admission,_=self.modules()
        from scripts.dev_instance.manifest import declared_manifest,receipt_for
        from scripts.dev_instance.config import build_config,build_org
        def declare(profile):
            self.manifest=declared_manifest(self.fixture.base,self.root.name,self.fixture.worktree,
                self.manifest['head'],profile,self.manifest['api']['port'],self.manifest['postgres']['port'])
            fixture_json(self.root,'manifest.json',self.manifest)
            fixture_json(self.root,'receipt.json',receipt_for(self.manifest))
            fixture_json(self.root,'config/agentdesk.yaml',build_config(self.root,profile,
                self.manifest['api']['port'],self.manifest['postgres']['port'],self.root.name))
            fixture_json(self.root,'config/org.yaml',build_org(self.root,profile))
        original=copy.deepcopy(self.manifest['profile'])
        for role,value in [('driver','1474932782395293736'),('driver','900007')]:
            with self.subTest(role=role,value=value):
                profile=copy.deepcopy(original);profile['bots'][role]=value;declare(profile)
                factory=mock.Mock()
                with mock.patch.object(driver,'load_assertions') as imported:
                    self.assertEqual(self.execute(rest_factory=factory)['status'],'UNEVALUABLE(identity)')
                factory.assert_not_called();imported.assert_not_called()
        declare(original)
        self.ids['announce_bot_token']='1479017284805722200'
        self.observed[0]['application_id']=self.ids['announce_bot_token'];self.write_record()
        with self.assertRaises(admission.IdentityRefused):
            admission.admit(self.fixture.base,self.root.name,time.monotonic()+10,request=self.request)

    def test_rest_fixed_endpoint_roles_mentions_and_no_cleanup_authority(self):
        _,admission,rest=self.modules()
        admitted=admission.admit(self.fixture.base,self.root.name,time.monotonic()+10,request=self.request)
        calls=[]
        def request(role,method,path,body=None):
            calls.append((role,method,path,body))
            if path.endswith('/threads'):
                return {'id':'900008','parent_id':'900004','owner_id':'900003','type':11}
            if method=='POST':return {'id':'900009','channel_id':'900008','author':{'id':'900003'}}
            return []
        client=rest.Client(admitted,time.monotonic()+10,request=request)
        thread=client.create_thread('900004','owned');client.send_prompt(thread,'DISPATCH:synthetic')
        self.assertEqual(client.messages(thread,'900009'),[])
        self.assertEqual([row[:2] for row in calls],[('notify','POST'),('notify','POST'),('announce','GET')])
        self.assertEqual(calls[0][3]['type'],11)
        self.assertEqual(calls[1][3]['allowed_mentions'],{'parse':[]})
        self.assertFalse(hasattr(client,'delete_message'));self.assertFalse(hasattr(client,'archive'))
        with self.assertRaises(Exception):client.send_prompt('123456','foreign')
        self.assertEqual(len(calls),3)

    def test_current_metadata_is_required_even_for_the_same_credential_value(self):
        _,admission,_=self.modules()
        path=self.root/'credential/announce_bot_token'
        path.rename(self.root/'credential/old-synthetic')
        path.write_bytes(self.tokens['announce_bot_token']);path.chmod(0o600)
        (self.root/'credential/old-synthetic').unlink()
        with self.assertRaises(admission.IdentityRefused):
            admission.admit(self.fixture.base,self.root.name,time.monotonic()+10,request=self.request)

    def test_coherent_operating_four_app_identities_are_never_admitted(self):
        _,admission,_=self.modules()
        from scripts.dev_instance.manifest import declared_manifest,receipt_for
        from scripts.dev_instance.config import build_config,build_org
        original=copy.deepcopy(self.manifest['profile']);original_ids=dict(self.ids)
        for name in sorted(self.ids):
            with self.subTest(credential=name):
                self.ids=dict(original_ids);self.ids[name]=dict(zip(sorted(self.ids),sorted(admission.OPERATING_BOT_IDS)))[name]
                profile=copy.deepcopy(original)
                for role,key in [('claude','claude_bot_token'),('codex','codex_bot_token'),('driver','notify_bot_token')]:
                    profile['bots'][role]=self.ids[key]
                self.manifest=declared_manifest(self.fixture.base,self.root.name,self.fixture.worktree,
                    self.manifest['head'],profile,self.manifest['api']['port'],self.manifest['postgres']['port'])
                for relative,data in [('manifest.json',self.manifest),('receipt.json',receipt_for(self.manifest)),
                    ('config/agentdesk.yaml',build_config(self.root,profile,self.manifest['api']['port'],
                        self.manifest['postgres']['port'],self.root.name)),('config/org.yaml',build_org(self.root,profile))]:
                    fixture_json(self.root,relative,data)
                self.record['identities']=[{'name':key,'application_id':self.ids[key],
                    **identity._credential(self.root,key)[0]} for key in sorted(self.ids)]
                self.write_record()
                with self.assertRaises(admission.IdentityRefused):
                    admission.admit(self.fixture.base,self.root.name,time.monotonic()+10,request=self.request)

    def test_assertion_author_overrides_are_scoped_after_admission(self):
        driver,admission,_=self.modules()
        admitted=admission.admit(self.fixture.base,self.root.name,time.monotonic()+10,request=self.request)
        module=importlib.import_module('scripts.e2e.tui_relay.assertions')
        before=(module.OUR_BOT_ID,module.PROVIDER_BOT_IDS)
        with driver.load_assertions(admitted.manifest['profile']['bots']) as configured:
            self.assertEqual(configured.OUR_BOT_ID,'900003')
            self.assertEqual(configured.PROVIDER_BOT_IDS,{'claude':'900001','codex':'900002'})
        self.assertEqual((module.OUR_BOT_ID,module.PROVIDER_BOT_IDS),before)

    def test_http_transport_reads_only_role_credentials_and_fixed_discord_url(self):
        _,admission,rest=self.modules()
        admitted=admission.admit(self.fixture.base,self.root.name,time.monotonic()+10,request=self.request)
        import io
        seen=[]
        def open_request(request,timeout):
            seen.append((request.full_url,request.get_method(),request.get_header('Authorization')))
            if request.get_method()=='POST':
                data={'id':'900008','parent_id':'900004','owner_id':'900003','type':11}
            else:data=[]
            response=io.BytesIO(json.dumps(data).encode());response.status=200
            return response
        with mock.patch.object(rest,'_transport') as transport:
            transport.return_value.__enter__.return_value.open.side_effect=open_request
            client=rest.Client(admitted,time.monotonic()+10)
            thread=client.create_thread('900004','owned');client.messages(thread,'900009')
        self.assertEqual([row[:2] for row in seen],[
            ('https://discord.com/api/v10/channels/900004/threads','POST'),
            ('https://discord.com/api/v10/channels/900008/messages?after=900009&limit=100','GET')])
        self.assertEqual(seen[0][2],'Bot '+self.tokens['notify_bot_token'].decode())
        self.assertEqual(seen[1][2],'Bot '+self.tokens['announce_bot_token'].decode())

    def test_inline_secret_arguments_are_rejected_without_echo(self):
        driver,_,_=self.modules()
        import contextlib,io
        output,error=io.StringIO(),io.StringIO()
        with contextlib.redirect_stdout(output),contextlib.redirect_stderr(error):
            code=driver.main(['--instance',self.root.name,'--provider','claude','--token','SENTINEL_INLINE_SECRET'])
        self.assertEqual(code,3)
        self.assertNotIn('SENTINEL_INLINE_SECRET',output.getvalue()+error.getvalue())
        self.assertEqual(json.loads(output.getvalue()),{'status':'UNEVALUABLE(identity)','transmissions':0})

    def test_http_error_drops_echoed_token_body_and_request(self):
        _,admission,rest=self.modules()
        admitted=admission.admit(self.fixture.base,self.root.name,time.monotonic()+10,request=self.request)
        import io,urllib.error
        secret=self.tokens['notify_bot_token'].decode()
        error=urllib.error.HTTPError('https://discord.com/api/v10',403,secret,{},io.BytesIO(secret.encode()))
        with mock.patch.object(rest,'_transport') as transport:
            transport.return_value.__enter__.return_value.open.side_effect=error
            with self.assertRaises(Exception) as caught:
                rest.Client(admitted,time.monotonic()+10).create_thread('900004','owned')
        self.assertNotIn(secret,str(caught.exception));self.assertIsNone(caught.exception.__cause__)


class AgyDriverAdmissionTests(unittest.TestCase):
    def test_agy_driver_admission_requires_the_test_antigravity_discord_identity(self):
        from scripts.dev_instance import driver_admission as admission
        fixture = declarations.InstanceDeclarationTests(methodName='runTest')
        fixture.setUp(); self.addCleanup(fixture.doCleanups)
        manifest = fixture.prepare(declarations.agy_profile(), agy_login=declarations.agy_login(fixture.home))
        root = Path(manifest['root'])
        ids = {'antigravity_bot_token':'1573471163663785994', 'announce_bot_token':'900006', 'notify_bot_token':'900003'}
        for name in ids:
            (root/'credential'/name).write_text('FAKE_SENTINEL_'+name); (root/'credential'/name).chmod(0o600)
        record = {'schema':1, 'instance_id':root.name, 'nonce':str(uuid.uuid4()), 'pid':43210, 'supervisor_pid':43211,
                  'head':manifest['head'], 'port':manifest['api']['port'], 'binary_sha256':'a'*64, 'live_ready':True,
                  'verified':True, 'identities':[{'name':name, 'application_id':ids[name], **identity._credential(root, name)[0]}
                                                 for name in sorted(ids)]}
        fixture_json(root, 'runtime/instance-run.json', record)
        fixture_json(root, 'runtime/resources-owner.json', {'sockets':{}})
        def request(answer):
            def call(url, deadline, token=None):
                if url.endswith('/users/@me'):
                    return {'id': answer(token.decode().removeprefix('FAKE_SENTINEL_'))}
                return {'test_instance': {k: v for k, v in record.items() if k not in ('verified', 'supervisor_pid')}}
            return call
        admitted = admission.admit(fixture.base, root.name, time.monotonic()+10, request=request(ids.get))
        self.assertEqual(admitted.manifest['profile']['bots']['antigravity'], ids['antigravity_bot_token'])
        with self.assertRaises(admission.IdentityRefused):
            admission.admit(fixture.base, root.name, time.monotonic()+10,
                            request=request(lambda name: '900099' if name == 'antigravity_bot_token' else ids[name]))

if __name__=='__main__':unittest.main()
