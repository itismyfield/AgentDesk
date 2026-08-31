#!/usr/bin/env python3
"""Native Win32 supervisor for AgentDesk's SID-scoped Cargo build token."""

from __future__ import annotations

from contextlib import contextmanager, nullcontext
import ctypes
from ctypes import wintypes
import os
import re
import shutil
import signal
import subprocess
import sys
import time
from typing import Callable, Iterator, Mapping


WAIT_OBJECT_0 = 0
WAIT_ABANDONED_0 = 0x80
WAIT_TIMEOUT = 0x102
WAIT_FAILED = 0xFFFFFFFF
STILL_ACTIVE = 259
MUTEX_ALL_ACCESS = 0x001F0001
TOKEN_QUERY = 0x0008
OWNER_SECURITY_INFORMATION = 0x1
DACL_SECURITY_INFORMATION = 0x4
SE_DACL_PROTECTED = 0x1000
HANDLE_FLAG_INHERIT = 0x1
SE_KERNEL_OBJECT = 6
ACL_SIZE_INFORMATION_CLASS = 2
ACCESS_ALLOWED_ACE_TYPE = 0
CREATE_SUSPENDED = 0x00000004
CREATE_NEW_PROCESS_GROUP = 0x00000200
CREATE_UNICODE_ENVIRONMENT = 0x00000400
JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000
JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION = 1
JOB_OBJECT_EXTENDED_LIMIT_INFORMATION = 9
CTRL_BREAK_EVENT = 1
SIGNAL_GRACE_SECONDS = 2.0


class BuildTokenWindowsError(RuntimeError):
    """The Win32 authority could not be used without weakening fail-closed rules."""


class _SID_AND_ATTRIBUTES(ctypes.Structure):
    _fields_ = [("Sid", ctypes.c_void_p), ("Attributes", wintypes.DWORD)]


class _TOKEN_USER(ctypes.Structure):
    _fields_ = [("User", _SID_AND_ATTRIBUTES)]


class _SECURITY_ATTRIBUTES(ctypes.Structure):
    _fields_ = [
        ("nLength", wintypes.DWORD),
        ("lpSecurityDescriptor", ctypes.c_void_p),
        ("bInheritHandle", wintypes.BOOL),
    ]


class _ACL_SIZE_INFORMATION(ctypes.Structure):
    _fields_ = [
        ("AceCount", wintypes.DWORD),
        ("AclBytesInUse", wintypes.DWORD),
        ("AclBytesFree", wintypes.DWORD),
    ]


class _ACE_HEADER(ctypes.Structure):
    _fields_ = [
        ("AceType", ctypes.c_ubyte),
        ("AceFlags", ctypes.c_ubyte),
        ("AceSize", wintypes.WORD),
    ]


class _ACCESS_ALLOWED_ACE(ctypes.Structure):
    _fields_ = [("Header", _ACE_HEADER), ("Mask", wintypes.DWORD), ("SidStart", wintypes.DWORD)]


class _IO_COUNTERS(ctypes.Structure):
    _fields_ = [(name, ctypes.c_ulonglong) for name in (
        "ReadOperationCount", "WriteOperationCount", "OtherOperationCount",
        "ReadTransferCount", "WriteTransferCount", "OtherTransferCount",
    )]


class _JOBOBJECT_BASIC_LIMIT_INFORMATION(ctypes.Structure):
    _fields_ = [
        ("PerProcessUserTimeLimit", ctypes.c_longlong),
        ("PerJobUserTimeLimit", ctypes.c_longlong),
        ("LimitFlags", wintypes.DWORD),
        ("MinimumWorkingSetSize", ctypes.c_size_t),
        ("MaximumWorkingSetSize", ctypes.c_size_t),
        ("ActiveProcessLimit", wintypes.DWORD),
        ("Affinity", ctypes.c_size_t),
        ("PriorityClass", wintypes.DWORD),
        ("SchedulingClass", wintypes.DWORD),
    ]


class _JOBOBJECT_EXTENDED_LIMIT_INFORMATION(ctypes.Structure):
    _fields_ = [
        ("BasicLimitInformation", _JOBOBJECT_BASIC_LIMIT_INFORMATION),
        ("IoInfo", _IO_COUNTERS),
        ("ProcessMemoryLimit", ctypes.c_size_t),
        ("JobMemoryLimit", ctypes.c_size_t),
        ("PeakProcessMemoryUsed", ctypes.c_size_t),
        ("PeakJobMemoryUsed", ctypes.c_size_t),
    ]


