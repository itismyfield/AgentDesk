#!/usr/bin/env python3
"""Owners for first-come-first-served build-token handoff (#5968).

`acquire()` used to retry a non-blocking flock on a bare timer, so the kernel
never queued the waiters and arrival order bought nothing. On 2026-09-17 a lane
that released the token and immediately re-entered won every handoff: the gap
between its release and its re-acquire was far shorter than the 0.5s poll, so no
outside waiter ever saw a free window. One process waited 49 minutes without
progress while a process that arrived 4m33s later ran a full test sweep, and two
further lanes sat behind that queue for about 40 minutes.

The headline test reproduces exactly that shape -- three staggered waiters plus
one latecomer timed to arrive in the release window -- and pins the order. The
rest pin what the fix must not cost: the periodic notice, the deadline (for a
queued waiter as much as for the oldest one), and liveness, since a ticket file
does not disappear on SIGKILL the way an flock does.

Canonical-token safety: every test binds a temporary token, and the subprocess
waiters seal `os.open` so touching /tmp/adk-build-token.lock fails the fixture.
"""

from __future__ import annotations

import fcntl
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SCRIPTS = REPO / "scripts"
CANONICAL = "/tmp/adk-build-token.lock"

sys.path.insert(0, str(SCRIPTS))
import build_token as bt  # noqa: E402

# argv: token label ready results hold [gate]. `ready` is written before any
# waiting starts, so the parent can tell "about to queue" from "still importing";
# `gate` (the latecomer only) then holds it until the parent opens the window.
_WAITER = f"""
import os, sys, time
sys.path.insert(0, {str(SCRIPTS)!r})
_real_open = os.open
def _guard(path, *a, **k):
    if "adk-build-token.lock" in str(path):
        raise AssertionError("fixture breach: canonical build token was opened")
    return _real_open(path, *a, **k)
os.open = _guard
import build_token as bt
token, label, ready, results, hold = sys.argv[1:6]
gate = sys.argv[6] if len(sys.argv) > 6 else ""
open(ready, "w").write(label)
while gate and not os.path.exists(gate):
    time.sleep(0.002)
fd = os.open(token, os.O_RDWR)
bt.acquire(fd, token, 120.0)
seen = os.open(results, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
os.write(seen, (label + chr(10)).encode())   # one short append, so it is atomic
os.close(seen)
time.sleep(float(hold))
os.close(fd)
"""

SETTLE = 0.3   # 300x the gap between a waiter's marker and its place in line
HOLD = 0.2     # how long each waiter keeps the token once it wins it


