"""B2a identity and same-run health gates; no real credentials or network."""
import copy
import hashlib
import hmac
import importlib
import io
import json
import os
from pathlib import Path
import time
import subprocess
import sys
import uuid
import unittest
from unittest import mock

from scripts.dev_instance import files, lifecycle
from tests import test_dev_instance_lifecycle as fixtures

NAMES = ('announce_bot_token', 'claude_bot_token', 'codex_bot_token', 'notify_bot_token')


class IdentityAdmissionTests(unittest.TestCase):
    def setUp(self):
        self.fixture = fixtures.InstanceSupervisorTests(methodName='runTest')
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.root = self.fixture.root
        self.manifest = self.fixture.manifest
        self.frame = {'kind': 'admission', 'schema': 1,
                      'identities': dict(zip(NAMES, ('900003','900001','900002','900006')))}
        for index, name in enumerate(NAMES):
            path = self.root / 'credential' / name
            path.write_text('SYNTHETIC_PRIVATE_IDENTITY_' + str(index))
            path.chmod(0o600)

    def pipe(self, value):
        read, write = os.pipe()
        os.write(write, value)
        os.close(write)
        stream = os.fdopen(read, 'r')
        self.addCleanup(stream.close)
        return stream

    def identity(self):
        self.assertIsNotNone(importlib.util.find_spec('scripts.dev_instance.identity'),
                             'the normal B2a caller needs its identity implementation')
        return importlib.import_module('scripts.dev_instance.identity')

    def probe(self, **kwargs):
        values = {('SYNTHETIC_PRIVATE_IDENTITY_' + str(i)).encode(): self.frame['identities'][name]
                  for i, name in enumerate(NAMES)}
        return self.identity().probe(self.root, self.frame, time.monotonic()+2,
                                     request=kwargs.get('request', lambda token, deadline: values[token]),
                                     declared_roles=self.manifest['profile']['bots'])

    def test_explicit_incomplete_admission_is_not_ignored_by_existing_up(self):
        for name in NAMES:
            (self.root / 'credential' / name).unlink()
        digest = self.fixture.binary_fixture()
        bad = {'kind': 'admission', 'schema': 1, 'identities': {NAMES[0]: '910001'}}
        with self.assertRaises(files.InstanceError):
            self.fixture.supervise(digest, control_stream=self.pipe((json.dumps(bad)+'\n').encode()))
        self.assertFalse((self.root / 'child-capture.json').exists())

    def test_live_four_private_names_validate_without_granting_legacy_three_mode(self):
        try:
            files.validate_tree(self.root)
        except files.InstanceError as error:
            self.fail('canonical live4 must be storable: ' + str(error))
        with self.assertRaises(files.InstanceError):
            files.validate_tree(self.root, require_credentials=True)

    def test_authentic_observed_ids_and_file_identity_are_required_for_all_four(self):
        observed = self.probe()
        self.assertEqual({item['name']: item['application_id'] for item in observed}, self.frame['identities'])
        for item in observed:
            metadata = (self.root/'credential'/item['name']).stat()
            self.assertEqual((item['device'], item['inode']), (metadata.st_dev, metadata.st_ino))
        self.assertNotIn('SYNTHETIC_PRIVATE_IDENTITY_', json.dumps(observed))

    def test_each_wrong_authenticated_identity_rejects(self):
        for index, name in enumerate(NAMES):
            wrong = ('SYNTHETIC_PRIVATE_IDENTITY_' + str(index)).encode()
            with self.subTest(name=name), self.assertRaises(files.InstanceError):
                self.probe(request=lambda token, deadline: '990000' if token == wrong else
                           self.frame['identities'][NAMES[int(token.rsplit(b'_', 1)[1])]])

    def test_schema_missing_duplicate_and_unknown_expected_ids_refuse(self):
        identity = self.identity()
        for mutation in ('missing', 'duplicate', 'unknown', 'integer', 'zero'):
            frame = copy.deepcopy(self.frame)
            if mutation == 'missing': frame['identities'].pop(NAMES[0])
            if mutation == 'duplicate': frame['identities'][NAMES[0]] = frame['identities'][NAMES[1]]
            if mutation == 'unknown': frame['unexpected'] = True
            if mutation == 'integer': frame['identities'][NAMES[0]] = 1234
            if mutation == 'zero': frame['identities'][NAMES[0]] = '0'
            with self.subTest(mutation=mutation), self.assertRaises(files.InstanceError):
                identity.parse_admission(frame)

    def test_provider_role_identity_mismatch_is_refused_before_api(self):
        request = mock.Mock()
        self.frame['identities']['claude_bot_token'] = '999999'
        with self.assertRaises(files.InstanceError): self.probe(request=request)
        request.assert_not_called()

    def test_credential_link_mode_extra_duplicate_and_midprobe_replacement_refuse(self):
        identity = self.identity()
        path = self.root/'credential'/NAMES[0]
        original = path.read_bytes()
        path.chmod(0o644)
        with self.assertRaises(files.InstanceError): self.probe()
        path.chmod(0o600)
        extra = self.root/'credential/driver_bot_token'
        extra.write_bytes(b'SYNTHETIC_LEGACY'); extra.chmod(0o600)
        with self.assertRaises(files.InstanceError): self.probe()
        extra.unlink()
        def replace(token, deadline):
            path.write_bytes(original+b'replaced')
            return self.frame['identities'][NAMES[int(token.rsplit(b'_', 1)[1])]]
        with self.assertRaises(files.InstanceError): self.probe(request=replace)
        path.write_bytes(original)
        other = self.root/'credential'/NAMES[1]
        other.write_bytes(original)
        with self.assertRaises(files.InstanceError): self.probe()

    def test_initial_prefill_preserves_legacy_partial_and_following_frames(self):
        first = (json.dumps(self.frame)+'\n').encode()
        legacy = '{"root":"old","nonce":"old","action":"down"}\n'
        admission, stream = lifecycle.initial_frame(self.pipe(first+legacy.encode()))
        self.assertEqual(admission, self.frame)
        self.assertEqual(stream.readline(), legacy)
        for prefix in (legacy.encode(), b'{"action":'):
            admission, stream = lifecycle.initial_frame(self.pipe(prefix))
            self.assertIsNone(admission)
            self.assertEqual(stream.readline(), prefix.decode())
        read, write = os.pipe(); self.addCleanup(os.close, write)
        stream = os.fdopen(read); self.addCleanup(stream.close)
        before = time.monotonic()
        self.assertIsNone(lifecycle.initial_frame(stream)[0])
        self.assertLess(time.monotonic()-before, .1)

    def test_raw_control_health_rejects_each_stale_or_missing_witness(self):
        identity = self.identity(); observed = self.probe()
        expected = {'schema': 1, 'instance_id': self.manifest['instance_id'], 'head': self.manifest['head'],
                    'pid': 123456, 'binary_sha256': 'a'*64, 'nonce': 'current-nonce',
                    'port': self.manifest['api']['port'], 'identities': observed, 'live_ready': False}
        result = identity.verify_health({'test_instance': expected}, self.manifest, 'current-nonce',
                                        123456, 'a'*64, observed)
        self.assertEqual(result, expected)
        for field in ('instance_id','head','pid','binary_sha256','nonce','port','identities','live_ready'):
            changed = copy.deepcopy(expected)
            changed[field] = True if field == 'live_ready' else [] if field == 'identities' else 'stale'
            with self.subTest(field=field), self.assertRaises(files.InstanceError):
                identity.verify_health({'test_instance': changed}, self.manifest, 'current-nonce',
                                       123456, 'a'*64, observed)
        with self.assertRaises(files.InstanceError):
            identity.verify_health({}, self.manifest, 'current-nonce', 123456, 'a'*64, observed)

    def test_ready_uses_only_current_scope_id_nonce_and_pid(self):
        identity = self.identity(); observed = self.probe()
        expected = {'schema':1, 'instance_id':self.manifest['instance_id'], 'head':self.manifest['head'],
                    'pid':123456, 'binary_sha256':'a'*64, 'nonce':'current-nonce',
                    'port':self.manifest['api']['port'], 'identities':observed, 'live_ready':True}
        body = {'test_instance':{'instance_id':self.manifest['instance_id'], 'nonce':'current-nonce',
                                 'pid':123456, 'head':'ignored-health-head', 'binary_sha256':'0'*64,
                                 'identities':[]}}
        with mock.patch.object(identity, 'request_json', return_value=body):
            result = identity.wait_health(self.manifest, 'current-nonce', 123456, 'a'*64,
                                          observed, time.monotonic()+2)
        self.assertEqual(result, expected)

    def test_ready_refuses_foreign_scope_id_nonce_or_pid(self):
        identity = self.identity(); observed = self.probe()
        current = {'instance_id':self.manifest['instance_id'], 'nonce':'current-nonce', 'pid':123456}
        for field, value in [('instance_id','foreign-instance'), ('nonce','foreign-run'), ('pid',654321)]:
            body = {'test_instance':{**current, field:value}}
            with self.subTest(field=field), mock.patch.object(identity, 'request_json', return_value=body):
                with self.assertRaises(files.InstanceError):
                    identity.wait_health(self.manifest, 'current-nonce', 123456, 'a'*64,
                                         observed, time.monotonic()+2)

    def test_health_observed_credential_replacement_is_not_accepted(self):
        identity = self.identity(); observed = self.probe()
        path = self.root/'credential'/NAMES[0]
        path.write_bytes(b'SYNTHETIC_REPLACED')
        with self.assertRaises(files.InstanceError): identity.recheck_credentials(self.root, observed)

    def test_late_extra_credential_invalidates_observed_inventory(self):
        identity = self.identity(); observed = self.probe()
        extra = self.root/'credential/driver_bot_token'
        extra.write_bytes(b'SYNTHETIC_LEGACY'); extra.chmod(0o600)
        with self.assertRaises(files.InstanceError): identity.recheck_credentials(self.root, observed)

    def test_validated_role_snapshot_does_not_follow_mutated_manifest(self):
        changed = copy.deepcopy(self.manifest)
        changed['profile']['bots']['claude'] = '999999'
        (self.root/'manifest.json').write_text(json.dumps(changed))
        self.frame['identities']['claude_bot_token'] = '999999'
        request = mock.Mock()
        with self.assertRaises(files.InstanceError): self.probe(request=request)
        request.assert_not_called()

    def test_runtime_ready_receipt_is_private_bounded_and_nonsecret(self):
        payload = {'schema':1,'verified':True,'live_ready':True,'identities':self.probe()}
        identity = files.publish_runtime(self.root,'runtime/instance-run.json',payload)
        self.assertEqual((self.root/'runtime/instance-run.json').stat().st_mode & 0o777,0o600)
        self.assertNotIn('SYNTHETIC_PRIVATE_IDENTITY_', (self.root/'runtime/instance-run.json').read_text())
        with self.assertRaises(files.InstanceError):
            files.publish_runtime(self.root,'runtime/foreign.json',payload)
        with self.assertRaises(files.InstanceError):
            files.publish_runtime(self.root,'runtime/instance-run.json',payload)


