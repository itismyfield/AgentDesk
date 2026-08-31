from __future__ import annotations
from contextlib import nullcontext, redirect_stderr
import ctypes
import importlib.util
import inspect
import io
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock
import uuid
ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "scripts" / "build_token_win32.py"
WORKFLOW = ROOT / ".github" / "workflows" / "ci-pr.yml"
SPEC = importlib.util.spec_from_file_location("build_token_win32_test", HELPER)
assert SPEC and SPEC.loader
module = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(module)
DRIVER = r"""
import importlib.util, os, sys
spec = importlib.util.spec_from_file_location("build_token_win32_driver", sys.argv[1])
module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
nonce, mode, args = sys.argv[2], sys.argv[3], sys.argv[4:]
try:
    if mode == "run":
        raise SystemExit(module._supervise_windows_for_test(args, dict(os.environ), nonce))
    api = module._Win32()
    if mode == "abandon":
        handle = api.open_mutex(nonce)
        assert api.wait(handle) in (module.WAIT_OBJECT_0, module.WAIT_ABANDONED_0)
        open(args[0], "w").close(); os._exit(0)
    if mode == "owner-mismatch":
        api.EqualSid = lambda *_args: 0
        api.open_mutex(nonce)
        raise SystemExit(70)
except module.BuildTokenWindowsError as error:
    print(error, file=sys.stderr); raise SystemExit(73)
"""
def driver(nonce: str, mode: str, *command: str) -> list[str]:
    return [sys.executable, "-c", DRIVER, str(HELPER), nonce, mode, *command]
