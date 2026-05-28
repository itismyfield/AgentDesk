#!/bin/bash
# Shared path resolver for migrated launchd helper scripts.
#
# These scripts are packaged as release helpers, so they must not bake in an
# operator's home directory. Existing operators can preserve old paths by
# exporting the env vars below from launchd.env or their routine environment.

agentdesk_source_portable_resolver() {
  if [[ -z "${HOME:-}" ]]; then
    HOME="$(cd ~ && pwd)"
    export HOME
  fi

  local initial_agentdesk_root_dir="${AGENTDESK_ROOT_DIR-}"
  local initial_workspace_root="${AGENTDESK_WORKSPACE_ROOT-}"
  local initial_entrypoint_dir="${AGENTDESK_MIGRATED_ENTRYPOINT_DIR-}"
  local initial_operator_workdir="${AGENTDESK_OPERATOR_WORKDIR-}"
  local initial_agentfactory_workdir="${AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR-}"
  local initial_obsidian_vault_root="${OBSIDIAN_VAULT_ROOT-}"
  local initial_obsidian_remote_root="${OBSIDIAN_REMOTE_VAULT_ROOT-}"
  local initial_obsidian_agents_src="${AGENTDESK_OBSIDIAN_AGENTS_SRC-}"
  local initial_obsidian_skill_root="${AGENTDESK_OBSIDIAN_SKILL_ROOT-}"
  local initial_agent_feedback_inbox="${AGENTDESK_AGENT_FEEDBACK_INBOX-}"
  local initial_operator_bin_dir="${AGENTDESK_OPERATOR_BIN_DIR-}"
  local initial_path="${PATH-}"

  # shellcheck source=/dev/null
  if [[ "${AGENTDESK_SOURCE_ZPROFILE:-1}" != "0" && -f "$HOME/.zprofile" ]]; then
    source "$HOME/.zprofile"
  fi

  if [[ -n "$initial_agentdesk_root_dir" ]]; then
    export AGENTDESK_ROOT_DIR="$initial_agentdesk_root_dir"
  else
    export AGENTDESK_ROOT_DIR="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"
  fi
  if [[ -n "$initial_workspace_root" ]]; then
    export AGENTDESK_WORKSPACE_ROOT="$initial_workspace_root"
  else
    export AGENTDESK_WORKSPACE_ROOT="${AGENTDESK_WORKSPACE_ROOT:-$AGENTDESK_ROOT_DIR/workspaces}"
  fi
  if [[ -n "$initial_entrypoint_dir" ]]; then
    export AGENTDESK_MIGRATED_ENTRYPOINT_DIR="$initial_entrypoint_dir"
  else
    export AGENTDESK_MIGRATED_ENTRYPOINT_DIR="${AGENTDESK_MIGRATED_ENTRYPOINT_DIR:-$AGENTDESK_ROOT_DIR/scripts/launchd-migrated}"
  fi
  if [[ -n "$initial_operator_workdir" ]]; then
    export AGENTDESK_OPERATOR_WORKDIR="$initial_operator_workdir"
  else
    export AGENTDESK_OPERATOR_WORKDIR="${AGENTDESK_OPERATOR_WORKDIR:-$HOME}"
  fi
  if [[ -n "$initial_agentfactory_workdir" ]]; then
    export AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR="$initial_agentfactory_workdir"
  else
    export AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR="${AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR:-$AGENTDESK_WORKSPACE_ROOT/agentfactory}"
  fi

  local default_obsidian_vault_root="$HOME/ObsidianVault"
  if [[ -d "$AGENTDESK_ROOT_DIR/ObsidianVault" ]]; then
    default_obsidian_vault_root="$AGENTDESK_ROOT_DIR/ObsidianVault"
  fi
  if [[ -n "$initial_obsidian_vault_root" ]]; then
    export OBSIDIAN_VAULT_ROOT="$initial_obsidian_vault_root"
  else
    export OBSIDIAN_VAULT_ROOT="${OBSIDIAN_VAULT_ROOT:-$default_obsidian_vault_root}"
  fi
  if [[ -n "$initial_obsidian_remote_root" ]]; then
    export OBSIDIAN_REMOTE_VAULT_ROOT="$initial_obsidian_remote_root"
  else
    export OBSIDIAN_REMOTE_VAULT_ROOT="${OBSIDIAN_REMOTE_VAULT_ROOT:-$OBSIDIAN_VAULT_ROOT/RemoteVault}"
  fi
  if [[ -n "$initial_obsidian_agents_src" ]]; then
    export AGENTDESK_OBSIDIAN_AGENTS_SRC="$initial_obsidian_agents_src"
  else
    export AGENTDESK_OBSIDIAN_AGENTS_SRC="${AGENTDESK_OBSIDIAN_AGENTS_SRC:-$OBSIDIAN_REMOTE_VAULT_ROOT/adk-config/agents}"
  fi
  if [[ -n "$initial_obsidian_skill_root" ]]; then
    export AGENTDESK_OBSIDIAN_SKILL_ROOT="$initial_obsidian_skill_root"
  else
    export AGENTDESK_OBSIDIAN_SKILL_ROOT="${AGENTDESK_OBSIDIAN_SKILL_ROOT:-$OBSIDIAN_REMOTE_VAULT_ROOT/99_Skills}"
  fi
  if [[ -n "$initial_agent_feedback_inbox" ]]; then
    export AGENTDESK_AGENT_FEEDBACK_INBOX="$initial_agent_feedback_inbox"
  else
    export AGENTDESK_AGENT_FEEDBACK_INBOX="${AGENTDESK_AGENT_FEEDBACK_INBOX:-$OBSIDIAN_REMOTE_VAULT_ROOT/agents/ch-pmd/inbox}"
  fi

  local bin_dir="${initial_operator_bin_dir:-${AGENTDESK_OPERATOR_BIN_DIR:-$HOME/bin}}"
  local current_path="${initial_path:-${PATH:-}}"
  case ":$current_path:" in
    *":$bin_dir:"*) export PATH="$current_path" ;;
    *) export PATH="$bin_dir:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin${current_path:+:$current_path}" ;;
  esac

  export LANG="${LANG:-ko_KR.UTF-8}"
  export LC_ALL="${LC_ALL:-ko_KR.UTF-8}"
}

agentdesk_obsidian_skill_path() {
  printf '%s/%s/SKILL.md' "$AGENTDESK_OBSIDIAN_SKILL_ROOT" "$1"
}

agentdesk_optional_file_or_skip() {
  local label="$1"
  local path="$2"
  if [[ ! -f "$path" ]]; then
    echo "optional connector skipped: $label file not found: $path" >&2
    exit 0
  fi
}

agentdesk_optional_dir_or_skip() {
  local label="$1"
  local path="$2"
  if [[ ! -d "$path" ]]; then
    echo "optional connector skipped: $label directory not found: $path" >&2
    exit 0
  fi
}
