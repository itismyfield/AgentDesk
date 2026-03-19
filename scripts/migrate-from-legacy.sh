#!/bin/bash
# ──────────────────────────────────────────────────────────────────────────────
# migrate-from-legacy.sh — Migrate RCC + PCD data into AgentDesk
#
# Reads:
#   ~/.remotecc/config/org.yaml          (agent definitions)
#   ~/.remotecc/config/bot_settings.json (discord bot tokens)
#   PCD .env                             (announce/notify tokens)
#   PCD SQLite DB                        (all operational data)
#
# Produces:
#   agentdesk.yaml                       (unified config)
#   AgentDesk SQLite DB                  (migrated data)
#   ~/.agentdesk/role-context/           (copied runtime files)
#   ~/.agentdesk/prompts/
#   ~/.agentdesk/skills/
#
# Usage:
#   ./scripts/migrate-from-legacy.sh [--dry-run] [--skip-config] [--skip-db] [--skip-files]
#
# Idempotent: safe to run multiple times.
# ──────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Defaults ──────────────────────────────────────────────────────────────────
RCC_CONFIG_DIR="${REMOTECC_CONFIG_DIR:-$HOME/.remotecc/config}"
RCC_ORG_YAML="$RCC_CONFIG_DIR/org.yaml"
RCC_BOT_JSON="$RCC_CONFIG_DIR/bot_settings.json"

PCD_DB="${PCD_DB_PATH:-$HOME/.local/state/pixel-claw-dashboard/prod/pixel-claw-dashboard.sqlite}"
PCD_ENV="${PCD_ENV_PATH:-$HOME/PixelClawDashboard/.env}"

AD_HOME="${AGENTDESK_HOME:-$HOME/.agentdesk}"
AD_DB="$AD_HOME/agentdesk.sqlite"
AD_YAML="$PROJECT_DIR/agentdesk.yaml"

DRY_RUN=false
SKIP_CONFIG=false
SKIP_DB=false
SKIP_FILES=false

# ── Parse Args ────────────────────────────────────────────────────────────────
for arg in "$@"; do
  case "$arg" in
    --dry-run)    DRY_RUN=true ;;
    --skip-config) SKIP_CONFIG=true ;;
    --skip-db)    SKIP_DB=true ;;
    --skip-files) SKIP_FILES=true ;;
    -h|--help)
      echo "Usage: $0 [--dry-run] [--skip-config] [--skip-db] [--skip-files]"
      exit 0
      ;;
    *) echo "Unknown option: $arg"; exit 1 ;;
  esac
done

# ── Helpers ───────────────────────────────────────────────────────────────────
info()  { printf "\033[1;34m[INFO]\033[0m  %s\n" "$*"; }
warn()  { printf "\033[1;33m[WARN]\033[0m  %s\n" "$*"; }
ok()    { printf "\033[1;32m[OK]\033[0m    %s\n" "$*"; }
fail()  { printf "\033[1;31m[FAIL]\033[0m  %s\n" "$*"; exit 1; }

row_count() {
  local db="$1" table="$2"
  sqlite3 "$db" "SELECT COUNT(*) FROM $table;" 2>/dev/null || echo "0"
}

# ── Preflight ─────────────────────────────────────────────────────────────────
info "AgentDesk Migration from Legacy RCC + PCD"
info "  RCC org.yaml:   $RCC_ORG_YAML"
info "  RCC bot_json:   $RCC_BOT_JSON"
info "  PCD .env:       $PCD_ENV"
info "  PCD database:   $PCD_DB"
info "  AgentDesk home: $AD_HOME"
info "  AgentDesk DB:   $AD_DB"
info "  AgentDesk YAML: $AD_YAML"
echo ""

if ! command -v sqlite3 &>/dev/null; then
  fail "sqlite3 is required but not found"
fi

if ! command -v python3 &>/dev/null; then
  fail "python3 is required for YAML/JSON parsing"
fi

mkdir -p "$AD_HOME"

# ══════════════════════════════════════════════════════════════════════════════
# Phase 1: Generate agentdesk.yaml from legacy configs
# ══════════════════════════════════════════════════════════════════════════════
if [ "$SKIP_CONFIG" = false ]; then
  info "Phase 1: Generating agentdesk.yaml..."

  if [ ! -f "$RCC_ORG_YAML" ]; then
    warn "RCC org.yaml not found at $RCC_ORG_YAML — skipping config generation"
    SKIP_CONFIG=true
  fi
