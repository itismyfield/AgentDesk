import ast
import contextlib
import importlib.util
import io
import os
import signal
import subprocess
import sys
import threading
import time
import types
import unittest
from pathlib import Path
from unittest import mock

SCRIPT_PATH = Path(__file__).parents[1] / "scripts" / "ci-timeout.py"
SPEC = importlib.util.spec_from_file_location("ci_timeout", SCRIPT_PATH)
assert SPEC and SPEC.loader
ci_timeout = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = ci_timeout
SPEC.loader.exec_module(ci_timeout)


class Clock:
    now = 0.0

    def __init__(self):
        self.sleeps = []

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.now += seconds


class Process:
    def __init__(self, results=(), default=None):
        self.pid, self.returncode = 4413, None
        self.results, self.default = list(results), default
        self.terminate, self.kill = mock.Mock(), mock.Mock()

    def poll(self):
        result = self.results.pop(0) if self.results else self.default
        if result is not None:
            self.returncode = result
        return result


class CiTimeoutTests(unittest.TestCase):
    def fake_run(self, proc, *, timeout=1, pending=lambda: set(), clock=None, diag=False):
        clock = clock or Clock()
        with contextlib.ExitStack() as stack:
            for patcher in (
                mock.patch.object(ci_timeout, "_mask_forwarded_signals", return_value=True),
                mock.patch.object(ci_timeout.signal, "sigpending", side_effect=pending),
                mock.patch.object(ci_timeout, "_popen", return_value=proc),
                mock.patch.object(ci_timeout, "_monotonic", side_effect=clock.monotonic),
                mock.patch.object(ci_timeout, "_sleep", side_effect=clock.sleep),
            ):
                stack.enter_context(patcher)
            if not diag:
                stack.enter_context(mock.patch.object(ci_timeout, "_diagnose_capped"))
            return ci_timeout.run_command(timeout, ["cargo", "test"]), clock

    # F1
    def test_timeout_is_bounded_and_returns_124(self):
        proc = Process(default=None)

        def send(*_args, force=False, **_kwargs):
            if force:
                proc.returncode = proc.default = -signal.SIGKILL

        with mock.patch.object(ci_timeout, "_send_process_signal", side_effect=send):
            rc, clock = self.fake_run(proc, timeout=2)
        self.assertEqual(rc, 124)
        self.assertLessEqual(clock.now, 27.1)

    # Existing R4 fixture from line 33 of the original 82-line test.
    def test_killpg_fallback_terminates_then_kills_after_grace_period(self):
        proc = Process(default=None)
        with mock.patch.object(ci_timeout, "os", types.SimpleNamespace(environ={})):
            rc, _ = self.fake_run(proc)
        proc.terminate.assert_called_once_with()
        proc.kill.assert_called_once_with()
        self.assertEqual(rc, 124)

    # F2 + F4
    def test_external_sigkill_is_normalized_to_137(self):
        result = subprocess.run(
            [sys.executable, str(SCRIPT_PATH), "2", sys.executable, "-c",
             "import os,signal;os.kill(os.getpid(),signal.SIGKILL)"],
            capture_output=True, timeout=5,
        )
        self.assertEqual(result.returncode, 137)
        for child_rc, expected in ((0, 0), (7, 7), (-signal.SIGHUP, 129)):
            with self.subTest(child_rc=child_rc):
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    rc, _ = self.fake_run(Process([child_rc]))
                self.assertEqual(rc, expected)
                self.assertNotIn("::error::", stderr.getvalue())
        with mock.patch.dict(os.environ, {"AGENTDESK_CI_TIMEOUT_REPORT": "1"}), \
             contextlib.redirect_stderr(stderr := io.StringIO()):
            self.fake_run(Process([0]))
        self.assertIn("::notice::ci-timeout:", stderr.getvalue())

    # F3
    def test_missing_diagnostic_dumper_does_not_change_timeout_rc(self):
        proc, clock = Process(default=None), Clock()

        def popen(command, _enabled):
            if command == ["cargo", "test"]:
                return proc
            raise FileNotFoundError(command[0])

        def send(*_args, force=False, **_kwargs):
            if force:
                proc.returncode = proc.default = -signal.SIGKILL

        with mock.patch.object(ci_timeout, "_popen", side_effect=popen), \
             mock.patch.object(ci_timeout, "_diagnostic_commands", return_value=[["missing"]]), \
             mock.patch.object(ci_timeout, "_send_process_signal", side_effect=send), \
             mock.patch.object(ci_timeout, "_mask_forwarded_signals", return_value=True), \
             mock.patch.object(ci_timeout.signal, "sigpending", return_value=set()), \
             mock.patch.object(ci_timeout, "_monotonic", side_effect=clock.monotonic), \
             mock.patch.object(ci_timeout, "_sleep", side_effect=clock.sleep), \
             contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(ci_timeout.run_command(1, ["cargo", "test"]), 124)

    # F5 + F6
    def test_sigterm_and_send_exceptions_preserve_origin_rc(self):
        errors = (ProcessLookupError(), PermissionError(), OSError(), AttributeError())
        for error in errors:
            for origin in ("timeout", "signal"):
                with self.subTest(error=type(error).__name__, origin=origin):
                    seen = [0]

                    def pending():
                        seen[0] += 1
                        if origin == "timeout" or seen[0] == 1:
                            return set()
                        return {signal.SIGTERM} if seen[0] == 2 else {signal.SIGINT}

                    with mock.patch.object(ci_timeout.os, "killpg", side_effect=error), \
                         contextlib.redirect_stderr(io.StringIO()):
                        rc, _ = self.fake_run(Process(default=None), pending=pending)
                    self.assertEqual(rc, 124 if origin == "timeout" else 143)

    # F7
    def test_unreaped_child_stops_after_kill_wait_and_warns(self):
        stderr = io.StringIO()
        with mock.patch.object(ci_timeout.os, "killpg"), contextlib.redirect_stderr(stderr):
            rc, clock = self.fake_run(Process(default=None))
        self.assertEqual((rc, stderr.getvalue().count("unreaped after KILL_WAIT")), (124, 1))
        self.assertLessEqual(clock.now, 16.1)

    # F8(a,d,e)
    def test_diagnostic_path_has_only_bounded_poll_apis(self):
        source, tree = SCRIPT_PATH.read_text(), ast.parse(SCRIPT_PATH.read_text())
        with self.subTest("F8-a vocabulary"):
            for token in ("subprocess.run(", ".wait()", ".communicate()"):
                self.assertNotIn(token, source)
        self.assertIn("# R1: bounded poll only", source)
        forbidden, aliases, violations = {"run", "call", "check_call", "check_output"}, set(), []
        for node in ast.walk(tree):
            if isinstance(node, ast.Assign) and isinstance(node.value, ast.Attribute):
                value = node.value
                if isinstance(value.value, ast.Name) and value.value.id == "subprocess" and value.attr in forbidden:
                    aliases.update(t.id for t in node.targets if isinstance(t, ast.Name))
            if not isinstance(node, ast.Call):
                continue
            func = node.func
            if isinstance(func, ast.Attribute) and isinstance(func.value, ast.Name) \
                    and func.value.id == "subprocess" and func.attr in forbidden:
                violations.append(node.lineno)
            if isinstance(func, ast.Name) and func.id in aliases:
                violations.append(node.lineno)
            if isinstance(func, ast.Attribute) and func.attr in {"wait", "communicate"} \
                    and not any(k.arg == "timeout" for k in node.keywords):
                violations.append(node.lineno)
        with self.subTest("F8-d AST"):
            self.assertEqual((aliases, violations), (set(), []))

    # F8(b,c)
    def test_two_hanging_dumpers_share_one_diagnostic_cap(self):
        primary, dumpers, checkpoints = Process(default=None), [], []
        real_popen, real_checkpoint = subprocess.Popen, ci_timeout._checkpoint

        def popen(command, **options):
            if command == ["primary"]:
                return primary
            dumpers.append(real_popen(command, **options))
            return dumpers[-1]

        def send(*_args, force=False, **_kwargs):
            if force:
                primary.returncode = primary.default = -signal.SIGKILL

        def checkpoint(state, enabled, name):
            checkpoints.append(name)
            return real_checkpoint(state, enabled, name)

        sleeper = [sys.executable, "-c", "import time;time.sleep(600)"]
        started = time.monotonic()
        try:
            with mock.patch.object(ci_timeout, "_mask_forwarded_signals", return_value=False), \
                 mock.patch.object(ci_timeout.subprocess, "Popen", side_effect=popen), \
                 mock.patch.object(ci_timeout, "_diagnostic_commands", return_value=[sleeper] * 2), \
                 mock.patch.object(ci_timeout, "_send_process_signal", side_effect=send), \
                 mock.patch.object(ci_timeout, "DIAGNOSTIC_CAP_SECONDS", 0.05), \
                 mock.patch.object(ci_timeout, "TERMINATION_GRACE_SECONDS", 0.01), \
                 mock.patch.object(ci_timeout, "KILL_WAIT_SECONDS", 0.01), \
                 mock.patch.object(ci_timeout, "_checkpoint", side_effect=checkpoint), \
                 contextlib.redirect_stderr(io.StringIO()):
                rc = ci_timeout.run_command(0.01, ["primary"])
        finally:
            for dumper in dumpers:
                try:
                    dumper.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    dumper.kill(); dumper.wait(timeout=1)
        self.assertEqual((rc, len(dumpers)), (124, 2))
        self.assertIn("diagnostic_poll", checkpoints)
        self.assertLess(time.monotonic() - started, 0.5)

    # F9
    def test_post_cutoff_signals_never_use_any_send_path(self):
        for has_killpg in (True, False):
            for reaped in (True, False):
                proc, state = Process(), ci_timeout._RunState(reaped=reaped, cutoff=True)
                proc.returncode = 0 if reaped else None
                fake_os = types.SimpleNamespace(environ={})
                if has_killpg:
                    fake_os.killpg = mock.Mock()
                with mock.patch.object(ci_timeout, "os", fake_os):
                    ci_timeout._send_process_signal(proc, state, signal.SIGTERM)
                    ci_timeout._send_process_signal(proc, state, signal.SIGTERM, force=True)
                if has_killpg:
                    fake_os.killpg.assert_not_called()
                proc.terminate.assert_not_called(); proc.kill.assert_not_called()
                with mock.patch.object(ci_timeout.signal, "sigpending") as pending:
                    self.assertFalse(ci_timeout._checkpoint(state, True, "post_cutoff"))
                pending.assert_not_called(); self.assertIsNone(state.first_signum)
                expected = 0 if reaped else 124
                self.assertEqual(
                    ci_timeout._select_return_code(
                        state, timed_out=not reaped, child_returncode=proc.returncode
                    ),
                    expected,
                )
        proc, state = Process(), ci_timeout._RunState(reaped=False)
        proc.returncode = 0
        with mock.patch.object(ci_timeout.os, "killpg") as killpg:
            ci_timeout._send_process_signal(proc, state, signal.SIGTERM)
        killpg.assert_not_called()

    # F10-①
    @unittest.skipUnless(hasattr(signal, "pthread_sigmask"), "POSIX required")
    def test_popen_seam_signal_has_required_event_order(self):
        events, proc = [], Process(default=None)
        original_mask, original_pending, old_mask = signal.pthread_sigmask, signal.sigpending, [None]

        def mask(how, signals):
            events.append("mask_blocked"); old_mask[0] = original_mask(how, signals); return old_mask[0]

        def pending():
            result = original_pending()
            events.append("checkpoint_observed" if signal.SIGTERM in result else "checkpoint1_clear")
            return result

        def popen(*_args):
            events.append("popen_entered"); os.kill(os.getpid(), signal.SIGTERM)
            events.extend(("signal_pending", "popen_returned")); return proc

        def send(*_args, **_kwargs):
            events.append("term_sent"); proc.returncode = proc.default = -signal.SIGTERM

        try:
            with mock.patch.object(ci_timeout.signal, "pthread_sigmask", side_effect=mask), \
                 mock.patch.object(ci_timeout.signal, "sigpending", side_effect=pending), \
                 mock.patch.object(ci_timeout, "_popen", side_effect=popen), \
                 mock.patch.object(ci_timeout, "_diagnose_capped"), \
                 mock.patch.object(ci_timeout, "_send_process_signal", side_effect=send):
                rc = ci_timeout.run_command(30, ["cargo", "test"])
            signal.sigwait({signal.SIGTERM})
        finally:
            if old_mask[0] is not None:
                original_mask(signal.SIG_SETMASK, old_mask[0])
        required = ["mask_blocked", "checkpoint1_clear", "popen_entered", "signal_pending",
                    "popen_returned", "checkpoint_observed", "term_sent"]
        self.assertEqual([events.index(e) for e in required], sorted(events.index(e) for e in required))
        self.assertEqual(rc, 143)

    # F10-②..⑥
    def test_signal_boundaries_polling_and_tie_break(self):
        state = ci_timeout._RunState()
        with mock.patch.object(ci_timeout.signal, "sigpending", return_value={signal.SIGTERM, signal.SIGINT}):
            self.assertTrue(ci_timeout._checkpoint(state, True, "final"))
        state.cutoff = True
        self.assertEqual(ci_timeout._select_return_code(state, timed_out=False, child_returncode=0), 143)
        with mock.patch.object(ci_timeout.signal, "sigpending") as pending:
            self.assertFalse(ci_timeout._checkpoint(ci_timeout._RunState(cutoff=True), True, "after"))
        pending.assert_not_called()

        clock, proc, seen, points = Clock(), Process(default=None), [None, None], []
        real_checkpoint = ci_timeout._checkpoint

        def popen(*_args):
            clock.now += 100; seen[0] = clock.now + 0.15; seen[1] = clock.now; return proc

        def pending_signal():
            return {signal.SIGTERM} if seen[0] is not None and clock.now >= seen[0] else set()

        def send(*_args, **_kwargs):
            proc.returncode = proc.default = -signal.SIGTERM

        def checkpoint(s, enabled, name):
            points.append((name, clock.now)); return real_checkpoint(s, enabled, name)

        with mock.patch.object(ci_timeout, "_mask_forwarded_signals", return_value=True), \
             mock.patch.object(ci_timeout, "_popen", side_effect=popen), \
             mock.patch.object(ci_timeout.signal, "sigpending", side_effect=pending_signal), \
             mock.patch.object(ci_timeout, "_diagnose_capped"), \
             mock.patch.object(ci_timeout, "_send_process_signal", side_effect=send), \
             mock.patch.object(ci_timeout, "_monotonic", side_effect=clock.monotonic), \
             mock.patch.object(ci_timeout, "_sleep", side_effect=clock.sleep), \
             mock.patch.object(ci_timeout, "_checkpoint", side_effect=checkpoint):
            rc = ci_timeout.run_command(30, ["cargo", "test"])
        names = {name for name, _ in points}
        required = {"checkpoint1", "checkpoint2", "poll", "term_boundary",
                    "grace_boundary", "kill_boundary", "final"}
        observed = next(at for name, at in points if name == "poll" and at >= seen[0])
        self.assertEqual(rc, 143)
        self.assertLessEqual(observed - seen[0], 0.1 + 1e-9)
        self.assertLessEqual(clock.now - seen[1], 55.1)
        self.assertTrue(required.issubset(names))
        self.assertLessEqual(max(clock.sleeps), ci_timeout.POLL_INTERVAL_SECONDS)

    # F11
    def test_spawn_and_input_errors_preserve_legacy_rows(self):
        with mock.patch.object(ci_timeout.signal, "pthread_sigmask") as mask_call, \
             mock.patch.object(ci_timeout.signal, "sigpending", return_value=set()), \
             mock.patch.object(ci_timeout.signal, "signal") as handlers, \
             mock.patch.object(ci_timeout, "_popen", side_effect=FileNotFoundError()):
            with self.assertRaises(FileNotFoundError):
                ci_timeout.run_command(1, ["missing"])
        handlers.assert_not_called(); self.assertEqual(mask_call.call_count, 1)
        for argv in (["ci-timeout.py"], ["ci-timeout.py", "bad", "true"]):
            with mock.patch.object(sys, "argv", argv), mock.patch.object(ci_timeout, "_popen") as popen:
                self.assertEqual(ci_timeout.main(), 2); popen.assert_not_called()
        result = subprocess.run([sys.executable, str(SCRIPT_PATH), "1", "/not/a/program"],
                                capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 1)

    # F12
    @unittest.skipUnless(hasattr(signal, "pthread_sigmask"), "POSIX required")
    def test_child_mask_is_unblocked_and_wrapper_is_single_threaded(self):
        code = ("import signal;m=signal.pthread_sigmask(signal.SIG_BLOCK,[]);"
                "print(int(signal.SIGTERM in m),int(signal.SIGINT in m))")
        result = subprocess.run([sys.executable, str(SCRIPT_PATH), "2", sys.executable, "-c", code],
                                capture_output=True, text=True, timeout=5)
        self.assertEqual((result.returncode, result.stdout.strip()), (0, "0 0"))
        threads, proc = [], Process([0])

        def popen(_command, **options):
            threads.append(threading.active_count())
            self.assertIs(options.get("preexec_fn"), ci_timeout._unblock_forwarded_signals)
            return proc

        with mock.patch.object(ci_timeout, "_mask_forwarded_signals", return_value=True), \
             mock.patch.object(ci_timeout.signal, "sigpending", return_value=set()), \
             mock.patch.object(ci_timeout.subprocess, "Popen", side_effect=popen):
            self.assertEqual(ci_timeout.run_command(1, ["true"]), 0)
        self.assertEqual(threads, [1])
        tree, imports, constructors = ast.parse(SCRIPT_PATH.read_text()), [], []
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                imports += [a.name for a in node.names if a.name in {"threading", "concurrent.futures"}]
            elif isinstance(node, ast.ImportFrom) and node.module in {"threading", "concurrent.futures"}:
                imports.append(node.module)
            elif isinstance(node, ast.Call):
                name = node.func.attr if isinstance(node.func, ast.Attribute) else \
                    node.func.id if isinstance(node.func, ast.Name) else ""
                if name in {"Thread", "ThreadPoolExecutor"}:
                    constructors.append(node.lineno)
        self.assertEqual((imports, constructors), ([], []))

    # F13
    def test_checkpoint_one_pending_signal_never_spawns_or_cleans_up(self):
        for pending, expected in (({signal.SIGINT}, 130), ({signal.SIGTERM}, 143),
                                  ({signal.SIGTERM, signal.SIGINT}, 143)):
            with self.subTest(pending=pending), \
                 mock.patch.object(ci_timeout, "_mask_forwarded_signals", return_value=True), \
                 mock.patch.object(ci_timeout.signal, "sigpending", return_value=pending), \
                 mock.patch.object(ci_timeout.signal, "signal") as handlers, \
                 mock.patch.object(ci_timeout, "_popen") as popen, \
                 mock.patch.object(ci_timeout, "_diagnose_capped") as diagnose, \
                 mock.patch.object(ci_timeout, "_cleanup") as cleanup, \
                 mock.patch.object(ci_timeout, "_send_process_signal") as send:
                started = time.monotonic(); rc = ci_timeout.run_command(30, ["cargo", "test"])
            self.assertEqual(rc, expected); self.assertLess(time.monotonic() - started, 55.1)
            for operation in (popen, diagnose, cleanup, send, handlers):
                operation.assert_not_called()


if __name__ == "__main__":
    unittest.main()
