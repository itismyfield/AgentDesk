#!/usr/bin/python3
import fcntl
import json
import os
from pathlib import Path
import select
import sys
import time

if '--version' in sys.argv or '-v' in sys.argv:
    print('99.1.0')
    sys.exit(0)

root = Path(os.environ['ADK_U4_CASE_DIR'])
cfg = json.loads((root / 'case.json').read_text())


def mark(name):
    (root / name).write_text('1')


def wait(name):
    deadline = time.monotonic() + 40
    while not (root / name).exists():
        if time.monotonic() > deadline:
            os._exit(91)
        time.sleep(.005)


with (root / 'starts').open('a+') as count:
    fcntl.flock(count, fcntl.LOCK_EX)
    count.seek(0)
    attempt = len(count.readlines()) + 1
    count.write(str(os.getpid()) + '\n')
    count.flush()
(root / f'argv-{attempt}.json').write_text(json.dumps(sys.argv))

records = cfg['records']
exit_code = cfg.get('exit', 1)
if attempt > 1 and 'retry_records' in cfg:
    records = cfg['retry_records']
    exit_code = 0

if cfg.get('warm'):
    incoming = os.open(cfg['fifo'], os.O_RDWR | os.O_NONBLOCK)
    mark('fifo_open')
    if not select.select([incoming], [], [], 20)[0]:
        os._exit(92)
    os.read(incoming, 8192)
    os.close(incoming)
    mark('submitted')
    with open(cfg['output'], 'a') as output:
        for record in records:
            output.write(record + '\n')
        output.flush()
        os.fsync(output.fileno())
        mark('ready')
        wait('warm_end')
        if cfg['warm'] == 'rotate':
            replacement = Path(cfg['output'] + '.replacement')
            replacement.write_text('{"type":"result","subtype":"success","result":"replacement"}\n')
            replacement.replace(cfg['output'])
        elif cfg['warm'] == 'death':
            Path(cfg['pane_dead']).write_text('1')
    mark('exiting')
    sys.exit(0)

file_mode = cfg.get('file', False)
if file_mode:
    output_path = sys.argv[sys.argv.index('--output-file') + 1]
    out_fd = os.open(output_path, os.O_WRONLY | os.O_APPEND)
else:
    sys.stdin.read()
    out_fd = 1

if cfg.get("delay_records"):
    mark("records_waiting")
    wait("record_go")

for record in records:
    os.write(out_fd, (record + '\n').encode())
if cfg.get('stderr', '') and attempt == 1:
    os.write(2, cfg['stderr'].encode())

if cfg.get('barriers'):
    if os.fork() == 0:
        os.close(0)
        os.close(2)
        wait('stdout_go')
        if file_mode:
            os.write(out_fd, b'{"type":"assistant","message":{"content":[{"type":"text","text":"late child output"}]}}\n')
        os.close(out_fd)
        mark('stdout_closed')
        os._exit(0)
    if os.fork() == 0:
        os.close(0)
        os.close(1)
        if file_mode:
            os.close(out_fd)
        wait('stderr_go')
        if cfg.get('stderr_append'):
            try:
                os.write(2, b'late descendant append\n')
                mark('stderr_append_open')
            except BrokenPipeError:
                mark('stderr_reader_closed')
        os.close(2)
        mark('stderr_closed')
        os._exit(0)
    os.close(out_fd)
    os.close(2)
    mark('ready')
    wait('exit_go')
else:
    if file_mode:
        os.close(out_fd)
    mark('ready')
mark('exiting')
os._exit(exit_code)
