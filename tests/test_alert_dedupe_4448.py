"""Wiring contracts for issue #4448 alert authority and dedupe guards."""

from __future__ import annotations

import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]


class AlertDedupeWiringTests(unittest.TestCase):
    def test_auto_queue_failed_entry_alert_uses_stable_db_dedupe(self) -> None:
        planning = (REPO_ROOT / "src/services/auto_queue/planning.rs").read_text(
            encoding="utf-8"
        )
        dispatch_failure = (
            REPO_ROOT / "src/db/auto_queue/entries/dispatch_failure.rs"
        ).read_text(encoding="utf-8")

        self.assertIn("FAILED_ENTRY_ALERT_REASON_CODE", planning)
        self.assertIn("failed_entry_alert_session_key", planning)
        self.assertIn("failure-transition", planning)
        self.assertIn("failure_transition_id", planning)
        self.assertIn("record_entry_dispatch_failure_with_alert_on_pg", planning)
        self.assertIn("enqueue_outbox_pg_on_tx_with_ttl", dispatch_failure)
        self.assertIn("FAILED_ENTRY_ALERT_DEDUPE_TTL_SECS", planning)

    def test_auto_queue_monitor_has_restart_safe_once_reconciliation(self) -> None:
        monitor = (REPO_ROOT / "scripts/auto-queue-monitor.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn("AQ_MONITOR_STATE_FILE", monitor)
        self.assertIn("AQ_MONITOR_ONCE", monitor)
        self.assertIn("AQ_MONITOR_COOLDOWN_SECS", monitor)
        self.assertIn("auto_queue_monitor_state.py", monitor)
        self.assertIn("run-locked", monitor)
        self.assertIn("review_entered_at", monitor)
        self.assertIn("turn_active", monitor)
        self.assertIn("awaiting_bg", monitor)
        self.assertIn("awaiting_user", monitor)
        self.assertIn("/api/message-outbox/monitor-alerts", monitor)
        self.assertIn("action_id", monitor)
        state_helper = (
            REPO_ROOT / "scripts/auto_queue_monitor_state.py"
        ).read_text(encoding="utf-8")
        route = (
            REPO_ROOT / "src/server/routes/message_outbox.rs"
        ).read_text(encoding="utf-8")
        self.assertIn('"pending_action"', state_helper)
        self.assertIn('source: "auto-queue-monitor"', route)

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
        self.assertIn("kanban_human_alert_channel_id", regression_alerts)
        self.assertNotIn("FALLBACK_ALERT_CHANNEL", regression_alerts)


if __name__ == "__main__":
    unittest.main()