fi

if [ "$SKIP_CONFIG" = false ]; then
  # If the target already exists and is non-trivial, ask before overwriting
  if [ -f "$AD_YAML" ] && [ "$(wc -l < "$AD_YAML")" -gt 5 ]; then
    if [ "$DRY_RUN" = false ]; then
      printf "[CONFIRM] agentdesk.yaml already exists (%s lines). Overwrite? [y/N] " "$(wc -l < "$AD_YAML")"
      read -r REPLY
      if [[ ! "$REPLY" =~ ^[Yy]$ ]]; then
        warn "Skipping config generation"
        SKIP_CONFIG=true
      fi
    else
      info "(dry-run) Would prompt to overwrite agentdesk.yaml"
    fi
  fi
fi

if [ "$SKIP_CONFIG" = false ]; then
  # Read PCD .env tokens
  ANNOUNCE_TOKEN=""
  NOTIFY_TOKEN=""
  GUILD_ID=""
  if [ -f "$PCD_ENV" ]; then
    ANNOUNCE_TOKEN=$(grep -E '^DISCORD_ANNOUNCE_BOT_TOKEN=' "$PCD_ENV" | head -1 | cut -d= -f2- || true)
    NOTIFY_TOKEN=$(grep -E '^DISCORD_NOTIFY_BOT_TOKEN=' "$PCD_ENV" | head -1 | cut -d= -f2- || true)
    GUILD_ID=$(grep -E '^DISCORD_GUILD_ID=' "$PCD_ENV" | head -1 | cut -d= -f2- || true)
  fi

  if [ "$DRY_RUN" = true ]; then
    info "(dry-run) Would generate agentdesk.yaml from org.yaml + bot_settings.json + PCD .env"
  else
    python3 - "$RCC_ORG_YAML" "$RCC_BOT_JSON" "$AD_YAML" "$ANNOUNCE_TOKEN" "$NOTIFY_TOKEN" "$GUILD_ID" <<'PYEOF'
import sys, json, os

org_path = sys.argv[1]
bot_path = sys.argv[2]
out_path = sys.argv[3]
announce_token = sys.argv[4] if len(sys.argv) > 4 else ""
notify_token = sys.argv[5] if len(sys.argv) > 5 else ""
guild_id = sys.argv[6] if len(sys.argv) > 6 else ""

# --- Parse org.yaml (simple line-by-line, no pyyaml dependency) ---
agents = {}
channels = {}
current_agent_id = None
in_agents = False
in_channels_by_id = False
current_channel_id = None

with open(org_path, "r") as f:
    for line in f:
        stripped = line.rstrip()

        # Detect sections
        if stripped == "agents:":
            in_agents = True
            in_channels_by_id = False
            continue
        if stripped.startswith("channels:"):
            in_agents = False
            continue
        if stripped.strip() == "by_id:":
            in_channels_by_id = True
            continue
        if stripped.startswith("meeting:") or stripped.startswith("suffix_map:"):
            in_agents = False
            in_channels_by_id = False
            continue

        if in_agents:
            # Agent id line: "  agent-id:"
            if line.startswith("  ") and not line.startswith("    ") and stripped.endswith(":") and not stripped.startswith("#"):
                current_agent_id = stripped.strip().rstrip(":")
                agents[current_agent_id] = {}
                continue
            if current_agent_id and line.startswith("    "):
                key_val = stripped.strip()
                if ":" in key_val:
                    key, val = key_val.split(":", 1)
                    key = key.strip()
                    val = val.strip().strip('"').strip("'")
                    agents[current_agent_id][key] = val

        if in_channels_by_id:
            # Channel id line: '    "1234":'
            if line.startswith("    ") and not line.startswith("      ") and ":" in stripped:
                cid = stripped.strip().rstrip(":").strip('"').strip("'")
                if cid.isdigit():
                    current_channel_id = cid
                    channels[current_channel_id] = {}
                    continue
            if current_channel_id and line.startswith("      "):
                key_val = stripped.strip()
                if ":" in key_val:
                    key, val = key_val.split(":", 1)
                    key = key.strip()
                    val = val.strip().strip('"').strip("'")
                    channels[current_channel_id][key] = val

# --- Build agent entries ---
lines = []
lines.append("server:")
lines.append("  port: 8791")
lines.append('  host: "0.0.0.0"')
lines.append("")
lines.append("discord:")
lines.append("  bots:")
lines.append("    command:")
if announce_token:
    lines.append(f'      token: "{announce_token}"')
