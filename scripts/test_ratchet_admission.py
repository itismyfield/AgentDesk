#!/usr/bin/env python3
"""Focused tests for the ratchet cap-admission guard (#4269)."""

from __future__ import annotations

import unittest

from scripts.ratchet_admission import (
    ADMISSION_WARN_THRESHOLD,
    AdmissionEvent,
    admission_warning_messages,
    validate_admission_delta,
)


class RatchetAdmissionGuardTest(unittest.TestCase):
    def _event(self, decompose_issue: object) -> AdmissionEvent:
        return AdmissionEvent(
            ratchet="hotfile_ratchet",
            file="src/services/discord/example.rs",
            old_cap=100,
            new_cap=120,
            decompose_issue=decompose_issue,
            count=1,
        )

    def test_admission_requires_decompose_issue_and_accepts_it_when_present(self) -> None:
        common = {
            "ratchet": "hotfile_ratchet",
            "current_caps": {"src/services/discord/example.rs": 120},
            "prior_caps": {"src/services/discord/example.rs": 100},
            "prior_events": [],
        }

        rejected = validate_admission_delta(
            **common,
            current_events=[self._event(None)],
        )
        self.assertTrue(
            any("decompose_issue is mandatory" in error for error in rejected),
            "cap admission without decompose_issue must be rejected",
        )

        accepted = validate_admission_delta(
            **common,
            current_events=[self._event(4269)],
        )
        self.assertEqual(accepted, [], "linked cap admission must be accepted")

    def test_more_than_named_threshold_emits_decomposition_warning(self) -> None:
        events = [
            AdmissionEvent(
                ratchet="hotfile_ratchet",
                file="src/services/discord/example.rs",
                old_cap=100 + count,
                new_cap=101 + count,
                decompose_issue=4200 + count,
                count=count,
            )
            for count in range(1, ADMISSION_WARN_THRESHOLD + 2)
        ]

        warnings = admission_warning_messages(events, "hotfile_ratchet")
        self.assertEqual(len(warnings), 1)
        self.assertIn("prioritize decomposition", warnings[0])
        self.assertIn("src/services/discord/example.rs", warnings[0])


if __name__ == "__main__":
    unittest.main()