class _JOBOBJECT_BASIC_ACCOUNTING_INFORMATION(ctypes.Structure):
    _fields_ = [
        ("TotalUserTime", ctypes.c_longlong),
        ("TotalKernelTime", ctypes.c_longlong),
        ("ThisPeriodTotalUserTime", ctypes.c_longlong),
        ("ThisPeriodTotalKernelTime", ctypes.c_longlong),
        ("TotalPageFaultCount", wintypes.DWORD),
        ("TotalProcesses", wintypes.DWORD),
        ("ActiveProcesses", wintypes.DWORD),
        ("TotalTerminatedProcesses", wintypes.DWORD),
    ]


class _STARTUPINFOW(ctypes.Structure):
    _fields_ = [
        ("cb", wintypes.DWORD), ("lpReserved", wintypes.LPWSTR),
        ("lpDesktop", wintypes.LPWSTR), ("lpTitle", wintypes.LPWSTR),
        ("dwX", wintypes.DWORD), ("dwY", wintypes.DWORD),
        ("dwXSize", wintypes.DWORD), ("dwYSize", wintypes.DWORD),
        ("dwXCountChars", wintypes.DWORD), ("dwYCountChars", wintypes.DWORD),
        ("dwFillAttribute", wintypes.DWORD), ("dwFlags", wintypes.DWORD),
        ("wShowWindow", wintypes.WORD), ("cbReserved2", wintypes.WORD),
        ("lpReserved2", ctypes.POINTER(ctypes.c_ubyte)),
        ("hStdInput", wintypes.HANDLE), ("hStdOutput", wintypes.HANDLE),
        ("hStdError", wintypes.HANDLE),
    ]


class _PROCESS_INFORMATION(ctypes.Structure):
    _fields_ = [
        ("hProcess", wintypes.HANDLE), ("hThread", wintypes.HANDLE),
        ("dwProcessId", wintypes.DWORD), ("dwThreadId", wintypes.DWORD),
    ]


class _OwnedHandle:
    def __init__(self, api: object, value: int, kind: str) -> None:
        self.api, self.value, self.kind = api, value, kind

    def close(self) -> None:
        if self.value:
            value, self.value = self.value, 0
            self.api.close_raw(value, self.kind)


class _Child:
    def __init__(self, process: _OwnedHandle, thread: _OwnedHandle, pid: int) -> None:
        self.process, self.thread, self.pid = process, thread, pid

    def close(self) -> None:
        self.thread.close()
        self.process.close()


class _SignalRelay:
    def __init__(self) -> None:
        self.signum: int | None = None
        self.count = 0

    def __call__(self, signum: int, _frame: object) -> None:
        self.signum = self.signum or signum
        self.count += 1


def _owner_only_sddl(sid: str) -> str:
    return f"O:{sid}D:P(A;;0x{MUTEX_ALL_ACCESS:08X};;;{sid})"


def _mutex_name(sid: str, test_nonce: str | None = None) -> str:
    if test_nonce is None:
        return f"Global\\AgentDesk.BuildToken.{sid}"
    if not re.fullmatch(r"[A-Za-z0-9._-]{1,96}", test_nonce):
        raise BuildTokenWindowsError("invalid Win32 test-authority nonce")
    return f"Global\\AgentDesk.BuildToken.Test.{sid}.{test_nonce}"


def _prepare_process_inputs(
    command: list[str], child_env: Mapping[str, str]
) -> tuple[str, ctypes.Array[ctypes.c_wchar], ctypes.Array[ctypes.c_wchar]]:
    if not command:
        raise BuildTokenWindowsError("protected command is required")
    executable = shutil.which(command[0], path=child_env.get("PATH"))
    if executable is None:
        raise FileNotFoundError(command[0])
    executable = os.path.abspath(executable)
    command_line = ctypes.create_unicode_buffer(
        subprocess.list2cmdline([executable, *command[1:]])
    )
    entries = []
    for key, value in child_env.items():
        if not key or "\0" in key or "=" in key or "\0" in value:
            raise BuildTokenWindowsError("invalid Win32 child environment")
        entries.append((key, value))
    entries.sort(key=lambda item: item[0].casefold())
    environment = ctypes.create_unicode_buffer(
        "\0".join(f"{key}={value}" for key, value in entries) + "\0"
    )
    return executable, command_line, environment


