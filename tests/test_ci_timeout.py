import importlib.util
import signal
import subprocess
import types
import unittest
from pathlib import Path
from unittest import mock


SCRIPT_PATH = Path(__file__).parents[1] / "scripts" / "ci-timeout.py"
SPEC = importlib.util.spec_from_file_location("ci_timeout", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
ci_timeout = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ci_timeout)


class FakeProcess:
    def __init__(self, timeout_count: int) -> None:
        self.pid = 4413
        self.terminate = mock.Mock()
        self.kill = mock.Mock()
        self.timeout_count = timeout_count
        self.wait_calls = 0

    def wait(self, timeout=None):
        self.wait_calls += 1
        if self.wait_calls <= self.timeout_count:
            raise subprocess.TimeoutExpired("cargo test", timeout)
        return 0


class CiTimeoutTests(unittest.TestCase):
    def test_killpg_fallback_terminates_then_kills_after_grace_period(self):
        proc = FakeProcess(timeout_count=2)

        with (
            mock.patch.object(ci_timeout, "os", types.SimpleNamespace()),
            mock.patch.object(ci_timeout.subprocess, "Popen", return_value=proc),
            mock.patch.object(ci_timeout.signal, "signal", return_value=signal.SIG_DFL),
        ):
            return_code = ci_timeout.run_command(30, ["cargo", "test"])

        proc.terminate.assert_called_once_with()
        proc.kill.assert_called_once_with()
        self.assertEqual(proc.wait_calls, 3)
        self.assertEqual(return_code, 124)

    def test_sigterm_and_sigint_are_forwarded_with_shell_return_code(self):
        for signum in (signal.SIGTERM, signal.SIGINT):
            with self.subTest(signum=signum):
                installed_handlers = {}

                def install_handler(installed_signum, handler):
                    previous = installed_handlers.get(installed_signum, signal.SIG_DFL)
                    installed_handlers[installed_signum] = handler
                    return previous

                wait_results = [
                    lambda timeout: installed_handlers[signum](signum, None),
                    -signum,
                ]

                def next_wait(timeout=None):
                    result = wait_results.pop(0)
                    return result(timeout) if callable(result) else result

                proc = mock.Mock(pid=4413)
                proc.wait = mock.Mock(side_effect=next_wait)

                with (
                    mock.patch.object(ci_timeout.signal, "signal", side_effect=install_handler),
                    mock.patch.object(ci_timeout.subprocess, "Popen", return_value=proc),
                    mock.patch.object(ci_timeout, "_send_process_signal") as send_signal,
                ):
                    return_code = ci_timeout.run_command(30, ["cargo", "test"])

                send_signal.assert_called_once_with(proc, signum)
                self.assertEqual(return_code, 128 + signum)


if __name__ == "__main__":
    unittest.main()
