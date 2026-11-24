"""Contract tests for scheduled-message PostgreSQL CI path coverage."""

from __future__ import annotations

import unittest

if __package__:
    from .test_high_risk_recovery_path_filter import load_filters, select
else:
    from test_high_risk_recovery_path_filter import load_filters, select


class ScheduledMessagesCiWiringTest(unittest.TestCase):
    def setUp(self) -> None:
        self.filters = load_filters()

    def test_scheduler_submodules_trigger_pg_and_high_risk_lanes(self) -> None:
        self.assertLessEqual({"rust_or_policy", "high_risk_recovery"},
                             select(self.filters, ["src/services/scheduled_messages/owner_input.rs"]))
        self.assertIn("rust_or_policy", select(self.filters, ["src/lib.rs"]))

    def test_route_submodules_trigger_high_risk_lane(self) -> None:
        self.assertIn("high_risk_recovery", select(self.filters, ["src/server/routes/scheduled_messages/image_input.rs"]))

    def test_image_outbox_children_trigger_high_risk_lane(self) -> None:
        for path in ("src/services/message_outbox/attachments.rs", "src/server/outbox_worker.rs", "src/server/outbox_worker/claim.rs"):
            with self.subTest(path=path):
                self.assertIn("high_risk_recovery", select(self.filters, [path]))


if __name__ == "__main__":
    unittest.main()
