#!/usr/bin/env python3
"""Run a command with a wall-clock timeout and return 124 on expiry."""

from __future__ import annotations

import os
import signal
import subprocess
import sys

TERMINATION_GRACE_SECONDS = 5


class _ForwardedSignal(Exception):
    def __init__(self, signum: int) -> None:
        super().__init__(signum)
        self.signum = signum


def _send_process_signal(
    proc: subprocess.Popen[bytes], signum: int, *, force: bool = False
) -> None:
    """Signal the child process group, or its process on platforms without killpg."""
    try:
        if hasattr(os, "killpg"):
            os.killpg(proc.pid, signal.SIGKILL if force else signum)
        elif force:
            proc.kill()
        else:
            proc.terminate()
    except ProcessLookupError:
        pass


def _wait_after_signal(proc: subprocess.Popen[bytes]) -> None:
    try:
        proc.wait(timeout=TERMINATION_GRACE_SECONDS)
    except subprocess.TimeoutExpired:
        _send_process_signal(proc, signal.SIGTERM, force=True)
        proc.wait()


def _terminate_and_wait(proc: subprocess.Popen[bytes]) -> None:
    _send_process_signal(proc, signal.SIGTERM)
    _wait_after_signal(proc)


def run_command(timeout: float, command: list[str]) -> int:
    proc = subprocess.Popen(command, start_new_session=True)
    previous_handlers: dict[int, signal.Handlers] = {}

    def forward_signal(signum: int, _frame: object) -> None:
        _send_process_signal(proc, signum)
        raise _ForwardedSignal(signum)

    try:
        for signum in (signal.SIGTERM, signal.SIGINT):
            previous_handlers[signum] = signal.signal(signum, forward_signal)

        try:
            return proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            _terminate_and_wait(proc)
            return 124
        except _ForwardedSignal as forwarded:
            _wait_after_signal(proc)
            return 128 + forwarded.signum
    finally:
        for signum, handler in previous_handlers.items():
            signal.signal(signum, handler)


def main() -> int:
    if len(sys.argv) < 3:
        print("usage: ci-timeout.py SECONDS COMMAND [ARG...]", file=sys.stderr)
        return 2

    try:
        timeout = float(sys.argv[1])
    except ValueError:
        print(f"invalid timeout seconds: {sys.argv[1]!r}", file=sys.stderr)
        return 2

    return run_command(timeout, sys.argv[2:])


if __name__ == "__main__":
    raise SystemExit(main())
