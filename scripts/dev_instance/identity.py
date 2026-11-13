"""Authenticate explicit app identities and bind non-ready runtime witnesses."""
from contextlib import contextmanager
import hashlib
import hmac
import http.client
import ipaddress
import json
import os
from pathlib import Path
import re
import queue
import socket
import threading
import stat
import subprocess
import time
import urllib.error
import urllib.request
import uuid

from .files import InstanceError, LIVE_CREDENTIAL_NAMES, _open_parent, _token_bytes

RUN_ENV = 'AGENTDESK_TEST_RUN'
_FIELDS = ('device', 'inode', 'size', 'mtime_ns', 'ctime_ns')


def parse_admission(frame):
    if (type(frame) is not dict or set(frame) != {'kind', 'schema', 'identities'}
            or frame['kind'] != 'admission' or type(frame['schema']) is not int or frame['schema'] != 1
            or type(frame['identities']) is not dict or set(frame['identities']) != LIVE_CREDENTIAL_NAMES):
        raise InstanceError('invalid explicit identity admission')
    values = list(frame['identities'].values())
    if any(type(value) is not str or not re.fullmatch(r'[1-9][0-9]{0,19}', value)
           or int(value) > 2**64-1 for value in values) or len(set(values)) != 4:
        raise InstanceError('four distinct explicit application IDs are required')
    return frame['identities']


def _metadata(metadata):
    if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.geteuid()
            or stat.S_IMODE(metadata.st_mode) != 0o600 or metadata.st_nlink != 1
            or metadata.st_size not in range(1, 4098)):
        raise InstanceError('identity credential is not a private owned regular file')
    return dict(zip(_FIELDS, (metadata.st_dev, metadata.st_ino, metadata.st_size,
                              metadata.st_mtime_ns, metadata.st_ctime_ns)))