class SharedWriterTests(unittest.TestCase):
    def test_actual_rust_writer_accepts_both_existing_utility_basenames(self):
        from tests.test_dev_instance_credential import InstanceCredentialCliTests
        fixture = InstanceCredentialCliTests(methodName='runTest')
        fixture.setUp(); self.addCleanup(fixture.doCleanups)
        for index, name in enumerate(('announce_bot_token', 'notify_bot_token')):
            token = ('SYNTHETIC_UTILITY_STDIN_ONLY_'+str(index)).encode()
            result = fixture.credential(name, token+b'\n')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((fixture.root/'credential'/name).read_bytes(), token)
            self.assertNotIn(token, result.stdout+result.stderr)


class IdentityTransportTests(unittest.TestCase):
    def test_slow_body_is_bounded_by_total_deadline(self):
        import threading
        from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
        from scripts.dev_instance import identity
        class Handler(BaseHTTPRequestHandler):
            def log_message(self,*args): pass
            def do_GET(self):
                body=b'{"id":"900001"}'
                self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers()
                try:
                    for byte in body:
                        self.wfile.write(bytes([byte]));self.wfile.flush();time.sleep(.04)
                except (BrokenPipeError,ConnectionResetError): pass
        server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        self.addCleanup(server.server_close);self.addCleanup(server.shutdown)
        started=time.monotonic()
        try: identity.request_json(f'http://127.0.0.1:{server.server_port}/slow',started+.08)
        except (files.InstanceError,ConnectionError): pass
        self.assertLess(time.monotonic()-started,.30,'slow-drip body bypassed the absolute request deadline')

    def test_dns_wait_is_bounded_without_any_credential_handoff(self):
        import socket
        from scripts.dev_instance import identity
        def slow_resolver(*args,**kwargs):
            time.sleep(.40)
            return [(socket.AF_INET,socket.SOCK_STREAM,socket.IPPROTO_TCP,'',('127.0.0.1',1))]
        with mock.patch.object(socket,'getaddrinfo',slow_resolver):
            started=time.monotonic()
            with self.assertRaises((files.InstanceError,ConnectionError)):
                identity.request_json('http://fixture.invalid:1/',started+.05)
            self.assertLess(time.monotonic()-started,.30,'DNS exceeded the absolute request deadline')

    def test_proxy_is_ignored_and_redirect_does_not_forward_credentials(self):
        import threading
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        from scripts.dev_instance import identity
        seen = []
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args): pass
            def do_GET(self):
                seen.append(self.path)
                if self.path == '/redirect':
                    self.send_response(302); self.send_header('Location','/forbidden'); self.end_headers()
                else:
                    data = b'{"id":"900001"}'
                    self.send_response(200);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
        server = ThreadingHTTPServer(('127.0.0.1',0),Handler)
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        self.addCleanup(server.server_close);self.addCleanup(server.shutdown)
        with mock.patch.dict(os.environ, {'HTTP_PROXY':'http://synthetic.invalid:1','HTTPS_PROXY':'http://synthetic.invalid:1'}):
            body=identity.request_json(f'http://127.0.0.1:{server.server_port}/identity',time.monotonic()+2,b'SYNTHETIC_AUTH_ONLY')
            self.assertEqual(body['id'],'900001')
            with self.assertRaises(files.InstanceError):
                identity.request_json(f'http://127.0.0.1:{server.server_port}/redirect',time.monotonic()+2,b'SYNTHETIC_AUTH_ONLY')
        self.assertEqual(seen,['/identity','/redirect'])


