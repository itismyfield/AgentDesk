#!/usr/bin/env bash
set -euo pipefail

CONFIG_PATH="${AGENTDESK_CONFIG:-$HOME/.adk/release/config/agentdesk.yaml}"
ROLE_MAP_PATH="${AGENTDESK_ROLE_MAP:-$HOME/.adk/release/config/role_map.json}"
MODE="dry-run"
AGENT_FILTER=""
PROVIDER_FILTER=""
OLD_CHANNEL_ID=""
NEW_CHANNEL_ID=""
ARCHIVE_CATEGORY_ID="${VOICE_ARCHIVE_CATEGORY_ID:-}"
DISCORD_TOKEN="${DISCORD_BOT_TOKEN:-${AGENTDESK_DISCORD_TOKEN:-}}"
GUILD_ID="${DISCORD_GUILD_ID:-}"
CONFIRM="false"
SKIP_DISCORD="false"
SKIP_DB="false"
SKIP_ARCHIVE="false"
SKIP_HARDCODED="false"
ROLLBACK="false"

usage() {
  cat <<'USAGE'
Usage:
  scripts/voice-channel-migrate.sh [--dry-run]
  scripts/voice-channel-migrate.sh --dry-run --agent project-agentdesk --provider codex
  scripts/voice-channel-migrate.sh --apply --old-channel OLD_ID [--new-channel NEW_ID] --confirm
  scripts/voice-channel-migrate.sh --rollback --dry-run --old-channel OLD_ID --new-channel NEW_ID
  scripts/voice-channel-migrate.sh --rollback --apply --old-channel OLD_ID --new-channel NEW_ID --confirm

Options:
  --config PATH              Runtime agentdesk.yaml. Default: ~/.adk/release/config/agentdesk.yaml
  --role-map PATH            Legacy role_map.json path. Default: ~/.adk/release/config/role_map.json
  --agent ID                 Limit to one agent id.
  --provider PROVIDER        Limit to one provider key such as claude or codex.
  --old-channel ID           Limit to one existing text channel id.
  --new-channel ID           Reuse an already-created voice channel id instead of creating one.
  --archive-category-id ID   Move the old text channel to this category when archiving.
  --discord-token TOKEN      Discord bot token. Defaults to DISCORD_BOT_TOKEN or AGENTDESK_DISCORD_TOKEN.
  --guild-id ID              Discord guild id. Defaults to discord.guild_id from config.
  --skip-discord             Do not call Discord REST; requires --new-channel for --apply.
  --skip-db                  Do not update the agents DB materialization.
  --skip-archive             Do not rename/move/readonly the old text channel.
  --skip-hardcoded           Do not replace old ids in release prompts, memories, and skills.
  --confirm                  Required for --apply.
  -h, --help                 Show this help.

Environment:
  DATABASE_URL               If set, apply DB updates through psql.
  VOICE_ARCHIVE_CATEGORY_ID  Default archive category id.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) MODE="dry-run" ;;
    --apply) MODE="apply" ;;
    --rollback) ROLLBACK="true" ;;
    --config) CONFIG_PATH="$2"; shift ;;
    --role-map) ROLE_MAP_PATH="$2"; shift ;;
    --agent) AGENT_FILTER="$2"; shift ;;
    --provider) PROVIDER_FILTER="$2"; shift ;;
    --old-channel) OLD_CHANNEL_ID="$2"; shift ;;
    --new-channel) NEW_CHANNEL_ID="$2"; shift ;;
    --archive-category-id) ARCHIVE_CATEGORY_ID="$2"; shift ;;
    --discord-token) DISCORD_TOKEN="$2"; shift ;;
    --guild-id) GUILD_ID="$2"; shift ;;
    --skip-discord) SKIP_DISCORD="true" ;;
    --skip-db) SKIP_DB="true" ;;
    --skip-archive) SKIP_ARCHIVE="true" ;;
    --skip-hardcoded) SKIP_HARDCODED="true" ;;
    --confirm) CONFIRM="true" ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 64 ;;
  esac
  shift
done

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 69
  fi
}

require_cmd ruby
require_cmd jq

