#!/usr/bin/env python3
"""Run a command with a wall-clock timeout and bounded cleanup.

Diagnostic process creation is best-effort and outside the cleanup bound.
"""

from __future__ import annotations

import os
import shlex
import shutil
import signal
import subprocess
import sys
import time
from dataclasses import dataclass

TERMINATION_GRACE_SECONDS = 5.0
DIAGNOSTIC_CAP_SECONDS = 10.0
KILL_WAIT_SECONDS = 10.0
POLL_INTERVAL_SECONDS = 0.1
_FORWARDED_SIGNALS = (signal.SIGTERM, signal.SIGINT)
_monotonic, _sleep = time.monotonic, time.sleep


@dataclass
class _RunState:
    first_signum: int | None = None
    reaped: bool = False
    cutoff: bool = False


def _mask_forwarded_signals() -> bool:
    required = ("pthread_sigmask", "sigpending", "SIG_BLOCK", "SIG_UNBLOCK")
    if not all(hasattr(signal, name) for name in required):
        return False
    signal.pthread_sigmask(signal.SIG_BLOCK, _FORWARDED_SIGNALS)
    return True


def _unblock_forwarded_signals() -> None:
    signal.pthread_sigmask(signal.SIG_UNBLOCK, _FORWARDED_SIGNALS)


def _checkpoint(state: _RunState, enabled: bool, name: str) -> bool:
    """Record the first target signal observed before cutoff C."""
    if state.cutoff or not enabled:
        return False
    pending = signal.sigpending()
    if state.first_signum is None:
        # sigpending() is unordered: TERM wins a simultaneous TERM+INT tie.
        if signal.SIGTERM in pending:
            state.first_signum = signal.SIGTERM
        elif signal.SIGINT in pending:
            state.first_signum = signal.SIGINT
    return state.first_signum is not None


def _popen(command: list[str], enabled: bool) -> subprocess.Popen[bytes]:
    options = {"preexec_fn": _unblock_forwarded_signals} if enabled else {}
    return subprocess.Popen(command, start_new_session=True, **options)


def _poll_child(
    proc: subprocess.Popen[bytes],
    state: _RunState,
    enabled: bool,
    deadline: float,
    *,
    stop_on_signal: bool,
) -> tuple[int | None, str]:
    while True:
        # R2″ checkpoint ③: every bounded poll iteration observes pending signals.
        _checkpoint(state, enabled, "poll")
        if state.first_signum is not None and stop_on_signal:
            return None, "signal"
        returncode = proc.poll()
        if returncode is not None:
            state.reaped = True
            return returncode, "reaped"
        remaining = deadline - _monotonic()
        if remaining <= 0:
            return None, "deadline"
        _sleep(min(POLL_INTERVAL_SECONDS, remaining))


def _refresh_reaped(proc: subprocess.Popen[bytes], state: _RunState) -> None:
    if state.reaped or proc.returncode is not None or proc.poll() is not None:
        state.reaped = True


def _send_process_signal(
    proc: subprocess.Popen[bytes],
    state: _RunState,
    signum: int,
    *,
    force: bool = False,
) -> None:
    if state.cutoff or state.reaped or proc.returncode is not None:
        return
    try:
        if hasattr(os, "killpg"):
            os.killpg(proc.pid, signal.SIGKILL if force else signum)
        elif force:
            proc.kill()
        else:
            proc.terminate()
    except (ProcessLookupError, OSError, AttributeError):
        pass


def _diagnostic_commands(proc: subprocess.Popen[bytes]) -> list[list[str]]:
    commands = []
    if sys.platform == "darwin" and os.path.exists("/usr/bin/sample"):
        commands.append(["/usr/bin/sample", str(proc.pid), "5"])
    elif shutil.which("gdb"):
        commands.append(
            ["gdb", "-batch", "-ex", "thread apply all bt", "-p", str(proc.pid)]
        )
    if ps := shutil.which("ps"):
        commands.append(
            [ps, "-o", "pid,ppid,pgid,stat,etime,command", "-g", str(proc.pid)]
        )
    return commands


def _diagnose_with_deadline(
    proc: subprocess.Popen[bytes], state: _RunState, enabled: bool
) -> None:
    """Poll returned diagnostic children against one shared deadline.

    Diagnostic Popen creation is deliberately outside the deadline: Python cannot
    safely interrupt Popen and still guarantee ownership of a child it has not
    returned.  Once Popen returns, the child is registered immediately and is
    killed best-effort if it remains alive at the shared deadline.  No
    process-global signal handler or timer is installed here.
    """
    # R1: bounded poll only; dumper creation itself is best-effort and unbounded.
    deadline = _monotonic() + DIAGNOSTIC_CAP_SECONDS
    dumpers: list[subprocess.Popen[bytes]] = []

    print("::group::ci-timeout diagnostics", file=sys.stderr)
    try:
        if not enabled:
            print(
                "ci-timeout: diagnostics skipped without POSIX signal masking",
                file=sys.stderr,
            )
            return
        for command in _diagnostic_commands(proc):
            if _monotonic() >= deadline:
                break
            try:
                dumper = _popen(command, enabled)
                dumpers.append(dumper)
                _checkpoint(state, enabled, "diagnostic_poll")
            except (OSError, subprocess.SubprocessError) as error:
                print(f"ci-timeout: diagnostic failed: {error}", file=sys.stderr)
        while dumpers:
            _checkpoint(state, enabled, "diagnostic_poll")
            dumpers = [dumper for dumper in dumpers if dumper.poll() is None]
            remaining = deadline - _monotonic()
            if not dumpers or remaining <= 0:
                break
            _sleep(min(POLL_INTERVAL_SECONDS, remaining))
        for dumper in dumpers:
            try:
                dumper.kill()
            except (ProcessLookupError, OSError, AttributeError):
                pass
            if dumper.poll() is None:
                print(
                    f"::warning::ci-timeout: diagnostic pid {dumper.pid} unreaped",
                    file=sys.stderr,
                )
    finally:
        print("::endgroup::", file=sys.stderr)


