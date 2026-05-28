#!/usr/bin/env python3
"""Dry-run portable env overrides for legacy migrated launchd operators."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
from typing import Any


MIGRATED_AGENTFACTORY_ROUTINES = (
    "migrated-launchd/memento-daily-report.js",
    "migrated-launchd/memento-hygiene.js",
    "migrated-launchd/memory-merge.js",
)


def env_path(name: str) -> str | None:
    value = os.environ.get(name, "").strip()
    return value or None


def as_path(value: str | Path) -> Path:
    return Path(value).expanduser()


def string_path(path: Path) -> str:
    return path.as_posix()


def resolve_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--legacy-home",
        default=env_path("HOME") or env_path("USERPROFILE"),
        help="Existing operator home to preserve. Defaults to HOME/USERPROFILE.",
    )
    parser.add_argument(
        "--root",
        default=env_path("AGENTDESK_ROOT_DIR"),
        help="Resolved AgentDesk release root. Defaults to <legacy-home>/.adk/release.",
    )
    parser.add_argument(
        "--obsidian-vault-root",
        default=env_path("OBSIDIAN_VAULT_ROOT"),
        help="Existing Obsidian vault root to preserve.",
    )
    parser.add_argument(
        "--obsidian-agents-src",
        default=env_path("AGENTDESK_OBSIDIAN_AGENTS_SRC"),
        help="Existing Obsidian agent prompt source to preserve.",
    )
    parser.add_argument(
        "--agentfactory-workdir",
        default=env_path("AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR"),
        help="Existing agentfactory workdir used by migrated memory jobs.",
    )
    parser.add_argument(
        "--operator-workdir",
        default=env_path("AGENTDESK_OPERATOR_WORKDIR"),
        help="Existing default operator workdir for migrated helpers.",
    )
    parser.add_argument(
        "--operator-bin-dir",
        default=env_path("AGENTDESK_OPERATOR_BIN_DIR"),
        help="Existing operator bin directory previously represented by a PATH literal.",
    )
    return parser.parse_args()


def build_plan(args: argparse.Namespace) -> dict[str, Any]:
    if not args.legacy_home:
        raise SystemExit("--legacy-home is required when HOME/USERPROFILE is unavailable")

    legacy_home = as_path(args.legacy_home)
    root = as_path(args.root) if args.root else legacy_home / ".adk" / "release"
    obsidian_vault_root = (
        as_path(args.obsidian_vault_root)
        if args.obsidian_vault_root
        else legacy_home / "ObsidianVault"
    )
    obsidian_remote_root = Path(
        env_path("OBSIDIAN_REMOTE_VAULT_ROOT") or obsidian_vault_root / "RemoteVault"
    )
    obsidian_agents_src = (
        as_path(args.obsidian_agents_src)
        if args.obsidian_agents_src
        else obsidian_remote_root / "adk-config" / "agents"
    )
    agentfactory_workdir = (
        as_path(args.agentfactory_workdir)
        if args.agentfactory_workdir
        else root / "workspaces" / "agentfactory"
    )
    operator_workdir = (
        as_path(args.operator_workdir) if args.operator_workdir else legacy_home
    )
    operator_bin_dir = (
        as_path(args.operator_bin_dir) if args.operator_bin_dir else legacy_home / "bin"
    )

    env_overrides = {
        "AGENTDESK_ROOT_DIR": string_path(root),
        "AGENTDESK_OPERATOR_WORKDIR": string_path(operator_workdir),
        "AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR": string_path(agentfactory_workdir),
        "AGENTDESK_OPERATOR_BIN_DIR": string_path(operator_bin_dir),
        "OBSIDIAN_VAULT_ROOT": string_path(obsidian_vault_root),
        "OBSIDIAN_REMOTE_VAULT_ROOT": string_path(obsidian_remote_root),
        "AGENTDESK_OBSIDIAN_AGENTS_SRC": string_path(obsidian_agents_src),
        "AGENTDESK_OBSIDIAN_SKILL_ROOT": string_path(
            obsidian_remote_root / "99_Skills"
        ),
    }

    return {
        "dry_run": True,
        "legacy_home": string_path(legacy_home),
        "proposed_owner": "launchd.env and routine-owned environment",
        "env_overrides": env_overrides,
        "routine_overrides": {
            script_ref: {
                "workdir_env": "AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR",
                "resolved_workdir": env_overrides[
                    "AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR"
                ],
            }
            for script_ref in MIGRATED_AGENTFACTORY_ROUTINES
        },
        "launchd_env_lines": [
            f"export {key}={json.dumps(value)}"
            for key, value in env_overrides.items()
        ],
    }


def main() -> int:
    plan = build_plan(resolve_args())
    print(json.dumps(plan, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
