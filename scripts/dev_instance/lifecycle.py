"""Bound one declared child without claiming live readiness or orphan authority."""

import hashlib
from contextlib import ExitStack, contextmanager
import json
import math
import threading
import os
from pathlib import Path
import re
import select
import signal
import subprocess
import time

from .files import InstanceError, safe_absolute, publish_runtime
from . import identity
from .lease import LEASE_ENV, acquire
from .manifest import child_environment, validate, worktree_identity

MAX_WALL_SECONDS = 600
MAX_STOP_GRACE_SECONDS = 30


class CleanupPending(InstanceError):
    def __init__(self, child):
        super().__init__("owned child cleanup pending")
        self.child = child


class _VerificationRefused(Exception):
    pass


@contextmanager
def _verified_sigterm(enabled):
    stopped = threading.Event()
    previous = None
    if enabled:
        if threading.current_thread() is not threading.main_thread():
            raise InstanceError('verified supervisor requires its original main thread')
        def requested(signum, frame):
            if not stopped.is_set():
                stopped.received_at = time.monotonic()
                stopped.set()
        previous = signal.signal(signal.SIGTERM, requested)
    try:
        yield stopped
    finally:
        if enabled:
            signal.signal(signal.SIGTERM, previous)


def _bounds(wall_timeout, stop_grace, max_gateway):
    valid = lambda value: type(value) in (int, float) and math.isfinite(value) and value > 0
    if (type(max_gateway) is not int or max_gateway != 1 or not valid(wall_timeout)
            or not valid(stop_grace) or wall_timeout > MAX_WALL_SECONDS
            or stop_grace > min(wall_timeout, MAX_STOP_GRACE_SECONDS)):
        raise InstanceError("unsupported lifecycle resource bounds")


def _stop_child(child, grace, deadline=None):
    # This function receives only this supervisor's sole, unreaped Popen handle.
    if child.poll() is not None:
        return
    remaining = lambda: grace if deadline is None else max(0, deadline-time.monotonic())
    child.terminate()
    try:
        child.wait(timeout=remaining())
    except subprocess.TimeoutExpired:
        child.kill()
        try:
            child.wait(timeout=remaining())
        except subprocess.TimeoutExpired:
            raise CleanupPending(child) from None


class _PrefixedControl:
    def __init__(self, prefix, stream):
        self.prefix, self.stream = prefix, stream

    def readline(self, limit=-1):
        prefix, self.prefix = self.prefix, ''
        return prefix if prefix.endswith('\n') else prefix+self.stream.readline(
            limit if limit < 0 else max(0, limit-len(prefix)))


def initial_frame(stream):
    """Only a complete pre-filled typed frame opts in; legacy stdin never waits."""
    try:
        descriptor = stream.fileno()
    except (AttributeError, OSError, ValueError):
        return None, stream
    data = bytearray()
    while len(data) <= 4096 and select.select([descriptor], [], [], 0)[0]:
        byte = os.read(descriptor, 1)
        if not byte:
            break
        data.extend(byte)
        if byte == b'\n':
            break
    if len(data) > 4096:
        raise InstanceError('initial control frame exceeds its bound')
    text = data.decode('utf-8')
    if text.endswith('\n'):
        try:
            frame = json.loads(text)
        except ValueError:
            frame = None
        if type(frame) is dict and frame.get('kind') == 'admission':
            identity.parse_admission(frame)
            return frame, stream
        if type(frame) is dict and frame.get('kind') == 'orphan-control':
            raise InstanceError('cooperative orphan control is not available')
    return None, _PrefixedControl(text, stream) if text else stream