def wait_path(path: Path, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {path}")
class _TraceApi:
    def assign_job(self, _job: object, _child: object, trace: list[str]) -> None:
        trace.append("assign-job")
    def resume_primary(self, _child: object, trace: list[str]) -> None:
        trace.append("resume-primary")
    def close_primary(self, _child: object, trace: list[str]) -> None:
        trace.append("close-thread")
class _CoreApi:
    def __init__(self, fail: str | None = None) -> None:
        self.fail, self.closed, self.released = fail, [], 0
        self.waits = [module.WAIT_OBJECT_0, module.WAIT_OBJECT_0]
    def close_raw(self, value: int, _kind: str) -> None:
        self.closed.append(value)
    def _handle(self, value: int, kind: str) -> object:
        return module._OwnedHandle(self, value, kind)
    def open_mutex(self, _nonce: str | None) -> object:
        return self._handle(1, "mutex")
    def wait(self, handle: object, _milliseconds: int = 50) -> int:
        if self.fail == "wait" and handle.kind == "process":
            raise module.BuildTokenWindowsError("wait")
        return self.waits.pop(0) if self.waits else module.WAIT_OBJECT_0
    def release_mutex(self, _handle: object) -> None:
        self.released += 1
    def new_job(self) -> object:
        return self._handle(2, "Job")
    def create_suspended(self, _exe: str, _cmd: object, _env: object, trace: list[str]) -> object:
        if self.fail == "create":
            raise module.BuildTokenWindowsError("create")
        trace.append("create-suspended")
        return module._Child(self._handle(3, "process"), self._handle(4, "thread"), 99)
    def assign_job(self, _job: object, _child: object, trace: list[str]) -> None:
        if self.fail == "assign":
            raise module.BuildTokenWindowsError("assign")
        trace.append("assign-job")
    def resume_primary(self, _child: object, trace: list[str]) -> None:
        if self.fail == "resume":
            raise module.BuildTokenWindowsError("resume")
        trace.append("resume-primary")
    def close_primary(self, child: object, trace: list[str]) -> None:
        child.thread.close(); trace.append("close-thread")
    def process_exit_code(self, _child: object) -> int:
        return 37
    def active_job_processes(self, _job: object) -> int:
        return 0
    def terminate_job(self, _job: object, _code: int) -> None:
        pass
    def terminate_process(self, _child: object, _code: int) -> None:
        pass
    def generate_break(self, _pid: int) -> bool:
        return True
class PortableWin32BuildTokenContractTests(unittest.TestCase):
    def test_public_api_fails_closed_off_windows_and_has_no_authority_override(self):
        self.assertEqual(list(inspect.signature(module.supervise_windows).parameters), ["command", "child_env"])
        if sys.platform != "win32":
            with self.assertRaises(module.BuildTokenWindowsError):
                module.supervise_windows(["dummy"], {})
        self.assertNotIn("Test.", module._mutex_name("S-1-5-21"))
        self.assertIn(".Test.", module._mutex_name("S-1-5-21", "nonce"))
    def test_sid_scoped_name_and_owner_only_protected_dacl_are_canonical(self):
        sid = "S-1-5-21-1"
        self.assertEqual(module._mutex_name(sid), rf"Global\AgentDesk.BuildToken.{sid}")
        self.assertEqual(module._owner_only_sddl(sid), f"O:{sid}D:P(A;;0x001F0001;;;{sid})")
    def test_security_verification_rejects_owner_dacl_ace_and_inheritance_mutants(self):
        good, bad = [True, True, 1, module.ACCESS_ALLOWED_ACE_TYPE, 0, module.MUTEX_ALL_ACCESS, True], [False, False, 2, 1, 1, 0, False]
        def write(pointer: object, kind: object, value: int) -> None: ctypes.cast(pointer, ctypes.POINTER(kind)).contents.value = value
        for axis in range(-1, len(good)):
            values = good.copy(); values[axis] = bad[axis] if axis >= 0 else values[axis]
            owner_ok, protected, count, ace_type, ace_flags, mask, sid_ok = values
            api = object.__new__(module._Win32); ace = module._ACCESS_ALLOWED_ACE(); ace.Header.AceType, ace.Header.AceFlags, ace.Mask = ace_type, ace_flags, mask
            api._check = lambda result, _action: self.assertTrue(result); api.LocalFree = lambda _value: None
            api.GetSecurityInfo = lambda _h, _o, _i, owner, _g, dacl, _s, descriptor: (write(owner, ctypes.c_void_p, 1), write(dacl, ctypes.c_void_p, 2), write(descriptor, ctypes.c_void_p, 3), 0)[-1]
            api.GetSecurityDescriptorControl = lambda _d, control, _r: (write(control, module.wintypes.WORD, module.SE_DACL_PROTECTED if protected else 0), 1)[-1]
            api.GetAclInformation = lambda _d, info, _size, _class: (setattr(ctypes.cast(info, ctypes.POINTER(module._ACL_SIZE_INFORMATION)).contents, "AceCount", count), 1)[-1]
            api.GetAce = lambda _d, _index, pointer: (write(pointer, ctypes.c_void_p, ctypes.addressof(ace)), 1)[-1]
            answers = iter((sid_ok, owner_ok)); api.EqualSid = lambda _one, _two: next(answers)
            context = nullcontext() if axis < 0 else self.assertRaises(module.BuildTokenWindowsError)
            with context: api._verify_mutex(9, 1)
    def test_create_process_inputs_are_quoted_unicode_and_noninheriting(self):
        with mock.patch.object(module.shutil, "which", return_value="C:/Program Files/Cargo/cargo.exe"):
            executable, command, environment = module._prepare_process_inputs(
                ["cargo", "arg with space"], {"z": "2", "A": "1", "Path": "bin"}
            )
        self.assertTrue(os.path.isabs(executable))
        self.assertIn('"arg with space"', command.value)
        block = "".join(environment)
        self.assertEqual(block.split("\0")[:3], ["A=1", "Path=bin", "z=2"])
        self.assertTrue(block.endswith("\0\0"))
        self.assertFalse(module.PROCESS_INHERITS_HANDLES)
        self.assertEqual(module.PROCESS_CREATION_FLAGS, 0x604)
        with self.assertRaises(module.BuildTokenWindowsError):
            module._prepare_process_inputs(["cargo"], {"PATH": "a", "Path": "b"})
    def test_suspended_child_is_assigned_before_primary_thread_resumes(self):
        trace = ["create-suspended"]
        module._assign_and_resume(_TraceApi(), object(), object(), trace)
        self.assertEqual(trace, ["create-suspended", "assign-job", "resume-primary", "close-thread"])
        cancelled = ["create-suspended"]
        self.assertFalse(module._assign_and_resume(_TraceApi(), object(), object(), cancelled, lambda: True))
        self.assertEqual(cancelled, ["create-suspended", "assign-job"])
    def test_create_assign_resume_wait_failures_cleanup_every_handle_once(self):
        prepared = ("dummy.exe", object(), object())
        for failure in ("create", "assign", "resume", "wait"):
            api = _CoreApi(failure)
            with mock.patch.object(module, "_prepare_process_inputs", return_value=prepared):
                with self.assertRaises(module.BuildTokenWindowsError):
                    module._supervise_windows(["dummy"], {}, api, trace=[])
            expected = [2, 1] if failure == "create" else [4, 3, 2, 1]
            self.assertCountEqual(api.closed, expected, failure)
            self.assertEqual(len(api.closed), len(set(api.closed)), failure)
            self.assertEqual(api.released, 1, failure)
        child = module._Child(mock.Mock(value=3), mock.Mock(value=0), 9)
        for terminated, waited, fails in (([1], [module.WAIT_OBJECT_0], False), ([0, 1], [module.WAIT_TIMEOUT, module.WAIT_OBJECT_0], True), ([1, 1], [module.WAIT_FAILED, module.WAIT_OBJECT_0], True)):
            api = mock.Mock(); api.TerminateProcess.side_effect = terminated; api.WaitForSingleObject.side_effect = waited; api._error.return_value = module.BuildTokenWindowsError("terminate")
            context = self.assertRaises(module.BuildTokenWindowsError) if fails else nullcontext()
            with context: module._Win32._terminate_unassigned(api, child)
            self.assertEqual((api.TerminateProcess.call_count, api.WaitForSingleObject.call_count), (len(terminated), len(waited)))
    def test_wait_status_and_signal_exit_semantics_are_exact(self):
        api, relay = _CoreApi(), module._SignalRelay()
        child = module._Child(api._handle(3, "process"), api._handle(0, "thread"), 99)
        self.assertEqual(module._wait_foreground(api, api._handle(2, "Job"), child, relay), 37)
        api.waits = [module.WAIT_OBJECT_0]; relay(module.signal.SIGTERM, None)
        self.assertEqual(module._wait_foreground(api, api._handle(2, "Job"), child, relay), 128 + signal.SIGTERM)
        self.assertEqual({module.WAIT_OBJECT_0, module.WAIT_ABANDONED_0, module.WAIT_TIMEOUT}, {0, 0x80, 0x102})
        for status, releases in ((module.WAIT_TIMEOUT, 0), (module.WAIT_OBJECT_0, 1)):
            cancelled, relay = _CoreApi(), module._SignalRelay()
            def cancel(_handle: object, _milliseconds: int = 50, status: int = status) -> int:
                relay(signal.SIGTERM, None); return status
            cancelled.wait = cancel
            with mock.patch.object(module, "_prepare_process_inputs", return_value=("x", object(), object())):
                self.assertEqual(module._supervise_windows(["x"], {}, cancelled, relay=relay, trace=[]), 128 + signal.SIGTERM)
            self.assertEqual(cancelled.closed, [1]); self.assertEqual(cancelled.released, releases)
    def test_helper_only_change_selects_windows_required_lane(self):
        workflow = WORKFLOW.read_text()
        self.assertIn("win32_build_token: ${{ steps.filter.outputs.win32_build_token }}", workflow)
        self.assertIn("scripts/build_token_win32.py", workflow)
        self.assertIn("tests/test_build_token_win32_5663.py", workflow)
        self.assertIn("needs.changes.outputs.win32_build_token == 'true'", workflow)
        self.assertIn("win32_build_token_required_context:", workflow)
        self.assertIn("FILTER_NAME: win32_build_token", workflow)
        self.assertIn("FILTER_OUTPUT: '${{ needs.changes.outputs.win32_build_token }}'", workflow)
        self.assertIn('git cat-file -e "HEAD:scripts/build_token_win32.py"', workflow)
        self.assertIn('git cat-file -e "HEAD:tests/test_build_token_win32_5663.py"', workflow)
        self.assertIn("python -m unittest -v tests.test_build_token_win32_5663", workflow)
@unittest.skipUnless(sys.platform == "win32", "native Win32 contract")
class NativeWin32BuildTokenContractTests(unittest.TestCase):
    def nonce(self) -> str:
        return f"{os.getpid()}.{uuid.uuid4().hex}"
    def test_native_commands_contend_on_one_temporary_sid_mutex(self):
        with tempfile.TemporaryDirectory() as tmp:
            first_mark, second_mark, nonce = Path(tmp) / "first", Path(tmp) / "second", self.nonce()
            code = "from pathlib import Path; import sys,time; Path(sys.argv[1]).touch(); time.sleep(float(sys.argv[2]))"
            first = subprocess.Popen(driver(nonce, "run", sys.executable, "-c", code, str(first_mark), ".5"))
            self.addCleanup(first.kill); wait_path(first_mark)
            second = subprocess.Popen(driver(nonce, "run", sys.executable, "-c", code, str(second_mark), "0"))
            time.sleep(.12); self.assertFalse(second_mark.exists())
            self.assertEqual(first.wait(timeout=5), 0); self.assertEqual(second.wait(timeout=5), 0)
    def test_native_keeper_handle_exposes_real_wait_abandoned(self):
        with tempfile.TemporaryDirectory() as tmp:
            nonce, ready, api = self.nonce(), Path(tmp) / "owned", module._Win32()
            keeper = api.open_mutex(nonce)
            try:
                owner = subprocess.run(driver(nonce, "abandon", str(ready)), check=False)
                self.assertEqual(owner.returncode, 0); self.assertTrue(ready.exists())
                recovered = subprocess.run(driver(nonce, "run", sys.executable, "-c", "pass"), text=True, capture_output=True, check=False)
                self.assertEqual(recovered.returncode, 0); self.assertIn("recovered abandoned", recovered.stderr)
            finally:
                keeper.close()
    def test_native_owner_mismatch_is_rejected_before_wait_or_spawn(self):
        result = subprocess.run(driver(self.nonce(), "owner-mismatch"), text=True, capture_output=True, check=False)
        self.assertEqual(result.returncode, 73); self.assertIn("owner", result.stderr)
    def test_native_mutex_job_process_and_thread_handles_are_noninheritable(self):
        api, nonce = module._Win32(), self.nonce()
        mutex, job = api.open_mutex(nonce), api.new_job()
        with mock.patch.object(module.shutil, "which", return_value=sys.executable):
            exe, command, env = module._prepare_process_inputs([sys.executable, "-c", "pass"], dict(os.environ))
        child = api.create_suspended(exe, command, env)
        try:
            for handle, kind in ((mutex.value, "mutex"), (job.value, "Job"), (child.process.value, "process"), (child.thread.value, "thread")):
                flags = module.wintypes.DWORD()
                api._check(api.GetHandleInformation(handle, ctypes.byref(flags)), f"inspect {kind}")
                self.assertFalse(flags.value & module.HANDLE_FLAG_INHERIT)
        finally:
            api.terminate_process(child, 1); api.wait(child.process, 2000); child.close(); job.close(); mutex.close()
    def test_native_suspended_child_cannot_execute_before_job_assignment_and_resume(self):
        with tempfile.TemporaryDirectory() as tmp:
            marker, api = Path(tmp) / "ran", module._Win32()
            job = api.new_job()
            exe, command, env = module._prepare_process_inputs([sys.executable, "-c", "from pathlib import Path; import sys; Path(sys.argv[1]).touch()", str(marker)], dict(os.environ))
            child = api.create_suspended(exe, command, env)
            try:
                time.sleep(.1); self.assertFalse(marker.exists())
                module._assign_and_resume(api, job, child); wait_path(marker)
            finally:
                module._terminate_and_drain(api, job, child, 1); child.close(); job.close()
    def test_native_job_close_kills_background_descendant_before_lease_release(self):
        with tempfile.TemporaryDirectory() as tmp:
            ready, trigger, survived, nonce = Path(tmp) / "ready", Path(tmp) / "trigger", Path(tmp) / "survived", self.nonce()
            child = "from pathlib import Path; import sys,time; Path(sys.argv[1]).touch();\nwhile not Path(sys.argv[2]).exists(): time.sleep(.005)\nPath(sys.argv[3]).touch()"
            first_code = "import subprocess,sys,time; from pathlib import Path; subprocess.Popen([sys.executable,'-c',sys.argv[1],*sys.argv[2:]]);\nwhile not Path(sys.argv[2]).exists(): time.sleep(.005)"
            first = subprocess.Popen(driver(nonce, "run", sys.executable, "-c", first_code, child, str(ready), str(trigger), str(survived)))
            self.addCleanup(first.kill); wait_path(ready)
            second_code = "from pathlib import Path; import sys,time; Path(sys.argv[1]).touch(); time.sleep(.2)"
            second = subprocess.Popen(driver(nonce, "run", sys.executable, "-c", second_code, str(trigger))); self.addCleanup(second.kill)
            self.assertEqual(first.wait(timeout=5), 0); self.assertEqual(second.wait(timeout=5), 0); self.assertFalse(survived.exists())
    def test_native_ctrl_break_forwards_escalates_and_reaps(self):
        with tempfile.TemporaryDirectory() as tmp:
            marker, handled, nonce = Path(tmp) / "ready", Path(tmp) / "handled", self.nonce()
            code = "from pathlib import Path; import signal,sys,time; signal.signal(signal.SIGBREAK,lambda *_: Path(sys.argv[2]).touch()); Path(sys.argv[1]).touch()\nwhile True: time.sleep(.1)"
            process = subprocess.Popen(driver(nonce, "run", sys.executable, "-c", code, str(marker), str(handled)), creationflags=subprocess.CREATE_NEW_PROCESS_GROUP)
            self.addCleanup(process.kill); wait_path(marker)
            process.send_signal(signal.CTRL_BREAK_EVENT)
            self.assertEqual(process.wait(timeout=7), 128 + signal.SIGBREAK); self.assertTrue(handled.exists())
    def test_native_foreground_exit_code_is_preserved_exactly(self):
        for exit_code in (37, 259):
            result = subprocess.run(driver(self.nonce(), "run", sys.executable, "-c", f"raise SystemExit({exit_code})"), check=False)
            self.assertEqual(result.returncode, exit_code)
if __name__ == "__main__":
    unittest.main()
