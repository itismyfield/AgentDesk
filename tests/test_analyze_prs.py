import unittest

from scripts.analyze_prs import has_duplicate_guard_ack, has_non_empty_body_field


class PrAnalyzerBodyFieldTests(unittest.TestCase):
    def test_blank_risk_does_not_borrow_populated_rollback(self):
        body = "- Risk:\n- Rollback notes: revert this PR"

        self.assertFalse(has_non_empty_body_field(body, ["risk", "risk assessment"]))
        self.assertTrue(has_non_empty_body_field(body, ["rollback notes", "rollback"]))

    def test_template_placeholders_are_not_meaningful_fields(self):
        body = """
## WorkFingerprint

- Risk:
- Rollback notes:
"""

        self.assertFalse(has_non_empty_body_field(body, ["risk", "risk assessment"]))
        self.assertFalse(has_non_empty_body_field(body, ["rollback notes", "rollback"]))

    def test_multiline_field_value_stops_at_next_field_label(self):
        body = """
- Risk:
  Limited to analyzer reporting.
- Rollback notes:
  Revert the analyzer change.
"""

        self.assertTrue(has_non_empty_body_field(body, ["risk", "risk assessment"]))
        self.assertTrue(has_non_empty_body_field(body, ["rollback notes", "rollback"]))


class PrAnalyzerDuplicateGuardTests(unittest.TestCase):
    def test_unchecked_template_duplicate_guard_is_not_acknowledgement(self):
        body = "- [ ] **Duplicate PR guard:** I have checked for overlapping open PRs."

        self.assertFalse(has_duplicate_guard_ack(body))

    def test_checked_template_duplicate_guard_is_acknowledgement(self):
        body = "- [x] **Duplicate PR guard:** I have checked for overlapping open PRs."

        self.assertTrue(has_duplicate_guard_ack(body))

    def test_filled_duplicate_overlap_field_is_acknowledgement(self):
        body = (
            "- duplicate/overlap check: compared against sibling upstream-pr "
            "branches; no overlapping scope found."
        )

        self.assertTrue(has_duplicate_guard_ack(body))

    def test_blank_duplicate_field_does_not_borrow_next_value(self):
        body = "- Duplicate PR guard:\n- Risk: limited to analyzer reporting"

        self.assertFalse(has_duplicate_guard_ack(body))


if __name__ == "__main__":
    unittest.main()
