#!/usr/bin/env python3
"""Serialize the release scripts' Cargo builds behind one locked build token.

`run()` takes an exclusive `flock` on the token, runs one foreground command
while holding it, and releases only after that direct child is reaped, so a
second wrapper blocks while a first wrapper's build is still alive.

Wiring is scoped, not universal: build-release.sh and deploy-release.sh route
through this wrapper; `Makefile`'s signing target and install.sh's source
install do not, and the #5663 owner test pins that list by scanning the repo.

Signal supervision is *derived*, not enumerated. Earlier revisions carried a
hand-written list of terminating signals; it missed eight, then missed SIGEMT,
and each miss let the wrapper die while its build ran on, freeing the token.
`supervised_signals()` inverts the default: every signal the platform reports is
supervised unless it lands in a small standards-anchored exception set, so a
signal this file never names is still covered.

Scope stays bounded: SIGKILL/SIGSTOP cannot be caught, synchronous faults are
excluded, and supervision reaches the wrapper and its direct child only.
Descendants outliving that child are not covered and no process-group kill is
used, because the build's group intentionally holds an sccache daemon.
"""

from __future__ import annotations

import contextlib
import errno
import os
import signal
import subprocess
import sys
import time
from collections.abc import Iterable, Iterator, Mapping, Sequence

CANONICAL_TOKEN_PATH = "/tmp/adk-build-token.lock"
WAIT_TIMEOUT_ENV = "ADK_BUILD_TOKEN_WAIT_TIMEOUT_SECS"
# Names an inherited fd for contention notices, for callers that pipe the
# build log (build-release.sh runs cargo through `tail -1`). Absent: stderr.
DIAG_FD_ENV = "ADK_BUILD_TOKEN_DIAG_FD"
DEFAULT_WAIT_TIMEOUT_SECS = 14400.0
WAIT_POLL_SECS = 0.5
WAIT_NOTICE_SECS = 300.0
EXIT_USAGE = 64
EXIT_TOKEN_UNUSABLE = 69
EXIT_TOKEN_TIMEOUT = 75
_WOULD_BLOCK = (errno.EAGAIN, errno.EWOULDBLOCK, errno.EACCES)

# POSIX.1 sigaction(): the call shall fail with EINVAL for exactly these two.
_UNCATCHABLE = ("SIGKILL", "SIGSTOP")
# POSIX signals raised by the thread's own instruction stream, where returning
# re-executes the faulting instruction. Membership is decided by POSIX, so a
# platform-only signal such as Darwin's SIGEMT cannot appear here and is
# supervised by construction rather than by being named as a target.
_SYNCHRONOUS_FAULTS = ("SIGABRT", "SIGBUS", "SIGFPE", "SIGILL", "SIGSEGV", "SIGSYS", "SIGTRAP")
# Default action is discard/stop/continue rather than terminate.
_DEFAULT_NOT_TERMINATE = ("SIGCHLD", "SIGCLD", "SIGCONT", "SIGURG", "SIGWINCH",
                          "SIGTSTP", "SIGTTIN", "SIGTTOU")
# Darwin signal(3) lists these as "discard signal". Platform-scoped on purpose:
# Linux signal(7) gives SIGIO the Term default, so a universal exclusion would
# reintroduce the original class of miss on Linux.
_DEFAULT_NOT_TERMINATE_DARWIN = ("SIGINFO", "SIGIO")


class BuildTokenError(RuntimeError):
    """The token could not be held for the whole command."""


class BuildTokenTimeout(BuildTokenError):
    """The token stayed held by someone else past the wait deadline."""


class _Cancelled(BaseException):
    """A supervised signal arrived before any command was started."""


def excluded_signal_names() -> frozenset[str]:
    """Names subtracted from the platform's signals to leave the terminating ones."""
    names = set(_UNCATCHABLE) | set(_SYNCHRONOUS_FAULTS) | set(_DEFAULT_NOT_TERMINATE)
    if sys.platform == "darwin":
        names |= set(_DEFAULT_NOT_TERMINATE_DARWIN)
    return frozenset(names)


