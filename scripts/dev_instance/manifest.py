"""Prepare and revalidate declarations without starting a daemon or PostgreSQL."""

import json
import math
import os
import re
import socket
import stat
import subprocess
from pathlib import Path

import yaml

from .config import HERDR_CONFIG, build_config, build_org, declared_hosts, provider_home, validate_claude_login, claude_runtime_path
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


class _OrgLoader(yaml.SafeLoader):
    def compose_node(self, parent, index):
        if self.check_event(yaml.AliasEvent) or getattr(self.peek_event(), 'anchor', None) is not None:
            raise yaml.YAMLError('org anchors and aliases are not allowed')
        return super().compose_node(parent, index)

    def construct_mapping(self, node, deep=False):
        keys = set()
        for key_node, _ in node.value:
            if key_node.tag != 'tag:yaml.org,2002:str':
                raise yaml.YAMLError('org keys must be strings without merges')
            key = self.construct_object(key_node, deep=deep)
            if key in keys:
                raise yaml.YAMLError('duplicate org key')
            keys.add(key)
        return super().construct_mapping(node, deep=deep)

    def construct_yaml_bool(self, node):
        if not re.fullmatch(r'true|True|TRUE|false|False|FALSE', node.value):
            raise yaml.YAMLError('unsupported org boolean')
        return node.value.lower() == 'true'


# Keep this dialect local to org; Native uses YAML 1.2 boolean spellings.
_OrgLoader.yaml_implicit_resolvers = {
    key: [(tag, pattern) for tag, pattern in rules if tag != 'tag:yaml.org,2002:bool']
    for key, rules in yaml.SafeLoader.yaml_implicit_resolvers.items()
}
_OrgLoader.add_implicit_resolver('tag:yaml.org,2002:bool', re.compile(r'^(?:true|True|TRUE|false|False|FALSE)$'), list('tTfF'))
_OrgLoader.add_constructor('tag:yaml.org,2002:bool', _OrgLoader.construct_yaml_bool)


def read_org(path):
    try:
        text = read_regular_text(path)
        try:
            value = json.loads(text, object_pairs_hook=_unique_object)
        except json.JSONDecodeError:
            value = yaml.load(text, Loader=_OrgLoader)
        if type(value) is not dict:
            raise InstanceError('org declaration must be a mapping')
        return value
    except (OSError, ValueError, yaml.YAMLError):
        raise InstanceError('invalid org declaration file') from None


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


def _claude_login_for_root(value, root):
    if value is None:
        return None
    if type(value) is not dict or not isinstance(value.get('login_home'), str):
        raise InstanceError('explicit Claude login HOME required')
    home = safe_absolute(value['login_home'])
    if root.parent != home / '.adk/dev/instances':
        raise InstanceError('Claude login HOME does not own the instance pool')
    return validate_claude_login(value, home, root)


def declared_manifest(base, instance_id, worktree, head, profile, api_port, pg_port, claude_login=None):
    applied = validate_profile(profile)
    root = instance_root(base, instance_id)
    safe_absolute(worktree)
    if not isinstance(head, str) or not re.fullmatch(r"[0-9a-f]{40}", head):
        raise InstanceError("invalid committed head")
    if type(api_port) is not int or api_port not in API_PORTS or type(pg_port) is not int or pg_port not in PG_PORTS:
        raise InstanceError("port is outside the loopback test pool")
    claude_login = _claude_login_for_root(claude_login, root)
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
    } | ({"claude_login": claude_login} if claude_login is not None else {})


def child_environment(manifest, ambient):
    reject_database_env(ambient)
    # Build from constants and validated declarations; ambient secrets never enter.
    root = safe_absolute(manifest["root"])
    expected = declared_manifest(root.parent, manifest["instance_id"], manifest["worktree"],
                                 manifest["head"], manifest["profile"],
                                 manifest["api"]["port"], manifest["postgres"]["port"], manifest.get("claude_login"))
    if not same_json(manifest, expected):
        raise InstanceError("child environment differs from its declaration")
    environment = {"PATH": claude_runtime_path(safe_absolute(manifest["worktree"]), "claude_login" in manifest),
                   "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8"}
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
        "HOME": str(root / "home"), "TMPDIR": str(root / "tmp"),
        "XDG_CONFIG_HOME": str(root / "xdg"),
        "CLAUDE_CONFIG_DIR": manifest["claude_login"]["config_dir"] if "claude_login" in manifest else str(provider_home(root, "claude")),
        "CODEX_HOME": str(provider_home(root, "codex")),
        "AGENTDESK_DCSERVER_LABEL": "com.agentdesk.test." + manifest["instance_id"],
        "AGENTDESK_RELEASE_TMUX_SESSION": "AdkTest-" + manifest["instance_id"] + "-dcserver",
    })
    if "claude_login" in manifest:
        environment["AGENTDESK_CLAUDE_PATH"] = manifest["claude_login"]["cli"]
    return environment


def receipt_for(manifest):
    return {key: manifest[key] for key in (
        "schema_version", "status", "live_ready", "instance_id", "root", "worktree",
        "head", "api", "postgres", "applied_env", "execution_node_id", "preferred_node_seed_required", "hosts"
    )} | {"features": manifest["profile"]["features"],
         "bots": manifest["profile"]["bots"], "channels": manifest["profile"]["channels"]} | ({"claude_login": manifest["claude_login"]} if "claude_login" in manifest else {})


