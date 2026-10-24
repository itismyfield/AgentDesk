"""Owned socket fixtures and a completion watchdog for deploy call mutation tests."""

import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def listen(ready: Path) -> None:
    release = ready.with_suffix(".release")
    expired = ready.with_suffix(".expired")
    deadline = time.monotonic() + 180
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as server:
        server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind(("127.0.0.1", 0))
        server.listen(4)
        server.settimeout(0.1)
        ready.write_text(str(server.getsockname()[1]), encoding="ascii")
        connection = None
        while not release.exists() and time.monotonic() < deadline:
            try:
                if connection is None:
                    connection, _ = server.accept()
                    connection.settimeout(0.1)
                elif not connection.recv(65536):
                    break
            except socket.timeout:
                continue
            except ConnectionResetError:
                break
        if time.monotonic() >= deadline:
            expired.touch()
        if connection is not None:
            connection.close()


def measure(case: Path) -> int:
    release = case.with_suffix(".release")
    expired = case.with_suffix(".expired")
    started = time.monotonic()
    # Completion is the oracle; the watchdog is outside the production call's 10/15 s cap.
    # Holding fixture EOF until release keeps removed-cap and inherited-pipe mutants observable.
    with tempfile.TemporaryFile(mode="w+") as output:
        process = subprocess.Popen(["bash", str(case)], stdout=output, stderr=subprocess.STDOUT,
                                   text=True, start_new_session=True)
        timed_out = False
        try:
            try:
                process.wait(timeout=60)
            except subprocess.TimeoutExpired:
                timed_out = True
            release.touch()
            try:
                return_code = process.wait(timeout=60)
            except subprocess.TimeoutExpired:
                return_code = 125
        finally:
            release.touch()
            # The session belongs to this case, including children that retain its output.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=30)
        output.seek(0)
        sys.stdout.write(output.read())
    failed = timed_out or expired.exists() or return_code != 0
    print(f"measure label={case} elapsed={time.monotonic() - started:.3f}s rc={return_code} "
          f"pid={process.pid} fixture_expired={expired.exists()} timeout_assertion={'FAILED' if failed else 'ok'}")
    return int(failed)


if __name__ == "__main__":
    action, target = sys.argv[1:]
    if action == "listen":
        listen(Path(target))
    elif action == "measure":
        sys.exit(measure(Path(target)))
    else:
        raise SystemExit(f"unknown fixture action: {action}")