if [[ ! -f "$CONFIG_PATH" ]]; then
  echo "config not found: $CONFIG_PATH" >&2
  exit 66
fi

if [[ "$MODE" == "apply" && "$CONFIRM" != "true" ]]; then
  echo "--apply requires --confirm" >&2
  exit 64
fi

if [[ "$MODE" == "apply" && -z "$OLD_CHANNEL_ID" ]]; then
  echo "--apply requires --old-channel so migration happens one channel at a time" >&2
  exit 64
fi

if [[ "$ROLLBACK" == "true" && ( -z "$OLD_CHANNEL_ID" || -z "$NEW_CHANNEL_ID" ) ]]; then
  echo "--rollback requires --old-channel and --new-channel" >&2
  exit 64
fi

if [[ -z "$GUILD_ID" ]]; then
  GUILD_ID="$(ruby -ryaml -e 'cfg=YAML.load_file(ARGV[0]); puts cfg.dig("discord", "guild_id").to_s' "$CONFIG_PATH")"
fi

targets_tsv() {
  ruby -ryaml -e '
    config_path, agent_filter, provider_filter, old_filter = ARGV
    cfg = YAML.load_file(config_path)
    target_re = /\A(project-agentdesk|adk-dashboard|project-agentmanager|project-skillmanager|project-scheduler|adk-deadlock-manager|ch-|personal-|family-|chef-goat)/
    exclude_re = /\A(project-newsbot|token-manager|adk-cdx)\z/
    explicit = !agent_filter.empty? || !provider_filter.empty? || !old_filter.empty?
    (cfg["agents"] || []).each do |agent|
      agent_id = agent["id"].to_s
      next if !explicit && (agent_id !~ target_re || agent_id =~ exclude_re)
      next if !agent_filter.empty? && agent_id != agent_filter
      channels = agent["channels"] || {}
      channels.each do |provider, channel|
        next if !provider_filter.empty? && provider.to_s != provider_filter
        next unless channel.is_a?(Hash)
        channel_id = channel["id"].to_s
        next if channel_id.empty?
        next if !old_filter.empty? && channel_id != old_filter
        name = channel["name"].to_s
        workspace = channel["workspace"].to_s
        db_columns =
          case provider.to_s
          when "claude" then "discord_channel_id,discord_channel_cc"
          when "codex" then "discord_channel_alt,discord_channel_cdx"
          else "discord_channel_id"
          end
        puts [agent_id, provider, channel_id, name, workspace, db_columns].join("\t")
      end
    end
  ' "$CONFIG_PATH" "$AGENT_FILTER" "$PROVIDER_FILTER" "$TARGET_CHANNEL_FILTER"
}

TARGET_CHANNEL_FILTER="$OLD_CHANNEL_ID"
if [[ "$ROLLBACK" == "true" ]]; then
  TARGET_CHANNEL_FILTER="$NEW_CHANNEL_ID"
fi

selected_targets="$(targets_tsv)"

if [[ -z "$selected_targets" ]]; then
  echo "no matching agent channel bindings found" >&2
  exit 65
fi

unique_old_channel_count="$(printf '%s\n' "$selected_targets" | awk -F '\t' '{print $3}' | sort -u | wc -l | tr -d ' ')"
if [[ "$MODE" == "apply" && "$ROLLBACK" != "true" && "$unique_old_channel_count" != "1" ]]; then
  echo "--apply must resolve to exactly one old channel; matched $unique_old_channel_count" >&2
  exit 64
fi

