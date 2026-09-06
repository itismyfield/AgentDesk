#!/usr/bin/env python3
"""Owners for the derived-signal build-token supervision (#5663).

Two earlier revisions hand-enumerated the terminating signals and shipped a
whitelist that missed eight signals, then missed SIGEMT. Each miss produced the
same failure: the wrapper died, its build stayed alive, and a second wrapper
took the token (`wrapper_rc=-7 / child_alive / second_rc=0`). These tests pin
the replacement contract -- the set is *derived* from `signal.Signals` -- and
reproduce the SIGEMT case end to end.

Canonical-token safety: every `run()` call passes `path=` a temporary token, and
subprocess drivers additionally rebind `build_token.run` to a
`functools.partial` bound to that temporary path so `main()` is covered too. The
drivers seal `os.open`, so any attempt to touch /tmp/adk-build-token.lock fails
the fixture itself rather than reaching the real file.
"""

from __future__ import annotations

import functools
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

SUPERVISED_NAMES_UNDER_TEST = (
    "SIGHUP", "SIGINT", "SIGQUIT", "SIGEMT", "SIGALRM", "SIGTERM",
    "SIGXCPU", "SIGVTALRM", "SIGPROF", "SIGUSR1", "SIGUSR2",
)

_SEAL = f"""
import functools, os, resource, signal, sys, time
resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
sys.path.insert(0, {str(SCRIPTS)!r})
import build_token as bt
bt.CANONICAL_TOKEN_PATH = "/sealed/canonical/must-not-be-opened"
_real_open = os.open
def _guard(path, *a, **k):
    if "adk-build-token.lock" in str(path):
        raise AssertionError("fixture breach: canonical build token was opened")
    return _real_open(path, *a, **k)
os.open = _guard
TOKEN = sys.argv[1]
bt.run = functools.partial(bt.run, path=TOKEN)
run = bt.run
"""

_CHILD_IGNORES_EMT = """
import os, signal, sys, time
signal.signal(signal.SIGEMT, signal.SIG_IGN)
open(sys.argv[1], "w").write(str(os.getpid()))
deadline = time.monotonic() + 90
while time.monotonic() < deadline and not os.path.exists(sys.argv[2]):
    time.sleep(0.05)
"""

_CHILD_FD_SCAN = """
import os, sys
token = os.stat(sys.argv[2])
hits = []
for name in os.listdir("/dev/fd"):
    try:
        st = os.fstat(int(name))
    except (ValueError, OSError):
        continue
    if (st.st_dev, st.st_ino) == (token.st_dev, token.st_ino):
        hits.append(name)
open(sys.argv[1], "w").write(repr(hits))
"""


