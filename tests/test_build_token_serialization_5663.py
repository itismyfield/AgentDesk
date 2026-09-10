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

import fcntl
import contextlib
import functools
import os
import signal
import subprocess
import sys
import tempfile
import time
import types
import unittest
from pathlib import Path
from unittest import mock

REPO = Path(__file__).resolve().parent.parent
SCRIPTS = REPO / "scripts"
CANONICAL = "/tmp/adk-build-token.lock"

sys.path.insert(0, str(SCRIPTS))
import build_token as bt  # noqa: E402

SUPERVISED_NAMES_UNDER_TEST = (
    "SIGHUP", "SIGINT", "SIGQUIT", "SIGEMT", "SIGALRM", "SIGTERM",
    "SIGXCPU", "SIGVTALRM", "SIGPROF", "SIGUSR1", "SIGUSR2",
)
# Outside the wrapper by design, as build_token.py says (install.sh matches its
# source install plus the two help texts that print the command for an operator).
UNWIRED_BY_DESIGN = ("Makefile", "scripts/install.sh")

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


def release_cargo_sites() -> dict[str, list[str]]:
    """Release cargo invocations per tracked build script, discovered by scanning."""
    tracked = subprocess.run(["git", "-C", str(REPO), "ls-files"], check=True,
                             capture_output=True, text=True).stdout.split()
    found: dict[str, list[str]] = {}
    for rel in tracked:
        if rel.endswith(".sh") or Path(rel).name == "Makefile":
            hits = [s for s in map(str.strip, joined_lines((REPO / rel).read_text("utf-8")))
                    if "cargo build" in s and not s.startswith(("#", "echo"))
                    and ("--release" in s or "--profile" in s)]
            if hits:
                found[rel] = hits
    return found