print_plan() {
  echo "mode: $MODE"
  if [[ "$ROLLBACK" == "true" ]]; then
    echo "operation: rollback new voice channel -> old text channel"
  else
    echo "operation: text channel -> guild voice channel"
  fi
  echo "config: $CONFIG_PATH"
  echo
  printf '%-24s %-8s %-20s %-28s %s\n' "agent" "provider" "old_channel_id" "channel_name" "db_columns"
  printf '%-24s %-8s %-20s %-28s %s\n' "-----" "--------" "--------------" "------------" "----------"
  printf '%s\n' "$selected_targets" | while IFS=$'\t' read -r agent provider channel_id name _workspace db_columns; do
    printf '%-24s %-8s %-20s %-28s %s\n' "$agent" "$provider" "$channel_id" "${name:-"(unnamed)"}" "$db_columns"
  done
  echo
  if [[ "$ROLLBACK" == "true" ]]; then
    echo "dry-run actions:"
    echo "- replace config/DB occurrences of $NEW_CHANNEL_ID with $OLD_CHANNEL_ID"
    echo "- leave Discord channels untouched unless you manually restore archive permissions/category"
  else
    echo "dry-run actions:"
    if [[ -n "$NEW_CHANNEL_ID" ]]; then
      echo "- reuse existing voice channel: $NEW_CHANNEL_ID"
    else
      echo "- create one GUILD_VOICE channel with the old channel name and parent category"
    fi
    echo "- replace matching agentdesk.yaml channel ids"
    echo "- replace legacy role_map.json string occurrences when the file exists"
    echo "- replace old channel ids in release agent prompts, memories, and skills"
    echo "- update agents.discord_channel_* DB materialization"
    if [[ "$SKIP_ARCHIVE" == "true" ]]; then
      echo "- skip old text channel archive"
    else
      echo "- rename old text channel to <name>-archive, deny @everyone SEND_MESSAGES, and move to archive category when provided"
    fi
  fi
}

discord_api() {
  local method="$1"
  local path="$2"
  local data="${3:-}"
  if [[ -z "$DISCORD_TOKEN" ]]; then
    echo "Discord token is required for REST operation" >&2
    exit 67
  fi
  if [[ -n "$data" ]]; then
    curl -fsS -X "$method" "https://discord.com/api/v10$path" \
      -H "Authorization: Bot $DISCORD_TOKEN" \
      -H "Content-Type: application/json" \
      -d "$data"
  else
    curl -fsS -X "$method" "https://discord.com/api/v10$path" \
      -H "Authorization: Bot $DISCORD_TOKEN"
  fi
}

create_voice_channel() {
  local old_id="$1"
  local old_json
  old_json="$(discord_api GET "/channels/$old_id")"
  local parent_id name overwrites payload
  parent_id="$(jq -r '.parent_id // ""' <<<"$old_json")"
  name="$(jq -r '.name' <<<"$old_json")"
  overwrites="$(jq -c '.permission_overwrites // []' <<<"$old_json")"
  payload="$(jq -nc \
    --arg name "$name" \
    --arg parent "$parent_id" \
    --argjson overwrites "$overwrites" \
    '{name:$name,type:2,permission_overwrites:$overwrites} + (if $parent == "" then {} else {parent_id:$parent} end)')"
  discord_api POST "/guilds/$GUILD_ID/channels" "$payload" | jq -r '.id'
}

