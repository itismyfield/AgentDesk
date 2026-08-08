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
            lines[1:],
            [
                "cargo <test> <--all-targets> "
                "<db::postgres::tests::test_database_server_identity_normalizes_loopback_aliases_without_collisions>",
                "cargo <test> <--all-targets> "
                "<reconcile::dispatch_delivery_reconcile_tests::dispatch_delivery_reconcile_classifies_rows_without_postgres>",
                "cargo <test> <--all-targets> "
                "<services::observability::cancellation_observability_tests::turn_cancelled_emit_records_normalized_payload_without_pg>",
            ],
        )


if __name__ == "__main__":
    unittest.main()
