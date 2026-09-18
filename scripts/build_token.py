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

Waiting is first-come-first-served. `acquire()` retries a non-blocking flock, so
the kernel never queues the waiters; on 2026-09-17 that let a lane release the
token and immediately re-enter, winning every handoff while a process that had
waited 49 minutes never saw a free window (#5968). Waiters therefore take a
numbered ticket next to the token and only the oldest one retries.
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
# The waiting line lives beside the token it orders, so a temporary token in a
# test gets its own queue and never touches the canonical one.
QUEUE_DIR_SUFFIX = ".q"
# Sticky, like /tmp itself: other lanes -- possibly other uids, the token is
# 0o666 -- must be able to enqueue without being able to evict each other.
_QUEUE_DIR_MODE = 0o1777
# Both are dot-prefixed so a scan skips them: the counter is not a ticket, and a
# ticket mid-creation is not yet anyone's place in line.
_SEQUENCE_FILE = ".seq"
_PENDING_PREFIX = ".pending-"
# Bound on taking the counter: a thousandfold margin over the syscalls it
# guards, and short enough to stay noise against any real wait deadline.
_SEQUENCE_TRIES = 20
_SEQUENCE_RETRY_SECS = 0.005
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


def queue_dir(path: str) -> str:
    """Where `path`'s waiters line up."""
    return path + QUEUE_DIR_SUFFIX


def ticket_sequence(name: str) -> int | None:
    """The arrival number encoded in a ticket filename, or None if it is not one."""
    try:
        return int(name.partition("-")[0])
    except ValueError:
        return None


def reclaim_if_abandoned(qdir: str, name: str) -> bool:
    """True when this ticket has no live owner; its file is removed on the way out.

    A ticket records a place in line, but unlike an flock a file does not vanish
    when its owner is killed, and a queue that accumulates dead entries is a
    worse deadlock than the unfairness it replaced. So the liveness proof is
    itself an flock, on the ticket: whoever can take it knows the kernel already
    released it, which only happens once the owner's last fd is gone.
    """
    import fcntl

    ticket = os.path.join(qdir, name)
    try:
        # Read-only is enough: flock locks the description, not the access mode,
        # and a ticket owned by another uid is readable but not writable.
        fd = os.open(ticket, os.O_RDONLY | os.O_CLOEXEC)
    except FileNotFoundError:
        return True
    except OSError:
        return False  # Unreadable: leave the place standing rather than cut in.
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            return False  # Still locked, so its owner is alive and still queued.
        with contextlib.suppress(OSError):
            os.unlink(ticket)
        return True
    finally:
        os.close(fd)


def claim_sequence(qdir: str, queued: Iterable[str]) -> int:
    """Take the next arrival number, never below a ticket already in the queue."""
    import fcntl

    ahead = [seq for seq in map(ticket_sequence, queued) if seq is not None]
    fd = os.open(os.path.join(qdir, _SEQUENCE_FILE),
                 os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o666)
    try:
        # This lock spans one read-modify-write, never a build -- but it is taken
        # before `acquire()`'s deadline loop starts, so a stopped process holding
        # it must not be able to block a caller indefinitely. Bounded spin, then
        # give up: the caller's own except-OSError turns that into no ordering.
        for attempt in range(_SEQUENCE_TRIES):
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except OSError:
                if attempt == _SEQUENCE_TRIES - 1:
                    raise
                time.sleep(_SEQUENCE_RETRY_SECS)
        try:
            counter = int(os.read(fd, 64).decode("ascii", "replace").strip() or 0)
        except ValueError:
            counter = 0
        # Re-seed from the live queue too: a counter truncated or removed out of
        # band would otherwise hand a newcomer a number ahead of everyone.
        issued = max([counter, 0, *ahead]) + 1
        os.lseek(fd, 0, os.SEEK_SET)
        os.ftruncate(fd, 0)
        os.write(fd, str(issued).encode("ascii"))
        return issued
    finally:
        os.close(fd)


class _Ticket:
    """One waiter's place in line, held open for as long as it waits."""

    def __init__(self, qdir: str, name: str, sequence: int) -> None:
        self.qdir, self.name, self.sequence = qdir, name, sequence

    def survey(self) -> tuple[int, int]:
        """(waiters older than me, waiters including me), reclaiming dead tickets."""
        try:
            entries = os.listdir(self.qdir)
        except OSError:
            return 0, 1  # No queue to read: degrade to unordered contention.
        ahead = total = 0
        for name in entries:
            if name.startswith(".") or ticket_sequence(name) is None:
                continue
            if name != self.name and reclaim_if_abandoned(self.qdir, name):
                continue
            total += 1
            # The nonce breaks a tie that only a re-seeded counter can produce,
            # and it breaks it the same way in every waiter that reads it.
            if (ticket_sequence(name), name) < (self.sequence, self.name):
                ahead += 1
        return ahead, max(total, 1)


@contextlib.contextmanager
def enqueue(path: str) -> Iterator[_Ticket | None]:
    """Take a place in `path`'s waiting line, giving it up however the wait ends."""
    import fcntl

    qdir, fd, pending, ticket = queue_dir(path), -1, None, None
    try:
        os.makedirs(qdir, exist_ok=True)
        with contextlib.suppress(OSError):
            os.chmod(qdir, _QUEUE_DIR_MODE)
        nonce = secrets.token_hex(8)
        sequence = claim_sequence(qdir, os.listdir(qdir))
        name = f"{sequence:020d}-{os.getpid()}-{nonce}"
        pending = os.path.join(qdir, f"{_PENDING_PREFIX}{os.getpid()}-{nonce}")
        fd = os.open(pending, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC, 0o666)
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        os.write(fd, f"pid {os.getpid()} waiting since {time.strftime('%H:%M:%S')}\n".encode())
        # Locked before it is visible, so a scan can never read a ticket still
        # being created as one whose owner died.
        os.rename(pending, os.path.join(qdir, name))
        pending, ticket = None, _Ticket(qdir, name, sequence)
    except OSError:
        # The queue is best effort. A /tmp that cannot hold it costs ordering,
        # never the build: fall back to the unordered contention it replaced.
        ticket = None
    try:
        yield ticket
    finally:
        for leftover in (pending, None if ticket is None else os.path.join(qdir, ticket.name)):
            if leftover is not None:
                with contextlib.suppress(OSError):
                    os.unlink(leftover)
        if fd >= 0:
            with contextlib.suppress(OSError):
                os.close(fd)


def acquire(fd: int, path: str, timeout: float) -> None:
    """Block until this fd owns the token, or raise past the deadline.

    Only the oldest waiter retries the lock, which is what makes the handoff
    first-come-first-served: a process that releases the token and re-enters
    lands behind everyone already waiting instead of racing them for the
    sub-poll-interval window its own release opened. Acquisition itself stays a
    non-blocking retry so the periodic notice and the deadline keep running on
    this thread -- a blocking flock would need a timer thread or SIGALRM, and
    this wrapper's whole contract is that no signal path frees a live build's
    token.
    """
    import fcntl

    started = time.monotonic()
    deadline = started + timeout
    notice_at, since = started, time.strftime("%H:%M:%S")
    with enqueue(path) as ticket:
        while True:
            ahead, queued = ticket.survey() if ticket is not None else (0, 1)
            if ahead == 0:
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    return
                except OSError as exc:
                    if exc.errno not in _WOULD_BLOCK:
                        raise BuildTokenError(f"build token {path} is unusable: {exc}") from exc
            now = time.monotonic()
            if now >= notice_at:
                # A stall that prints nothing is indistinguishable from a hung
                # build, and the default deadline here is four hours. The place
                # in line separates the two stalls that used to read alike: a
                # long build ahead holds the position, being overtaken raises it.
                note = (f"build token: waiting for {path} since {since}: another release"
                        f" build holds it (queue position {ahead + 1} of {queued};"
                        f" {now - started:.0f}s of {timeout:g}s; raise"
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
# /opt/homebrew/bin, $HOME/.cache/sccache, 10G and 0 literals are copied from
# `setup_sccache_env` (scripts/_defaults.sh:25) -- a bash function and a dict cannot
# share an implementation -- so those four defaults move in both places or neither.
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
    # 0 disables the idle exit. Campaign builds queue behind the token for tens of
    # minutes, so the 600s default reaps the daemon between them and its counters
    # restart at zero -- which reads as "sccache is off" and gets it re-enabled.
    env["SCCACHE_IDLE_TIMEOUT"] = env.get("SCCACHE_IDLE_TIMEOUT") or "0"
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
