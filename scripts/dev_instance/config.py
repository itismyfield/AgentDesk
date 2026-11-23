"""Build a standalone runtime config from an already validated test profile."""

import json
import os
import stat
import sys
import subprocess
from pathlib import Path

from .files import InstanceError, read_regular_text, safe_absolute
from .profile import profile_spec

HERDR_CONFIG = "[session]\nresume_agents_on_restore = false\n"


TMUX_FORMULA_BINS = (Path('/opt/homebrew/opt/tmux/bin'), Path('/usr/local/opt/tmux/bin'))


def claude_runtime_path(worktree, enabled):
    path = str(worktree / 'target/debug') + ':/usr/bin:/bin'
    if not enabled or sys.platform != 'darwin':
        return path
    try:
        source = next(p for p in TMUX_FORMULA_BINS if p.exists())
        directory = source.resolve(strict=True)
        binary = directory / 'tmux'
        if ({p.name for p in directory.iterdir()} != {'tmux'}
                or binary.is_symlink() or not binary.is_file() or not os.access(binary, os.X_OK)):
            raise InstanceError('tmux formula directory must expose only executable tmux')
    except (OSError, StopIteration):
        raise InstanceError('isolated tmux formula directory unavailable') from None
    return path + ':' + str(directory)


def postgres_connections(config: dict, reserve: int) -> int:
    pool = max(config['database']['pool_max'], 1)
    warmup = max((pool * 3 + 1) // 2, 2)
    gateways = len(config['discord']['bots'])
    cluster = config['cluster']
    leader = int(cluster['enabled'] and cluster['role'] in {'hub', 'auto'})
    # server::run retains the runtime pool while its native warmup pool is open.
    return pool + warmup + gateways + leader + reserve


def validate_claude_login(value, home, root=None):
    if value is None:
        return None
    keys = {'mode', 'account_slot', 'login_home', 'config_dir', 'cli'}
    if (type(value) is not dict or set(value) != keys or value['mode'] != 'default_active'
            or type(value['account_slot']) is not int or value['account_slot'] not in (2, 3)):
        raise InstanceError('explicit non-company Claude login required')
    home = safe_absolute(home)
    # Keep old declarations removable; new prepares select the instance data home.
    allowed_data = {home / '.claude'}
    if root is not None:
        allowed_data.add(provider_home(safe_absolute(root), 'claude'))
    if (safe_absolute(value['login_home']) != home
            or safe_absolute(value['config_dir']) not in allowed_data
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


class BlockedAgyLogin(InstanceError):
    """The dedicated agy login home cannot be shown to carry zero MCP servers."""


AGY_CLI_CANDIDATES = ('/opt/homebrew/bin/agy', '/usr/local/bin/agy')
# Cheapest model with effort in the id; agy honours it as `--model`, not as a settings.json key.
AGY_MODEL = 'gemini-3.8-flash-low'


def validate_agy_login(value, root):
    """Bind agy to `<instances parent>/agy-login`, never the real or instance HOME."""
    if (type(value) is not dict or set(value) != {'mode', 'login_home', 'cli'}
            or value['mode'] != 'dedicated_home'
            or not isinstance(value['login_home'], str) or not isinstance(value['cli'], str)):
        raise InstanceError('explicit dedicated agy login required')
    root = safe_absolute(root)
    cli = safe_absolute(value['cli'])
    try:
        metadata = cli.lstat()
    except OSError:
        raise InstanceError('agy CLI is unavailable') from None
    if (safe_absolute(value['login_home']) != root.parent.parent / 'agy-login'
            or not stat.S_ISREG(metadata.st_mode) or not os.access(cli, os.X_OK)):
        raise InstanceError('agy login binding is not the dedicated location')
    return dict(value)


def discover_agy_login(home, root):
    cli = next((Path(p).resolve(strict=True) for p in AGY_CLI_CANDIDATES if Path(p).exists()), None)
    if cli is None:
        raise InstanceError('agy CLI is unavailable')
    return validate_agy_login({'mode': 'dedicated_home', 'login_home': str(safe_absolute(home) / '.adk/dev/agy-login'),
                               'cli': str(cli)}, root)


def check_agy_login_home(value):
    """Admit only a private login home whose agy MCP config is absent or empty."""
    home = safe_absolute(value['login_home'])
    try:
        metadata = home.lstat()
        if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.geteuid() or metadata.st_mode & 0o077:
            raise ValueError()
        path = safe_absolute(home / '.gemini/config/mcp_config.json')
        if not os.path.lexists(path):
            return
        text = read_regular_text(path)  # agy itself creates this file empty on its first run
        servers = json.loads(text).get('mcpServers', {}) if text else {}
        if type(servers) is not dict or servers:
            raise ValueError()
    except (OSError, ValueError, AttributeError, InstanceError):
        raise BlockedAgyLogin('agy login home MCP state is not empty') from None


def provider_home(root: Path, provider: str) -> Path:
    return root / "home/.adk/profiles" / provider / f"test-{provider}"


def _org_home(root, provider, claude_login, agy_login):
    if provider == "claude" and claude_login:
        return claude_login["config_dir"]
    if provider == "antigravity":
        return agy_login["login_home"]
    return str(provider_home(root, provider))


def runtime_mode(provider):
    return "pipe" if provider == "antigravity" else "tui"


def build_org(root: Path, profile: dict, claude_login=None, agy_login=None) -> dict:
    providers = profile_spec(profile)["providers"]
    return {
        "version": 1, "name": "Isolated test instance",
        "provider_auth_profiles": {
            f"test-{provider}": {"provider": provider, "home": _org_home(root, provider, claude_login, agy_login), "env": {}}
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
    providers = profile_spec(profile)["providers"]
    for provider, channel in zip(providers, channels):
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
                "runtime": runtime_mode(provider),
                "peer_agents": False,
                "workspace": str(root / "workspaces" / provider),
                **({"model": AGY_MODEL} if provider == "antigravity" else {}),
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
            provider: {"runtime": runtime_mode(provider), "transcript_homes": []}
            for provider in providers
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