def signal_domain(source: object = signal) -> tuple[tuple[int, str], ...]:
    """The (number, name) pairs to consider, before any exclusion is applied.

    `signal.Signals` holds only *named* `SIG*` constants, so an enum-only
    domain leaves Linux's realtime range -- unnamed between SIGRTMIN and
    SIGRTMAX, default disposition Term -- unsupervised, which is the original
    class of miss. `source` is injectable so the range logic stays testable
    on a platform that has no realtime signals at all.
    """
    pairs = {int(sig): sig.name for sig in getattr(source, "Signals", ())}
    low, high = getattr(source, "SIGRTMIN", None), getattr(source, "SIGRTMAX", None)
    if low is not None and high is not None:
        for num in range(int(low), int(high) + 1):
            pairs.setdefault(num, f"SIGRT{num}")
    return tuple(sorted(pairs.items()))


def supervised_signals(signals: Iterable[signal.Signals] | None = None,
                       source: object = signal) -> tuple[int, ...]:
    """Derive the signals whose default disposition would terminate this wrapper.

    Aliases collapse by number. Signals already SIG_IGN are left alone: that is
    the caller's decision (nohup on SIGHUP, CPython startup on SIGPIPE/SIGXFSZ)
    and overriding it would turn a survivable broken pipe into a wrapper death.
    """
    excluded = excluded_signal_names()
    chosen: set[int] = set()
    domain = signal_domain(source) if signals is None else [(int(s), s.name) for s in signals]
    for num, name in domain:
        if name in excluded:
            continue
        try:
            ignored = signal.getsignal(num) is signal.SIG_IGN
        except (OSError, ValueError):
            # Unknown disposition: stay supervised. Dropping the number is the
            # miss this derivation exists to prevent; installing it is guarded.
            ignored = False
        if ignored:
            continue
        chosen.add(num)
    return tuple(sorted(chosen))


def wait_timeout_secs(env: Mapping[str, str]) -> float:
    """Read the wait deadline, falling back to the default on any unusable value."""
    raw = env.get(WAIT_TIMEOUT_ENV)
    if raw is None:
        return DEFAULT_WAIT_TIMEOUT_SECS
    try:
        parsed = float(raw)
    except (TypeError, ValueError):
        return DEFAULT_WAIT_TIMEOUT_SECS
    if not (0 < parsed < float("inf")):
        return DEFAULT_WAIT_TIMEOUT_SECS
    return parsed


def assert_live_token(fd: int, path: str = CANONICAL_TOKEN_PATH) -> None:
    """Fail closed if the locked fd no longer names the token at `path`."""
    held = os.fstat(fd)
    try:
        live = os.stat(path)
    except OSError as exc:
        raise BuildTokenError(f"build token {path} disappeared while held") from exc
    if (held.st_dev, held.st_ino) != (live.st_dev, live.st_ino):
        raise BuildTokenError(f"build token {path} was replaced while held")


class _Supervisor:
    """Forwards a terminating signal to the child instead of dying ahead of it."""

    def __init__(self) -> None:
        self.pending: int | None = None
        self.child: subprocess.Popen[bytes] | None = None
        self.previous: dict[int, object] = {}
        self.installed: list[int] = []

    def handle(self, signum: int, _frame: object) -> None:
        if self.pending is None:
            self.pending = signum
        if self.child is None:
            raise _Cancelled()
        with contextlib.suppress(ProcessLookupError, OSError):
            self.child.send_signal(signum)


@contextlib.contextmanager
def _supervised() -> Iterator[_Supervisor]:
    supervisor = _Supervisor()
    try:
        for signum in supervised_signals():
            try:
                supervisor.previous[signum] = signal.signal(signum, supervisor.handle)
            except (OSError, ValueError):
                continue
            supervisor.installed.append(signum)
        yield supervisor
    finally:
        for signum in supervisor.installed:
            with contextlib.suppress(OSError, ValueError):
                signal.signal(signum, supervisor.previous[signum])