def _bind(dll: object, name: str, restype: object, *argtypes: object) -> object:
    function = getattr(dll, name)
    function.restype = restype
    function.argtypes = list(argtypes)
    return function


class _Win32:
    def __init__(self, loader: Callable[..., object] | None = None) -> None:
        loader = loader or getattr(ctypes, "WinDLL", None)
        if loader is None:
            raise BuildTokenWindowsError("native Win32 ctypes loader is unavailable")
        kernel = loader("kernel32", use_last_error=True)
        advapi = loader("advapi32", use_last_error=True)
        H, P, D, B = wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD, wintypes.BOOL
        self.GetCurrentProcess = _bind(kernel, "GetCurrentProcess", H)
        self.CloseHandle = _bind(kernel, "CloseHandle", B, H)
        self.LocalFree = _bind(kernel, "LocalFree", P, P)
        self.SetHandleInformation = _bind(kernel, "SetHandleInformation", B, H, D, D)
        self.GetHandleInformation = _bind(kernel, "GetHandleInformation", B, H, ctypes.POINTER(D))
        self.CreateMutexExW = _bind(kernel, "CreateMutexExW", H, P, wintypes.LPCWSTR, D, D)
        self.WaitForSingleObject = _bind(kernel, "WaitForSingleObject", D, H, D)
        self.ReleaseMutex = _bind(kernel, "ReleaseMutex", B, H)
        self.CreateJobObjectW = _bind(kernel, "CreateJobObjectW", H, P, wintypes.LPCWSTR)
        self.SetInformationJobObject = _bind(kernel, "SetInformationJobObject", B, H, D, P, D)
        self.QueryInformationJobObject = _bind(kernel, "QueryInformationJobObject", B, H, D, P, D, P)
        self.AssignProcessToJobObject = _bind(kernel, "AssignProcessToJobObject", B, H, H)
        self.TerminateJobObject = _bind(kernel, "TerminateJobObject", B, H, wintypes.UINT)
        self.CreateProcessW = _bind(kernel, "CreateProcessW", B, wintypes.LPCWSTR, wintypes.LPWSTR, P, P, B, D, P, wintypes.LPCWSTR, P, P)
        self.ResumeThread = _bind(kernel, "ResumeThread", D, H)
        self.GetExitCodeProcess = _bind(kernel, "GetExitCodeProcess", B, H, ctypes.POINTER(D))
        self.TerminateProcess = _bind(kernel, "TerminateProcess", B, H, wintypes.UINT)
        self.GenerateConsoleCtrlEvent = _bind(kernel, "GenerateConsoleCtrlEvent", B, D, D)
        self.OpenProcessToken = _bind(advapi, "OpenProcessToken", B, H, D, ctypes.POINTER(H))
        self.GetTokenInformation = _bind(advapi, "GetTokenInformation", B, H, D, P, D, ctypes.POINTER(D))
        self.ConvertSidToStringSidW = _bind(advapi, "ConvertSidToStringSidW", B, P, ctypes.POINTER(wintypes.LPWSTR))
        self.ConvertStringSecurityDescriptorToSecurityDescriptorW = _bind(
            advapi, "ConvertStringSecurityDescriptorToSecurityDescriptorW", B,
            wintypes.LPCWSTR, D, ctypes.POINTER(P), ctypes.POINTER(D),
        )
        self.GetSecurityInfo = _bind(advapi, "GetSecurityInfo", D, H, D, D, ctypes.POINTER(P), P, ctypes.POINTER(P), P, ctypes.POINTER(P))
        self.EqualSid = _bind(advapi, "EqualSid", B, P, P)
        self.GetSecurityDescriptorControl = _bind(advapi, "GetSecurityDescriptorControl", B, P, ctypes.POINTER(wintypes.WORD), ctypes.POINTER(D))
        self.GetAclInformation = _bind(advapi, "GetAclInformation", B, P, P, D, D)
        self.GetAce = _bind(advapi, "GetAce", B, P, D, ctypes.POINTER(P))

    @staticmethod
    def _error(action: str) -> BuildTokenWindowsError:
        return BuildTokenWindowsError(f"{action}: Windows error {ctypes.get_last_error()}")

    def _check(self, result: int, action: str) -> None:
        if not result:
            raise self._error(action)

    def close_raw(self, handle: int, kind: str) -> None:
        self._check(self.CloseHandle(handle), f"close Win32 {kind} handle")

    def _current_sid(self) -> tuple[ctypes.Array[ctypes.c_char], int, str]:
        token = wintypes.HANDLE()
        self._check(self.OpenProcessToken(self.GetCurrentProcess(), TOKEN_QUERY, ctypes.byref(token)), "open process token")
        token_handle = _OwnedHandle(self, token.value, "token")
        try:
            size = wintypes.DWORD()
            self.GetTokenInformation(token, 1, None, 0, ctypes.byref(size))
            if not size.value:
                raise self._error("size current TokenUser SID")
            buffer = ctypes.create_string_buffer(size.value)
            self._check(self.GetTokenInformation(token, 1, buffer, size, ctypes.byref(size)), "read current TokenUser SID")
            sid = ctypes.cast(buffer, ctypes.POINTER(_TOKEN_USER)).contents.User.Sid
            text_pointer = wintypes.LPWSTR()
            self._check(self.ConvertSidToStringSidW(sid, ctypes.byref(text_pointer)), "format current TokenUser SID")
            try:
                return buffer, sid, text_pointer.value
            finally:
                self.LocalFree(ctypes.cast(text_pointer, ctypes.c_void_p))
        finally:
            token_handle.close()

    def _noninheritable(self, handle: int, kind: str) -> None:
        self._check(self.SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0), f"make {kind} handle non-inheritable")
        flags = wintypes.DWORD()
        self._check(self.GetHandleInformation(handle, ctypes.byref(flags)), f"inspect {kind} handle inheritance")
        if flags.value & HANDLE_FLAG_INHERIT:
            raise BuildTokenWindowsError(f"Win32 {kind} handle remained inheritable")

    def _verify_mutex(self, handle: int, expected_sid: int) -> None:
        owner, dacl, descriptor = ctypes.c_void_p(), ctypes.c_void_p(), ctypes.c_void_p()
        status = self.GetSecurityInfo(handle, SE_KERNEL_OBJECT, OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                                      ctypes.byref(owner), None, ctypes.byref(dacl), None, ctypes.byref(descriptor))
        if status:
            raise BuildTokenWindowsError(f"inspect build-token mutex security: Windows error {status}")
        try:
            if not owner.value or not self.EqualSid(owner, expected_sid):
                raise BuildTokenWindowsError("build-token mutex owner is not current TokenUser SID")
            control, revision = wintypes.WORD(), wintypes.DWORD()
            self._check(self.GetSecurityDescriptorControl(descriptor, ctypes.byref(control), ctypes.byref(revision)), "inspect mutex DACL control")
            if not dacl.value or not control.value & SE_DACL_PROTECTED:
                raise BuildTokenWindowsError("build-token mutex DACL is absent or inherited")
            info = _ACL_SIZE_INFORMATION()
            self._check(self.GetAclInformation(dacl, ctypes.byref(info), ctypes.sizeof(info), ACL_SIZE_INFORMATION_CLASS), "inspect mutex ACL")
            ace_pointer = ctypes.c_void_p()
            if info.AceCount != 1 or not self.GetAce(dacl, 0, ctypes.byref(ace_pointer)):
                raise BuildTokenWindowsError("build-token mutex must have exactly one access rule")
            ace = ctypes.cast(ace_pointer, ctypes.POINTER(_ACCESS_ALLOWED_ACE)).contents
            ace_sid = ctypes.c_void_p(ctypes.addressof(ace) + _ACCESS_ALLOWED_ACE.SidStart.offset)
            if ace.Header.AceType != ACCESS_ALLOWED_ACE_TYPE or ace.Mask != MUTEX_ALL_ACCESS or not self.EqualSid(ace_sid, expected_sid):
                raise BuildTokenWindowsError("build-token mutex access rule is not owner-only full control")
        finally:
            self.LocalFree(descriptor)

    def open_mutex(self, test_nonce: str | None = None) -> _OwnedHandle:
        _sid_buffer, sid, sid_text = self._current_sid()
        descriptor = ctypes.c_void_p()
        self._check(self.ConvertStringSecurityDescriptorToSecurityDescriptorW(
            _owner_only_sddl(sid_text), 1, ctypes.byref(descriptor), None), "build owner-only mutex DACL")
        try:
            attributes = _SECURITY_ATTRIBUTES(ctypes.sizeof(_SECURITY_ATTRIBUTES), descriptor, False)
            raw = self.CreateMutexExW(ctypes.byref(attributes), _mutex_name(sid_text, test_nonce), 0, MUTEX_ALL_ACCESS)
        finally:
            self.LocalFree(descriptor)
        if not raw:
            raise self._error("create SID-scoped build-token mutex")
        handle = _OwnedHandle(self, raw, "mutex")
        try:
            self._noninheritable(raw, "mutex")
            self._verify_mutex(raw, sid)
            return handle
        except BaseException:
            handle.close()
            raise

    def wait(self, handle: _OwnedHandle, milliseconds: int = 50) -> int:
        status = self.WaitForSingleObject(handle.value, milliseconds)
        if status == WAIT_FAILED:
            raise self._error(f"wait for {handle.kind}")
        return status

    def release_mutex(self, handle: _OwnedHandle) -> None:
        self._check(self.ReleaseMutex(handle.value), "release build-token mutex")

    def new_job(self) -> _OwnedHandle:
        raw = self.CreateJobObjectW(None, None)
        if not raw:
            raise self._error("create build-token child Job")
        handle = _OwnedHandle(self, raw, "Job")
        try:
            self._noninheritable(raw, "Job")
            info = _JOBOBJECT_EXTENDED_LIMIT_INFORMATION()
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            self._check(self.SetInformationJobObject(raw, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                                                     ctypes.byref(info), ctypes.sizeof(info)), "set Job kill-on-close")
            return handle
        except BaseException:
            handle.close()
            raise

    def create_suspended(
        self, executable: str, command_line: ctypes.Array[ctypes.c_wchar],
        environment: ctypes.Array[ctypes.c_wchar], trace: list[str] | None = None,
    ) -> _Child:
        startup, process = _STARTUPINFOW(), _PROCESS_INFORMATION()
        startup.cb = ctypes.sizeof(startup)
        flags = CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT
        self._check(self.CreateProcessW(executable, command_line, None, None, False, flags,
                                        environment, None, ctypes.byref(startup), ctypes.byref(process)), "create suspended protected command")
        child = _Child(_OwnedHandle(self, process.hProcess, "process"),
                       _OwnedHandle(self, process.hThread, "primary-thread"), process.dwProcessId)
        try:
            self._noninheritable(child.process.value, "process")
            self._noninheritable(child.thread.value, "primary-thread")
            if trace is not None:
                trace.append("create-suspended")
            return child
        except BaseException:
            self.TerminateProcess(child.process.value, 126)
            self.WaitForSingleObject(child.process.value, 2000)
            child.close()
            raise

    def assign_job(self, job: _OwnedHandle, child: _Child, trace: list[str] | None = None) -> None:
        self._check(self.AssignProcessToJobObject(job.value, child.process.value), "assign suspended command to Job")
        if trace is not None:
            trace.append("assign-job")

    def resume_primary(self, child: _Child, trace: list[str] | None = None) -> None:
        previous = self.ResumeThread(child.thread.value)
        if previous != 1:
            raise BuildTokenWindowsError(f"primary ResumeThread returned {previous}, expected 1")
        if trace is not None:
            trace.append("resume-primary")

    def close_primary(self, child: _Child, trace: list[str] | None = None) -> None:
        child.thread.close()
        if trace is not None:
            trace.append("close-thread")

    def process_exit_code(self, child: _Child) -> int:
        code = wintypes.DWORD()
        self._check(self.GetExitCodeProcess(child.process.value, ctypes.byref(code)), "read protected-command exit")
        if code.value == STILL_ACTIVE:
            raise BuildTokenWindowsError("process signalled completion but remains active")
        return code.value

    def generate_break(self, pid: int) -> bool:
        return bool(self.GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid))

    def terminate_job(self, job: _OwnedHandle, code: int) -> None:
        self._check(self.TerminateJobObject(job.value, code), "terminate protected-command Job")

    def active_job_processes(self, job: _OwnedHandle) -> int:
        info = _JOBOBJECT_BASIC_ACCOUNTING_INFORMATION()
        self._check(self.QueryInformationJobObject(job.value, JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION,
                                                  ctypes.byref(info), ctypes.sizeof(info), None), "inspect active Job processes")
        return info.ActiveProcesses