def supervise(base, instance_id, ambient, *, binary_sha256, head, wall_timeout,
              stop_grace, max_gateway=1, spawn=subprocess.Popen, resources=None, control_stream=None):
    _bounds(wall_timeout, stop_grace, max_gateway)
    started = time.monotonic()
    admission, control_stream = initial_frame(control_stream)
    if admission is not None and resources is None:
        raise InstanceError('verified admission requires owned runtime resources')
    manifest = validate(base, instance_id, ambient)
    if (not isinstance(head, str) or manifest["head"] != head
            or not isinstance(binary_sha256, str) or not re.fullmatch(r"[0-9a-f]{64}", binary_sha256)):
        raise InstanceError("explicit lifecycle source identity mismatch")
    binary = safe_absolute(Path(manifest["worktree"]) / "target/debug/agentdesk")
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise InstanceError("declared lane binary is unavailable")
    descriptor = os.open(binary, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "rb") as source:
        if hashlib.file_digest(source, "sha256").hexdigest() != binary_sha256:
            raise InstanceError("declared lane binary SHA mismatch")
        environment = child_environment(manifest, ambient)
        root = Path(manifest["root"])
        observed = identity.probe(root, admission, started+wall_timeout,
                                  declared_roles=manifest["profile"]["bots"]) if admission is not None else None
        with acquire(root, instance_id) as owner:
            owner.verify()
            if worktree_identity(manifest["worktree"]) != head:
                raise InstanceError("lifecycle source HEAD changed")
            remaining = wall_timeout - (time.monotonic() - started)
            if remaining <= 0:
                raise InstanceError("lifecycle startup budget exhausted")
            environment[LEASE_ENV] = json.dumps(owner.witness(), sort_keys=True, separators=(",", ":"))
            verification = None
            if observed is not None:
                run = identity.run_payload(manifest, source, binary_sha256, observed)
                environment[identity.RUN_ENV] = json.dumps(run, separators=(",", ":"))
                verification = (source.fileno(), binary_sha256, observed)
            if resources is not None:
                try:
                    return _supervise_resources(manifest, resources, binary, environment, owner,
                                                started + wall_timeout, stop_grace, spawn, control_stream,
                                                verification)
                except _VerificationRefused:
                    raise InstanceError('runtime witness verification refused') from None
            child = spawn([str(binary), "dcserver"], cwd=str(root), env=environment,
                          stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                          stderr=subprocess.DEVNULL, close_fds=True, pass_fds=(owner.fd,))
            status = "STOPPED"
            try:
                owner.verify()
                remaining = max(0, wall_timeout - (time.monotonic() - started))
                child.wait(timeout=remaining)
            except subprocess.TimeoutExpired:
                status = "TIMEOUT"
                _stop_child(child, stop_grace)
            except BaseException:
                _stop_child(child, stop_grace)
                raise
            if status == "STOPPED" and child.returncode != 0:
                status = "FAILED"
            return {"status": status, "live_ready": False, "instance_id": instance_id,
                    "head": head, "binary_sha256": binary_sha256, "child_pid": child.pid,
                    "child_exit": child.returncode, "nonce": owner.nonce,
                    "max_gateway": 1, "restarts": 0}


