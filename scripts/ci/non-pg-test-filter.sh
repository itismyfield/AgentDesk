#!/usr/bin/env bash

# Canonical libtest substring filter for lanes that must exclude PostgreSQL
# tests. Keep the arguments in one array so workflow callers and the
# selection-set adjudicator receive the same words.
# The workflow shell consumes this after sourcing the file.
# shellcheck disable=SC2034
NON_PG_SKIP_ARGS=(--skip _pg --skip pg_ --skip postgres)
readonly -a NON_PG_SKIP_ARGS

# Derive the positive PostgreSQL selector from the same canonical pairs. This
# keeps the nightly PG/non-PG selection sets complementary as the filter moves.
PG_INCLUDE_ARGS=()
for ((index = 1; index < ${#NON_PG_SKIP_ARGS[@]}; index += 2)); do
  PG_INCLUDE_ARGS+=("${NON_PG_SKIP_ARGS[$index]}")
done
unset index
readonly -a PG_INCLUDE_ARGS

# The broad substring filter also matches these 15 source-verified library
# tests even though their bodies do not connect to PostgreSQL. Replay all of
# them in the full non-PG sweeps to preserve their macOS/Windows coverage.
NON_PG_FILTER_FALSE_POSITIVES=(
  db::dispatched_session_canonical_identity::pg_tests::canonical_identity_conflict_is_http_409_ready
  db::postgres::tests::agent_roster_sync_gated_to_leader_or_single_node
  db::postgres::tests::background_backpressure_disabled_when_reserve_zero
  db::postgres::tests::background_backpressure_saturating_boundaries
  db::postgres::tests::background_backpressure_yields_only_at_or_past_budget
  db::postgres::tests::bootstrap_migration_pool_has_longer_scoped_deadline
  db::postgres::tests::checksum_hex_formats_lowercase_byte_pairs
  db::postgres::tests::checksum_resolution_filters_down_migrations_to_avoid_false_positive
  db::postgres::tests::clamp_foreground_reserve_always_leaves_a_background_slot
  db::postgres::tests::env_lock_after_lifecycle_lock_trips_the_order_tripwire
  db::postgres::tests::runtime_pool_settings_enable_dead_peer_detection
  db::postgres::tests::startup_pool_settings_raise_pool_size_and_acquire_timeout
  reconcile::dispatch_delivery_reconcile_tests::dispatch_delivery_reconcile_classifies_rows_without_postgres
  services::discord::turn_bridge::completion_guard::completion_postgres::runtime_completion_policy_tests::runtime_auto_queue_terminal_sync_matches_dispatch_completion_policy
  services::observability::cancellation_observability_tests::turn_cancelled_emit_records_normalized_payload_without_pg
)
readonly -a NON_PG_FILTER_FALSE_POSITIVES

run_non_pg_filter_false_positives() {
  local test_filter
  for test_filter in "${NON_PG_FILTER_FALSE_POSITIVES[@]}"; do
    cargo test --lib "$test_filter"
  done
}
