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


if __name__ == "__main__":
    unittest.main()