def _supervise_resources(manifest, specification, binary, environment, lease, deadline, grace, spawn, control_stream,
                         verification=None):
    from . import keeper
    from .manifest import resource_spec
    specification = resource_spec(specification)
    if specification["stop_grace"] != grace or specification["startup_timeout"] > deadline-time.monotonic():
        raise InstanceError("resource bounds exceed supervisor admission")
    process, control = keeper.launch(manifest, specification, lease.nonce)
    child, requested, timed_out, stop_end = None, False, False, None
    cooperative, forced, signal_forwarded = False, False, False
    signal_scope, stop_requested = ExitStack(), None
    forward_failed = threading.Event()
    signal_deadline = None

    def stop_gateway(end):
        nonlocal cooperative, forced
        if verification is not None:
            try:
                ack = identity.cooperative_stop(manifest, lease.nonce, child,
                                                verification[1], verification[2], end)
                if not ack.get('gateway_wait_ack') or ack.get('exit_code') != child.returncode:
                    raise identity.ControlRefused('original Gateway wait was not acknowledged')
                cooperative = True
                return
            except identity.ControlRefused:
                forced = True
        # A failed private control keeps its attribution even after known-own cleanup.
        _stop_child(child, grace, end)

    def request_signal_stop():
        try:
            keeper.request(manifest['root'], lease.nonce, 'down', timeout=grace+1)
        except (EOFError, OSError, ValueError):
            forward_failed.set()
    try:
        ready = keeper.receive(control)
        if ready.get("state") != "RUNNING":
            return {**ready, "status": "HOLD", "live_ready": False,
                    "max_gateway": 1, "restarts": 0}
        lease.verify()
        if time.monotonic() >= deadline:
            raise InstanceError("resource startup exhausted wall budget")
        child = spawn([str(binary), "dcserver"], cwd=manifest["root"], env=environment,
                      stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                      stderr=subprocess.DEVNULL, close_fds=True, pass_fds=(lease.fd,) + ((verification[0],) if verification else ()))
        if verification is not None:
            try:
                witness = identity.wait_health(manifest, lease.nonce, child.pid, verification[1],
                                               verification[2], deadline, alive=lambda: child.poll() is None)
                if time.monotonic() >= deadline:
                    raise InstanceError('verification exhausted startup budget')
                # Startup retains B1's wrapper-death behavior; only a verified run intercepts SIGTERM.
                stop_requested = signal_scope.enter_context(_verified_sigterm(True))
                publish_runtime(Path(manifest['root']), 'runtime/instance-run.json',
                                {**witness, 'verified': True},
                                live_sockets={path: tuple(value) for path, value in ready['sockets'].items()})
            except (InstanceError, OSError):
                # Keep a witness refusal separate from legacy control-channel uncertainty.
                raise _VerificationRefused from None
        if control_stream is not None:
            def controls():
                try:
                    for line in iter(lambda: control_stream.readline(32770), ""):
                        frame = json.loads(line)
                        if len(line) <= 32769 and keeper.valid_request(frame, Path(manifest["root"]), lease.nonce):
                            keeper.request(manifest["root"], lease.nonce, frame["action"], timeout=grace+1)
                except (EOFError, OSError, ValueError):
                    return
            threading.Thread(target=controls, daemon=True).start()
        while child.poll() is None:
            if stop_requested is not None and stop_requested.is_set() and not signal_forwarded:
                signal_forwarded = True
                signal_deadline = stop_requested.received_at + grace
                threading.Thread(target=request_signal_stop, daemon=True).start()
            if signal_forwarded and (forward_failed.is_set() or time.monotonic() >= signal_deadline):
                requested, forced = True, True
                stop_end = time.monotonic() + grace
                _stop_child(child, grace, stop_end)
                break
            remaining = deadline-time.monotonic()
            if remaining <= 0:
                requested, timed_out = True, True
                stop_end = time.monotonic() + grace
                stop_gateway(stop_end)
                break
            ready, _, _ = select.select([control], [], [], min(remaining, .05))
            if ready:
                frame = keeper.receive(control)
                stop_end = frame.get("deadline")
                if (set(frame) != {"action", "nonce", "deadline"} or frame["action"] != "stop"
                        or frame["nonce"] != lease.nonce or type(stop_end) not in (int, float)
                        or not math.isfinite(stop_end) or stop_end > time.monotonic()+grace):
                    raise InstanceError("keeper control authority changed")
                requested = True
                if signal_deadline is not None:
                    stop_end = min(stop_end, signal_deadline)
                stop_gateway(stop_end)
                break
        stop_end = stop_end or time.monotonic()+grace
        keeper.send(control, {"nonce": lease.nonce, "exit": child.wait(timeout=max(0, stop_end-time.monotonic())),
                              "deadline": stop_end})
        control.settimeout(grace + 1)
        result = keeper.receive(control)
        if result.get("state") != "STOPPED" or not result.get("known_resources_closed"):
            raise CleanupPending(process)
        if process.wait(timeout=grace) != 0:
            raise CleanupPending(process)
        return {**result, "status": "HOLD" if forced else "TIMEOUT" if timed_out else
                "STOPPED" if requested or child.returncode == 0 else "FAILED",
                "child_pid": child.pid, "child_exit": child.returncode,
                "max_gateway": 1, "restarts": 0, "head": manifest["head"],
                **({"verified": True, "cooperative_stop": cooperative, "forced_cleanup": forced,
                    "gateway_wait_ack": child.returncode is not None} if verification else {})}
    except (EOFError, OSError, ValueError) as error:
        if child is not None:
            _stop_child(child, grace, stop_end or time.monotonic()+grace)
        raise CleanupPending(process) from error
    except BaseException:
        if child is not None:
            stop_end = stop_end or time.monotonic()+grace
            _stop_child(child, grace, stop_end)
            try:
                keeper.send(control, {"nonce": lease.nonce, "exit": child.returncode, "deadline": stop_end})
            except OSError:
                pass
        raise
    finally:
        signal_scope.close()
        control.close()
        try:
            process.wait(timeout=grace + 1)
        except subprocess.TimeoutExpired:
            # The keeper retains pending original handles; killing it would orphan them.
            raise CleanupPending(process) from None