def acquire(fd: int, path: str, timeout: float) -> None:
    """Block until this fd owns the token, or raise past the deadline."""
    import fcntl

    started = time.monotonic()
    deadline = started + timeout
    notice_at, since = started, time.strftime("%H:%M:%S")
    while True:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return
        except OSError as exc:
            if exc.errno not in _WOULD_BLOCK:
                raise BuildTokenError(f"build token {path} is unusable: {exc}") from exc
        now = time.monotonic()
        if now >= notice_at:
            # A stall that prints nothing is indistinguishable from a hung
            # build, and the default deadline here is four hours.
            note = (f"build token: waiting for {path} since {since}: another release"
                    f" build holds it ({now - started:.0f}s of {timeout:g}s; raise"
                    f" {WAIT_TIMEOUT_ENV} to wait longer)\n")
            try:
                os.write(int(os.environ[DIAG_FD_ENV]), note.encode())
            except (KeyError, ValueError, OSError):
                sys.stderr.write(note)
                sys.stderr.flush()
            notice_at = now + WAIT_NOTICE_SECS
        if now >= deadline:
            raise BuildTokenTimeout(
                f"build token {path} still held after {timeout:g}s: raise"
                f" {WAIT_TIMEOUT_ENV} to wait longer, or clear the holder -- an"
                " ancestor of this process holding the token deadlocks here")
        time.sleep(WAIT_POLL_SECS)


@contextlib.contextmanager
def hold_token(path: str, env: Mapping[str, str]) -> Iterator[int]:
    """Hold the token for the body. O_CLOEXEC keeps the fd out of the child."""
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o666)
    try:
        acquire(fd, path, wait_timeout_secs(env))
        assert_live_token(fd, path)
        yield fd
    finally:
        os.close(fd)


def _exit_code(returncode: int) -> int:
    return 128 - returncode if returncode < 0 else returncode


def run_protected(command: Sequence[str], env: Mapping[str, str], supervisor: _Supervisor) -> int:
    """Run one foreground child and reap it before the caller releases the token."""
    # Block the supervised signals across the spawn. Without this a signal
    # landing between Popen returning and the assignment below would find no
    # child, unwind, and strand a live build with the token released. Blocked
    # signals stay pending in the kernel and are delivered on unblock, by which
    # time the handler can forward them.
    signal.pthread_sigmask(signal.SIG_BLOCK, supervisor.installed)
    try:
        # The blocked mask is inherited across exec -- subprocess only restores
        # dispositions, not the mask -- so the child would silently ignore the
        # very signals it must still receive. Clear it in the child before exec.
        child = subprocess.Popen(
            list(command), env=dict(env), close_fds=True,
            preexec_fn=lambda: signal.pthread_sigmask(
                signal.SIG_UNBLOCK, supervisor.installed),
        )
        supervisor.child = child
    finally:
        signal.pthread_sigmask(signal.SIG_UNBLOCK, supervisor.installed)
    try:
        return _exit_code(child.wait())
    finally:
        supervisor.child = None
        if child.poll() is None:
            with contextlib.suppress(ProcessLookupError, OSError):
                child.kill()
            child.wait()


def run(command: Sequence[str], env: Mapping[str, str] | None = None,
        path: str = CANONICAL_TOKEN_PATH) -> int:
    """Run `command` while holding the build token at `path`."""
    child_env = dict(os.environ if env is None else env)
    if sys.platform == "win32":
        from build_token_win32 import BuildTokenWindowsError, supervise_windows
        try:
            return supervise_windows(list(command), child_env)
        except BuildTokenWindowsError as exc:
            print(f"build token: {exc}", file=sys.stderr)
            return EXIT_TOKEN_UNUSABLE
    with _supervised() as supervisor:
        try:
            with hold_token(path, child_env) as fd:
                assert_live_token(fd, path)
                rc = run_protected(command, child_env, supervisor)
        except BuildTokenTimeout as exc:
            print(f"build token: {exc}", file=sys.stderr)
            rc = EXIT_TOKEN_TIMEOUT
        except BuildTokenError as exc:
            print(f"build token: {exc}", file=sys.stderr)
            rc = EXIT_TOKEN_UNUSABLE
        except _Cancelled:
            rc = EXIT_TOKEN_UNUSABLE
        pending = supervisor.pending
    if pending is not None:
        with contextlib.suppress(OSError, ValueError):
            signal.signal(pending, signal.SIG_DFL)
        os.kill(os.getpid(), pending)
    return rc


def parse_command(argv: Sequence[str]) -> list[str]:
    rest = list(argv[1:])
    if rest and rest[0] == "--":
        rest = rest[1:]
    if not rest:
        raise BuildTokenError("usage: build_token.py -- COMMAND [ARG...]")
    return rest


def main(argv: Sequence[str]) -> int:
    try:
        command = parse_command(argv)
    except BuildTokenError as exc:
        print(f"build token: {exc}", file=sys.stderr)
        return EXIT_USAGE
    return run(command)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