class ControlProtocolTests(unittest.TestCase):
    def setUp(self):
        from scripts.dev_instance import identity
        self.identity = identity
        self.base = IdentityAdmissionTests(methodName='runTest')
        self.base.setUp()
        self.addCleanup(self.base.doCleanups)
        self.root, self.manifest = self.base.root, copy.deepcopy(self.base.manifest)
        self.observed = self.base.probe()
        self.nonce, self.digest = str(uuid.uuid4()), 'a'*64
        self.mode, self.requests, self.paths, self.mutate = 'normal', [], [], lambda value: None
        self.ack = self.root/'runtime/instance-control-ack.json'

    def api(self, name):
        value = getattr(self.identity, name, None)
        self.assertTrue(callable(value), name+' must authenticate the existing lifecycle caller')
        return value

    @staticmethod
    def canonical(value):
        return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()

    def key(self):
        value = bytearray(b'agentdesk-test-control-key-v1\0')
        for name in sorted(NAMES):
            encoded, raw = name.encode(), (self.root/'credential'/name).read_bytes()
            value.extend(len(encoded).to_bytes(4, 'big')+encoded+len(raw).to_bytes(4, 'big')+raw)
        return hashlib.sha256(value).digest()

    def sign(self, value, direction):
        return hmac.new(self.key(), b'agentdesk-test-control-v1/'+direction+b'\0'+self.canonical(value),
                        hashlib.sha256).hexdigest()

    def start(self):
        import threading
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        self.child = subprocess.Popen([sys.executable, '-c', 'import sys; sys.stdin.read()'],
                                      stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        def cleanup():
            if self.child.poll() is None: self.child.terminate()
            self.child.wait(timeout=3)
            if not self.child.stdin.closed: self.child.stdin.close()
        self.addCleanup(cleanup)
        owner = self
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args): pass
            def do_GET(self):
                owner.paths.append(self.path); self.send_response(403); self.end_headers()
            def do_POST(self):
                owner.paths.append(self.path)
                request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                owner.requests.append(request)
                unsigned = {key: value for key, value in request.items() if key != 'proof'}
                if (self.path != '/_agentdesk/test-control' or request['action'] != 'stop'
                        or request['proof'] != owner.sign(unsigned, b'request')):
                    self.send_response(403); self.end_headers(); return
                if owner.mode == 'redirect':
                    self.send_response(302); self.send_header('Location', '/control-forbidden'); self.end_headers(); return
                witness = {'schema':1, 'instance_id':owner.manifest['instance_id'], 'head':owner.manifest['head'],
                           'pid':owner.child.pid, 'binary_sha256':owner.digest, 'nonce':owner.nonce,
                           'port':owner.manifest['api']['port'], 'identities':owner.observed, 'live_ready':False}
                reply = {'schema':1, 'action':request['action'], 'challenge':request['challenge'],
                         'witness':witness, 'accepted':True}
                owner.mutate(reply)
                reply['proof'] = owner.sign(reply, b'response')
                if owner.mode == 'forged': reply['proof'] = '0'*64
                if request['action'] == 'stop':
                    if owner.mode not in {'missing', 'eof-missing'}:
                        ack = copy.deepcopy(reply)
                        if owner.mode == 'old-ack':
                            ack['challenge'] = str(uuid.uuid4()); ack.pop('proof')
                            ack['proof'] = owner.sign(ack, b'response')
                        owner.ack.write_bytes(owner.canonical(ack)); owner.ack.chmod(0o600)
                        if owner.mode == 'public-ack': owner.ack.chmod(0o644)
                        if owner.mode == 'linked-ack': os.link(owner.ack, owner.root/'hardlink-ack')
                    if owner.mode != 'no-exit': owner.child.stdin.close()
                    if owner.mode.startswith('eof'):
                        self.close_connection = True; return
                body = owner.canonical(reply)
                self.send_response(200); self.send_header('Content-Length', str(len(body))); self.end_headers()
                self.wfile.write(body)
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.manifest['api']['port'] = server.server_port
        thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
        self.addCleanup(server.server_close); self.addCleanup(server.shutdown)

    def test_single_stop_waits_for_the_original_child(self):
        stop = self.api('cooperative_stop')
        self.start()
        with mock.patch.object(self.child, 'wait', wraps=self.child.wait) as wait:
            result = stop(self.manifest, self.nonce, self.child, self.digest, self.observed, time.monotonic()+3)
            wait.assert_called_once()
        self.assertIsNone(result)
        self.assertEqual(self.child.returncode, 0)
        self.assertEqual([item['action'] for item in self.requests], ['stop'])
        self.assertEqual(str(uuid.UUID(self.requests[0]['challenge'])), self.requests[0]['challenge'])

    def test_public_nonce_does_not_authenticate_a_forged_control_reply(self):
        stop = self.api('cooperative_stop'); self.start(); self.mode = 'forged'
        with self.assertRaises(self.identity.ControlRefused):
            stop(self.manifest, self.nonce, self.child, self.digest, self.observed, time.monotonic()+2)

    def test_signed_reply_still_requires_exact_schema_challenge_and_witness(self):
        stop = self.api('cooperative_stop'); self.start(); self.mode = 'no-exit'
        variants = [('schema', True), ('action', 'probe'), ('challenge', str(uuid.uuid4())),
                    ('accepted', False), ('extra', True)]
        for field, value in variants:
            self.mutate = lambda reply, field=field, value=value: reply.__setitem__(field, value)
            with self.subTest(field=field, value=value), self.assertRaises(self.identity.ControlRefused):
                stop(self.manifest, self.nonce, self.child, self.digest, self.observed, time.monotonic()+2)
        self.mutate = lambda reply: reply['witness'].__setitem__('nonce', 'old-run')
        with self.assertRaises(self.identity.ControlRefused):
            stop(self.manifest, self.nonce, self.child, self.digest, self.observed, time.monotonic()+2)
        self.assertEqual(len(self.requests), len(variants)+1)

    def test_valid_http_reply_without_durable_ack_is_refused_before_wait(self):
        stop = self.api('cooperative_stop'); self.start(); self.mode = 'missing'
        with mock.patch.object(self.child, 'wait', wraps=self.child.wait) as wait:
            with self.assertRaises(self.identity.ControlRefused):
                stop(self.manifest, self.nonce, self.child, self.digest, self.observed, time.monotonic()+2)
            wait.assert_not_called()

    def test_eof_is_accepted_only_with_the_signed_fresh_ack(self):
        stop = self.api('cooperative_stop'); self.start(); self.mode = 'eof'
        result = stop(self.manifest, self.nonce, self.child, self.digest, self.observed, time.monotonic()+2)
        self.assertIsNone(result); self.assertEqual(self.child.returncode, 0)

    def test_eof_without_ack_cannot_claim_gateway_exit(self):
        stop = self.api('cooperative_stop'); self.start(); self.mode = 'eof-missing'
        with self.assertRaises(self.identity.ControlRefused):
            stop(self.manifest, self.nonce, self.child, self.digest, self.observed, time.monotonic()+2)

    def test_old_challenge_and_public_ack_files_are_refused(self):
        stop = self.api('cooperative_stop'); self.start(); self.mode = 'old-ack'
        with self.assertRaises(self.identity.ControlRefused):
            stop(self.manifest, self.nonce, self.child, self.digest, self.observed, time.monotonic()+2)

    def test_wait_timeout_cannot_be_replaced_by_http_success(self):
        stop = self.api('cooperative_stop'); self.start(); self.mode = 'no-exit'
        with mock.patch.object(self.child, 'wait', side_effect=subprocess.TimeoutExpired('owned-child', .01)) as wait:
            with self.assertRaises(self.identity.ControlRefused):
                stop(self.manifest, self.nonce, self.child, self.digest, self.observed, time.monotonic()+2)
            wait.assert_called_once()

    def test_ack_must_remain_private_even_with_a_valid_signature(self):
        stop = self.api('cooperative_stop'); self.start(); self.mode = 'public-ack'
        with self.assertRaises(self.identity.ControlRefused):
            stop(self.manifest, self.nonce, self.child, self.digest, self.observed, time.monotonic()+2)

    def test_ack_hardlink_is_not_adopted_as_a_private_acknowledgement(self):
        stop = self.api('cooperative_stop'); self.start(); self.mode = 'linked-ack'
        with self.assertRaises(self.identity.ControlRefused):
            stop(self.manifest, self.nonce, self.child, self.digest, self.observed, time.monotonic()+2)

    def test_removed_epoch_and_control_ready_fields_are_rejected(self):
        stop = self.api('cooperative_stop'); self.start(); self.mode = 'no-exit'
        for field in ('epoch', 'control_ready'):
            self.mutate = lambda reply, field=field: reply.__setitem__(field, 7)
            with self.subTest(field=field), self.assertRaises(self.identity.ControlRefused):
                stop(self.manifest, self.nonce, self.child, self.digest, self.observed, time.monotonic()+2)
        self.assertEqual(len(self.requests), 2)

    def test_post_control_ignores_proxy_and_does_not_follow_redirects(self):
        import threading
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        stop = self.api('cooperative_stop'); self.start(); self.mode = 'redirect'; received = []
        class Proxy(BaseHTTPRequestHandler):
            def log_message(self, *args): pass
            def do_POST(self):
                received.append(self.path); self.send_response(403); self.end_headers()
        server = ThreadingHTTPServer(('127.0.0.1', 0), Proxy)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close); self.addCleanup(server.shutdown)
        with mock.patch.dict(os.environ, {'HTTP_PROXY':f'http://127.0.0.1:{server.server_port}',
                                         'http_proxy':f'http://127.0.0.1:{server.server_port}',
                                         'NO_PROXY':'', 'no_proxy':''}):
            with self.assertRaises(self.identity.ControlRefused):
                stop(self.manifest, self.nonce, self.child, self.digest, self.observed, time.monotonic()+2)
        self.assertEqual(received, [])
        self.assertEqual(self.paths, ['/_agentdesk/test-control'])

    def test_changed_credential_metadata_refuses_before_any_control_request(self):
        stop = self.api('cooperative_stop'); self.start()
        (self.root/'credential'/NAMES[0]).write_bytes(b'SYNTHETIC_REPLACED')
        with self.assertRaises(self.identity.ControlRefused):
            stop(self.manifest, self.nonce, self.child, self.digest, self.observed, time.monotonic()+2)
        self.assertEqual(self.requests, [])

    def test_observed_inventory_cannot_carry_extra_private_fields(self):
        stop = self.api('cooperative_stop'); self.start()
        observed = copy.deepcopy(self.observed); observed[0]['private'] = 'SYNTHETIC_UNEXPECTED'
        with mock.patch.object(self.identity, '_post_control') as request:
            with self.assertRaises(self.identity.ControlRefused):
                stop(self.manifest, self.nonce, self.child, self.digest, observed, time.monotonic()+2)
            request.assert_not_called()

    def test_cross_rust_public_synthetic_key_and_request_vector(self):
        derive = self.api('_control_key'); mac = self.api('_control_mac')
        for name, value in zip(sorted(NAMES), (b'fixture-announce', b'fixture-claude', b'fixture-codex', b'fixture-notify')):
            (self.root/'credential'/name).write_bytes(value)
        observed = [{**item, **self.identity._credential(self.root, item['name'])[0]} for item in self.observed]
        key = derive(self.root, observed)
        self.assertEqual(key.hex(), 'b5608c372adc5486f387dfa64c820fb55585348284b575a6d206e6de12293d8a')
        request = {'schema':1, 'action':'stop', 'root':'/tmp/adk-control-vector',
                   'instance_id':'12345678-1234-4234-8234-123456789abc',
                   'nonce':'22345678-1234-4234-8234-123456789abc',
                   'challenge':'32345678-1234-4234-8234-123456789abc'}
        self.assertEqual(mac(key, b'request', request),
                         'ab2729a91124a68241bd4ddd2ee230cc3c6291238b957c382512951fba238249')

    def test_key_derivation_uses_exact_raw_file_bytes_including_final_newline(self):
        derive = self.api('_control_key')
        path = self.root/'credential'/NAMES[0]
        path.write_bytes(path.read_bytes()+b'\n')
        observed = [{**item, **self.identity._credential(self.root, item['name'])[0]} for item in self.observed]
        self.assertEqual(derive(self.root, observed), self.key())


if __name__ == '__main__': unittest.main()