def _cleanup(proc: subprocess.Popen[bytes], state: _RunState, enabled: bool) -> None:
    if not state.reaped and proc.returncode is None:
        _diagnose_with_deadline(proc, state, enabled)

    _checkpoint(state, enabled, "term_boundary")  # R2″ ④: TERM boundary
    _refresh_reaped(proc, state)
    _send_process_signal(proc, state, signal.SIGTERM)
    if not state.reaped:
        _poll_child(
            proc,
            state,
            enabled,
            _monotonic() + TERMINATION_GRACE_SECONDS,
            stop_on_signal=False,
        )

    _checkpoint(state, enabled, "grace_boundary")  # R2″ ④: KILL boundary
    _refresh_reaped(proc, state)
    _send_process_signal(proc, state, signal.SIGTERM, force=True)
    if not state.reaped:
        _poll_child(
            proc,
            state,
            enabled,
            _monotonic() + KILL_WAIT_SECONDS,
            stop_on_signal=False,
        )

    _checkpoint(state, enabled, "kill_boundary")  # R2″ ④: post-KILL
    _refresh_reaped(proc, state)
    if not state.reaped:
        print(
            f"::warning::ci-timeout: child pid {proc.pid} unreaped after KILL_WAIT",
            file=sys.stderr,
        )


def _normalize_return_code(returncode: int) -> int:
    return 128 - returncode if returncode < 0 else returncode


def _select_return_code(
    state: _RunState, *, timed_out: bool, child_returncode: int | None
) -> int:
    """Select rc for signal, timeout, and child-exit rows after spawn."""
    if state.first_signum is not None:
        return 128 + state.first_signum
    if timed_out:
        return 124
    if child_returncode is None:
        raise RuntimeError("ci-timeout: missing child return code")
    return _normalize_return_code(child_returncode)


def _report(timeout: float, command: list[str], elapsed: float, rc: int) -> None:
    if os.environ.get("AGENTDESK_CI_TIMEOUT_REPORT") == "1":
        print(
            f"::notice::ci-timeout: {elapsed:.1f}s / {timeout:g}s, rc {rc} "
            f"— {shlex.join(command)}",
            file=sys.stderr,
        )


def run_command(timeout: float, command: list[str]) -> int:
    enabled, state = _mask_forwarded_signals(), _RunState()
    previous_handlers: dict[int, signal.Handlers] = {}
    checkpoint1_started = _monotonic()
    # Checkpoint ① is the special no-spawn rc-table row 1a.
    if _checkpoint(state, enabled, "checkpoint1"):
        rc = _select_return_code(state, timed_out=False, child_returncode=None)
        _report(timeout, command, _monotonic() - checkpoint1_started, rc)
        return rc

    try:
        proc = _popen(command, enabled)

        if not enabled:
            # Preserve the pre-masking contract on platforms without POSIX APIs.
            def forward_signal(signum: int, _frame: object) -> None:
                if state.cutoff:
                    return
                if state.first_signum is None:
                    state.first_signum = signum
                _send_process_signal(proc, state, signum)

            for signum in _FORWARDED_SIGNALS:
                previous_handlers[signum] = signal.signal(signum, forward_signal)

        spawn_completed = _monotonic()
        child_rc, timed_out = None, False
        # Checkpoint ② observes signals that arrived inside Popen.
        if _checkpoint(state, enabled, "checkpoint2"):
            _cleanup(proc, state, enabled)
        else:
            child_rc, outcome = _poll_child(
                proc, state, enabled, spawn_completed + timeout, stop_on_signal=True
            )
            timed_out = outcome == "deadline"
            if outcome == "signal":
                _cleanup(proc, state, enabled)
            elif timed_out:
                elapsed = _monotonic() - spawn_completed
                print(
                    f"::error::ci-timeout: exceeded {timeout:g}s after {elapsed:.1f}s "
                    f"— {shlex.join(command)}",
                    file=sys.stderr,
                )
                _cleanup(proc, state, enabled)

        # Checkpoint ⑤ is immediately before cutoff C and spawn-path rc selection.
        _checkpoint(state, enabled, "final")
        state.cutoff = True
        rc = _select_return_code(state, timed_out=timed_out, child_returncode=child_rc)
        _report(timeout, command, _monotonic() - spawn_completed, rc)
        return rc
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