else:
    lines.append('      token: "YOUR_COMMAND_BOT_TOKEN"')
lines.append("    notify:")
if notify_token:
    lines.append(f'      token: "{notify_token}"')
else:
    lines.append('      token: "YOUR_NOTIFY_BOT_TOKEN"')
if guild_id:
    lines.append(f'  guild_id: "{guild_id}"')
else:
    lines.append('  guild_id: "YOUR_GUILD_ID"')
lines.append("")

lines.append("agents:")
for aid, props in agents.items():
    name = props.get("display_name", aid)
    # Extract short name and korean name from display_name like "TD (테크니컬 디렉터)"
    short_name = name.split("(")[0].strip() if "(" in name else name.split(" ")[0]
    name_ko = ""
    if "(" in name and ")" in name:
        name_ko = name.split("(")[1].split(")")[0].strip()
    provider = props.get("provider", "claude")

    # Find channels for this agent
    claude_ch = ""
    codex_ch = ""
    for cid, cprops in channels.items():
        if cprops.get("agent") == aid:
            cprov = cprops.get("provider", "claude")
            if cprov == "claude" and not claude_ch:
                claude_ch = cid
            elif cprov == "codex" and not codex_ch:
                codex_ch = cid

    lines.append(f"  - id: {aid}")
    lines.append(f'    name: "{short_name}"')
    if name_ko:
        lines.append(f'    name_ko: "{name_ko}"')
    lines.append(f"    provider: {provider}")
    if claude_ch or codex_ch:
        lines.append("    channels:")
        if claude_ch:
            lines.append(f'      claude: "{claude_ch}"')
        if codex_ch:
            lines.append(f'      codex: "{codex_ch}"')
    lines.append("")

lines.append("policies:")
lines.append('  dir: "./policies"')
lines.append("  hot_reload: true")
lines.append("")
lines.append("data:")
lines.append('  dir: "~/.agentdesk"')
lines.append("")
lines.append("kanban:")
lines.append("  timeout_requested_minutes: 45")
lines.append("  timeout_in_progress_minutes: 100")
lines.append("  max_review_rounds: 3")

with open(out_path, "w") as f:
    f.write("\n".join(lines) + "\n")

print(f"  Generated {out_path} with {len(agents)} agents")
PYEOF
    ok "agentdesk.yaml generated"
  fi
fi

# ══════════════════════════════════════════════════════════════════════════════
# Phase 2: Migrate PCD SQLite data
# ══════════════════════════════════════════════════════════════════════════════
if [ "$SKIP_DB" = false ]; then
  info "Phase 2: Migrating PCD SQLite data..."

  if [ ! -f "$PCD_DB" ]; then
    warn "PCD database not found at $PCD_DB — skipping DB migration"
    SKIP_DB=true
  fi
fi

if [ "$SKIP_DB" = false ]; then
  # Initialize AgentDesk schema if DB doesn't exist
  if [ ! -f "$AD_DB" ]; then
    info "Creating AgentDesk database with initial schema..."
    sqlite3 "$AD_DB" < "$PROJECT_DIR/migrations/001_initial.sql"
    ok "Database created"
  fi

  # ── Pre-migration row counts (source) ──
  info "Source DB row counts:"
  SRC_TABLES=(
    agents kanban_cards task_dispatches dispatched_sessions
    round_table_meetings round_table_entries messages
    offices departments kanban_repo_sources
    pipeline_stages skill_usage_events kanban_reviews
  )
  for t in "${SRC_TABLES[@]}"; do
    c=$(row_count "$PCD_DB" "$t")
    printf "  %-30s %s\n" "$t" "$c"
  done

  if [ "$DRY_RUN" = true ]; then
    info "(dry-run) Would migrate ${#SRC_TABLES[@]} tables from PCD to AgentDesk"
  else
    # ── Migrate agents ──
    info "Migrating agents..."
    sqlite3 "$AD_DB" <<EOSQL
ATTACH DATABASE '$PCD_DB' AS pcd;

