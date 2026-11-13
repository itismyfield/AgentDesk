"""Authenticate explicit app identities and bind non-ready runtime witnesses."""
from contextlib import contextmanager
import hashlib
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
import time
import urllib.error
import urllib.request

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