class TokenTestCase(unittest.TestCase):
    """Gives every test its own temporary token; the canonical path is untouched."""

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

    def driver(self, body: str, *args: str, env: dict[str, str] | None = None):
        merged = dict(os.environ)
        merged.update(env or {})
        proc = subprocess.Popen(
            [sys.executable, "-c", _SEAL + body, str(self.token), *args],
            env=merged, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        for pipe in (proc.stdout, proc.stderr):
            self.addCleanup(pipe.close)
        return proc


class DerivationTests(unittest.TestCase):
    """The supervised set must be derived, never re-enumerated by hand."""

    def test_the_set_is_recomputed_from_signal_signals(self) -> None:
        excluded = bt.excluded_signal_names()
        expected = sorted({num for num, name in bt.signal_domain(signal) if name not in excluded
                           and signal.getsignal(num) is not signal.SIG_IGN})
        self.assertEqual(list(bt.supervised_signals()), expected)
        self.assertTrue(expected, "platform reported no terminating signals")

    def test_the_domain_spans_the_realtime_range_not_just_named_members(self) -> None:
        # Linux names only the ends of its realtime range -- the numbers between
        # default to Term with no enum member -- and Darwin has none to skip on.
        realtime = type("Rt", (), {"Signals": (signal.SIGTERM,), "SIGRTMIN": 34, "SIGRTMAX": 38})
        self.assertEqual(bt.supervised_signals(source=realtime), (int(signal.SIGTERM), *range(34, 39)))
        enum_only = type("NoRt", (), {"Signals": (signal.SIGTERM,)})
        self.assertEqual(bt.signal_domain(enum_only), ((int(signal.SIGTERM), "SIGTERM"),))
        if hasattr(signal, "SIGRTMIN"):  # Linux CI: the same hole, for real
            self.assertIn(int(signal.SIGRTMIN) + 1, bt.supervised_signals())

    def test_the_darwin_discard_set_stays_platform_scoped(self) -> None:
        # Linux signal(7) gives SIGIO the Term default, so a wider scope here would
        # reintroduce the original class of miss.
        darwin_only = set(bt._DEFAULT_NOT_TERMINATE_DARWIN)
        self.assertEqual(darwin_only & bt.excluded_signal_names(),
                         darwin_only if sys.platform == "darwin" else set())

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
    def test_every_release_cargo_site_in_the_tree_is_wired_or_declared_unwired(self) -> None:
        sites = release_cargo_sites()
        self.assertGreaterEqual(len(sites), 4, sites)
        doc = (SCRIPTS / "build_token.py").read_text(encoding="utf-8")
        for rel, lines in sites.items():
            wired = rel not in UNWIRED_BY_DESIGN
            for line in lines:
                self.assertEqual("build_token.py" in line, wired, f"{rel}: wiring: {line}")
                if wired and "| tail -" in line:
                    self.assertIn("3>&2", line, "a log pipe must not swallow the notices")
        for rel in UNWIRED_BY_DESIGN:
            self.assertIn(rel, sites, f"{rel} stopped building a release; fix the disclosure")
            self.assertIn(Path(rel).name, doc, f"{rel} builds a release undisclosed")

    def test_the_win32_backend_has_a_production_caller(self) -> None:
        source = (SCRIPTS / "build_token.py").read_text(encoding="utf-8")
        self.assertIn("supervise_windows", source)
        self.assertIn('sys.platform == "win32"', source)

    def test_ci_runs_this_suite(self) -> None:
        checks = (SCRIPTS / "ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn("tests.test_build_token_serialization_5663", checks)


class DeployPreflightTokenTests(unittest.TestCase):
    def test_source_build_queues_but_artifact_and_host_pressure_refuse(self) -> None:
        # Execute the actual preflight/build branches, never the full deployment.
        deploy = (SCRIPTS / "deploy-release.sh").read_text()
        preflight = deploy.split('if [ "$DEPLOY_TEST_MODE" != "1" ]; then', 1)[1]
        preflight = 'if [ "$DEPLOY_TEST_MODE" != "1" ]; then' + preflight.split("# #743:", 1)[0]
        build = deploy.split('if [ -z "${AGENTDESK_DEPLOY_BINARY:-}" ]; then')[-1]
        build = 'if [ -z "${AGENTDESK_DEPLOY_BINARY:-}" ]; then' + build.split("# Rebuild dashboard", 1)[0]
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            token, marker = root / "token", root / "built"
            driver = root / "driver.py"
            driver.write_text(_SEAL + """
assert sys.argv[2] == "scripts/build_token.py"
assert sys.argv[3] == "--"
assert sys.argv[4:6] == ["bash", "-c"]
command = sys.argv[4:-1]
idx = command.index("cargo")
assert command[idx:idx + 2] == ["cargo", "build"]
command[idx:] = [sys.executable, "-c", "from pathlib import Path; Path(" + repr(sys.argv[-1]) + ").touch()"]
raise SystemExit(bt.main([sys.argv[2], "--"] + command))
""")
            script = root / "scenario.sh"
            script.write_text("""set -eu
. "$1/_defaults.sh"
DEPLOY_TEST_MODE=0
REPO="$1/.."
SCRIPT_DIR="$6"
SOURCE_BINARY="$4"
DEPLOY_BUILD_PROFILE="$5"
_preflight_builder_pids() { case "$1" in cargo|rustc) echo 55555;; esac; }
_preflight_cpu_count() { echo 8; }
_preflight_loadavg_1min() { echo "${TEST_LOAD:-1}"; }
_preflight_mem_pressure_level() { echo "${TEST_PRESSURE:-1}"; }
_preflight_high_cpu_processes() { :; }
_preflight_deploy_target_pids() { :; }
DRIVER="$2"; TOKEN="$3"; MARKER="$4"
python3() { command "$TEST_PYTHON" "$DRIVER" "$TOKEN" "$@" "$MARKER"; }
""" + preflight + build)
            # Real default guard and real high-CPU classifier, only OS data mocked.
            defaults = root / "_defaults.sh"
            defaults.write_text((SCRIPTS / "_defaults.sh").read_text() + """
_preflight_cpu_count() { echo 8; }
_preflight_loadavg_1min() { echo "${AFTER_LOAD:-1}"; }
_preflight_mem_pressure_level() { echo "${AFTER_PRESSURE:-1}"; }
_preflight_deploy_target_pids() { :; }
pgrep() { [ -n "${AFTER_BUILDER:-}" ] && [ "$2" = "$AFTER_BUILDER" ] && echo 55555; return 0; }
ps() {
 case "$1" in
  -o) echo 123;;
  *) printf '111 123 99 01:00:00 00:59:00 python\\n222 123 99 01:00:00 00:59:00 bash\\n'
     [ -z "${AFTER_HOT:-}" ] || printf '333 456 99 01:00:00 00:59:00 rustc\\n';;
 esac
}
""")
            env = dict(os.environ, TEST_PYTHON=sys.executable,
                       ADK_BUILD_TOKEN_WAIT_TIMEOUT_SECS="2")
            for key in list(env):
                if key.startswith("AGENTDESK_DEPLOY_") or key == bt.LEASE_ENV:
                    env.pop(key)
            command = ["bash", str(script), str(SCRIPTS), str(driver), str(token), str(marker)]
            for profile in ("release", "dev"):
                with self.subTest(profile=profile), token.open("a+") as holder:
                    fcntl.flock(holder, fcntl.LOCK_EX)
                    with subprocess.Popen(command + [profile, str(root)], env=env, stdout=subprocess.PIPE,
                                          stderr=subprocess.PIPE, text=True) as child:
                        try:
                            notice = child.stderr.readline()
                            self.assertIn("build token: waiting for", notice)
                            self.assertFalse(marker.exists(), "build started before token release")
                            self.assertIsNone(child.poll(), "busy token must queue, not reject")
                        finally:
                            fcntl.flock(holder, fcntl.LOCK_UN)
                        out, err = child.communicate(timeout=10)
                        self.assertEqual(child.returncode, 0, out + err)
                        self.assertTrue(marker.exists())
                        marker.unlink()
            for extra, reason in (({"TEST_LOAD": "99"}, "load average"),
                                  ({"TEST_PRESSURE": "4"}, "memory pressure"),
                                  ({"AGENTDESK_DEPLOY_BINARY": "artifact"}, "concurrent build tool"),
                                  ({"AFTER_BUILDER": "cargo"}, "concurrent build tool"),
                                  ({"AFTER_BUILDER": "rustc"}, "concurrent build tool"),
                                  ({"AFTER_HOT": "1"}, "SUSTAINED runaway"),
                                  ({"AFTER_BUILDER": "UnrealEditor"}, "concurrent build tool"),
                                  ({"AFTER_LOAD": "99"}, "load average"),
                                  ({"AFTER_PRESSURE": "4"}, "memory pressure")):
                with self.subTest(extra=extra):
                    result = subprocess.run(command + ["release", str(root)], env=dict(env, **extra),
                                            capture_output=True, text=True, timeout=10)
                    self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                    self.assertIn(reason, result.stderr)
                    self.assertNotIn("build token: waiting", result.stderr)
                    self.assertFalse(marker.exists())


class WaitTimeoutTests(TokenTestCase):
    def test_unusable_overrides_fall_back_to_the_default(self) -> None:
        for raw in ("", "nope", "0", "-5", "nan", "inf"):
            self.assertEqual(bt.wait_timeout_secs({bt.WAIT_TIMEOUT_ENV: raw}),
                             bt.DEFAULT_WAIT_TIMEOUT_SECS, raw)
        self.assertEqual(bt.wait_timeout_secs({}), bt.DEFAULT_WAIT_TIMEOUT_SECS)

    def test_a_usable_override_is_honored(self) -> None:
        self.assertEqual(bt.wait_timeout_secs({bt.WAIT_TIMEOUT_ENV: "1.5"}), 1.5)

    def test_the_first_blocked_wait_reports_and_the_timeout_names_its_override(self) -> None:
        # A stall that prints nothing is indistinguishable from a hung build.
        notes = self.tmp / "notes.txt"
        note_fd = os.open(notes, os.O_WRONLY | os.O_CREAT, 0o600)
        self.addCleanup(os.close, note_fd)
        os.environ[bt.DIAG_FD_ENV] = str(note_fd)
        self.addCleanup(os.environ.pop, bt.DIAG_FD_ENV, None)
        fcntl.flock(self.open_token(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        with self.assertRaises(bt.BuildTokenTimeout) as caught:
            bt.acquire(self.open_token(), str(self.token), 0.1)
        notice = notes.read_text()  # only the first-wait notice reaches this fd
        self.assertIn(str(self.token), notice, "the first wait must name the token")
        self.assertIn(bt.WAIT_TIMEOUT_ENV, notice, "the only escape hatch must be named")
        self.assertIn(bt.WAIT_TIMEOUT_ENV, str(caught.exception))
        self.assertIn("ancestor", str(caught.exception))


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
        rc = bt.run([sys.executable, "-c", ""], env={bt.WAIT_TIMEOUT_ENV: "1"}, path=str(self.token))
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

        before_usr1, before_pipe = signal.getsignal(signal.SIGUSR1), signal.getsignal(signal.SIGPIPE)
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


class ReentrantContractTests(TokenTestCase):
    """One-hop opt-in, using the real CLI with only its token path isolated."""

    def isolated_cli(self, entered: Path | None = None) -> Path:
        source = (SCRIPTS / "build_token.py").read_text()
        constant = f'CANONICAL_TOKEN_PATH = "{CANONICAL}"'
        self.assertEqual(source.count(constant), 1)
        replacement = f"CANONICAL_TOKEN_PATH = {str(self.token)!r}"
        sealed = source.replace(constant, replacement)
        self.assertEqual(sealed.replace(replacement, constant), source)
        # Functions remain byte-identical; only the constant and entry seal differ.
        seal = """
_open_before_seal = os.open
def _sealed_open(path, *args, **kwargs):
    if "adk-build-token.lock" in str(path):
        raise AssertionError("fixture breach: canonical token open")
    return _open_before_seal(path, *args, **kwargs)
os.open = _sealed_open
"""
        if entered is not None:
            seal += (f"if LEASE_ENV in os.environ:\n    with open({str(entered)!r}, 'w') as out:\n"
                     "        out.write('%d %d' % (os.getpid(), os.getppid()))\n")
        wrapper = self.tmp / "build_token.py"
        wrapper.write_text(sealed.replace('if __name__ == "__main__":', seal + '\nif __name__ == "__main__":'))
        return wrapper

    @contextlib.contextmanager
    def offer(self, holder: int):
        with bt.delegated_spawn(holder, [sys.executable, str(SCRIPTS / "build_token.py")], {}) as offered:
            yield offered

    def receive(self, raw: str, command: list[str] | None = None, *, delegate: bool = False) -> int:
        real_open = os.open
        unexpected = self.tmp / "unexpected.command"
        def sealed_open(path, *args, **kwargs):
            self.assertNotIn("adk-build-token.lock", str(path), "fixture breach")
            return real_open(path, *args, **kwargs)
        with mock.patch.dict(os.environ, {bt.LEASE_ENV: raw}), mock.patch.object(os, "open", sealed_open):
            rc = bt.run(command or [sys.executable, "-c", f"open({str(unexpected)!r}, 'w').write('ran')"],
                        env={bt.WAIT_TIMEOUT_ENV: "1"}, path=str(self.token), delegate_lease=delegate)
        if command is None:
            self.assertFalse(unexpected.exists(), "rejected lease ran the protected command")
        return rc

    def test_nested_cli_completes_under_outer_lease_without_self_deadlock(self) -> None:
        entered, completed = self.tmp / "inner.entered", self.tmp / "command.completed"
        before = self.token.stat()
        # This is a Python sentinel, not Cargo or a deploy simulation. Its
        # independent open must still see a live lease when it finally runs.
        sentinel = """
import fcntl, os, sys
fd = os.open(sys.argv[1], os.O_RDWR)
try:
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        with open(sys.argv[2], "a") as completed:
            completed.write("ran\\n")
    else:
        raise AssertionError("nested command ran without a live outer lease")
finally:
    os.close(fd)
"""
        scan = self.tmp / "leaf.fds"
        leaf = (_CHILD_FD_SCAN + f"\nassert {bt.LEASE_ENV!r} not in os.environ\n"
                f"sys.argv = [sys.argv[0], sys.argv[2], {str(completed)!r}]\n" + sentinel)
        protected = [sys.executable, "-c", leaf, str(scan), str(self.token)]
        wrapper = self.isolated_cli(entered)
        inner = [sys.executable, str(wrapper), "--", *protected]
        outer = subprocess.Popen(
            [sys.executable, str(wrapper), "--delegate-lease", "--", *inner],
            env={**os.environ, bt.WAIT_TIMEOUT_ENV: "1", bt.DIAG_FD_ENV: ""},
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.addCleanup(outer.stdout.close)
        self.addCleanup(outer.stderr.close)
        self.addCleanup(outer.wait, timeout=10)
        self.addCleanup(outer.terminate)
        _, err = outer.communicate(timeout=15)
        self.assertNotIn("fixture breach", err)
        self.assertTrue(entered.exists(), f"inner wrapper never started: {err}")
        inner_pid, parent_pid = map(int, entered.read_text().split())
        self.assertNotEqual(inner_pid, outer.pid)
        self.assertEqual(parent_pid, outer.pid, "inner must be the real protected child")
        after = self.token.stat()
        self.assertEqual((before.st_dev, before.st_ino), (after.st_dev, after.st_ino))
        self.assertEqual(
            outer.returncode, 0,
            "accepted nested invocation must finish while the outer wrapper owns "
            f"the same token, instead of timing out reacquiring it: {err}",
        )
        self.assertTrue(completed.exists(), "nested protected command was never run")
        self.assertEqual(completed.read_text(), "ran\n", "protected command must run once")
        self.assertEqual(scan.read_text(), "[]", "delegated leaf inherited a lease FD")

    def test_malformed_carriers_and_non_wrapper_delegation_are_refused(self) -> None:
        for raw in ("", "1", "1:1:3:11:0:0:" + "a" * 32,
                    "1:1:1000000:1000001:0:0:" + "a" * 32):
            with self.subTest(raw=raw):
                self.assertEqual(self.receive(raw), bt.EXIT_TOKEN_UNUSABLE)
        marker = self.tmp / "shell.ran"
        self.assertEqual(bt.run(["/bin/sh", "-c", f"touch {marker}"], path=str(self.token),
                                delegate_lease=True), bt.EXIT_TOKEN_UNUSABLE)
        self.assertFalse(marker.exists())

    def test_foreign_open_and_unlocked_leases_reject_genuine_tickets(self) -> None:
        for held in (True, False):
            with self.subTest(held=held):
                issuer = self.open_token()
                if held:
                    fcntl.flock(issuer, fcntl.LOCK_EX)
                with self.offer(issuer) as (env, _):
                    parts = env[bt.LEASE_ENV].split(":")
                    foreign = fcntl.fcntl(self.open_token(), fcntl.F_DUPFD_CLOEXEC, 10)
                    ticket = fcntl.fcntl(int(parts[3]), fcntl.F_DUPFD_CLOEXEC, 10)
                    parts[2:4] = [str(foreign), str(ticket)]
                    self.assertEqual(self.receive(":".join(parts)), bt.EXIT_TOKEN_UNUSABLE)
                fcntl.flock(issuer, fcntl.LOCK_UN)

    def test_ticket_nonce_generation_and_identity_rejections_close_received_fds(self) -> None:
        with bt.hold_token(str(self.token), {}) as issuer:
            for field, value in ((1, "2"), (4, "-1"), (6, "b" * 32), (6, "")):
                with self.subTest(field=field), self.offer(issuer) as (env, _):
                    parts = env[bt.LEASE_ENV].split(":")
                    owned = [fcntl.fcntl(int(parts[i]), fcntl.F_DUPFD_CLOEXEC, 10) for i in (2, 3)]
                    parts[2:4] = list(map(str, owned))
                    parts[field] = value
                    self.assertEqual(self.receive(":".join(parts)), bt.EXIT_TOKEN_UNUSABLE)
                    for fd in owned:
                        with self.assertRaises(OSError):
                            os.fstat(fd)

    def test_only_one_sibling_consumes_the_ticket_and_outer_lease_survives(self) -> None:
        wrapper, marker = self.isolated_cli(), self.tmp / "sibling.ran"
        leaf = [sys.executable, "-c", f"open({str(marker)!r}, 'a').write('ran\\n')"]
        with bt.hold_token(str(self.token), {}) as issuer, self.offer(issuer) as (env, fds):
            siblings = [subprocess.Popen([sys.executable, str(wrapper), "--", *leaf],
                        env={**os.environ, **env, bt.WAIT_TIMEOUT_ENV: "1"}, pass_fds=fds,
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE) for _ in range(2)]
            for child in siblings:
                self.addCleanup(child.wait, timeout=10)
                self.addCleanup(child.terminate)
                self.addCleanup(child.stdout.close)
                self.addCleanup(child.stderr.close)
                child.communicate(timeout=15)
            self.assertEqual(sorted(child.returncode for child in siblings), [0, 69])
            self.assertEqual(marker.read_text(), "ran\n")
            probe = self.open_token()
            with self.assertRaises(BlockingIOError):
                fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_process_carrier_is_used_with_explicit_child_env_and_leaf_is_stripped(self) -> None:
        out = self.tmp / "leaf.env"
        command = [sys.executable, "-c", f"import os; open({str(out)!r}, 'w').write(str({bt.LEASE_ENV!r} in os.environ))"]
        with bt.hold_token(str(self.token), {}) as issuer, self.offer(issuer) as (env, _):
            parts = env[bt.LEASE_ENV].split(":")
            parts[2:4] = [str(fcntl.fcntl(int(parts[i]), fcntl.F_DUPFD_CLOEXEC, 10)) for i in (2, 3)]
            self.assertEqual(self.receive(":".join(parts), command), 0)
            self.assertEqual(out.read_text(), "False")

    def test_inherited_receiver_cannot_issue_a_second_generation(self) -> None:
        with bt.hold_token(str(self.token), {}) as issuer, self.offer(issuer) as (env, _):
            parts = env[bt.LEASE_ENV].split(":")
            owned = [fcntl.fcntl(int(parts[i]), fcntl.F_DUPFD_CLOEXEC, 10) for i in (2, 3)]
            parts[2:4] = list(map(str, owned))
            self.assertEqual(self.receive(":".join(parts), delegate=True), 69)
            for fd in owned:
                with self.assertRaises(OSError):
                    os.fstat(fd)

    def test_option_boundary_windows_refusal_and_spawn_failure_cleanup(self) -> None:
        with mock.patch.object(bt, "run", return_value=7) as run:
            self.assertEqual(bt.main(["build_token.py", "--", "--delegate-lease", "arg"]), 7)
            self.assertEqual(run.call_args.args[0], ["--delegate-lease", "arg"])
            self.assertFalse(run.call_args.kwargs["delegate_lease"])
        with mock.patch.object(sys, "platform", "win32"):
            self.assertEqual(bt.run(["unused"], delegate_lease=True), 69)
            with mock.patch.dict(os.environ, {bt.LEASE_ENV: "stale"}):
                self.assertEqual(bt.run(["unused"]), 69)
        with bt.hold_token(str(self.token), {}) as issuer:
            passed = []
            def fail_spawn(*args, **kwargs):
                passed.extend(kwargs["pass_fds"])
                raise OSError("spawn refused")
            with mock.patch.object(subprocess, "Popen", side_effect=fail_spawn), self.assertRaises(bt.BuildTokenError):
                bt.run_protected([sys.executable, bt.__file__], {}, bt._Supervisor(), issuer)
            self.assertEqual(len(passed), 2)
            for fd in passed:
                with self.assertRaises(OSError):
                    os.fstat(fd)


@unittest.skipUnless(sys.platform == "darwin", "SIGEMT is Darwin-specific here")
class SigemtLifetimeTests(TokenTestCase):
    """The exact regression: SIGEMT to the wrapper must not free a live build."""

    def test_sigemt_never_frees_the_token_while_the_direct_child_lives(self) -> None:
        wrapper_pid = self.tmp / "wrapper.pid"
        child_pid = self.tmp / "child.pid"
        release = self.tmp / "release"
        body = (f"open({str(wrapper_pid)!r}, 'w').write(str(os.getpid()))\n"
                f"run([sys.executable, '-c', {_CHILD_IGNORES_EMT!r},"
                f" {str(child_pid)!r}, {str(release)!r}])\n")
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


class MutationOwnerTests(TokenTestCase):
    # Each of these fails if one specific production line is dropped or inverted.
    def test_the_win32_branch_delegates_to_its_backend(self) -> None:
        noop = [sys.executable, "-c", ""]
        backend = types.ModuleType("build_token_win32")
        backend.BuildTokenWindowsError = RuntimeError
        backend.supervise_windows = lambda command, env: 21 if command == noop else 0
        with mock.patch.dict(sys.modules, {"build_token_win32": backend}), \
                mock.patch.object(sys, "platform", "win32"):
            rc = bt.run(noop, env={}, path=str(self.token))
        self.assertEqual(rc, 21, "the win32 branch must delegate and return its result")

    def test_a_token_replaced_while_a_wrapper_waits_is_refused(self) -> None:
        holder = self.open_token()
        fcntl.flock(holder, fcntl.LOCK_EX | fcntl.LOCK_NB)
        waiter = self.driver("raise SystemExit(run([sys.executable, '-c', '']))",
                             env={bt.WAIT_TIMEOUT_ENV: "30", bt.DIAG_FD_ENV: ""})
        self.addCleanup(waiter.kill)
        self.assertIn("waiting for", waiter.stderr.readline())  # blocked, its fd open
        self.token.unlink()
        self.token.touch()
        fcntl.flock(holder, fcntl.LOCK_UN)
        _, err = waiter.communicate(timeout=60)
        self.assertEqual(waiter.returncode, bt.EXIT_TOKEN_UNUSABLE, err)
        self.assertIn("was replaced while held", err)

    def test_the_wrapper_forwards_the_signal_to_a_child_that_never_self_signals(self) -> None:
        pid_file, forwarded = self.tmp / "child.pid", self.tmp / "forwarded"
        # The child neither ignores the signal nor sends it to itself: only a real
        # forward from the wrapper can run its trap.
        script = (f"trap 'printf TERM > {forwarded}; exit 29' TERM;"
                  f" echo $$ > {pid_file}; sleep 6 & wait")
        proc = self.driver(f"raise SystemExit(run(['/bin/sh', '-c', {script!r}]))")
        self.addCleanup(proc.kill)
        wait_for(pid_file)
        os.kill(proc.pid, signal.SIGTERM)  # the wrapper only, never the child
        _, err = proc.communicate(timeout=60)
        self.assertEqual(forwarded.read_text() if forwarded.exists() else "(nothing)",
                         "TERM", f"the wrapper never forwarded the signal: {err}")
        self.assertNotEqual(int(pid_file.read_text()), proc.pid, "the child must be a child")
        self.assertEqual(proc.returncode, -int(signal.SIGTERM), err)


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
