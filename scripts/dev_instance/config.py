"""Build a standalone runtime config from an already validated test profile."""

from pathlib import Path

HERDR_CONFIG = "[session]\nresume_agents_on_restore = false\n"


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
            "peer_directory": False,
            "peer_keywords": [],
            "preferred_intake_node_labels": [],
            "channels": {provider: {
                "id": channel,
                "provider": provider,
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
