"""Machine-local agy cell config, deploy requests and the test instance for one head.

`enabled=false` in the config is the single off switch: nothing is queued, prepared or sent.
Secrets flow only as a pipe from `agent-op read` into the credential writer's stdin.
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time

from scripts.dev_instance.files import AGY_CREDENTIAL_NAMES, InstanceError, safe_absolute
from scripts.e2e.continuous import companion, inbox
from scripts.e2e.continuous.release_host import deploying
from scripts.e2e.continuous.store import read_json, write_json

KEYS = {"schema_version", "enabled", "state_dir", "worktree", "profile", "admission", "resources",
        "deploy_lock", "credentials"}
SHA = re.compile(r"[0-9a-f]{40}")
NO_RESULT_S = 2900
WALL_TIMEOUT_S, STOP_GRACE_S, LIVE_WAIT_S = 500, 20, 150


def load_config(path):
    config = json.loads(Path(path).read_text())
    if not isinstance(config, dict) or set(config) != KEYS or config["schema_version"] != 1:
        raise InstanceError("agy cell config fields differ from schema 1")
    if type(config["enabled"]) is not bool:
        raise InstanceError("enabled must be an explicit boolean")
    for key in ("state_dir", "worktree", "profile", "admission", "resources"):
        config[key] = safe_absolute(config[key])
    credentials = config["credentials"]
    if (not isinstance(credentials, dict) or set(credentials) != AGY_CREDENTIAL_NAMES
            or any(not isinstance(ref, str) or not ref.startswith("op://") for ref in credentials.values())):
        raise InstanceError("credentials must map the three agy names to op references")
    if not isinstance(config["deploy_lock"], str) or not os.path.isabs(config["deploy_lock"]):
        raise InstanceError("deploy_lock must be an absolute path")
    return config


def enqueue(config_path, request_id, head):
    """Queue one deploy request; a missing or switched-off config queues nothing."""
    try:
        config = load_config(config_path)
    except (OSError, ValueError, InstanceError):
        return {"status": "REJECTED", "reason": "agy cell config unavailable or invalid"}
    if not config["enabled"]:
        return {"status": "DISABLED"}
    return companion.submit(str(config["state_dir"]), request_id, head)


def drain(config, run_request, now=None):
    """Run only the newest pending request; older ones are superseded, stale ones have no result."""
    if not config["enabled"]:
        return {"status": "DISABLED"}
    state = config["state_dir"]
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    with open(state / "drain.lock", "a") as handle:
        try:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            return {"status": "DEFERRED_BUSY"}
        if deploying(config["deploy_lock"]):
            return {"status": "DEFERRED_DEPLOY"}
        newest, older, _ = inbox.pending(state)
        if newest is None:
            return {"status": "IDLE"}
        now = time.time() if now is None else now
        updates = {request: f"SUPERSEDED({newest['request_id']})" for request in older}
        if now - newest["submitted_at"] > NO_RESULT_S:
            result = {"status": "FAIL", "reasons": {"request": ["NO_RESULT"]}}
        else:
            result = run_request(newest)
        status = "PASS" if result.get("status") == "PASS" else f"FAIL({result.get('status')})"
        write_json(state / "results" / f"{newest['request_id']}.json",
                   {**result, "request_id": newest["request_id"], "head": newest["deploy_sha"],
                    "verdict": status, "completed_at": now})
        inbox.record(state, {**updates, newest["request_id"]: status})
        return {"status": status, "request_id": newest["request_id"], "superseded": sorted(updates)}


def _execute(argv, timeout, env=None, cwd=None):
    result = subprocess.run([str(part) for part in argv], stdin=subprocess.DEVNULL, capture_output=True, text=True,
                            timeout=timeout, env=env, cwd=cwd, check=False)
    if result.returncode != 0:
        raise InstanceError(f"{Path(str(argv[0])).name} {argv[1] if len(argv) > 1 else ''} failed")
    return result.stdout


def _pipe(source, sink, env):
    """Connect source stdout to sink stdin; this process never reads the bytes in between."""
    producer = subprocess.Popen(source, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    try:
        consumer = subprocess.Popen([str(part) for part in sink], stdin=producer.stdout, stdout=subprocess.DEVNULL,
                                    stderr=subprocess.DEVNULL, env=env)
        producer.stdout.close()
        if consumer.wait(timeout=60) != 0 or producer.wait(timeout=60) != 0:
            raise InstanceError("credential pipe failed")
    finally:
        if producer.poll() is None:
            producer.kill()
            producer.wait()


def instance_env(config):
    return {"PATH": f"{config['worktree']}/target/debug:/usr/bin:/bin", "HOME": str(Path.home()), "LC_ALL": "C"}


def ensure(config, head, *, execute=_execute, pipe=_pipe):
    """Prepare the worktree, binary and a validated instance for exactly this head."""
    if not isinstance(head, str) or not SHA.fullmatch(head):
        raise InstanceError("ensure needs a full lowercase head")
    worktree, state = config["worktree"], config["state_dir"]
    script, env = worktree / "scripts/dev-instance.sh", instance_env(config)
    current = read_json(state / "instance.json", {})
    if current.get("head") == head:
        return current["instance_id"]
    execute(["git", "-C", worktree, "fetch", "origin"], 120)
    execute(["git", "-C", worktree, "checkout", "--detach", head], 60)
    if (execute(["git", "-C", worktree, "rev-parse", "HEAD"], 30).strip() != head
            or execute(["git", "-C", worktree, "status", "--porcelain"], 30).strip()):
        raise InstanceError("agy cell worktree is not the clean requested head")
    execute(["cargo", "build", "--bin", "agentdesk"], 1800, cwd=worktree)
    instance_id = json.loads(execute([script, "prepare", worktree, "--profile", config["profile"], "--agy-login"],
                                     120, env=env))["instance_id"]
    for name in sorted(config["credentials"]):
        pipe(["agent-op", "read", config["credentials"][name]],
             [script, "credential", "--instance", instance_id, "--name", name], env)
    if json.loads(execute([script, "validate", "--instance", instance_id], 60, env=env))["status"] != "DECLARATION_VALID":
        raise InstanceError("prepared agy instance did not validate")
    write_json(state / "instance.json", {"head": head, "instance_id": instance_id})
    previous = current.get("instance_id")
    if previous and previous != instance_id:
        try:
            execute([script, "down", "--instance", previous], 120, env=env)
        except (InstanceError, subprocess.SubprocessError):
            pass  # The old root stays for manual `down`; it cannot start without its own admission.
    return instance_id


def live_run(config, instance_id, head, scenario, *, popen=subprocess.Popen, clock=time.monotonic, sleep=time.sleep):
    """Run `up` with the admission frame on stdin, wait for its verified run, then stop it."""
    worktree = config["worktree"]
    binary = worktree / "target/debug/agentdesk"
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    frame = json.dumps(json.loads(config["admission"].read_text()), separators=(",", ":")) + "\n"
    record = Path.home() / ".adk/dev/instances" / instance_id / "runtime/instance-run.json"
    child = popen([str(worktree / "scripts/dev-instance.sh"), "up", "--instance", instance_id, "--head", head,
                   "--binary-sha256", digest, "--wall-timeout", str(WALL_TIMEOUT_S), "--stop-grace", str(STOP_GRACE_S),
                   "--resources", str(config["resources"])],
                  stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                  env=instance_env(config), text=True)
    try:
        child.stdin.write(frame)
        child.stdin.flush()
        deadline = clock() + LIVE_WAIT_S
        while read_json(record, {}).get("verified") is not True:
            if child.poll() is not None or clock() >= deadline:
                return {"status": "FAIL", "reasons": {"instance": ["not_live_before_deadline"]}, "transmissions": 0}
            sleep(1)
        return scenario(instance_id)
    finally:
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=STOP_GRACE_S + 20)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
