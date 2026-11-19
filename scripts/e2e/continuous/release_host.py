"""Supervisor for the release host: observed identity, the periodic core request, then one runner tick.

The running server names its own head, boot and Herdr channels through `/api/health/detail`; no
server configuration file is read. Off returns before any observation or lock; its only write
turns a status that still says enabled into the switched-off one.
"""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import sys
import time
import urllib.request

from . import channel_lock, companion, inbox, publication, runner
from .store import read_json


def detail(base_url):
    with urllib.request.urlopen(base_url.rstrip("/") + "/api/health/detail", timeout=5) as response:
        value = json.load(response)
    if not isinstance(value, dict):
        raise ValueError("health detail is not an object")
    return value


def identity(host, observed):
    """Identity of the server a run talks to, as that server reports it."""
    source = observed["release_source"]
    profile = json.dumps([host["base_url"], host["cells"]], sort_keys=True)
    return {"instance_id": f"release@{source['node_hostname']}", "boot_id": observed["boot_incarnation"],
            "head": source["deployed_repo_head"],
            "binary_sha256": hashlib.sha256(Path(host["binary"]).read_bytes()).hexdigest(),
            "profile_hash": hashlib.sha256(profile.encode()).hexdigest()}


def session_host(observed, channel_id):
    """The host the booted server admits for this channel, not what a config file says."""
    herdr = observed.get("herdr")
    channels = herdr.get("configured_channels") if isinstance(herdr, dict) else None
    if not isinstance(channels, list):
        return "unknown"
    return "herdr" if str(channel_id) in {str(item) for item in channels} else "tmux"


def deploying(lock_file):
    """A held deploy lock means the deploy smoke may be using the same test cells."""
    try:
        fd = os.open(lock_file, os.O_RDONLY | os.O_CLOEXEC)
    except OSError:
        return False
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return False
    except OSError:
        return True
    finally:
        os.close(fd)


def switched_off(config, now):
    """Report the switch-off to file consumers; a machine that never ran has no status and gets none."""
    state = config.get("state_dir")
    if isinstance(state, str) and os.path.isabs(state) and (Path(state) / "status.json").is_file():
        try:
            shown = read_json(Path(state) / "status.json", None)
        except ValueError:
            shown = None
        # A run that was in flight at the switch may publish enabled again; the next off tick corrects it.
        if not isinstance(shown, dict) or shown.get("enabled") is not False:
            publication.heartbeat(Path(state), config, now)
    return {"status": "DISABLED"}


def tick(config_path, now=None):
    config = runner.load_config(config_path)
    if config.get("enabled") is not True:
        return switched_off(config, time.time() if now is None else now)
    host, interval = config["release_host"], config.get("core_interval_s")
    if interval is not None and (type(interval) is not int or interval <= 0):
        raise ValueError("core_interval_s must be null or a positive integer")
    state, now = Path(runner.settings(config)["state_dir"]), time.time() if now is None else now
    if deploying(host["deploy_lock"]):
        # A deploy restarts the server and smokes the same cells; waiting for it is healthy.
        if (state / "status.json").is_file():
            publication.heartbeat(state, config, now)
        return {"status": "DEFERRED_DEPLOY"}
    try:
        seen = identity(host, detail(host["base_url"]))
    except (OSError, ValueError, KeyError, TypeError):
        # No heartbeat: an unobservable server must surface as a stale supervisor, not a kept result.
        return {"status": "HOST_UNAVAILABLE"}
    last, waiting = read_json(state / "summary.json", {}), inbox.pending(state)[0]
    fresh = (last.get("head") == seen["head"] and last.get("suite") == "P"
             and now - (last.get("completed_at") or 0) < interval) if interval is not None else True
    if not fresh and (waiting is None or waiting["deploy_sha"] != seen["head"]):
        # One request per head and interval slot; a waiting deploy request for this head already covers it.
        inbox.submit(state, f"core-{seen['head'][:12]}-{int(now // interval)}", seen["head"], now)
    return runner.tick(config_path, now, overrides=seen)


def after_deploy(config_path, request_id, deploy_sha):
    config = runner.load_config(config_path)
    if config.get("enabled") is not True:
        return switched_off(config, time.time())
    return companion.submit(config.get("state_dir"), request_id, deploy_sha)


def main(argv=None):
    parser = argparse.ArgumentParser(prog="python3 -m scripts.e2e.continuous.release_host")
    parser.add_argument("--config", required=True)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("tick")
    deploy = commands.add_parser("after-deploy")
    deploy.add_argument("--request-id", required=True)
    deploy.add_argument("--deploy-sha", required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "tick":
            result = tick(args.config)
        else:
            result = after_deploy(args.config, args.request_id, args.deploy_sha)
    except channel_lock.LeaseBusy:
        result = {"status": "DEFERRED_LEASE"}
    except (channel_lock.LeaseError, ValueError, OSError, KeyError, TypeError) as error:
        result = {"status": "REJECTED", "reason": f"{type(error).__name__}: {error}"[:200]}
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
