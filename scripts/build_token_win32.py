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
PROCESS_CREATION_FLAGS = CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT
PROCESS_INHERITS_HANDLES = False
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
    _fields_ = [(name, ctypes.c_ulonglong) for name in ("ReadOperationCount", "WriteOperationCount", "OtherOperationCount", "ReadTransferCount", "WriteTransferCount", "OtherTransferCount")]
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
        try:
            self.thread.close()
        finally:
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
def _validate_mutex_security(
    owner_matches: bool, dacl_present: bool, protected: bool, ace_count: int,
    ace_type: int, ace_flags: int, ace_mask: int, ace_sid_matches: bool,
) -> None:
    if not owner_matches:
        raise BuildTokenWindowsError("build-token mutex owner is not current TokenUser SID")
    if not dacl_present or not protected:
        raise BuildTokenWindowsError("build-token mutex DACL is absent or inherited")
    if ace_count != 1:
        raise BuildTokenWindowsError("build-token mutex must have exactly one access rule")
    if ace_type != ACCESS_ALLOWED_ACE_TYPE or ace_flags != 0 or ace_mask != MUTEX_ALL_ACCESS or not ace_sid_matches:
        raise BuildTokenWindowsError("build-token mutex access rule is not owner-only full control")
def _prepare_process_inputs(
    command: list[str], child_env: Mapping[str, str]
) -> tuple[str, ctypes.Array[ctypes.c_wchar], ctypes.Array[ctypes.c_wchar]]:
    if not command:
        raise BuildTokenWindowsError("protected command is required")
    entries, folded_keys = [], set()
    for key, value in child_env.items():
        folded = key.casefold()
        if not key or "\0" in key or "=" in key or "\0" in value or folded in folded_keys:
            raise BuildTokenWindowsError("invalid Win32 child environment")
        folded_keys.add(folded)
        entries.append((key, value))
    entries.sort(key=lambda item: item[0].casefold())
    search_path = next((value for key, value in entries if key.casefold() == "path"), None)
    executable = shutil.which(command[0], path=search_path)
    if executable is None:
        raise FileNotFoundError(command[0])
    executable = os.path.abspath(executable)
    command_line = ctypes.create_unicode_buffer(subprocess.list2cmdline([executable, *command[1:]]))
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
            control, revision = wintypes.WORD(), wintypes.DWORD()
            self._check(self.GetSecurityDescriptorControl(descriptor, ctypes.byref(control), ctypes.byref(revision)), "inspect mutex DACL control")
            info = _ACL_SIZE_INFORMATION()
            if dacl.value:
                self._check(self.GetAclInformation(dacl, ctypes.byref(info), ctypes.sizeof(info), ACL_SIZE_INFORMATION_CLASS), "inspect mutex ACL")
            ace_pointer = ctypes.c_void_p()
            ace_type, ace_flags, ace_mask, ace_sid_matches = -1, -1, 0, False
            if info.AceCount == 1 and self.GetAce(dacl, 0, ctypes.byref(ace_pointer)):
                ace = ctypes.cast(ace_pointer, ctypes.POINTER(_ACCESS_ALLOWED_ACE)).contents
                ace_sid = ctypes.c_void_p(ctypes.addressof(ace) + _ACCESS_ALLOWED_ACE.SidStart.offset)
                ace_type, ace_flags, ace_mask = ace.Header.AceType, ace.Header.AceFlags, ace.Mask
                ace_sid_matches = bool(self.EqualSid(ace_sid, expected_sid))
            _validate_mutex_security(
                bool(owner.value and self.EqualSid(owner, expected_sid)), bool(dacl.value),
                bool(control.value & SE_DACL_PROTECTED), info.AceCount,
                ace_type, ace_flags, ace_mask, ace_sid_matches,
            )
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
        self._check(self.CreateProcessW(executable, command_line, None, None,
                                        PROCESS_INHERITS_HANDLES, PROCESS_CREATION_FLAGS,
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
            try:
                self._terminate_unassigned(child)
            finally:
                child.close()
            raise
    def _terminate_unassigned(self, child: _Child) -> None:
        failure = None
        while True:
            if not self.TerminateProcess(child.process.value, 126):
                failure = failure or self._error("terminate unverified suspended command")
            status = self.WaitForSingleObject(child.process.value, 2000)
            if status == WAIT_OBJECT_0:
                if failure is not None: raise failure
                return
            if status != WAIT_TIMEOUT:
                failure = failure or BuildTokenWindowsError(f"unexpected unverified-process wait 0x{status:08X}")
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
        return code.value
    def generate_break(self, pid: int) -> bool:
        return bool(self.GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid))
    def terminate_job(self, job: _OwnedHandle, code: int) -> None:
        self._check(self.TerminateJobObject(job.value, code), "terminate protected-command Job")
    def terminate_process(self, child: _Child, code: int) -> None:
        self._check(self.TerminateProcess(child.process.value, code), "terminate protected command")
    def active_job_processes(self, job: _OwnedHandle) -> int:
        info = _JOBOBJECT_BASIC_ACCOUNTING_INFORMATION()
        self._check(self.QueryInformationJobObject(job.value, JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION,
                                                  ctypes.byref(info), ctypes.sizeof(info), None), "inspect active Job processes")
        return info.ActiveProcesses
def _assign_and_resume(
    api: object, job: _OwnedHandle, child: _Child, trace: list[str] | None = None,
    cancelled: Callable[[], bool] | None = None,
) -> bool:
    api.assign_job(job, child, trace)
    if cancelled is not None and cancelled():
        return False
    api.resume_primary(child, trace)
    api.close_primary(child, trace)
    return True
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
        api.terminate_process(child, code)
    deadline = time.monotonic() + SIGNAL_GRACE_SECONDS
    status = api.wait(child.process, 50)
    while status == WAIT_TIMEOUT and time.monotonic() < deadline:
        status = api.wait(child.process, 50)
    if status != WAIT_OBJECT_0:
        api.terminate_process(child, code)
        if api.wait(child.process, 2000) != WAIT_OBJECT_0:
            raise BuildTokenWindowsError("protected command did not terminate")
    deadline = time.monotonic() + SIGNAL_GRACE_SECONDS
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
            if not acquired or relay.signum is not None:
                return 128 + relay.signum
            job = api.new_job()
            child = None
            try:
                child = api.create_suspended(executable, command_line, environment, trace)
                if relay.signum is not None:
                    _terminate_and_drain(api, job, child, 128 + relay.signum)
                    return 128 + relay.signum
                if not _assign_and_resume(api, job, child, trace, lambda: relay.signum is not None):
                    _terminate_and_drain(api, job, child, 128 + relay.signum)
                    return 128 + relay.signum
                result = _wait_foreground(api, job, child, relay)
                if api.active_job_processes(job):
                    _terminate_and_drain(api, job, child, result)
                return result
            except BaseException:
                if child is not None and child.process.value:
                    _terminate_and_drain(api, job, child, 126)
                raise
            finally:
                try:
                    if child is not None:
                        child.close()
                finally:
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
