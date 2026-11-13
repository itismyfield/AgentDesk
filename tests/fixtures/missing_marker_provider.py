#!/usr/bin/python3
"""Offline stdin provider for a new managed execution after marker-loss retirement."""
import base64
import datetime
import json
import os
import pathlib
import sys
import termios
import time
import tty
import urllib.parse
import urllib.request
import uuid

if sys.argv[1:] == ['--version']:
    print('2.1.141 (Claude Code)')
    raise SystemExit(0)

context_path = pathlib.Path(os.environ['AGENTDESK_BINDING_CONTEXT'])
context = json.loads(context_path.read_text())
session = sys.argv[sys.argv.index('--session-id') + 1]
source = pathlib.Path(os.environ['ADK_MM_PROJECT_DIR']) / (session + '.jsonl')
source.parent.mkdir(parents=True, exist_ok=True)
source.touch(exist_ok=False)
envelope = base64.urlsafe_b64encode(json.dumps({'context': {'Captured': context},
    'observed': {'tmux': None, 'tmux_pane': None, 'provider_pid': str(os.getpid())}}).encode()).decode().rstrip('=')
url = os.environ['ADK_MM_HOOK_URL'] + '/hooks/claude/SessionStart?session_id=' + urllib.parse.quote(context['tmux_session'])
assert urllib.parse.urlparse(url).hostname == '127.0.0.1'
payload = {'hook_event_name': 'SessionStart', 'source': 'startup', 'session_id': session,
           'transcript_path': str(source), 'cwd': os.getcwd()}
published = datetime.datetime.now(datetime.timezone.utc)
request = urllib.request.Request(url, json.dumps(payload).encode(),
    {'content-type': 'application/json', 'x-agentdesk-binding-context': envelope,
     'x-agentdesk-relay-request-id': str(uuid.uuid4()),
     'x-agentdesk-relay-published-at': published.isoformat(),
     'x-agentdesk-relay-deadline': (published + datetime.timedelta(seconds=60)).isoformat()}, method='POST')
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
for attempt in range(100):
    try:
        with opener.open(request, timeout=3) as response:
            assert response.status == 202
            response.read()
        break
    except Exception:
        if attempt == 99:
            raise
        time.sleep(.1)

original = termios.tcgetattr(0)
tty.setraw(0)
try:
    os.write(1, '\x1b[2J\x1b[H\x1b[?2004hClaude Code v2.1.141\r\n──────────────────────────────\r\n❯ \r\n──────────────────────────────\r\n'.encode())
    data = b''
    while b'\r' not in data:
        data += os.read(0, 4096)
    prompt = data.split(b'\r')[0].replace(b'\x1b[200~', b'').replace(b'\x1b[201~', b'').decode()
    pathlib.Path(os.environ['ADK_MM_RECEIVED']).write_text(prompt)
    rows = [
        {'type': 'user', 'sessionId': session, 'uuid': str(uuid.uuid4()),
         'message': {'role': 'user', 'content': prompt}},
        {'type': 'assistant', 'sessionId': session, 'uuid': str(uuid.uuid4()), 'apiBlockIndex': 0,
         'message': {'id': str(uuid.uuid4()), 'role': 'assistant', 'stop_reason': 'end_turn',
                     'content': [{'type': 'text', 'text': 'Recovered managed execution reply.'}]}},
        {'type': 'system', 'subtype': 'turn_duration', 'sessionId': session},
        {'type': 'result', 'subtype': 'success', 'result': 'Recovered managed execution reply.', 'session_id': session},
    ]
    with source.open('a') as output:
        for row in rows:
            row['timestamp'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            output.write(json.dumps(row) + '\n')
        output.flush()
        os.fsync(output.fileno())
    os.write(1, b'\r\nRecovered managed execution reply.\r\n')
    while True:
        time.sleep(1)
finally:
    termios.tcsetattr(0, termios.TCSADRAIN, original)