def _assign_and_resume(api: object, job: _OwnedHandle, child: _Child, trace: list[str] | None = None) -> None:
    api.resume_primary(child, trace)
    api.assign_job(job, child, trace)
    api.close_primary(child, trace)


@contextmanager
def _windows_signal_handlers(relay: _SignalRelay) -> Iterator[None]:
    signals = dict.fromkeys(filter(None, (signal.SIGINT, signal.SIGTERM, getattr(signal, "SIGBREAK", None))))
    previous = [(item, signal.getsignal(item)) for item in signals]
    for item, _handler in previous:
        signal.signal(item, relay)
    try:
        yield
    finally:
        for item, handler in previous:
            signal.signal(item, handler)


def _terminate_and_drain(api: object, job: _OwnedHandle, child: _Child, code: int) -> None:
    try:
        api.terminate_job(job, code)
    except BuildTokenWindowsError:
        api.TerminateProcess(child.process.value, code)
    deadline = time.monotonic() + SIGNAL_GRACE_SECONDS
    while api.wait(child.process, 50) == WAIT_TIMEOUT and time.monotonic() < deadline:
        pass
    while api.active_job_processes(job) and time.monotonic() < deadline:
        time.sleep(0.05)
    if api.active_job_processes(job):
        raise BuildTokenWindowsError("protected-command Job did not drain")