def prepare(base, instance_id, worktree, profile, ambient, api_port=None, pg_port=None, claude_login=None):
    reject_database_env(ambient)
    validate_profile(profile)
    root = instance_root(base, instance_id)
    if claude_login is not None:
        claude_login = _claude_login_for_root(claude_login, root)
        claude_login["config_dir"] = str(provider_home(root, "claude"))
    head = worktree_identity(worktree)
    manifest = declared_manifest(base, instance_id, worktree, head, profile,
                                 allocate_port(api_port, API_PORTS), allocate_port(pg_port, PG_PORTS), claude_login)
    safe_absolute(base).mkdir(mode=0o700, parents=True, exist_ok=True)
    create_layout(root)
    if claude_login is not None:
        write_private_json(root, provider_home(root, "claude").relative_to(root) / ".claude.json",
                           {"hasCompletedOnboarding": True, "theme": "dark",
                            "projects": {str(root / "workspaces/claude"): {"hasTrustDialogAccepted": True}}})
        write_private_json(root, provider_home(root, "claude").relative_to(root) / "settings.json",
                           {"skipDangerousModePermissionPrompt": True})
    for relative in HOST_DIRS:
        (root / relative).mkdir(mode=0o700)
    write_private_text(root, "herdr-home/config.toml", HERDR_CONFIG)
    for provider in ("claude", "codex"):
        write_private_text(root, f"config/agents/{provider}.md", "Isolated test agent.\n")
    config = build_config(root, profile, manifest["api"]["port"], manifest["postgres"]["port"], instance_id)
    write_private_json(root, "config/agentdesk.yaml", config)
    write_private_json(root, "config/org.yaml", build_org(root, profile, manifest.get("claude_login")))
    write_private_json(root, MANIFEST_NAME, manifest)
    write_private_json(root, RECEIPT_NAME, receipt_for(manifest))
    return manifest


def resource_spec(value):
    keys = {"postgres_bin", "initdb_bin", "psql_bin", "herdr_bin", "startup_timeout",
            "stop_grace", "max_aux", "pg_pool", "pg_reserve"}
    if type(value) is not dict or set(value) != keys:
        raise InstanceError("invalid resource specification fields")
    for key, maximum in (("startup_timeout", 600), ("stop_grace", 30)):
        item = value[key]
        if type(item) not in (int, float) or not 0 < item <= maximum or not math.isfinite(item):
            raise InstanceError("resource deadline is outside its bound")
    for key, expected in (("max_aux", 2), ("pg_pool", 4), ("pg_reserve", 1)):
        if type(value[key]) is not int or value[key] != expected:
            raise InstanceError("resource capacity differs from its declared bound")
    for key in ("postgres_bin", "initdb_bin", "psql_bin", "herdr_bin"):
        if not isinstance(value[key], str):
            raise InstanceError("resource executable must be an explicit absolute path")
        try:
            path = safe_absolute(value[key])
            metadata = path.stat()
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o022 or not os.access(path, os.X_OK):
                raise OSError()
        except (OSError, ValueError):
            raise InstanceError("resource executable is not a safe executable file") from None
    return dict(value)


def validate(base, instance_id, ambient, require_credentials=False, check_ports=True, live_sockets=None, check_head=True, allow_missing_directories=False):
    reject_database_env(ambient)
    root = instance_root(base, instance_id)
    validate_tree(root, require_credentials=require_credentials, live_sockets=live_sockets)
    if any(not (root / relative).is_dir() and (not allow_missing_directories or (root / relative).exists())
           for relative in (*LAYOUT_DIRS, *HOST_DIRS)):
        raise InstanceError("declared instance directory is missing")
    manifest = read_json(root / MANIFEST_NAME)
    try:
        expected = declared_manifest(base, instance_id, manifest["worktree"], manifest["head"],
                                     manifest["profile"], manifest["api"]["port"], manifest["postgres"]["port"], manifest.get("claude_login"))
        if not same_json(manifest, expected) or not same_json(read_json(root / RECEIPT_NAME), receipt_for(expected)):
            raise InstanceError("manifest or receipt differs from its declaration")
        if check_head and worktree_identity(manifest["worktree"]) != manifest["head"]:
            raise InstanceError("worktree head changed")
        config = read_json(root / "config/agentdesk.yaml")
        if not same_json(config, build_config(root, manifest["profile"], manifest["api"]["port"],
                                             manifest["postgres"]["port"], instance_id)):
            raise InstanceError("explicit config differs from its declaration")
        if not same_json(read_org(root / "config/org.yaml"), build_org(root, manifest["profile"], manifest.get("claude_login"))):
            raise InstanceError("org catalog differs from its declaration")
    except (KeyError, TypeError, AttributeError):
        raise InstanceError("invalid startup manifest") from None
    if check_ports:
        check_port(manifest["api"]["port"], API_PORTS)
        check_port(manifest["postgres"]["port"], PG_PORTS)
    return manifest
