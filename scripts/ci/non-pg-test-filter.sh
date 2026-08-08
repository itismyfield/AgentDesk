#!/usr/bin/env bash

# Canonical libtest substring filter for lanes that must exclude PostgreSQL
# tests. Keep the arguments in one array so workflow callers and the
# selection-set adjudicator receive the same words.
# The workflow shell consumes this after sourcing the file.
# shellcheck disable=SC2034
NON_PG_SKIP_ARGS=(--skip _pg --skip pg_ --skip postgres)
readonly -a NON_PG_SKIP_ARGS

# The broad substring filter also matches these source-verified tests even
# though their bodies do not connect to PostgreSQL. Run them separately in
# the full non-PG sweeps so tightening a lane does not remove their coverage.
NON_PG_FILTER_FALSE_POSITIVES=(
  db::postgres::tests::test_database_server_identity_normalizes_loopback_aliases_without_collisions
  reconcile::dispatch_delivery_reconcile_tests::dispatch_delivery_reconcile_classifies_rows_without_postgres
  services::observability::cancellation_observability_tests::turn_cancelled_emit_records_normalized_payload_without_pg
)
readonly -a NON_PG_FILTER_FALSE_POSITIVES

run_non_pg_filter_false_positives() {
  local test_filter
  for test_filter in "${NON_PG_FILTER_FALSE_POSITIVES[@]}"; do
    cargo test --all-targets "$test_filter"
  done
}
