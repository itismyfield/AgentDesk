"""Prepare and revalidate declarations without starting a daemon or PostgreSQL."""

import json
import re
import socket
import subprocess
from pathlib import Path

from .config import HERDR_CONFIG, build_config, declared_hosts
from .files import (InstanceError, LAYOUT_DIRS, create_layout, instance_root, read_regular_text,
                    safe_absolute, validate_tree, write_private_json, write_private_text)
from .profile import same_json, validate_profile

API_PORTS = range(18791, 18807)
PG_PORTS = range(25432, 25448)
MANIFEST_NAME = "manifest.json"
RECEIPT_NAME = "receipt.json"
HOST_DIRS = ("runtime/tmux", "runtime/herdr", "herdr-home")


def reject_database_env(ambient):
    if any(key == "DATABASE_URL" or key.startswith("PG") for key in ambient):
        raise InstanceError("inherited database environment is forbidden")


def read_json(path):
    try:
        return json.loads(read_regular_text(path), object_pairs_hook=_unique_object)
    except (OSError, ValueError, UnicodeError):
        raise InstanceError("invalid JSON declaration file") from None


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise InstanceError("duplicate declaration key")
        result[key] = value
    return result


def worktree_identity(worktree):
    worktree = safe_absolute(worktree)
    environment = {"PATH": "/usr/bin:/bin", "LC_ALL": "C"}
    def git(*args):
        try:
            return subprocess.check_output(
                ["git", "-C", str(worktree), *args], env=environment,
                stderr=subprocess.DEVNULL, text=True).strip()
        except (OSError, subprocess.CalledProcessError):
            raise InstanceError("worktree identity cannot be verified") from None
    if git("rev-parse", "--show-toplevel") != str(worktree):
        raise InstanceError("worktree must be its exact canonical root")
    head = git("rev-parse", "HEAD")
    if not re.fullmatch(r"[0-9a-f]{40}", head) or git("status", "--porcelain"):
        raise InstanceError("a clean committed worktree is required")
    return head


def check_port(port, pool):
    if type(port) is not int or port not in pool:
        raise InstanceError("port is outside the loopback test pool")
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            probe.bind(("127.0.0.1", port))
    except OSError:
        raise InstanceError("test port is occupied or unavailable") from None
    return port


def allocate_port(requested, pool):
    if requested is not None:
        return check_port(requested, pool)
    for port in pool:
        try:
            return check_port(port, pool)
        except InstanceError:
            continue
    raise InstanceError("test port pool is exhausted")


def declared_manifest(base, instance_id, worktree, head, profile, api_port, pg_port):
    applied = validate_profile(profile)
    root = instance_root(base, instance_id)
    safe_absolute(worktree)
    if not isinstance(head, str) or not re.fullmatch(r"[0-9a-f]{40}", head):
        raise InstanceError("invalid committed head")
    if type(api_port) is not int or api_port not in API_PORTS or type(pg_port) is not int or pg_port not in PG_PORTS:
        raise InstanceError("port is outside the loopback test pool")
    db_identity = "adktest_" + instance_id.replace("-", "")
    return {
        "schema_version": 1, "status": "DECLARED", "live_ready": False,
        "instance_id": instance_id, "root": str(root), "worktree": str(worktree),
        "head": head, "profile": profile, "applied_env": applied,
        "api": {"host": "127.0.0.1", "port": api_port,
                "self_url": f"http://127.0.0.1:{api_port}"},
        "postgres": {"host": "127.0.0.1", "port": pg_port,
                     "data_dir": str(root / "postgres/data"),
                     "socket_dir": str(root / "postgres/socket"),
                     "database": db_identity, "role": db_identity, "cluster_started": False},
        "config_path": str(root / "config/agentdesk.yaml"),
        "execution_node_id": instance_id,
        "hosts": declared_hosts(root, instance_id),
        "preferred_node_seed_required": True,
    }


def child_environment(manifest, ambient):
    reject_database_env(ambient)
    # Build from constants and validated declarations; ambient secrets never enter.
    environment = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8"}
    flags = validate_profile(manifest["profile"])
    if not same_json(flags, manifest["applied_env"]):
        raise InstanceError("applied environment differs from profile")
    environment.update(flags)
    environment.update({
        "AGENTDESK_ROOT_DIR": manifest["root"], "AGENTDESK_CONFIG": manifest["config_path"],
        "AGENTDESK_API_URL": manifest["api"]["self_url"],
        "ADK_API_URL": manifest["api"]["self_url"],
        "AGENTDESK_TEST_MANIFEST": str(Path(manifest["root"]) / MANIFEST_NAME),
        "TMUX_TMPDIR": manifest["hosts"]["tmux"]["tmpdir"],
    })
    return environment


def receipt_for(manifest):
    return {key: manifest[key] for key in (
        "schema_version", "status", "live_ready", "instance_id", "root", "worktree",
        "head", "api", "postgres", "applied_env", "execution_node_id", "preferred_node_seed_required", "hosts"
    )} | {"features": manifest["profile"]["features"],
         "bots": manifest["profile"]["bots"], "channels": manifest["profile"]["channels"]}


def prepare(base, instance_id, worktree, profile, ambient, api_port=None, pg_port=None):
    reject_database_env(ambient)
    validate_profile(profile)
    root = instance_root(base, instance_id)
    head = worktree_identity(worktree)
    manifest = declared_manifest(base, instance_id, worktree, head, profile,
                                 allocate_port(api_port, API_PORTS), allocate_port(pg_port, PG_PORTS))
    safe_absolute(base).mkdir(mode=0o700, parents=True, exist_ok=True)
    create_layout(root)
    for relative in HOST_DIRS:
        (root / relative).mkdir(mode=0o700)
    write_private_text(root, "herdr-home/config.toml", HERDR_CONFIG)
    for provider in ("claude", "codex"):
        write_private_text(root, f"config/agents/{provider}.md", "Isolated test agent.\n")
    config = build_config(root, profile, manifest["api"]["port"], manifest["postgres"]["port"], instance_id)
    write_private_json(root, "config/agentdesk.yaml", config)
    write_private_json(root, MANIFEST_NAME, manifest)
    write_private_json(root, RECEIPT_NAME, receipt_for(manifest))
    return manifest


def validate(base, instance_id, ambient, require_credentials=False, check_ports=True):
    reject_database_env(ambient)
    root = instance_root(base, instance_id)
    validate_tree(root, require_credentials=require_credentials)
    if any(not (root / relative).is_dir() for relative in (*LAYOUT_DIRS, *HOST_DIRS)):
        raise InstanceError("declared instance directory is missing")
    manifest = read_json(root / MANIFEST_NAME)
    try:
        expected = declared_manifest(base, instance_id, manifest["worktree"], manifest["head"],
                                     manifest["profile"], manifest["api"]["port"], manifest["postgres"]["port"])
        if not same_json(manifest, expected) or not same_json(read_json(root / RECEIPT_NAME), receipt_for(expected)):
            raise InstanceError("manifest or receipt differs from its declaration")
        if worktree_identity(manifest["worktree"]) != manifest["head"]:
            raise InstanceError("worktree head changed")
        config = read_json(root / "config/agentdesk.yaml")
        if not same_json(config, build_config(root, manifest["profile"], manifest["api"]["port"],
                                             manifest["postgres"]["port"], instance_id)):
            raise InstanceError("explicit config differs from its declaration")
    except (KeyError, TypeError, AttributeError):
        raise InstanceError("invalid startup manifest") from None
    if check_ports:
        check_port(manifest["api"]["port"], API_PORTS)
        check_port(manifest["postgres"]["port"], PG_PORTS)
    return manifest
