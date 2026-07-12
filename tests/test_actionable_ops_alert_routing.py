import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
ANNOUNCE = "crate::services::message_outbox::ACTIONABLE_OPS_ALERT_BOT"


class ActionableOpsAlertRoutingContract(unittest.TestCase):
    def source(self, relative: str) -> str:
        return (ROOT / relative).read_text(encoding="utf-8")

    def assert_producer(self, relative: str, source: str, reason: str) -> None:
        text = self.source(relative)
        self.assertIn(ANNOUNCE, text, relative)
        self.assertIn(source, text, relative)
        self.assertIn(reason, text, relative)

    def test_all_rust_actionable_producers_use_announce_primary(self) -> None:
        for relative, source, reason in [
            (
                "src/server/outbox_delivery_alert.rs",
                "outbox_delivery_alert",
                "outbox_delivery_failed",
            ),
            (
                "src/github/sync.rs",
                "github_sync",
                "github_sync.terminal_open_issue",
            ),
            (
                "src/services/long_turn_watchdog.rs",
                "long_turn_watchdog",
                "long_turn_cluster",
            ),
            (
                "src/services/observability/relay_signal_alert.rs",
                "relay_signal_rollup",
                "relay_signal.threshold",
            ),
            ("src/services/slo/mod.rs", "slo_alerter", "slo_threshold_breach"),
            (
                "src/services/dispatch_watchdog.rs",
                "dispatch_watchdog",
                "dispatch_stuck",
            ),
            (
                "src/services/auto_queue/planning.rs",
                "auto-queue",
                "auto_queue.entry_dispatch_failed",
            ),
        ]:
            with self.subTest(relative=relative):
                self.assert_producer(relative, source, reason)

    def test_routine_stale_alert_overrides_both_targets_to_announce(self) -> None:
        text = self.source("src/services/routines/discord_log.rs")
        start = text.index("pub async fn log_stale_paused")
        end = text.index("async fn log_to_routine_target(", start)
        stale_path = text[start:end]
        self.assertIn("log_actionable_to_routine_target_with_ttl", stale_path)
        self.assertIn(ANNOUNCE, stale_path)

        helper_start = text.index("async fn log_actionable_to_routine_target_with_ttl")
        helper_end = text.index("async fn log_run_section", helper_start)
        self.assertIn(ANNOUNCE, text[helper_start:helper_end])

    def test_monitor_routes_only_stuck_and_anomaly_to_announce(self) -> None:
        route = self.source("src/server/routes/message_outbox.rs")
        self.assertIn('(\"alert\", \"STUCK\")', route)
        self.assertIn('(\"alert\", \"ANOMALY\")', route)
        self.assertIn('(\"alert\", \"REVIEW_LONG\") => (\"auto_queue.monitor_review_long\", \"notify\")', route)
        self.assertIn('(\"recovery\", _) => (\"auto_queue.monitor_recovery\", \"notify\")', route)

        monitor = self.source("scripts/auto-queue-monitor.sh")
        self.assertIn("--arg kind", monitor)
        self.assertIn('incident_kind\" != \"STUCK', monitor)
        self.assertIn('incident_kind\" != \"ANOMALY', monitor)
        self.assertIn("discord-sendmessage", monitor)

    def test_worker_fallback_is_exactly_announce_to_notify(self) -> None:
        delivery = self.source("src/server/outbox_actionable_delivery.rs")
        self.assertIn("is_actionable_ops_alert", delivery)
        self.assertIn("ACTIONABLE_OPS_ALERT_BOT", delivery)
        self.assertIn('deliver_with_bot(registry, pg_pool, row, \"notify\")', delivery)
        self.assertIn('status != \"200 OK\"', delivery)


if __name__ == "__main__":
    unittest.main()