-- agents: PCD has more columns, map to AgentDesk schema
INSERT OR REPLACE INTO agents (id, name, name_ko, department, provider, discord_channel_id, discord_channel_alt, avatar_emoji, status, xp, skills, created_at, updated_at)
SELECT
  a.id,
  a.name,
  a.name_ko,
  COALESCE(d.name, a.department_id),
  a.cli_provider,
  a.discord_channel_id,
  a.discord_channel_id_alt,
  a.avatar_emoji,
  CASE a.status
    WHEN 'idle' THEN 'idle'
    WHEN 'working' THEN 'idle'
    WHEN 'break' THEN 'idle'
    WHEN 'offline' THEN 'idle'
    ELSE 'idle'
  END,
  a.stats_xp,
  NULL,
  datetime(a.created_at / 1000, 'unixepoch'),
  datetime(a.created_at / 1000, 'unixepoch')
FROM pcd.agents a
LEFT JOIN pcd.departments d ON a.department_id = d.id;

DETACH DATABASE pcd;
EOSQL

    # ── Migrate kanban_cards ──
    info "Migrating kanban_cards..."
    sqlite3 "$AD_DB" <<EOSQL
ATTACH DATABASE '$PCD_DB' AS pcd;

INSERT OR REPLACE INTO kanban_cards (id, repo_id, title, status, priority, assigned_agent_id, github_issue_url, github_issue_number, latest_dispatch_id, review_round, metadata, created_at, updated_at)
SELECT
  k.id,
  k.github_repo,
  k.title,
  CASE k.status
    WHEN 'qa_pending' THEN 'review'
    WHEN 'qa_in_progress' THEN 'review'
    WHEN 'qa_failed' THEN 'review'
    WHEN 'pending_decision' THEN 'blocked'
    ELSE k.status
  END,
  k.priority,
  k.assignee_agent_id,
  k.github_issue_url,
  k.github_issue_number,
  k.latest_dispatch_id,
  0,
  k.metadata_json,
  datetime(k.created_at / 1000, 'unixepoch'),
  datetime(k.updated_at / 1000, 'unixepoch')
FROM pcd.kanban_cards k;

DETACH DATABASE pcd;
EOSQL

    # ── Migrate task_dispatches ──
    info "Migrating task_dispatches..."
    sqlite3 "$AD_DB" <<EOSQL
ATTACH DATABASE '$PCD_DB' AS pcd;

INSERT OR REPLACE INTO task_dispatches (id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result, parent_dispatch_id, chain_depth, created_at, updated_at)
SELECT
  td.id,
  -- Find kanban_card_id: PCD doesn't have a direct FK on dispatches,
  -- but kanban_cards.latest_dispatch_id links them
  (SELECT k.id FROM pcd.kanban_cards k WHERE k.latest_dispatch_id = td.id LIMIT 1),
  td.from_agent_id,
  td.to_agent_id,
  td.dispatch_type,
  td.status,
  td.title,
  td.context_file,
  td.result_summary,
  td.parent_dispatch_id,
  td.chain_depth,
  datetime(td.created_at / 1000, 'unixepoch'),
  COALESCE(datetime(td.completed_at / 1000, 'unixepoch'), datetime(td.created_at / 1000, 'unixepoch'))
FROM pcd.task_dispatches td;

DETACH DATABASE pcd;
EOSQL

    # ── Migrate sessions (dispatched_sessions → sessions) ──
    info "Migrating sessions..."
    sqlite3 "$AD_DB" <<EOSQL
ATTACH DATABASE '$PCD_DB' AS pcd;

INSERT OR REPLACE INTO sessions (session_key, agent_id, provider, status, active_dispatch_id, model, session_info, tokens, cwd, last_heartbeat, created_at)
SELECT
  ds.session_key,
  ds.linked_agent_id,
  ds.provider,
  ds.status,
  ds.active_dispatch_id,
  ds.model,
  ds.session_info,
  ds.tokens,
  ds.cwd,
  CASE WHEN ds.last_seen_at IS NOT NULL THEN datetime(ds.last_seen_at / 1000, 'unixepoch') ELSE NULL END,
  datetime(ds.connected_at / 1000, 'unixepoch')
FROM pcd.dispatched_sessions ds;

DETACH DATABASE pcd;
EOSQL

    # ── Migrate meetings (round_table_meetings → meetings) ──
    info "Migrating meetings..."
    sqlite3 "$AD_DB" <<EOSQL
ATTACH DATABASE '$PCD_DB' AS pcd;

