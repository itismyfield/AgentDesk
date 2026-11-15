"""Build a standalone runtime config from an already validated test profile."""

import json
import subprocess
from pathlib import Path

from .files import InstanceError, safe_absolute

HERDR_CONFIG = "[session]\nresume_agents_on_restore = false\n"


def postgres_connections(config: dict, reserve: int) -> int:
    pool = max(config['database']['pool_max'], 1)
    warmup = max((pool * 3 + 1) // 2, 2)
    gateways = len(config['discord']['bots'])
    cluster = config['cluster']
    leader = int(cluster['enabled'] and cluster['role'] in {'hub', 'auto'})
    # server::run retains the runtime pool while its native warmup pool is open.
    return pool + warmup + gateways + leader + reserve


def validate_claude_login(value, home):
    if value is None:
        return None
    keys = {'mode', 'account_slot', 'login_home', 'config_dir', 'cli'}
    if (type(value) is not dict or set(value) != keys or value['mode'] != 'default_active'
            or type(value['account_slot']) is not int or value['account_slot'] not in (2, 3)):
        raise InstanceError('explicit non-company Claude login required')
    home = safe_absolute(home)
    if (safe_absolute(value['login_home']) != home
            or safe_absolute(value['config_dir']) != home / '.claude'
            or not safe_absolute(value['cli']).is_relative_to(home / '.local')):
        raise InstanceError('Claude login binding is not the default location')
    return dict(value)


def active_claude_login(home):
    home = safe_absolute(home)
    try:
        result = subprocess.run([str(home / '.local/bin/cswap'), 'list', '--json'],
                                env={'HOME': str(home), 'PATH': '/opt/homebrew/bin:/usr/bin:/bin'},
                                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                stderr=subprocess.DEVNULL, text=True, timeout=30)
    except (OSError, subprocess.SubprocessError):
        raise InstanceError("active Claude slot unavailable") from None
    try:
        slot = json.loads(result.stdout)['activeAccountNumber']
    except (ValueError, KeyError, TypeError):
        raise InstanceError('active Claude slot unavailable') from None
    if result.returncode or type(slot) is not int or slot not in (2, 3):
        raise InstanceError('active Claude slot must be 2 or 3; switching is forbidden')
    return validate_claude_login({'mode': 'default_active', 'account_slot': slot,
        'login_home': str(home), 'config_dir': str(home / '.claude'),
        'cli': str((home / '.local/bin/claude').resolve(strict=True))}, home)


def login_runtime_path(worktree, login=None):
    original = str(worktree / 'target/debug') + ':/usr/bin:/bin'
    if login is None:
        return original
    return str(worktree / 'target/debug') + ':' + str(Path(login['login_home']) / '.local/bin') + ':/opt/homebrew/bin:/usr/bin:/bin'


def provider_home(root: Path, provider: str) -> Path:
    return root / "home/.adk/profiles" / provider / f"test-{provider}"


def build_org(root: Path, profile: dict, claude_login=None) -> dict:
    providers = ("claude", "codex")
    return {
        "version": 1, "name": "Isolated test instance",
        "provider_auth_profiles": {
            f"test-{provider}": {"provider": provider, "home": claude_login["config_dir"] if provider == "claude" and claude_login else str(provider_home(root, provider)), "env": {}}
            for provider in providers
        },
        "provider_auth_primary_profiles": {provider: f"test-{provider}" for provider in providers},
        "agents": {
            provider: {"display_name": f"Test {provider.title()}", "provider": provider,
                       "prompt_file": str(root / "config/agents" / f"{provider}.md"),
                       "workspace": str(root / "workspaces" / provider),
                       "peer_agents": False, "auth_profile": f"test-{provider}"}
            for provider in providers
        },
        "channels": {"by_id": {
            channel: {"agent": provider, "provider": provider, "peer_agents": False,
                      "workspace": str(root / "workspaces" / provider), "auth_profile": f"test-{provider}"}
            for provider, channel in zip(providers, profile["channels"])
        }},
    }


def declared_hosts(root: Path, instance_id: str) -> dict:
    return {
        "tmux": {"socket": str(root / "runtime/tmux/socket"),
                 "tmpdir": str(root / "runtime/tmux"), "prefix": "AdkTest-" + instance_id},
        "herdr": {"socket": str(root / "runtime/herdr/socket"),
                  "home": str(root / "herdr-home"), "session": "adk-test-" + instance_id,
                  "endpoint_key": "test-" + instance_id},
    }


def build_config(root: Path, profile: dict, api_port: int, pg_port: int,
                 instance_id: str) -> dict:
    features = profile["features"]
    channels = profile["channels"]
    ids = [int(channel) for channel in channels]
    database_name = "adktest_" + instance_id.replace("-", "")
    api_url = f"http://127.0.0.1:{api_port}"
    capacity = features["capacity_aware"]
    herdr = declared_hosts(root, instance_id)["herdr"]
    agents = []
    bots = {}
    for provider, channel in zip(("claude", "codex"), channels):
        agents.append({
            "id": provider,
            "name": f"Test {provider.title()}",
            "provider": provider,
            "auth_profile": f"test-{provider}",
            "peer_directory": False,
            "peer_keywords": [],
            "preferred_intake_node_labels": [],
            "channels": {provider: {
                "id": channel,
                "provider": provider,
                "auth_profile": f"test-{provider}",
                "runtime": "tui",
                "peer_agents": False,
                "workspace": str(root / "workspaces" / provider),
                "prompt_file": str(root / "config" / "agents" / f"{provider}.md"),
            }},
        })
        bots[provider] = {
            "provider": provider,
            "agent": provider,
            "auth": {
                "allowed_channel_ids": ids,
                "allowed_bot_ids": [int(profile["bots"]["driver"])],
                "allowed_user_ids": [],
                "allow_all_users": False,
            },
        }
    return {
        "server": {
            "host": "127.0.0.1", "port": api_port,
            "allow_insecure_nonloopback_bind": False,
        },
        "discord": {"bots": bots},
        "agents": agents,
        "providers": {
            provider: {"runtime": "tui", "transcript_homes": []}
            for provider in ("claude", "codex")
        },
        "data": {"dir": str(root / "data"), "db_name": "agentdesk.db"},
        "database": {
            "enabled": True, "host": "127.0.0.1", "port": pg_port,
            "dbname": database_name, "user": database_name,
            "pool_max": 4, "foreground_reserve": 1,
        },
        "cluster": {
            "enabled": capacity,
            "instance_id": instance_id,
            "role": "hub", "runtime_profile": "full",
            "execution_slots": 1 if capacity else None,
            "api_base_url": api_url,
            "gateway_preferred_instance_id": instance_id,
            "nodes": {}, "labels": [],
            "intake_routing": {
                "enabled": capacity,
                "mode": features["env"]["ADK_INTAKE_ROUTING_MODE"],
                "capacity_aware": capacity,
                "owner_authority_channel_ids": channels,
                "input_fallback_enabled": False,
                "input_fallback_pilot_channel_ids": [],
                "input_fallback_pair_instance_ids": [],
            },
        },
        "tui_o": {
            "writer": {
                "channels": ids if features["tui_writer"] else [],
                "all_tui": False, "late_adopt": True, "dormant_channels": [],
            },
            "turn": {
                "channels": ids if features["tui_turn"] else [],
                "all_owned": False,
            },
        },
        "policies": {"dir": str(root / "policies"), "hot_reload": False},
        "routines": {
            "enabled": False, "dir": str(root / "routines"),
            "additional_dirs": [], "headless_fresh_agents": False,
            "hot_reload": False,
        },
        "github": {"repos": [], "repo_dirs": {}},
        "mcp_servers": {}, "review_mcp_allowlist": [],
        "mcp": {"watch_credentials": False},
        "review": {"enabled": False},
        "memory": {
            "backend": "file", "shared_knowledge_injection": "off",
            "file": {
                "sak_path": str(root / "data" / "memory" / "knowledge.md"),
                "sam_path": str(root / "data" / "memory" / "shared"),
                "ltm_root": str(root / "data" / "memory" / "long-term"),
                "auto_memory_root": str(root / "data" / "memory" / "auto"),
            },
            "mcp": {"endpoint": "", "access_key_env": ""},
        },
        "session_hosts": {"herdr": {"endpoints": {herdr["endpoint_key"]: {
            "mode": "legacy_dedicated", "execution_node": instance_id,
            "socket_path": herdr["socket"], "herdr_home": herdr["home"],
            "herdr_session": herdr["session"],
        }}, "channels": {}}},
        "config_hot_reload": False,
    }
