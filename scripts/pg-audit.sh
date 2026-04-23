#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

fail() {
  echo "pg-audit: $*" >&2
  exit 1
}

require_fixed_string() {
  local needle="$1"
  local file="$2"
  if ! grep -Fq "$needle" "$file"; then
    fail "missing expected marker in $file: $needle"
  fi
}

echo "=== pg-audit category-5 guard ==="

pending_rows="$(
  awk '
    /^## 5\. SQLite <-> PG dual-write propagation gaps/ { in_section=1; next }
    /^## / && in_section { in_section=0 }
    in_section && /`follow-up`/ { print }
  ' docs/generated/pg-audit-checklist.md
)"
if [ -n "$pending_rows" ]; then
  echo "$pending_rows" >&2
  fail "category-5 checklist still contains follow-up rows"
fi

require_fixed_string "Temporary dual-write: policy-tick aggregation still reads SQLite-only state." src/services/api_friction.rs
require_fixed_string "PG outbox rows are authoritative whenever a pool is configured." src/services/message_outbox.rs
require_fixed_string "PG card_retrospectives rows are authoritative once a pool is attached." src/services/retrospectives.rs
require_fixed_string "PG pending_dm_replies rows are authoritative in mixed mode." src/services/discord_dm_reply_store.rs
require_fixed_string "SQLite-only compatibility path: PG auto-queue slot ownership is" src/db/auto_queue.rs

if sed -n '/pub fn rebind_slot_for_group_agent/,/pub async fn rebind_slot_for_group_agent_pg/p' src/db/auto_queue.rs \
  | grep -Fq 'TODO(#839)'; then
  fail "rebind_slot_for_group_agent still carries TODO(#839) ambiguity"
fi

if sed -n '/fn bind_slot_index_for_group_entries/,/pub fn release_slot_for_group_agent/p' src/db/auto_queue.rs \
  | grep -Fq 'TODO(#839)'; then
  fail "bind_slot_index_for_group_entries still carries TODO(#839) ambiguity"
fi

if sed -n '/pub fn release_slot_for_group_agent/,/#[derive(Debug, Clone, Default)]/p' src/db/auto_queue.rs \
  | grep -Fq 'TODO(#839)'; then
  fail "release_slot_for_group_agent still carries TODO(#839) ambiguity"
fi

echo "pg-audit: category-5 guard passed"