INSERT OR REPLACE INTO meetings (id, channel_id, title, status, effective_rounds, started_at, completed_at, summary)
SELECT
  m.id,
  NULL,
  m.agenda,
  m.status,
  m.total_rounds,
  datetime(m.started_at / 1000, 'unixepoch'),
  CASE WHEN m.completed_at IS NOT NULL THEN datetime(m.completed_at / 1000, 'unixepoch') ELSE NULL END,
  m.summary
FROM pcd.round_table_meetings m;

DETACH DATABASE pcd;
EOSQL

    # ── Migrate meeting_transcripts (round_table_entries → meeting_transcripts) ──
    info "Migrating meeting_transcripts..."
    sqlite3 "$AD_DB" <<EOSQL
ATTACH DATABASE '$PCD_DB' AS pcd;

INSERT OR REPLACE INTO meeting_transcripts (meeting_id, seq, round, speaker_agent_id, speaker_name, content, is_summary)
SELECT
  e.meeting_id,
  e.seq,
  e.round,
  e.speaker_role_id,
  e.speaker_name,
  e.content,
  e.is_summary
FROM pcd.round_table_entries e;

DETACH DATABASE pcd;
EOSQL

    # ── Migrate messages ──
    info "Migrating messages..."
    sqlite3 "$AD_DB" <<EOSQL
ATTACH DATABASE '$PCD_DB' AS pcd;

INSERT OR REPLACE INTO messages (sender_type, sender_id, receiver_type, receiver_id, content, message_type, created_at)
SELECT
  m.sender_type,
  m.sender_id,
  m.receiver_type,
  m.receiver_id,
  m.content,
  m.message_type,
  datetime(m.created_at / 1000, 'unixepoch')
FROM pcd.messages m;

DETACH DATABASE pcd;
EOSQL

    # ── Migrate offices ──
    info "Migrating offices..."
    sqlite3 "$AD_DB" <<EOSQL
ATTACH DATABASE '$PCD_DB' AS pcd;

INSERT OR REPLACE INTO offices (id, name, layout)
SELECT
  o.id,
  o.name,
  NULL
FROM pcd.offices o;

DETACH DATABASE pcd;
EOSQL

    # ── Migrate departments ──
    info "Migrating departments..."
    sqlite3 "$AD_DB" <<EOSQL
ATTACH DATABASE '$PCD_DB' AS pcd;

INSERT OR REPLACE INTO departments (id, name, office_id)
SELECT
  d.id,
  d.name,
  d.office_id
FROM pcd.departments d;

DETACH DATABASE pcd;
EOSQL

    # ── Migrate github_repos (from kanban_repo_sources) ──
    info "Migrating github_repos..."
    sqlite3 "$AD_DB" <<EOSQL
ATTACH DATABASE '$PCD_DB' AS pcd;

INSERT OR REPLACE INTO github_repos (id, display_name, sync_enabled, last_synced_at)
SELECT
  rs.id,
  rs.repo,
  1,
  datetime(rs.created_at / 1000, 'unixepoch')
FROM pcd.kanban_repo_sources rs;

DETACH DATABASE pcd;
EOSQL

    # ── Migrate pipeline_stages ──
    info "Migrating pipeline_stages..."
    sqlite3 "$AD_DB" <<EOSQL
ATTACH DATABASE '$PCD_DB' AS pcd;

INSERT OR REPLACE INTO pipeline_stages (id, repo_id, stage_name, stage_order, trigger_after, entry_skill, timeout_minutes, on_failure, skip_condition)
SELECT
  ps.id,
  ps.repo,
  ps.stage_name,
  ps.stage_order,
  ps.trigger_after,
  ps.entry_skill,
  ps.timeout_minutes,
  ps.on_failure,
  ps.skip_condition
FROM pcd.pipeline_stages ps;

DETACH DATABASE pcd;
EOSQL

    # ── Migrate skill_usage (skill_usage_events → skill_usage) ──
    info "Migrating skill_usage..."
    sqlite3 "$AD_DB" <<EOSQL
ATTACH DATABASE '$PCD_DB' AS pcd;

INSERT OR REPLACE INTO skill_usage (skill_id, agent_id, session_key, used_at)
SELECT
  su.skill_name,
  su.agent_role_id,
  su.session_key,
  datetime(su.used_at / 1000, 'unixepoch')
FROM pcd.skill_usage_events su;

DETACH DATABASE pcd;
EOSQL

    # ── Migrate review_decisions (kanban_reviews → review_decisions) ──
    info "Migrating review_decisions..."
    sqlite3 "$AD_DB" <<EOSQL