def _wait_foreground(api: object, job: _OwnedHandle, child: _Child, relay: _SignalRelay) -> int:
    forwarded, deadline = False, 0.0
    while True:
        status = api.wait(child.process, 50)
        if status == WAIT_OBJECT_0:
            result = api.process_exit_code(child)
            return 128 + relay.signum if relay.signum is not None else result
        if status != WAIT_TIMEOUT:
            raise BuildTokenWindowsError(f"unexpected process wait status 0x{status:08X}")
        if relay.signum is not None and not forwarded:
            forwarded = True
            deadline = time.monotonic() + SIGNAL_GRACE_SECONDS
            if not api.generate_break(child.pid):
                deadline = 0.0
        if forwarded and (relay.count > 1 or time.monotonic() >= deadline):
            _terminate_and_drain(api, job, child, 128 + relay.signum)
            return 128 + relay.signum


def _supervise_windows(
    command: list[str], child_env: dict[str, str], api: object,
    *, test_nonce: str | None = None, relay: _SignalRelay | None = None,
    trace: list[str] | None = None,
) -> int:
    executable, command_line, environment = _prepare_process_inputs(command, child_env)
    relay = relay or _SignalRelay()
    scope = nullcontext() if trace is not None else _windows_signal_handlers(relay)
    with scope:
        mutex = api.open_mutex(test_nonce)
        acquired = False
        try:
            while relay.signum is None:
                status = api.wait(mutex, 50)
                if status in (WAIT_OBJECT_0, WAIT_ABANDONED_0):
                    acquired = True
                    if status == WAIT_ABANDONED_0:
                        print("build-token: recovered abandoned SID-scoped mutex", file=sys.stderr)
                    break
                if status != WAIT_TIMEOUT:
                    raise BuildTokenWindowsError(f"unexpected mutex wait status 0x{status:08X}")
            if not acquired:
                return 128 + relay.signum
            job = api.new_job()
            child = None
            try:
                child = api.create_suspended(executable, command_line, environment, trace)
                if relay.signum is not None:
                    _terminate_and_drain(api, job, child, 128 + relay.signum)
                    return 128 + relay.signum
                _assign_and_resume(api, job, child, trace)
                result = _wait_foreground(api, job, child, relay)
                if api.active_job_processes(job):
                    _terminate_and_drain(api, job, child, result)
                return result
            except BaseException:
                if child is not None and child.process.value:
                    _terminate_and_drain(api, job, child, 126)
                raise
            finally:
                if child is not None:
                    child.close()
                job.close()
        finally:
            try:
                if acquired:
                    api.release_mutex(mutex)
            finally:
                mutex.close()


def _supervise_windows_for_test(
    command: list[str], child_env: dict[str, str], nonce: str
) -> int:
    if sys.platform != "win32":
        raise BuildTokenWindowsError("native Win32 test authority called off Windows")
    return _supervise_windows(command, child_env, _Win32(), test_nonce=nonce)


def supervise_windows(command: list[str], child_env: dict[str, str]) -> int:
    """Run one foreground command under the current user's canonical Win32 token."""
    if sys.platform != "win32":
        raise BuildTokenWindowsError("Win32 build-token backend called outside native Windows")
    return _supervise_windows(command, child_env, _Win32())
