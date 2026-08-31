#!/usr/bin/env python3
"""Run one foreground command under AgentDesk's host-wide Cargo token."""

from __future__ import annotations

import fcntl
import os
from pathlib import Path
import stat
import sys
from typing import Callable, Mapping, NoReturn, Sequence


TOKEN_PATH = Path("/tmp/adk-build-token.lock")
TOKEN_FD_ENV = "AGENTDESK_BUILD_TOKEN_FD"
TOKEN_ERROR_EXIT = 73


class BuildTokenError(RuntimeError):
    """The canonical token could not be acquired without splitting authority."""


def _validate_token_fd(fd: int, token_path: Path) -> None:
    try:
        opened = os.fstat(fd)
        current = os.lstat(token_path)
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


def _open_token(token_path: Path) -> int:
    flags = os.O_RDWR | os.O_CREAT | os.O_APPEND
    flags |= getattr(os, "O_CLOEXEC", 0)
    flags |= getattr(os, "O_NOFOLLOW", 0)
    try:
        fd = os.open(token_path, flags, 0o600)
    except OSError as error:
        raise BuildTokenError(f"cannot open build token authority: {error}") from error
    os.set_inheritable(fd, True)
    return fd


def _parse_inherited_fd(raw_fd: str) -> int:
    try:
        fd = int(raw_fd, 10)
    except ValueError as error:
        raise BuildTokenError("inherited build token FD is not numeric") from error
    if fd < 3:
        raise BuildTokenError("inherited build token FD must not alias stdio")
    return fd


def _acquire_token(
    token_path: Path = TOKEN_PATH,
    *,
    environ: Mapping[str, str] | None = None,
    on_open: Callable[[], None] | None = None,
) -> tuple[int, bool]:
    """Return ``(fd, inherited)`` after validating and acquiring one inode."""

    active_environ = os.environ if environ is None else environ
    raw_inherited_fd = active_environ.get(TOKEN_FD_ENV)
    if raw_inherited_fd is not None:
        fd = _parse_inherited_fd(raw_inherited_fd)
        _validate_token_fd(fd, token_path)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except (BlockingIOError, OSError) as error:
            raise BuildTokenError("inherited build token FD does not prove lock ownership") from error
        _validate_token_fd(fd, token_path)
        os.set_inheritable(fd, True)
        return fd, True

    fd = _open_token(token_path)
    try:
        _validate_token_fd(fd, token_path)
        if on_open is not None:
            on_open()
        fcntl.flock(fd, fcntl.LOCK_EX)
        _validate_token_fd(fd, token_path)
    except BaseException:
        os.close(fd)
        raise
    return fd, False


def run_command(
    command: Sequence[str],
    *,
    token_path: Path = TOKEN_PATH,
) -> NoReturn:
    if not command:
        raise BuildTokenError("protected command is required")
    fd, _inherited = _acquire_token(token_path)
    child_environ = os.environ.copy()
    child_environ[TOKEN_FD_ENV] = str(fd)
    child_environ["CARGO_BUILD_JOBS"] = "2"
    os.execvpe(command[0], list(command), child_environ)


def main(argv: Sequence[str]) -> int:
    if not argv or argv[0] != "--" or len(argv) == 1:
        print(f"usage: {Path(sys.argv[0]).name} -- <command> [args...]", file=sys.stderr)
        return 2
    try:
        run_command(argv[1:])
    except BuildTokenError as error:
        print(f"build-token: {error}", file=sys.stderr)
        return TOKEN_ERROR_EXIT
    except OSError as error:
        print(f"build-token: could not execute protected command: {error}", file=sys.stderr)
        return 126


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
