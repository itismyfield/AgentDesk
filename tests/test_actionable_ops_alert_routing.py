import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
ANNOUNCE = "crate::services::message_outbox::ACTIONABLE_OPS_ALERT_BOT"
NOTIFY = "crate::services::discord::bot_role::UtilityBotRole::Notify.alias()"


def collapse(text: str) -> str:
    """Normalize whitespace so rustfmt line-wrapping does not affect matching."""
    return " ".join(text.split())


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
                "src/github/sync.rs",
                "github_sync",
                "github_sync.terminal_open_issue",
            ),
            ("src/services/slo/mod.rs", "slo_alerter", "slo_threshold_breach"),
            (
                "src/services/dispatch_watchdog.rs",
                "dispatch_watchdog",
                "dispatch_stuck",
            ),
        ]:
            with self.subTest(relative=relative):
                self.assert_producer(relative, source, reason)

    def test_retired_human_alert_producers_enqueue_nothing(self) -> None:
        # #5993: terminal outbox failures and relay signals are WARN lines plus
        # observability events; neither producer may reach the outbox again.
        for relative in [
            "src/server/outbox_delivery_alert.rs",
            "src/services/observability/relay_signal_alert.rs",
        ]:
            with self.subTest(relative=relative):
                text = self.source(relative)
                self.assertNotIn("enqueue_outbox", text)
                self.assertNotIn(ANNOUNCE, text)
                self.assertIn("tracing::warn!", text)

    def test_long_turn_monitoring_is_not_registered_or_scheduled(self) -> None:
        # Elapsed turn duration no longer triggers manager alerts or automatic
        # watchdog extensions. Keep the removal contract instead of requiring
        # the retired producer to participate in alert routing.
        self.assertFalse((ROOT / "src/services/long_turn_watchdog.rs").exists())
        self.assertFalse((ROOT / "policies/timeouts/long-turn-monitor.js").exists())
        self.assertNotIn("long_turn_watchdog", self.source("src/services/mod.rs"))
        self.assertNotIn("long_turn_watchdog", self.source("src/server/mod.rs"))
        policy = self.source("policies/timeouts.js")
        self.assertNotIn("long-turn-monitor", policy)
        self.assertNotIn("_section_L", policy)

    def test_routine_stale_alert_routes_thread_to_announce_or_logs(self) -> None:
        text = self.source("src/services/routines/discord_log.rs")
        start = text.index("pub async fn log_stale_paused")
        end = text.index("async fn log_to_routine_target(", start)
        stale_path = text[start:end]
        self.assertIn("log_actionable_to_routine_target_with_ttl", stale_path)
        # #5993: no operator fallback target; a thread-less stall is a WARN.
        self.assertNotIn("health_target", text)
        self.assertNotIn("log_to_target_with_ttl(", stale_path)
        self.assertIn("tracing::warn!", stale_path)
        recovery = text[text.index("pub async fn log_recovery") : start]
        self.assertIn("tracing::info!", recovery)

        helper_start = text.index("async fn log_actionable_to_routine_target_with_ttl")
        helper_end = text.index("async fn log_run_section", helper_start)
        self.assertIn(ANNOUNCE, text[helper_start:helper_end])

    def test_worker_fallback_is_exactly_announce_to_notify(self) -> None:
        delivery = self.source("src/server/outbox_actionable_delivery.rs")
        self.assertIn("is_actionable_ops_alert", delivery)
        self.assertIn("ACTIONABLE_OPS_ALERT_BOT", delivery)

        # Same rustfmt multi-line reflow as above: the fallback call's
        # deliver_with_bot(..., Notify.alias()) arguments now span several
        # lines, so collapse whitespace before matching the tight call-site
        # coupling (fallback path specifically calls Notify, not the primary
        # bot).
        self.assertIn(
            collapse(f"deliver_with_bot( registry, pg_pool, row, {NOTIFY}, )"),
            collapse(delivery),
        )
        self.assertIn('status != \"200 OK\"', delivery)


if __name__ == "__main__":
    unittest.main()