def _credential(root, name, read=False):
    parent, leaf = _open_parent(root, 'credential/'+name, private=True)
    try:
        descriptor = os.open(leaf, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
        with os.fdopen(descriptor, 'rb') as source:
            before = _metadata(os.fstat(source.fileno()))
            value = _token_bytes(source.read(4098)) if read else None
            if (_metadata(os.fstat(source.fileno())) != before
                    or _metadata(os.stat(leaf, dir_fd=parent, follow_symlinks=False)) != before):
                raise InstanceError('identity credential changed during observation')
            return before, value
    finally:
        os.close(parent)


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


@contextmanager
def _transport(deadline):
    guards = []
    def remaining():
        value = deadline-time.monotonic()
        if value <= 0: raise TimeoutError('request deadline')
        return value
    def connect(address, timeout=None, source_address=None):
        host, port = address
        try:
            ip = ipaddress.ip_address(host)
            addresses = [(socket.AF_INET if ip.version == 4 else socket.AF_INET6,
                          socket.SOCK_STREAM, 0, '', (host, port))]
        except ValueError:
            result = queue.Queue(1)
            def resolve():
                try: result.put(socket.getaddrinfo(host, port, type=socket.SOCK_STREAM))
                except OSError: result.put([])
            threading.Thread(target=resolve, daemon=True).start()
            try: addresses = result.get(timeout=remaining())
            except queue.Empty: raise TimeoutError('DNS deadline') from None
        for family, kind, protocol, _, endpoint in addresses:
            channel = socket.socket(family, kind, protocol)
            try:
                channel.settimeout(remaining())
                channel.connect(endpoint)
                remaining()
                duplicate = channel.dup()
                def expire(owned=duplicate):
                    try: owned.shutdown(socket.SHUT_RDWR)
                    except OSError: pass
                timer = threading.Timer(max(0, deadline-time.monotonic()), expire)
                timer.daemon = True
                try:
                    timer.start()
                except RuntimeError:
                    duplicate.close()
                    raise InstanceError('deadline monitor unavailable') from None
                guards.append((timer, duplicate))
                return channel
            except OSError:
                channel.close()
            except BaseException:
                channel.close()
                raise
        raise ConnectionError('endpoint connection failed')
    def factory(kind):
        def create(*args, **kwargs):
            connection = kind(*args, **kwargs)
            connection._create_connection = connect
            return connection
        return create
    class HTTP(urllib.request.HTTPHandler):
        def http_open(self, request):
            return self.do_open(factory(http.client.HTTPConnection), request)
    class HTTPS(urllib.request.HTTPSHandler):
        def https_open(self, request):
            return self.do_open(factory(http.client.HTTPSConnection), request, context=self._context)
    try:
        yield urllib.request.build_opener(urllib.request.ProxyHandler({}), _NoRedirect(), HTTP(), HTTPS())
    finally:
        for timer, duplicate in guards:
            timer.cancel()
            timer.join()
            duplicate.close()


def request_json(url, deadline, token=None):
    remaining = deadline-time.monotonic()
    if remaining <= 0:
        raise InstanceError('identity or health deadline exhausted')
    headers = {'Accept': 'application/json'}
    if token is not None:
        try:
            headers['Authorization'] = 'Bot '+token.decode('ascii')
        except UnicodeError:
            raise InstanceError('identity credential encoding is invalid') from None
    try:
        with _transport(deadline) as opener, opener.open(
                urllib.request.Request(url, headers=headers), timeout=min(5, remaining)) as response:
            data = response.read(16385)
            if response.status != 200 or len(data) > 16384:
                raise InstanceError('identity or health response is invalid')
            result = json.loads(data)
            if time.monotonic() >= deadline:
                raise InstanceError('identity or health deadline exhausted')
            if type(result) is not dict:
                raise InstanceError('identity or health response is invalid')
            return result
    except urllib.error.HTTPError as error:
        error.close()
        raise InstanceError('identity or health request was refused') from None
    except (urllib.error.URLError, TimeoutError, OSError):
        raise ConnectionError('identity or health endpoint unavailable') from None
    except (ValueError, UnicodeError, http.client.HTTPException):
        raise InstanceError('identity or health response is invalid') from None


def probe(root, frame, deadline, request=None, declared_roles=None):
    expected = parse_admission(frame)
    if (type(declared_roles) is not dict or any(expected[role+'_bot_token'] != declared_roles.get(role)
                                              for role in ('claude', 'codex'))):
        raise InstanceError('identity admission differs from declared provider roles')
    recheck_credentials(root, [])
    request = request or (lambda token, end: request_json(
        'https://discord.com/api/v10/users/@me', end, token).get('id'))
    observed, tokens = [], set()
    for name in sorted(expected):
        metadata, token = _credential(root, name, read=True)
        if token in tokens:
            raise InstanceError('identity credentials must be distinct')
        tokens.add(token)
        try:
            actual = request(token, deadline)
        except (ConnectionError, OSError, ValueError):
            raise InstanceError('application identity probe failed') from None
        if actual != expected[name] or type(actual) is not str:
            raise InstanceError('authenticated application identity mismatch')
        observed.append({'name': name, 'application_id': actual, **metadata})
    recheck_credentials(root, observed)
    return observed


def recheck_credentials(root, observed):
    parent, _ = _open_parent(root, 'credential/entry', private=True)
    try:
        if set(os.listdir(parent)) != LIVE_CREDENTIAL_NAMES:
            raise InstanceError('explicit live mode requires exactly four credentials')
    finally:
        os.close(parent)
    for item in observed:
        metadata, _ = _credential(root, item['name'])
        if metadata != {key: item[key] for key in _FIELDS}:
            raise InstanceError('observed credential was replaced')


def run_payload(manifest, source, digest, observed):
    metadata = os.fstat(source.fileno())
    source.seek(0)
    if (not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o022
            or hashlib.file_digest(source, 'sha256').hexdigest() != digest):
        raise InstanceError('pinned executable differs from the build artifact')
    return {'schema': 1, 'head': manifest['head'], 'worktree': manifest['worktree'],
            'binary': {'fd': source.fileno(), 'device': metadata.st_dev,
                       'inode': metadata.st_ino, 'sha256': digest}, 'identities': observed}


def verify_health(body, manifest, nonce, pid, digest, observed):
    expected = {'schema': 1, 'instance_id': manifest['instance_id'], 'head': manifest['head'],
                'pid': pid, 'binary_sha256': digest, 'nonce': nonce, 'port': manifest['api']['port'],
                'identities': observed, 'live_ready': False}
    try:
        canonical = lambda value: json.dumps(value, sort_keys=True, allow_nan=False, separators=(',', ':'))
        if canonical(body.get('test_instance')) != canonical(expected):
            raise InstanceError('same-run bound health witness mismatch')
    except (ValueError, TypeError, AttributeError):
        raise InstanceError('same-run bound health witness mismatch') from None
    return expected


def wait_health(manifest, nonce, pid, digest, observed, deadline, alive=lambda: True):
    while time.monotonic() < deadline:
        if not alive():
            raise InstanceError('runtime exited before bound health verification')
        recheck_credentials(Path(manifest['root']), observed)
        try:
            body = request_json(f"http://127.0.0.1:{manifest['api']['port']}/api/health", deadline)
        except ConnectionError:
            time.sleep(min(.02, max(0, deadline-time.monotonic())))
            continue
        result = verify_health(body, manifest, nonce, pid, digest, observed)
        recheck_credentials(Path(manifest['root']), observed)
        return result
    raise InstanceError('bound health did not become verifiable before deadline')


class ControlRefused(Exception):
    """A cooperative stop lacks authenticated acknowledgement or original-child exit."""


class _ControlEOF(ControlRefused):
    pass


def _control_key(root, observed):
    try:
        if (type(observed) is not list or len(observed) != 4
                or any(type(item) is not dict or set(item) != {'name','application_id', *_FIELDS} for item in observed)
                or {item['name'] for item in observed} != LIVE_CREDENTIAL_NAMES
                or any(type(item[field]) is not int or item[field] < 0
                       for item in observed for field in _FIELDS)):
            raise ControlRefused('control credential inventory is invalid')
        recheck_credentials(root, observed)
        expected = {item['name']: {field:item[field] for field in _FIELDS} for item in observed}
        digest = hashlib.sha256(b'agentdesk-test-control-key-v1\0')
        for name in sorted(expected):
            parent, leaf = _open_parent(root, 'credential/'+name, private=True)
            try:
                descriptor = os.open(leaf, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
                with os.fdopen(descriptor, 'rb') as source:
                    before = _metadata(os.fstat(source.fileno()))
                    raw = source.read(4097)
                    if (before != expected[name] or not 1 <= len(raw) <= 4096
                            or _metadata(os.fstat(source.fileno())) != before
                            or _metadata(os.stat(leaf, dir_fd=parent, follow_symlinks=False)) != before):
                        raise ControlRefused('control credential identity changed')
                    encoded = name.encode('ascii')
                    digest.update(len(encoded).to_bytes(4, 'big')+encoded+len(raw).to_bytes(4, 'big')+raw)
            finally:
                os.close(parent)
        recheck_credentials(root, observed)
        return digest.digest()
    except (InstanceError, OSError, KeyError, TypeError, UnicodeError):
        raise ControlRefused('control credential identity is unverified') from None


def _control_bytes(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False, allow_nan=False).encode('utf-8')


def _control_mac(key, direction, value):
    return hmac.new(key, b'agentdesk-test-control-v1/'+direction+b'\0'+_control_bytes(value), hashlib.sha256).hexdigest()


def _control_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value: raise ValueError('duplicate control field')
        value[key] = item
    return value


def _control_remaining(deadline):
    remaining = deadline-time.monotonic()
    if remaining <= 0: raise ControlRefused('control deadline exhausted')
    return remaining


def _post_control(manifest, request, deadline):
    try:
        port = manifest['api']['port']
        if type(port) is not int or not 1 <= port <= 65535:
            raise ControlRefused('control endpoint is invalid')
        url = f'http://127.0.0.1:{port}/_agentdesk/test-control'
        message = urllib.request.Request(url, data=_control_bytes(request), method='POST',
                                         headers={'Accept':'application/json', 'Content-Type':'application/json'})
        with _transport(deadline) as opener, opener.open(message, timeout=min(5, _control_remaining(deadline))) as response:
            data = response.read(16385)
            if response.status != 200 or len(data) > 16384:
                raise ControlRefused('control response is invalid')
            _control_remaining(deadline)
            return json.loads(data, object_pairs_hook=_control_object)
    except urllib.error.HTTPError as error:
        error.close()
        raise ControlRefused('control request was refused') from None
    except urllib.error.URLError as error:
        if isinstance(error.reason, (ConnectionResetError, BrokenPipeError)):
            raise _ControlEOF('control channel closed') from None
        raise ControlRefused('control endpoint unavailable') from None
    except (http.client.RemoteDisconnected, http.client.IncompleteRead, ConnectionResetError, BrokenPipeError):
        raise _ControlEOF('control channel closed') from None
    except (InstanceError, TimeoutError, OSError, ValueError, UnicodeError, http.client.HTTPException):
        raise ControlRefused('control response is unverified') from None


def _verify_control(reply, key, action, challenge, manifest, nonce, pid, digest, observed, epoch=None):
    if (type(reply) is not dict or set(reply) != {'schema','action','challenge','witness','control_ready','epoch','accepted','proof'}
            or type(reply['schema']) is not int or reply['schema'] != 1 or reply['action'] != action
            or reply['challenge'] != challenge or reply['control_ready'] is not True
            or reply['accepted'] is not (action == 'stop') or type(reply['epoch']) is not int
            or not 1 <= reply['epoch'] <= 2**32-1 or (epoch is not None and reply['epoch'] != epoch)
            or type(reply['proof']) is not str or not re.fullmatch(r'[0-9a-f]{64}', reply['proof'])):
        raise ControlRefused('control reply fields are unverified')
    unsigned = {field:value for field,value in reply.items() if field != 'proof'}
    if not hmac.compare_digest(_control_mac(key, b'response', unsigned), reply['proof']):
        raise ControlRefused('control reply authentication failed')
    try:
        verify_health({'test_instance':reply['witness']}, manifest, nonce, pid, digest, observed)
        recheck_credentials(Path(manifest['root']), observed)
    except (InstanceError, OSError):
        raise ControlRefused('control run witness changed') from None
    return reply


def _control_request(manifest, nonce, action, key):
    request = {'schema':1, 'action':action, 'root':manifest['root'], 'instance_id':manifest['instance_id'],
               'nonce':nonce, 'challenge':str(uuid.uuid4())}
    request['proof'] = _control_mac(key, b'request', request)
    return request


def control_probe(manifest, nonce, pid, digest, observed, deadline):
    try:
        key = _control_key(Path(manifest['root']), observed)
        request = _control_request(manifest, nonce, 'probe', key)
        reply = _post_control(manifest, request, deadline)
        return _verify_control(reply, key, 'probe', request['challenge'], manifest, nonce, pid, digest, observed)
    except (InstanceError, OSError, ValueError, TypeError, KeyError):
        raise ControlRefused('control probe is unverified') from None


def _control_ack(root):
    parent, name = _open_parent(root, 'runtime/instance-control-ack.json', private=True)
    try:
        descriptor = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
        with os.fdopen(descriptor, 'rb') as source:
            metadata = os.fstat(source.fileno())
            fingerprint = lambda value: (value.st_dev, value.st_ino, value.st_size, value.st_mtime_ns, value.st_ctime_ns)
            if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.geteuid()
                    or stat.S_IMODE(metadata.st_mode) != 0o600 or metadata.st_nlink != 1
                    or not 1 <= metadata.st_size <= 16384):
                raise ControlRefused('control acknowledgement is not private')
            data = source.read(16385)
            leaf = os.stat(name, dir_fd=parent, follow_symlinks=False)
            if (fingerprint(os.fstat(source.fileno())) != fingerprint(metadata)
                    or fingerprint(leaf) != fingerprint(metadata) or not stat.S_ISREG(leaf.st_mode)
                    or leaf.st_uid != os.geteuid() or stat.S_IMODE(leaf.st_mode) != 0o600 or leaf.st_nlink != 1):
                raise ControlRefused('control acknowledgement changed')
            return json.loads(data, object_pairs_hook=_control_object), {
                'device':metadata.st_dev, 'inode':metadata.st_ino, 'sha256':hashlib.sha256(data).hexdigest()}
    finally:
        os.close(parent)


def cooperative_stop(manifest, nonce, child, digest, observed, deadline):
    try:
        if child.poll() is not None:
            raise ControlRefused('control target exited before authentication')
        snapshot = control_probe(manifest, nonce, child.pid, digest, observed, deadline)
        key = _control_key(Path(manifest['root']), observed)
        request = _control_request(manifest, nonce, 'stop', key)
        try:
            reply = _post_control(manifest, request, deadline)
        except _ControlEOF:
            pass
        else:
            _verify_control(reply, key, 'stop', request['challenge'], manifest, nonce, child.pid,
                            digest, observed, epoch=snapshot['epoch'])
        acknowledgement, receipt = _control_ack(Path(manifest['root']))
        _verify_control(acknowledgement, key, 'stop', request['challenge'], manifest, nonce, child.pid,
                        digest, observed, epoch=snapshot['epoch'])
        exit_code = child.wait(timeout=_control_remaining(deadline))
        if type(exit_code) is not int:
            raise ControlRefused('original child exit is unverified')
        return {'control_ack':True, 'gateway_wait_ack':True, 'exit_code':exit_code, 'pid':child.pid,
                'epoch':snapshot['epoch'], 'nonce':nonce, 'challenge':request['challenge'], 'ack':receipt}
    except (InstanceError, OSError, ValueError, TypeError, KeyError, subprocess.TimeoutExpired):
        raise ControlRefused('cooperative stop is unverified') from None
