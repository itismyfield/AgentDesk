"""Wiring contracts for issue #4448 alert authority and dedupe guards."""

from __future__ import annotations

import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]


class AlertDedupeWiringTests(unittest.TestCase):
    def test_auto_queue_failed_entry_records_state_without_an_alert_card(self) -> None:
        # #5993: the terminal entry-failure card is retired. The failure stays
        # visible as the entry transition row plus the caller WARN events.
        planning = (REPO_ROOT / "src/services/auto_queue/planning.rs").read_text(
            encoding="utf-8"
        )
        dispatch_failure = (
            REPO_ROOT / "src/db/auto_queue/entries/dispatch_failure.rs"
        ).read_text(encoding="utf-8")
        activate = (
            REPO_ROOT / "src/services/auto_queue/activate_command.rs"
        ).read_text(encoding="utf-8")
        fsm = (REPO_ROOT / "src/services/auto_queue/fsm.rs").read_text(
            encoding="utf-8"
        )

        for text in (planning, dispatch_failure):
            self.assertNotIn("enqueue_outbox", text)
            self.assertNotIn("EntryDispatchFailureAlert", text)
            self.assertNotIn("record_entry_dispatch_failure_with_alert_on_pg", text)
            self.assertNotIn("auto_queue.entry_dispatch_failed", text)
        self.assertIn("record_entry_dispatch_failure_on_pg", planning)
        self.assertIn("record_entry_transition_on_pg", dispatch_failure)
        self.assertIn('"activate_dispatch_create_failed_pg"', activate)
        self.assertIn('"activate_dispatch_create_failure_recorded_pg"', activate)
        self.assertIn('"activate_dispatch_create_failure_record_failed_pg"', activate)
        self.assertIn('"restore_run_create_dispatch_failed"', fsm)
        self.assertIn('"restore_run_create_dispatch_retry_scheduled"', fsm)

    def test_quality_regression_has_one_runtime_alert_authority(self) -> None:
        legacy = REPO_ROOT / "src/services/observability/quality_alert.rs"
        queries = (REPO_ROOT / "src/services/observability/queries.rs").read_text(
            encoding="utf-8"
        )
        source_registry = (
            REPO_ROOT / "src/services/discord/outbound/source_registry.rs"
        ).read_text(encoding="utf-8")
        maintenance = (REPO_ROOT / "src/server/maintenance/mod.rs").read_text(
            encoding="utf-8"
        )
        quality_module = (
            REPO_ROOT / "src/services/agent_quality/mod.rs"
        ).read_text(encoding="utf-8")
        regression_alerts = (
            REPO_ROOT / "src/services/agent_quality/regression_alerts.rs"
        ).read_text(encoding="utf-8")

        self.assertFalse(legacy.exists(), "legacy quality alert producer must be removed")
        self.assertNotIn("enqueue_quality_regression_alerts_pg", queries)
        self.assertIn("alert_count: 0", queries)
        self.assertNotIn('"agent_quality_rollup"', source_registry)
        self.assertIn("sole regression-alert authority", quality_module)
        self.assertIn(
            "agent_quality::regression_alerts::run_regression_alerter_pg", maintenance
        )
        self.assertIn("TURN_DROP_THRESHOLD: f64 = 0.15", regression_alerts)
        self.assertIn("REVIEW_DROP_THRESHOLD: f64 = 0.20", regression_alerts)
        self.assertIn("agent_quality_monitoring_channel_id", regression_alerts)
        # #5993: the shared human-alert fallback was retired.
        self.assertNotIn("kanban_human_alert_channel_id", regression_alerts)
        self.assertNotIn("FALLBACK_ALERT_CHANNEL", regression_alerts)


if __name__ == "__main__":
    unittest.main()