archive_text_channel() {
  local old_id="$1"
  local old_json payload name archived_name parent_fragment
  old_json="$(discord_api GET "/channels/$old_id")"
  name="$(jq -r '.name' <<<"$old_json")"
  archived_name="${name%-archive}-archive"
  parent_fragment="{}"
  if [[ -n "$ARCHIVE_CATEGORY_ID" ]]; then
    parent_fragment="$(jq -nc --arg parent "$ARCHIVE_CATEGORY_ID" '{parent_id:$parent}')"
  fi
  payload="$(jq -nc \
    --arg name "$archived_name" \
    --arg guild "$GUILD_ID" \
    --argjson channel "$old_json" \
    --argjson parent "$parent_fragment" '
      def add_send_deny:
        (tonumber? // 0) as $n
        | if (((($n / 2048) | floor) % 2) == 1) then $n else ($n + 2048) end
        | tostring;
      def readonly_overwrites($guild):
        ($channel.permission_overwrites // []) as $ows
        | if any($ows[]?; .id == $guild and .type == 0) then
            $ows | map(if .id == $guild and .type == 0 then .deny = (.deny | add_send_deny) else . end)
          else
            $ows + [{id:$guild,type:0,allow:"0",deny:"2048"}]
          end;
      {name:$name, permission_overwrites: readonly_overwrites($guild)} + $parent
    ')"
  discord_api PATCH "/channels/$old_id" "$payload" >/dev/null
}

replace_config_ids() {
  local from_id="$1"
  local to_id="$2"
  ruby -ryaml -e '
    path, from_id, to_id = ARGV
    cfg = YAML.load_file(path)
    count = 0
    (cfg["agents"] || []).each do |agent|
      (agent["channels"] || {}).each_value do |channel|
        next unless channel.is_a?(Hash)
        if channel["id"].to_s == from_id
          channel["id"] = to_id
          count += 1
        end
      end
    end
    raise "no channel ids matched #{from_id}" if count == 0
    backup = "#{path}.bak.#{Time.now.utc.strftime("%Y%m%d%H%M%S")}"
    File.write(backup, File.read(path))
    File.write(path, YAML.dump(cfg))
    warn "updated #{count} YAML channel id(s); backup: #{backup}"
  ' "$CONFIG_PATH" "$from_id" "$to_id"
}

replace_role_map_ids() {
  local from_id="$1"
  local to_id="$2"
  if [[ ! -f "$ROLE_MAP_PATH" ]]; then
    return 0
  fi
  ruby -rjson -e '
    def replace(value, from_id, to_id)
      case value
      when Hash
        value.transform_values { |v| replace(v, from_id, to_id) }
      when Array
        value.map { |v| replace(v, from_id, to_id) }
      when String
        value == from_id ? to_id : value
      else
        value
      end
    end
    path, from_id, to_id = ARGV
    raw = File.read(path)
    updated = replace(JSON.parse(raw), from_id, to_id)
    if JSON.generate(updated) != JSON.generate(JSON.parse(raw))
      backup = "#{path}.bak.#{Time.now.utc.strftime("%Y%m%d%H%M%S")}"
      File.write(backup, raw)
      File.write(path, JSON.pretty_generate(updated) + "\n")
      warn "updated legacy role_map; backup: #{backup}"
    end
  ' "$ROLE_MAP_PATH" "$from_id" "$to_id"
}

hardcoded_roots() {
  printf '%s\n' \
    "$HOME/.adk/release/config/agents" \
    "$HOME/.adk/release/memories" \
    "$HOME/.adk/release/skills"
}

replace_hardcoded_ids() {
  local from_id="$1"
  local to_id="$2"
  if [[ "$SKIP_HARDCODED" == "true" ]]; then
    echo "skip hardcoded id replacement"
    return 0
  fi
  local roots=()
  mapfile -t roots < <(hardcoded_roots)
  ruby -e '
    from_id, to_id, *roots = ARGV
    timestamp = Time.now.utc.strftime("%Y%m%d%H%M%S")
    changed = 0
    roots.each do |root|
      next unless File.exist?(root)
      if File.file?(root)
        files = [root]
      else
        files = Dir.glob(File.join(root, "**", "*"), File::FNM_DOTMATCH).select { |path| File.file?(path) }
      end
      files.each do |path|
        next if path.include?("/target/")
        data = File.binread(path)
        next if data.include?("\0")
        next unless data.include?(from_id)
        backup = "#{path}.bak.#{timestamp}"
        File.binwrite(backup, data)
        File.binwrite(path, data.gsub(from_id, to_id))
        warn "updated hardcoded id: #{path}; backup: #{backup}"
        changed += 1
      rescue Errno::EACCES, Errno::ENOENT, Encoding::UndefinedConversionError
        next
      end
    end
    warn "updated #{changed} hardcoded id file(s)"
  ' "$from_id" "$to_id" "${roots[@]}"
}

update_db_ids() {
  local from_id="$1"
  local to_id="$2"
  if [[ "$SKIP_DB" == "true" ]]; then
    echo "skip DB update"
    return 0
  fi
  local sql
  sql="UPDATE agents SET discord_channel_id = CASE WHEN discord_channel_id = '$from_id' THEN '$to_id' ELSE discord_channel_id END, discord_channel_alt = CASE WHEN discord_channel_alt = '$from_id' THEN '$to_id' ELSE discord_channel_alt END, discord_channel_cc = CASE WHEN discord_channel_cc = '$from_id' THEN '$to_id' ELSE discord_channel_cc END, discord_channel_cdx = CASE WHEN discord_channel_cdx = '$from_id' THEN '$to_id' ELSE discord_channel_cdx END WHERE discord_channel_id = '$from_id' OR discord_channel_alt = '$from_id' OR discord_channel_cc = '$from_id' OR discord_channel_cdx = '$from_id';"
  if [[ -n "${DATABASE_URL:-}" ]]; then
    require_cmd psql
    psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -c "$sql"
    return 0
  fi
  local db_enabled db_host db_port db_name db_user
  db_enabled="$(ruby -ryaml -e 'cfg=YAML.load_file(ARGV[0]); puts cfg.dig("database", "enabled").to_s' "$CONFIG_PATH")"
  if [[ "$db_enabled" == "true" ]] && command -v psql >/dev/null 2>&1; then
    db_host="$(ruby -ryaml -e 'cfg=YAML.load_file(ARGV[0]); puts(cfg.dig("database", "host") || "127.0.0.1")' "$CONFIG_PATH")"
    db_port="$(ruby -ryaml -e 'cfg=YAML.load_file(ARGV[0]); puts(cfg.dig("database", "port") || "5432")' "$CONFIG_PATH")"
    db_name="$(ruby -ryaml -e 'cfg=YAML.load_file(ARGV[0]); puts(cfg.dig("database", "dbname") || "agentdesk")' "$CONFIG_PATH")"
    db_user="$(ruby -ryaml -e 'cfg=YAML.load_file(ARGV[0]); puts(cfg.dig("database", "user") || ENV["USER"])' "$CONFIG_PATH")"
    psql -h "$db_host" -p "$db_port" -U "$db_user" -d "$db_name" -v ON_ERROR_STOP=1 -c "$sql"
    return 0
  fi
  echo "DB update SQL; run manually or set DATABASE_URL:" >&2
  echo "$sql" >&2
}

scan_hardcoded_ids() {
  local channel_id="$1"
  local roots=()
  mapfile -t roots < <(hardcoded_roots)
  echo "hardcoded-id scan for $channel_id"
  local found="false"
  for root in "${roots[@]}"; do
    [[ -e "$root" ]] || continue
    local matches
    matches="$(grep -RIl --exclude-dir target --exclude '*.sqlite' --exclude '*.db' "$channel_id" "$root" 2>/dev/null | head -n 20 || true)"
    if [[ -n "$matches" ]]; then
      printf '%s\n' "$matches"
      found="true"
    fi
  done
  if [[ "$found" == "false" ]]; then
    echo "(none)"
  fi
}

if [[ "$MODE" == "dry-run" ]]; then
  print_plan
  echo
  printf '%s\n' "$selected_targets" | awk -F '\t' '{print $3}' | sort -u | while read -r channel_id; do
    [[ -n "$channel_id" ]] && scan_hardcoded_ids "$channel_id"
  done
  exit 0
fi

from_id="$OLD_CHANNEL_ID"
to_id="$NEW_CHANNEL_ID"
if [[ "$ROLLBACK" == "true" ]]; then
  from_id="$NEW_CHANNEL_ID"
  to_id="$OLD_CHANNEL_ID"
else
  if [[ "$SKIP_DISCORD" != "true" && -z "$to_id" ]]; then
    to_id="$(create_voice_channel "$OLD_CHANNEL_ID")"
    echo "created voice channel: $to_id"
  fi
  if [[ -z "$to_id" ]]; then
    echo "--apply with --skip-discord requires --new-channel" >&2
    exit 64
  fi
fi

replace_config_ids "$from_id" "$to_id"
replace_role_map_ids "$from_id" "$to_id"
replace_hardcoded_ids "$from_id" "$to_id"
update_db_ids "$from_id" "$to_id"

if [[ "$ROLLBACK" != "true" && "$SKIP_DISCORD" != "true" && "$SKIP_ARCHIVE" != "true" ]]; then
  archive_text_channel "$OLD_CHANNEL_ID"
  echo "archived old text channel: $OLD_CHANNEL_ID"
fi

echo "completed: $from_id -> $to_id"