def wait_for(path: Path, timeout: float = 20.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {path}")


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def joined_lines(text: str) -> list[str]:
    """Join backslash continuations so a wrapped command reads as one line."""
    out: list[str] = []
    for raw in text.splitlines():
        if out and out[-1].endswith("\\"):
            out[-1] = out[-1][:-1] + " " + raw.strip()
        else:
            out.append(raw)
    return out


class TokenTestCase(unittest.TestCase):
    """Gives every test its own temporary token; the canonical path is untouched."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.token = self.tmp / "token.lock"
        self.token.touch()
        self.addCleanup(self._tmp.cleanup)
        self.assertNotEqual(str(self.token), CANONICAL)

    def driver(self, body: str, *args: str, env: dict[str, str] | None = None):
        merged = dict(os.environ)
        merged.update(env or {})
        proc = subprocess.Popen(
            [sys.executable, "-c", _SEAL + body, str(self.token), *args],
            env=merged, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        for pipe in (proc.stdout, proc.stderr):
            if pipe is not None:
                self.addCleanup(pipe.close)
        return proc


class DerivationTests(unittest.TestCase):
    """The supervised set must be derived, never re-enumerated by hand."""

    def test_the_set_is_recomputed_from_signal_signals(self) -> None:
        excluded = bt.excluded_signal_names()
        expected = sorted(
            {
                int(s)
                for s in signal.Signals
                if s.name not in excluded and signal.getsignal(s) is not signal.SIG_IGN
            }
        )
        self.assertEqual(list(bt.supervised_signals()), expected)
        self.assertTrue(expected, "platform reported no terminating signals")

    def test_no_supervised_signal_is_named_as_a_literal_target(self) -> None:
        source = (SCRIPTS / "build_token.py").read_text(encoding="utf-8")
        for name in SUPERVISED_NAMES_UNDER_TEST:
            for literal in (f'"{name}"', f"'{name}'"):
                self.assertTrue(
                    literal not in source,
                    f"{name} is enumerated as a target; the set must stay derived",
                )
        self.assertNotIn("_SUPERVISED_SIGNAL_NAMES", source)

    def test_an_unnamed_platform_signal_defaults_to_supervised(self) -> None:
        class Unknown(int):
            name = "SIGPLATFORMSPECIFIC"

        self.assertEqual(bt.supervised_signals([Unknown(int(signal.SIGTERM))]),
                         (int(signal.SIGTERM),))

    def test_uncatchable_and_non_terminating_defaults_are_excluded(self) -> None:
        supervised = bt.supervised_signals()
        for name in ("SIGKILL", "SIGSTOP", "SIGSEGV", "SIGILL", "SIGFPE", "SIGBUS",
                     "SIGABRT", "SIGSYS", "SIGTRAP", "SIGCHLD", "SIGCONT",
                     "SIGURG", "SIGWINCH", "SIGTSTP", "SIGTTIN", "SIGTTOU"):
            sig = getattr(signal, name, None)
            if sig is not None:
                self.assertNotIn(int(sig), supervised, f"{name} must not be supervised")

    def test_already_ignored_signals_keep_the_callers_disposition(self) -> None:
        for name in ("SIGPIPE", "SIGXFSZ"):
            sig = getattr(signal, name, None)
            if sig is not None and signal.getsignal(sig) is signal.SIG_IGN:
                self.assertNotIn(int(sig), bt.supervised_signals())

    @unittest.skipUnless(sys.platform == "darwin", "SIGEMT default action is Darwin-specific")
    def test_darwin_sigemt_is_supervised_and_not_excluded(self) -> None:
        self.assertIn(int(signal.SIGEMT), bt.supervised_signals())
        self.assertNotIn("SIGEMT", bt.excluded_signal_names())


class WiringTests(unittest.TestCase):
    def test_every_release_cargo_site_runs_through_the_wrapper(self) -> None:
        sites = 0
        for name in ("build-release.sh", "deploy-release.sh"):
            for line in joined_lines((SCRIPTS / name).read_text(encoding="utf-8")):
                stripped = line.strip()
                if "cargo build" not in stripped or stripped.startswith(("#", "echo")):
                    continue
                sites += 1
                self.assertIn("build_token.py", stripped,
                              f"{name}: unserialized cargo build: {stripped}")
        self.assertGreaterEqual(sites, 3, "expected the known release cargo sites")

    def test_the_win32_backend_has_a_production_caller(self) -> None:
        source = (SCRIPTS / "build_token.py").read_text(encoding="utf-8")
        self.assertIn("supervise_windows", source)
        self.assertIn('sys.platform == "win32"', source)

    def test_ci_runs_this_suite(self) -> None:
        checks = (SCRIPTS / "ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn("tests.test_build_token_serialization_5663", checks)


class WaitTimeoutTests(unittest.TestCase):
    def test_unusable_overrides_fall_back_to_the_default(self) -> None:
        for raw in ("", "nope", "0", "-5", "nan", "inf"):
            self.assertEqual(bt.wait_timeout_secs({bt.WAIT_TIMEOUT_ENV: raw}),
                             bt.DEFAULT_WAIT_TIMEOUT_SECS, raw)
        self.assertEqual(bt.wait_timeout_secs({}), bt.DEFAULT_WAIT_TIMEOUT_SECS)

    def test_a_usable_override_is_honored(self) -> None:
        self.assertEqual(bt.wait_timeout_secs({bt.WAIT_TIMEOUT_ENV: "1.5"}), 1.5)


class FailClosedTests(TokenTestCase):
    def test_a_replaced_token_is_rejected(self) -> None:
        fd = os.open(self.token, os.O_RDWR)
        self.addCleanup(os.close, fd)
        bt.assert_live_token(fd, str(self.token))
        self.token.unlink()
        self.token.touch()
        with self.assertRaises(bt.BuildTokenError):
            bt.assert_live_token(fd, str(self.token))

    def test_a_removed_token_is_rejected(self) -> None:
        fd = os.open(self.token, os.O_RDWR)
        self.addCleanup(os.close, fd)
        self.token.unlink()
        with self.assertRaises(bt.BuildTokenError):
            bt.assert_live_token(fd, str(self.token))

    def test_a_timeout_is_classified_before_its_base_error(self) -> None:
        self.assertTrue(issubclass(bt.BuildTokenTimeout, bt.BuildTokenError))
        holder = self.driver("run([sys.executable, '-c', 'import time; time.sleep(6)'])")
        self.addCleanup(holder.wait)
        self.addCleanup(holder.kill)
        time.sleep(1.5)
        rc = bt.run([sys.executable, "-c", ""],
                    env={bt.WAIT_TIMEOUT_ENV: "1"}, path=str(self.token))
        self.assertEqual(rc, bt.EXIT_TOKEN_TIMEOUT)


class ExitCodeTests(TokenTestCase):
    def test_child_exit_codes_propagate(self) -> None:
        for want in (0, 1, 42):
            rc = bt.run(["/bin/sh", "-c", f"exit {want}"], path=str(self.token))
            self.assertEqual(rc, want)

    def test_a_signalled_child_is_reported_as_128_minus_the_signal(self) -> None:
        rc = bt.run(["/bin/sh", "-c", "kill -TERM $$"], path=str(self.token))
        self.assertEqual(rc, 128 + int(signal.SIGTERM))


class HandlerRestorationTests(TokenTestCase):
    def test_prior_handlers_including_sig_ign_are_restored(self) -> None:
        def custom(_signum, _frame):  # pragma: no cover - never delivered
            raise AssertionError("unexpected delivery")

        before_usr1 = signal.getsignal(signal.SIGUSR1)
        before_pipe = signal.getsignal(signal.SIGPIPE)
        signal.signal(signal.SIGUSR1, custom)
        self.addCleanup(signal.signal, signal.SIGUSR1, before_usr1)
        self.assertEqual(bt.run([sys.executable, "-c", ""], path=str(self.token)), 0)
        self.assertIs(signal.getsignal(signal.SIGUSR1), custom)
        self.assertIs(signal.getsignal(signal.SIGPIPE), before_pipe)


class SerializationTests(TokenTestCase):
    def test_the_protected_command_never_inherits_the_token(self) -> None:
        out = self.tmp / "fds.txt"
        rc = bt.run([sys.executable, "-c", _CHILD_FD_SCAN, str(out), str(self.token)],
                    path=str(self.token))
        self.assertEqual(rc, 0)
        self.assertEqual(out.read_text(), "[]")

    def test_a_second_wrapper_blocks_until_the_first_finishes(self) -> None:
        first = self.driver("run([sys.executable, '-c', 'import time; time.sleep(3)'])")
        self.addCleanup(first.wait)
        self.addCleanup(first.kill)
        time.sleep(1.0)
        started = time.monotonic()
        rc = bt.run([sys.executable, "-c", ""], env={bt.WAIT_TIMEOUT_ENV: "30"}, path=str(self.token))
        waited = time.monotonic() - started
        self.assertEqual(rc, 0)
        self.assertGreater(waited, 0.5, "second wrapper did not wait for the first")


@unittest.skipUnless(sys.platform == "darwin", "SIGEMT is Darwin-specific here")
class SigemtLifetimeTests(TokenTestCase):
    """The exact regression: SIGEMT to the wrapper must not free a live build."""

    def test_sigemt_never_frees_the_token_while_the_direct_child_lives(self) -> None:
        wrapper_pid = self.tmp / "wrapper.pid"
        child_pid = self.tmp / "child.pid"
        release = self.tmp / "release"
        body = (
            f"open({str(wrapper_pid)!r}, 'w').write(str(os.getpid()))\n"
            f"run([sys.executable, '-c', {_CHILD_IGNORES_EMT!r},"
            f" {str(child_pid)!r}, {str(release)!r}])\n"
        )
        first = self.driver(body)
        reaped = []

        def cleanup() -> None:
            release.touch()
            if not reaped:
                try:
                    first.wait(timeout=15)
                except subprocess.TimeoutExpired:  # pragma: no cover
                    first.kill()
                    first.wait()

        self.addCleanup(cleanup)

        wait_for(wrapper_pid)
        wait_for(child_pid)
        w1 = int(wrapper_pid.read_text())
        c1 = int(child_pid.read_text())
        self.assertEqual(w1, first.pid, "driver must report its own pid")
        self.assertTrue(alive(c1), "protected child should be running")

        # Only PIDs this test created and recorded are ever signalled.
        os.kill(w1, signal.SIGEMT)
        time.sleep(1.0)

        self.assertTrue(alive(c1), "the protected child must outlive the signal")
        second = bt.run([sys.executable, "-c", ""], env={bt.WAIT_TIMEOUT_ENV: "2"},
                        path=str(self.token))
        self.assertEqual(
            second, bt.EXIT_TOKEN_TIMEOUT,
            "a second wrapper acquired the token while the first build was alive",
        )
        self.assertTrue(alive(c1), "child died before the serialization check ended")

        release.touch()
        first.wait(timeout=30)
        reaped.append(True)
        self.assertEqual(first.returncode, -int(signal.SIGEMT),
                         "wrapper must die from SIGEMT only after reaping its child")
        self.assertFalse(alive(c1), "child must be reaped before the token is released")
        self.assertEqual(bt.run([sys.executable, "-c", ""], env={bt.WAIT_TIMEOUT_ENV: "10"},
                                path=str(self.token)), 0,
                         "token must be released once the build is done")


class CliTests(TokenTestCase):
    def test_the_cli_requires_a_command(self) -> None:
        self.assertEqual(bt.main(["build_token.py"]), bt.EXIT_USAGE)
        self.assertEqual(bt.main(["build_token.py", "--"]), bt.EXIT_USAGE)

    def test_the_cli_runs_through_the_bound_temporary_token(self) -> None:
        proc = self.driver("raise SystemExit(bt.main(['build_token.py', '--',"
                           " '/bin/sh', '-c', 'exit 7']))")
        out, err = proc.communicate(timeout=60)
        self.assertEqual(proc.returncode, 7, err)
        self.assertNotIn("fixture breach", err)


if __name__ == "__main__":
    unittest.main()
