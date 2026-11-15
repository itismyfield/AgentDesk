"""Register a bookkeeping dispatch only on an admitted instance API."""
import json
import time
import urllib.error
import urllib.request
import uuid

from .discord_http import USER_AGENT
from .files import InstanceError
from .identity import _transport
from .manifest import API_PORTS


def _uuid(value):
    return type(value) is str and str(uuid.UUID(value)) == value and uuid.UUID(value).int != 0


class Client:
    def __init__(self, admission, deadline, request=None):
        api = admission.manifest['api']
        port = api['port']
        expected = {'host': '127.0.0.1', 'port': port, 'self_url': f'http://127.0.0.1:{port}'}
        if type(port) is not int or port not in API_PORTS or api != expected or admission.run['port'] != port:
            raise InstanceError('dispatch endpoint is outside the admitted instance')
        self.endpoint, self.deadline = expected['self_url'], deadline
        self.instance_id = admission.manifest['instance_id']
        self.request = request or self._http
        self.requests_started = 0

    def _http(self, url, body, deadline):
        remaining = deadline-time.monotonic()
        if remaining <= 0:raise InstanceError('dispatch registration deadline exhausted')
        message = urllib.request.Request(url, method='POST', data=json.dumps(body).encode(),
            headers={'Accept': 'application/json', 'Content-Type': 'application/json', 'User-Agent': USER_AGENT})
        try:
            with _transport(deadline) as opener, opener.open(message, timeout=min(5, remaining)) as response:
                data = response.read(65537)
                if response.status not in (200, 201) or len(data)>65536 or time.monotonic()>=deadline:
                    raise InstanceError('dispatch registration response refused')
                result = json.loads(data)
                if type(result) is not dict:raise InstanceError('dispatch registration response refused')
                return result
        except urllib.error.HTTPError as error:
            error.close()
            raise InstanceError('dispatch registration request refused') from None
        except (OSError, ValueError, urllib.error.URLError):
            raise InstanceError('dispatch registration request refused') from None

    def _post(self, path, body):
        if path not in ('/api/kanban-cards', '/api/dispatches') or time.monotonic()>=self.deadline:
            raise InstanceError('dispatch registration request refused')
        self.requests_started += 1
        return self.request(self.endpoint+path, body, self.deadline)

    def create(self, provider):
        if provider not in ('claude', 'codex'):raise InstanceError('unknown dispatch provider')
        card = self._post('/api/kanban-cards', {'title': 'Instance turn '+self.instance_id})['card']
        if type(card) is not dict or not _uuid(card.get('id')):raise InstanceError('invalid registered card')
        card_id = card['id']
        # The existing API allocates the dispatch ID; use its active receipt for DISPATCH.
        dispatch = self._post('/api/dispatches', {'kanban_card_id': card_id,
            'to_agent_id': provider, 'dispatch_type': 'scope-assessment',
            'title': 'Isolated instance marker response',
            'context': {'instance_id': self.instance_id}, 'skip_outbox': True})['dispatch']
        if (type(dispatch) is not dict or not _uuid(dispatch.get('id')) or dispatch.get('kanban_card_id') != card_id
                or dispatch.get('to_agent_id') != provider or dispatch.get('dispatch_type') != 'scope-assessment'
                or dispatch.get('status') not in ('pending', 'dispatched')):
            raise InstanceError('invalid registered dispatch')
        return {'card_id': card_id, 'dispatch_id': dispatch['id']}