class FairnessTestCase(unittest.TestCase):
    """Every test binds its own temporary token; the canonical path is untouched."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.token = self.tmp / "token.lock"
        self.token.touch()
        self.addCleanup(self._tmp.cleanup)
        self.assertNotEqual(str(self.token), CANONICAL)

    def open_token(self) -> int:
        fd = os.open(self.token, os.O_RDWR)
        self.addCleanup(os.close, fd)
        return fd

    def hold_token(self) -> int:
        fd = self.open_token()
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return fd

    def waiter(self, label: str, results: Path, gate: Path | None = None) -> subprocess.Popen:
        ready = self.tmp / f"ready-{label}"
        args = [str(self.token), label, str(ready), str(results), str(HOLD)]
        proc = subprocess.Popen([sys.executable, "-c", _WAITER, *args, *([str(gate)] if gate else [])],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(proc.wait)
        self.addCleanup(self._kill, proc)
        for pipe in (proc.stdout, proc.stderr):
            self.addCleanup(pipe.close)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and not ready.exists():
            self.assertIsNone(proc.poll(), f"{label} exited before it waited")
            time.sleep(0.01)
        self.assertTrue(ready.exists(), f"{label} never reached the wait")
        time.sleep(SETTLE)
        return proc

    @staticmethod
    def _kill(proc: subprocess.Popen) -> None:
        if proc.poll() is None:
            proc.kill()

    def diag_notes(self) -> Path:
        notes = self.tmp / "notes.txt"
        fd = os.open(notes, os.O_WRONLY | os.O_CREAT, 0o600)
        self.addCleanup(os.close, fd)
        os.environ[bt.DIAG_FD_ENV] = str(fd)
        self.addCleanup(os.environ.pop, bt.DIAG_FD_ENV, None)
        return notes

    def tickets(self) -> list[str]:
        qdir = Path(bt.queue_dir(str(self.token)))
        if not qdir.is_dir():
            return []
        return sorted(n for n in os.listdir(qdir) if not n.startswith("."))


class ArrivalOrderTests(FairnessTestCase):
    def test_a_latecomer_cannot_take_the_window_its_own_release_opened(self) -> None:
        # The 2026-09-17 shape. Three waiters queue in order behind a held token,
        # then a fourth is released into the exact instant the token frees --
        # the sub-poll-interval window the old loop could not defend. Order of
        # acquisition is the whole assertion.
        results = self.tmp / "order.txt"
        gate = self.tmp / "gate"
        holder = self.hold_token()
        waiters = [self.waiter(label, results) for label in ("w1", "w2", "w3")]
        latecomer = self.waiter("late", results, gate=gate)
        fcntl.flock(holder, fcntl.LOCK_UN)
        gate.touch()  # released first, so the latecomer's first try finds it free
        for proc in (*waiters, latecomer):
            _, err = proc.communicate(timeout=120)
            self.assertEqual(proc.returncode, 0, err)
        self.assertEqual(results.read_text().split(),
                         ["w1", "w2", "w3", "late"],
                         "the token was not handed out in arrival order")

    def test_the_oldest_waiter_is_the_only_one_that_retries_the_lock(self) -> None:
        # Same invariant without the timing: the token is free the whole time,
        # and a waiter that is not first in line must still refuse to take it.
        with bt.enqueue(str(self.token)) as elder:
            self.assertIsNotNone(elder, "the queue must be usable in a temp dir")
            with self.assertRaises(bt.BuildTokenTimeout):
                bt.acquire(self.open_token(), str(self.token), 1.0)
        # ...and once the elder gives up its place, the same call succeeds.
        bt.acquire(self.open_token(), str(self.token), 5.0)


class WaitNoticeTests(FairnessTestCase):
    def test_the_notice_still_reaches_the_diag_fd_while_blocked(self) -> None:
        notes = self.diag_notes()
        self.hold_token()
        with self.assertRaises(bt.BuildTokenTimeout):
            bt.acquire(self.open_token(), str(self.token), 0.1)
        notice = notes.read_text()
        self.assertIn(str(self.token), notice, "the wait must name the token")
        self.assertIn(bt.WAIT_TIMEOUT_ENV, notice, "the only escape hatch must be named")
        self.assertIn("queue position 1 of 1", notice)

    def test_the_notice_reports_the_place_in_line_not_just_the_elapsed_time(self) -> None:
        # Elapsed seconds alone read the same whether the build ahead is long or
        # the waiter is being overtaken; the position is what separates them.
        notes = self.diag_notes()
        self.hold_token()
        self.waiter("elder", self.tmp / "order.txt")
        with self.assertRaises(bt.BuildTokenTimeout):
            bt.acquire(self.open_token(), str(self.token), 0.1)
        self.assertIn("queue position 2 of 2", notes.read_text())


class DeadlineTests(FairnessTestCase):
    def test_the_deadline_still_expires_for_the_oldest_waiter(self) -> None:
        self.hold_token()
        started = time.monotonic()
        with self.assertRaises(bt.BuildTokenTimeout) as caught:
            bt.acquire(self.open_token(), str(self.token), 1.0)
        self.assertLess(time.monotonic() - started, 30)
        self.assertIn("ancestor", str(caught.exception))

    def test_the_deadline_still_expires_for_a_waiter_that_never_gets_a_turn(self) -> None:
        # New shape: queued behind someone else, this caller never reaches the
        # flock at all, so the deadline is the only thing that can free it.
        self.hold_token()
        self.waiter("elder", self.tmp / "order.txt")
        started = time.monotonic()
        with self.assertRaises(bt.BuildTokenTimeout):
            bt.acquire(self.open_token(), str(self.token), 1.0)
        self.assertLess(time.monotonic() - started, 30)

    def test_the_default_deadline_and_poll_interval_are_unchanged(self) -> None:
        # Raising either would hide starvation rather than fix it (#5968).
        self.assertEqual(bt.DEFAULT_WAIT_TIMEOUT_SECS, 14400.0)
        self.assertEqual(bt.WAIT_POLL_SECS, 0.5)


class TicketLifetimeTests(FairnessTestCase):
    def test_a_killed_waiters_ticket_never_blocks_the_queue(self) -> None:
        # flock releases itself when a process dies; a ticket file does not.
        # Without reclamation this queue would be a worse deadlock than the
        # unfairness it replaced.
        holder = self.hold_token()
        elder = self.waiter("elder", self.tmp / "order.txt")
        self.assertEqual(len(self.tickets()), 1, "the elder never took a place in line")
        elder.send_signal(signal.SIGKILL)
        elder.wait(timeout=30)
        self.assertEqual(len(self.tickets()), 1, "a killed waiter leaves its file behind")
        fcntl.flock(holder, fcntl.LOCK_UN)
        bt.acquire(self.open_token(), str(self.token), 30.0)
        self.assertEqual(self.tickets(), [], "the dead ticket was never reclaimed")

    def test_a_ticket_no_process_ever_locked_is_reclaimed_too(self) -> None:
        qdir = Path(bt.queue_dir(str(self.token)))
        qdir.mkdir()
        (qdir / f"{7:020d}-999999-abandoned").write_text("left over from a reboot")
        bt.acquire(self.open_token(), str(self.token), 30.0)
        self.assertEqual(self.tickets(), [])

    def test_a_ticket_outlives_neither_a_win_nor_a_timeout(self) -> None:
        bt.acquire(self.open_token(), str(self.token), 30.0)
        self.assertEqual(self.tickets(), [], "a winner kept its place in line")
        holder = os.open(self.token, os.O_RDWR)
        self.addCleanup(os.close, holder)
        with self.assertRaises(bt.BuildTokenTimeout):
            bt.acquire(os.open(self.token, os.O_RDWR), str(self.token), 0.1)
        self.assertEqual(self.tickets(), [], "a timed-out waiter kept its place in line")

    def test_a_clobbered_counter_cannot_hand_a_newcomer_an_older_place(self) -> None:
        qdir = Path(bt.queue_dir(str(self.token)))
        qdir.mkdir()
        (qdir / bt._SEQUENCE_FILE).write_text("not a number")
        standing = f"{42:020d}-999999-standing"
        (qdir / standing).write_text("")
        self.assertEqual(bt.claim_sequence(str(qdir), os.listdir(qdir)), 43)

    def test_a_filename_that_is_not_a_ticket_is_ignored(self) -> None:
        self.assertIsNone(bt.ticket_sequence("README"))
        self.assertIsNone(bt.ticket_sequence(".seq"))
        self.assertEqual(bt.ticket_sequence(f"{5:020d}-1234-abcd"), 5)


class DegradationTests(FairnessTestCase):
    def test_an_unusable_queue_costs_ordering_never_the_build(self) -> None:
        # /tmp that cannot hold the queue must not be able to stop a release.
        Path(bt.queue_dir(str(self.token))).write_text("not a directory")
        bt.acquire(self.open_token(), str(self.token), 30.0)

    def test_an_unusable_queue_still_honors_the_deadline(self) -> None:
        Path(bt.queue_dir(str(self.token))).write_text("not a directory")
        self.hold_token()
        with self.assertRaises(bt.BuildTokenTimeout):
            bt.acquire(self.open_token(), str(self.token), 0.1)


class WiringTests(unittest.TestCase):
    def test_ci_runs_this_suite(self) -> None:
        checks = (SCRIPTS / "ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn("tests.test_build_token_fairness_5968", checks)

    def test_the_callers_interface_is_unchanged(self) -> None:
        # The lane scripts, CI and deploy-release.sh drive this file through its
        # CLI and its env vars only; the queue must stay invisible to them.
        for rel, needle in (("scripts/build-release.sh", 'build_token.py" -- \\'),
                            ("scripts/deploy-release.sh", "build_token.py -- bash -c")):
            self.assertIn(needle, (REPO / rel).read_text(encoding="utf-8"), rel)
        self.assertEqual(bt.parse_command(["build_token.py", "--", "cargo", "build"]),
                         ["cargo", "build"])


if __name__ == "__main__":
    unittest.main()
