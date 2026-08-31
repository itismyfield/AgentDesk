#!/usr/bin/env python3
"""Supervise one foreground Cargo command under AgentDesk's build token."""

from __future__ import annotations

import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import time
from typing import Callable, Mapping, Sequence

if sys.platform != "win32":
    import fcntl


TOKEN_PATH = Path("/tmp/adk-build-token.lock")
TOKEN_ERROR_EXIT = 73
NESTING_ENV = "AGENTDESK_BUILD_TOKEN_ACTIVE"
LEGACY_FD_ENV = "AGENTDESK_BUILD_TOKEN_FD"
SIGNAL_GRACE_SECONDS = 2.0


class BuildTokenError(RuntimeError):
    """The protected command cannot use the canonical token safely."""


class _SupervisorSignal(BaseException):
    def __init__(self, signum: int) -> None:
        self.signum = signum


def _validate_command(command: Sequence[str]) -> None:
    if not command:
        raise BuildTokenError("protected command is required")
    executable = Path(command[0]).name.lower()
    if executable not in {"cargo", "cargo.exe"}:
        raise BuildTokenError("protected command must invoke cargo directly")
    for argument in command[1:]:
        if (
            argument in {"-j", "--jobs"}
            or (argument.startswith("-j") and len(argument) > 2)
            or argument.startswith("--jobs=")
        ):
            raise BuildTokenError("explicit Cargo jobs flags are forbidden; CARGO_BUILD_JOBS=2 is canonical")


def _child_environment(environ: Mapping[str, str]) -> dict[str, str]:
    if environ.get(NESTING_ENV) is not None or environ.get(LEGACY_FD_ENV) is not None:
        raise BuildTokenError("nested or inherited build-token execution is forbidden")
    child_environment = dict(environ)
    child_environment.pop(LEGACY_FD_ENV, None)
    child_environment[NESTING_ENV] = "1"
    child_environment["CARGO_BUILD_JOBS"] = "2"
    return child_environment


def _validate_posix_token_fd(fd: int, token_path: Path) -> None:
    try:
        opened = os.fstat(fd)
        current = os.lstat(token_path)
        status_flags = fcntl.fcntl(fd, fcntl.F_GETFL)
    except OSError as error:
        raise BuildTokenError(f"cannot inspect build token authority: {error}") from error
    if not stat.S_ISREG(opened.st_mode) or not stat.S_ISREG(current.st_mode):
        raise BuildTokenError("build token authority must be one regular file")
    if (opened.st_dev, opened.st_ino) != (current.st_dev, current.st_ino):
        raise BuildTokenError("build token pathname no longer names the opened inode")
    if opened.st_uid != os.geteuid() or current.st_uid != os.geteuid():
        raise BuildTokenError("build token authority is not owned by the current user")
    if opened.st_nlink != 1 or current.st_nlink != 1:
        raise BuildTokenError("build token authority must have exactly one link")
    if current.st_mode & 0o022:
        raise BuildTokenError("build token authority must not be group/world writable")
    if status_flags & os.O_ACCMODE != os.O_RDWR or not status_flags & os.O_APPEND:
        raise BuildTokenError("build token FD must be O_RDWR|O_APPEND")
    if os.get_inheritable(fd):
        raise BuildTokenError("build token FD must remain non-inheritable")


def _open_posix_token(token_path: Path) -> int:
    if not hasattr(os, "O_NOFOLLOW"):
        raise BuildTokenError("POSIX build token requires O_NOFOLLOW")
    flags = os.O_RDWR | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW
    flags |= getattr(os, "O_CLOEXEC", 0)
    try:
        fd = os.open(token_path, flags, 0o600)
    except OSError as error:
        raise BuildTokenError(f"cannot open build token authority: {error}") from error
    os.set_inheritable(fd, False)
    return fd


def _acquire_posix_token(
    token_path: Path,
    *,
    on_open: Callable[[], None] | None = None,
) -> int:
    fd = _open_posix_token(token_path)
    try:
        _validate_posix_token_fd(fd, token_path)
        if on_open is not None:
            on_open()
        fcntl.flock(fd, fcntl.LOCK_EX)
        _validate_posix_token_fd(fd, token_path)
    except BaseException:
        os.close(fd)
        raise
    return fd


def _supervised_signals() -> tuple[int, ...]:
    values = [signal.SIGINT, signal.SIGTERM]
    if hasattr(signal, "SIGHUP"):
        values.append(signal.SIGHUP)
    return tuple(values)


class _SignalRelay:
    def __init__(self) -> None:
        self.child: subprocess.Popen[bytes] | None = None
        self.received: int | None = None
        self.received_at: float | None = None

    def __call__(self, signum: int, _frame: object) -> None:
        if self.child is None:
            raise _SupervisorSignal(signum)
        if self.received is None:
            self.received = signum
            self.received_at = time.monotonic()
        try:
            os.killpg(self.child.pid, signum)
        except ProcessLookupError:
            pass


def _wait_for_child(child: subprocess.Popen[bytes], relay: _SignalRelay) -> int:
    while True:
        try:
            return_code = child.wait(timeout=0.1)
        except subprocess.TimeoutExpired:
            if relay.received_at is None or time.monotonic() - relay.received_at < SIGNAL_GRACE_SECONDS:
                continue
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
            return 128 + (relay.received or signal.SIGTERM)
        if relay.received is not None:
            return 128 + relay.received
        if return_code < 0:
            return 128 - return_code
        return return_code


def _supervise_posix(command: Sequence[str], child_env: Mapping[str, str], token_path: Path) -> int:
    relay = _SignalRelay()
    supervised_signals = _supervised_signals()
    old_handlers = {signum: signal.signal(signum, relay) for signum in supervised_signals}
    token_fd: int | None = None
    try:
        token_fd = _acquire_posix_token(token_path)
        old_mask = signal.pthread_sigmask(signal.SIG_BLOCK, supervised_signals)
        try:
            child = subprocess.Popen(
                list(command),
                env=dict(child_env),
                close_fds=True,
                start_new_session=True,
            )
            relay.child = child
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, old_mask)
        return _wait_for_child(child, relay)
    except _SupervisorSignal as interrupted:
        return 128 + interrupted.signum
    finally:
        if token_fd is not None:
            os.close(token_fd)
        for signum, old_handler in old_handlers.items():
            signal.signal(signum, old_handler)


def _supervise_for_test(command: Sequence[str], token_path: Path) -> int:
    _validate_command(command)
    child_env = _child_environment(os.environ)
    return _supervise_posix(command, child_env, token_path)


def _supervise_canonical(command: Sequence[str], child_env: Mapping[str, str]) -> int:
    if sys.platform == "win32":
        from build_token_win32 import supervise_windows

        return supervise_windows(command, child_env)
    return _supervise_posix(command, child_env, TOKEN_PATH)


def main(argv: Sequence[str]) -> int:
    if not argv or argv[0] != "--" or len(argv) == 1:
        print(f"usage: {Path(sys.argv[0]).name} -- <cargo-command> [args...]", file=sys.stderr)
        return 2
    try:
        command = argv[1:]
        _validate_command(command)
        child_env = _child_environment(os.environ)
        return _supervise_canonical(command, child_env)
    except BuildTokenError as error:
        print(f"build-token: {error}", file=sys.stderr)
        return TOKEN_ERROR_EXIT
    except OSError as error:
        print(f"build-token: could not execute protected command: {error}", file=sys.stderr)
        return 126


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
