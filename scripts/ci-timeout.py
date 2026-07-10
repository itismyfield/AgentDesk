#!/usr/bin/env python3
"""Run a command with a wall-clock timeout and return 124 on expiry."""

from __future__ import annotations

import os
import signal
import subprocess
import sys


def main() -> int:
    if len(sys.argv) < 3:
        print("usage: ci-timeout.py SECONDS COMMAND [ARG...]", file=sys.stderr)
        return 2

    try:
        timeout = float(sys.argv[1])
    except ValueError:
        print(f"invalid timeout seconds: {sys.argv[1]!r}", file=sys.stderr)
        return 2

    command = sys.argv[2:]
    proc = subprocess.Popen(command, start_new_session=True)
    try:
        return proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait()
        return 124


if __name__ == "__main__":
    raise SystemExit(main())
