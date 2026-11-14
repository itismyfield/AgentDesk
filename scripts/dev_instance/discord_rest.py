"""Direct Discord REST with root credentials and authority only over new test threads."""
import http.client
import json
from pathlib import Path
import time
import urllib.error
import urllib.request

from .files import InstanceError
from .identity import _credential, _transport, recheck_credentials
from .profile import snowflake


class Client:
    def __init__(self, admission, deadline, request=None):
        self.admission, self.deadline = admission, deadline
        self._request = request or self._http
        self.threads = set()

    def _http(self, role, method, path, body=None):
        try:
            root = Path(self.admission.manifest['root'])
            recheck_credentials(root, self.admission.observed)
            _, token = _credential(root, role+'_bot_token', read=True)
            headers = {'Authorization':'Bot '+token.decode('ascii'), 'Accept':'application/json',
                       'Content-Type':'application/json', 'User-Agent':'AgentDesk isolated instance Driver'}
            request = urllib.request.Request('https://discord.com/api/v10'+path, headers=headers,
                method=method, data=None if body is None else json.dumps(body).encode())
            remaining = self.deadline-time.monotonic()
            if remaining <= 0:raise TimeoutError()
            with _transport(self.deadline) as opener, opener.open(request,timeout=min(5,remaining)) as response:
                raw = response.read(1024*1024+1)
                if response.status not in (200,201) or len(raw)>1024*1024:
                    raise InstanceError('Discord response refused')
                result = json.loads(raw)
                if type(result) not in (dict,list) or time.monotonic() >= self.deadline:
                    raise InstanceError('Discord response refused')
                return result
        except urllib.error.HTTPError as error:
            error.close()
            raise InstanceError('Discord REST request refused') from None
        except (OSError, ValueError, urllib.error.URLError, http.client.HTTPException):
            raise InstanceError('Discord REST request unavailable') from None

    def create_thread(self, parent, name):
        if parent not in self.admission.manifest['profile']['channels']:
            raise InstanceError('thread parent is outside the test allowlist')
        result = self._request('notify','POST',f'/channels/{snowflake(parent)}/threads',
                               {'name':name,'type':11,'auto_archive_duration':60})
        if (type(result) is not dict or result.get('type') != 11 or result.get('parent_id') != parent
                or result.get('owner_id') != self.admission.manifest['profile']['bots']['driver']):
            raise InstanceError('created thread identity mismatch')
        thread = snowflake(result.get('id'))
        if thread in self.admission.manifest['profile']['channels']:
            raise InstanceError('created thread identity mismatch')
        self.threads.add(thread)
        return thread

    def send_prompt(self, thread, text):
        if thread not in self.threads:raise InstanceError('foreign thread refused')
        result = self._request('notify','POST',f'/channels/{thread}/messages',
                               {'content':text,'allowed_mentions':{'parse':[]}})
        if (type(result) is not dict or result.get('channel_id') != thread
                or result.get('author',{}).get('id') != self.admission.manifest['profile']['bots']['driver']):
            raise InstanceError('input message identity mismatch')
        return snowflake(result.get('id'))

    def messages(self, thread, after):
        if thread not in self.threads:raise InstanceError('foreign thread refused')
        result = self._request('announce','GET',f'/channels/{thread}/messages?after={snowflake(after)}&limit=100')
        if type(result) is not list or any(type(row) is not dict or row.get('channel_id') != thread for row in result):
            raise InstanceError('observed message channel mismatch')
        return result