ATTACH DATABASE '$PCD_DB' AS pcd;

INSERT OR REPLACE INTO review_decisions (kanban_card_id, dispatch_id, item_index, decision, decided_at)
SELECT
  kr.card_id,
  kr.review_dispatch_id,
  kr.round,
  kr.verdict,
  datetime(COALESCE(kr.completed_at, kr.created_at) / 1000, 'unixepoch')
FROM pcd.kanban_reviews kr;

DETACH DATABASE pcd;
EOSQL

    ok "All tables migrated"

    # ── Post-migration row counts ──
    info "Destination DB row counts:"
    DEST_TABLES=(
      agents kanban_cards task_dispatches sessions
      meetings meeting_transcripts messages
      offices departments github_repos
      pipeline_stages skill_usage review_decisions
    )
    for t in "${DEST_TABLES[@]}"; do
      c=$(row_count "$AD_DB" "$t")
      printf "  %-30s %s\n" "$t" "$c"
    done
  fi
fi

# ══════════════════════════════════════════════════════════════════════════════
# Phase 3: Copy runtime files
# ══════════════════════════════════════════════════════════════════════════════
if [ "$SKIP_FILES" = false ]; then
  info "Phase 3: Copying runtime files..."

  copy_dir_if_new() {
    local src="$1" dest="$2" label="$3"
    if [ ! -d "$src" ]; then
      warn "Source $label not found: $src"
      return
    fi
    if [ -d "$dest" ]; then
      info "$label already exists at $dest — skipping (use rm -rf to force)"
      return
    fi
    if [ "$DRY_RUN" = true ]; then
      info "(dry-run) Would copy $src → $dest"
    else
      mkdir -p "$(dirname "$dest")"
      cp -R "$src" "$dest"
      ok "Copied $label → $dest"
    fi
  }

  copy_dir_if_new "$HOME/.remotecc/role-context" "$AD_HOME/role-context" "role-context"
  copy_dir_if_new "$HOME/.remotecc/prompts"      "$AD_HOME/prompts"      "prompts"
  copy_dir_if_new "$HOME/.remotecc/skills"        "$AD_HOME/skills"       "skills"
fi

# ══════════════════════════════════════════════════════════════════════════════
# Phase 4: Validation summary
# ══════════════════════════════════════════════════════════════════════════════
echo ""
info "════════════════════════════════════════════════"
info "Migration Summary"
info "════════════════════════════════════════════════"

if [ "$DRY_RUN" = true ]; then
  info "(dry-run mode — no changes were made)"
fi

if [ "$SKIP_CONFIG" = false ] && [ "$DRY_RUN" = false ] && [ -f "$AD_YAML" ]; then
  agent_count=$(grep -c "^  - id:" "$AD_YAML" 2>/dev/null || echo "0")
  ok "Config: $AD_YAML ($agent_count agents)"
fi

if [ "$SKIP_DB" = false ] && [ "$DRY_RUN" = false ] && [ -f "$AD_DB" ]; then
  ok "Database: $AD_DB"
  echo ""
  printf "  %-30s %-10s %-10s\n" "Table" "Source" "Dest"
  printf "  %-30s %-10s %-10s\n" "-----" "------" "----"

  # Map source→dest table names for comparison
  SRC_NAMES=(agents kanban_cards task_dispatches dispatched_sessions round_table_meetings round_table_entries messages offices departments kanban_repo_sources pipeline_stages skill_usage_events kanban_reviews)
  DST_NAMES=(agents kanban_cards task_dispatches sessions meetings meeting_transcripts messages offices departments github_repos pipeline_stages skill_usage review_decisions)

  for i in "${!SRC_NAMES[@]}"; do
    src_t="${SRC_NAMES[$i]}"
    dst_t="${DST_NAMES[$i]}"
    src_c=$(row_count "$PCD_DB" "$src_t")
    dst_c=$(row_count "$AD_DB" "$dst_t")
    printf "  %-30s %-10s %-10s\n" "$src_t → $dst_t" "$src_c" "$dst_c"
  done
fi

if [ "$SKIP_FILES" = false ]; then
  echo ""
  for dir in role-context prompts skills; do
    if [ -d "$AD_HOME/$dir" ]; then
      count=$(find "$AD_HOME/$dir" -type f | wc -l | tr -d ' ')
      ok "Files: $AD_HOME/$dir ($count files)"
    fi
  done
fi

echo ""
ok "Migration complete."
