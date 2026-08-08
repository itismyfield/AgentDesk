"""Execution contract for the shared non-PG libtest filter."""

from __future__ import annotations

import importlib.util
import subprocess
import sys
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
FILTER = ROOT / "scripts/ci/non-pg-test-filter.sh"
MEMBERSHIP = ROOT / "scripts/check_pg_test_lane_membership.py"
EXPECTED_FALSE_POSITIVES = (
    "db::dispatched_session_canonical_identity::pg_tests::"
    "canonical_identity_conflict_is_http_409_ready",
    "db::postgres::tests::agent_roster_sync_gated_to_leader_or_single_node",
    "db::postgres::tests::background_backpressure_disabled_when_reserve_zero",
    "db::postgres::tests::background_backpressure_saturating_boundaries",
    "db::postgres::tests::background_backpressure_yields_only_at_or_past_budget",
    "db::postgres::tests::bootstrap_migration_pool_has_longer_scoped_deadline",
    "db::postgres::tests::checksum_hex_formats_lowercase_byte_pairs",
    "db::postgres::tests::"
    "checksum_resolution_filters_down_migrations_to_avoid_false_positive",
    "db::postgres::tests::clamp_foreground_reserve_always_leaves_a_background_slot",
    "db::postgres::tests::runtime_pool_settings_enable_dead_peer_detection",
    "db::postgres::tests::startup_pool_settings_raise_pool_size_and_acquire_timeout",
    "db::postgres::tests::"
    "test_database_server_identity_normalizes_loopback_aliases_without_collisions",
    "reconcile::dispatch_delivery_reconcile_tests::"
    "dispatch_delivery_reconcile_classifies_rows_without_postgres",
    "services::discord::turn_bridge::completion_guard::completion_postgres::"
    "runtime_completion_policy_tests::"
    "runtime_auto_queue_terminal_sync_matches_dispatch_completion_policy",
    "services::observability::cancellation_observability_tests::"
    "turn_cancelled_emit_records_normalized_payload_without_pg",
)


def load_membership_module():
    spec = importlib.util.spec_from_file_location("non_pg_filter_membership", MEMBERSHIP)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class NonPgTestFilter(unittest.TestCase):
    def test_source_exports_filter_and_replays_verified_false_positives(self) -> None:
        script = r'''
cargo() {
  printf 'cargo'
  printf ' <%s>' "$@"
  printf '\n'
}
source "$1"
printf 'filter'
printf ' <%s>' "${NON_PG_SKIP_ARGS[@]}"
printf '\n'
printf 'include'
printf ' <%s>' "${PG_INCLUDE_ARGS[@]}"
printf '\n'
run_non_pg_filter_false_positives
'''
        result = subprocess.run(
            ["bash", "-c", script, "bash", str(FILTER)],
            check=True,
            capture_output=True,
            text=True,
        )
        lines = result.stdout.splitlines()
        membership = load_membership_module()
        args = membership.load_non_pg_skip_args(ROOT)
        self.assertEqual(lines[0], "filter" + "".join(f" <{arg}>" for arg in args))
        self.assertEqual(
            lines[1], "include" + "".join(f" <{arg}>" for arg in args[1::2])
        )
        false_positives = membership.load_non_pg_false_positives(ROOT)
        self.assertEqual(false_positives, EXPECTED_FALSE_POSITIVES)
        self.assertEqual(
            lines[2:],
            [f"cargo <test> <--lib> <{test_id}>" for test_id in false_positives],
        )

    def test_every_replay_id_exists_in_libtest_manifest(self) -> None:
        membership = load_membership_module()
        self.assertEqual(
            set(membership.load_non_pg_false_positives(ROOT))
            - membership.load_lib_test_inventory(ROOT),
            set(),
        )

    def test_positive_and_negative_filters_partition_the_inventory(self) -> None:
        membership = load_membership_module()
        inventory = membership.load_lib_test_inventory(ROOT)
        tokens = membership.load_non_pg_skip_args(ROOT)[1::2]
        non_pg = {
            test_id
            for test_id in inventory
            if not any(token in test_id for token in tokens)
        }
        pg = {
            test_id
            for test_id in inventory
            if any(token in test_id for token in tokens)
        }
        self.assertEqual(non_pg & pg, set())
        self.assertEqual(inventory - (non_pg | pg), set())


if __name__ == "__main__":
    unittest.main()
