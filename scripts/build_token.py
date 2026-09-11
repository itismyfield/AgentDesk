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

An explicit --delegate-lease permits one cooperative wrapper hop. Ordinary
children inherit no lease; this does not make whole-deploy wrapping or ABBA safe.
"""

from __future__ import annotations

import contextlib
import errno
import os
import secrets
import shutil
import signal
import stat
import subprocess
import sys
import time
from collections.abc import Iterable, Iterator, Mapping, Sequence

CANONICAL_TOKEN_PATH = "/tmp/adk-build-token.lock"
WAIT_TIMEOUT_ENV = "ADK_BUILD_TOKEN_WAIT_TIMEOUT_SECS"
# Names an inherited fd for contention notices, for callers that pipe the
# build log (build-release.sh runs cargo through `tail -1`). Absent: stderr.
DIAG_FD_ENV = "ADK_BUILD_TOKEN_DIAG_FD"
LEASE_ENV = "ADK_BUILD_TOKEN_LEASE"
# Opt-out for the sccache activation below: these spellings (trimmed, case-folded)
# turn it off, anything else -- unset included -- leaves it on.
SCCACHE_OPT_OUT_ENV = "ADK_BUILD_TOKEN_SCCACHE"
_SCCACHE_OFF = frozenset({"0", "false", "no", "off"})
# Cargo's two wrapper switches. The release scripts clear them as a pair
# (docs/ci/sccache-setup.md), so either one present is already a caller decision.
_WRAPPER_ENV_KEYS = ("RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER")
_HOMEBREW_BIN = "/opt/homebrew/bin"
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


@contextlib.contextmanager
def inherited_lease(raw: str, path: str) -> Iterator[int]:
    """Consume one hop; prove current exclusivity, not historical OFD ownership."""
    import fcntl

    try:
        with contextlib.ExitStack() as cleanup:
            version, generation, lease, ticket, dev, ino, nonce = raw.split(":")
            fd, ticket_fd = int(lease), int(ticket)
            if fd < 10 or ticket_fd < 10 or fd == ticket_fd:
                raise BuildTokenError("invalid lease descriptors")
            cleanup.callback(os.close, fd)
            cleanup.callback(os.close, ticket_fd)
            if (version, generation) != ("1", "1") or len(nonce) != 32:
                raise BuildTokenError("invalid lease generation or nonce")
            held, receipt = os.fstat(fd), os.fstat(ticket_fd)
            if (not stat.S_ISREG(held.st_mode) or not stat.S_ISFIFO(receipt.st_mode)
                    or (held.st_dev, held.st_ino) != (int(dev), int(ino))
                    or fcntl.fcntl(ticket_fd, fcntl.F_GETFL) & os.O_ACCMODE != os.O_RDONLY):
                raise BuildTokenError("invalid lease identity or ticket type")
            assert_live_token(fd, path)
            os.set_inheritable(fd, False)
            os.set_inheritable(ticket_fd, False)
            os.set_blocking(ticket_fd, False)
            if not secrets.compare_digest(os.read(ticket_fd, 33), nonce.encode("ascii")):
                raise BuildTokenError("lease ticket was consumed or mismatched")
            probe = os.open(path, os.O_RDWR | os.O_CLOEXEC)
            cleanup.callback(os.close, probe)
            try:
                fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                pass
            else:
                raise BuildTokenError("inherited lease is not currently held")
            # A foreign shared OFD can lose its shared lock on conversion; only
            # cooperative issuers of exclusive leases belong to this protocol.
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            assert_live_token(fd, path)
            yield fd
            # Close only: LOCK_UN here would also unlock the issuer's OFD.
    except (OSError, ValueError, OverflowError) as exc:
        raise BuildTokenError(f"invalid inherited lease: {exc}") from exc


@contextlib.contextmanager
def delegated_spawn(fd: int | None, command: Sequence[str],
                    env: Mapping[str, str]) -> Iterator[tuple[dict[str, str], tuple[int, ...]]]:
    """Transfer two dedicated descriptors only to this Python wrapper."""
    child_env = dict(env)
    child_env.pop(LEASE_ENV, None)
    if fd is None:
        yield child_env, ()
        return
    import fcntl

    try:
        if (len(command) < 2
                or os.path.realpath(shutil.which(command[0]) or command[0]) != os.path.realpath(sys.executable)
                or os.path.realpath(command[1]) != os.path.realpath(__file__)):
            raise BuildTokenError("lease delegation requires this cooperative Python wrapper")
        # ExitStack closes both transfer duplicates even when Popen fails.
        with contextlib.ExitStack() as cleanup:
            lease_fd = fcntl.fcntl(fd, fcntl.F_DUPFD_CLOEXEC, 10)
            cleanup.callback(os.close, lease_fd)
            read_fd, write_fd = os.pipe()
            with os.fdopen(read_fd, "rb"), os.fdopen(write_fd, "wb") as writer:
                ticket_fd = fcntl.fcntl(read_fd, fcntl.F_DUPFD_CLOEXEC, 10)
                cleanup.callback(os.close, ticket_fd)
                nonce = secrets.token_hex(16)
                writer.write(nonce.encode("ascii"))
            held = os.fstat(fd)
            child_env[LEASE_ENV] = f"1:1:{lease_fd}:{ticket_fd}:{held.st_dev}:{held.st_ino}:{nonce}"
            try:
                yield child_env, (lease_fd, ticket_fd)
            finally:
                cleanup.close()  # Popen success/failure: close both transfer dups.
    except (OSError, ValueError, OverflowError) as exc:
        raise BuildTokenError(f"lease delegation failed: {exc}") from exc


def run_protected(command: Sequence[str], env: Mapping[str, str], supervisor: _Supervisor,
                  delegate_fd: int | None = None) -> int:
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
        with delegated_spawn(delegate_fd, command, env) as (child_env, inherited_fds):
            child = subprocess.Popen(
                list(command), env=child_env, close_fds=True, pass_fds=inherited_fds,
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


# sccache opt-in for campaign cargo, which reaches cargo only through here: a shell
# export of RUSTC_WRAPPER dies with the batch, and .cargo/config.toml ships
# `rustc-wrapper = ""`, so the environment is the only switch. The probe order and the
# /opt/homebrew/bin, $HOME/.cache/sccache and 10G literals are copied from
# `setup_sccache_env` (scripts/_defaults.sh:25) -- a bash function and a dict cannot
# share an implementation -- so those three defaults move in both places or neither.
# Two rules deliberately do NOT mirror it; do not "fix" them into agreement.
# (1) Precedence. setup_sccache_env is imperative -- build-release.sh, deploy-release.sh
# and install.sh call it to turn sccache on, so overwriting RUSTC_WRAPPER is the point
# of the call. This is ambient, so a caller decision stands, "" included: env beats
# .cargo/config.toml and "" is that file's own "no wrapper", which the release scripts
# export paired with CARGO_BUILD_RUSTC_WRAPPER. Both keys are therefore honoured, which
# also makes this a no-op under CI, whose workflows set RUSTC_WRAPPER at the `env:`
# level. (2) An unusable cache dir: the shell exports first and leaks mkdir's exit code,
# while here nothing is written at all. POSIX only; see docs/ci/sccache-setup.md 2.4.
def apply_sccache_env(env: dict[str, str]) -> None:
    """Enable sccache for the child when resolvable; otherwise change nothing."""
    if any(key in env for key in _WRAPPER_ENV_KEYS) or env.get(
            SCCACHE_OPT_OUT_ENV, "").strip().lower() in _SCCACHE_OFF:
        return
    path = env.get("PATH", os.defpath)
    if _HOMEBREW_BIN not in path.split(os.pathsep) and os.access(
            os.path.join(_HOMEBREW_BIN, "sccache"), os.X_OK):
        path = _HOMEBREW_BIN + os.pathsep + path
    sccache = shutil.which("sccache", path=path)
    if sccache is None:
        return  # No sccache: not one variable moves, PATH included.
    # Unset, sccache picks a per-platform dir and stops sharing hits with releases.
    cache_dir = env.get("SCCACHE_DIR") or os.path.join(
        env.get("HOME") or os.path.expanduser("~"), ".cache", "sccache")
    try:
        os.makedirs(cache_dir, exist_ok=True)
    except OSError:
        return  # An unusable cache directory costs the cache, never the build.
    env["PATH"] = path
    env["SCCACHE_DIR"] = cache_dir
    env["SCCACHE_CACHE_SIZE"] = env.get("SCCACHE_CACHE_SIZE") or "10G"
    env["RUSTC_WRAPPER"] = sccache


def run(command: Sequence[str], env: Mapping[str, str] | None = None,
        path: str = CANONICAL_TOKEN_PATH, *, delegate_lease: bool = False) -> int:
    """Run `command` while holding the build token at `path`."""
    child_env = dict(os.environ if env is None else env)
    carrier = os.environ.get(LEASE_ENV)  # Caller env= is never incoming authority.
    child_env.pop(LEASE_ENV, None)
    if sys.platform == "win32":
        if delegate_lease or carrier is not None:
            print("build token: inherited POSIX leases are unsupported on Windows", file=sys.stderr)
            return EXIT_TOKEN_UNUSABLE
        from build_token_win32 import BuildTokenWindowsError, supervise_windows
        try:
            return supervise_windows(list(command), child_env)
        except BuildTokenWindowsError as exc:
            print(f"build token: {exc}", file=sys.stderr)
            return EXIT_TOKEN_UNUSABLE
    apply_sccache_env(child_env)
    with _supervised() as supervisor:
        try:
            lease = inherited_lease(carrier, path) if carrier is not None else hold_token(path, child_env)
            with lease as fd:
                if carrier is not None and delegate_lease:
                    raise BuildTokenError("an inherited lease cannot be delegated again")
                assert_live_token(fd, path)
                rc = run_protected(command, child_env, supervisor, fd if delegate_lease else None)
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
    args = list(argv)
    delegate = len(args) > 1 and args[1] == "--delegate-lease"
    if delegate:  # Only before --; everything after -- remains command argv.
        del args[1]
    try:
        command = parse_command(args)
    except BuildTokenError as exc:
        print(f"build token: {exc}", file=sys.stderr)
        return EXIT_USAGE
    return run(command, delegate_lease=delegate)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
