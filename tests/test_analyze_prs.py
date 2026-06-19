import unittest

from scripts.analyze_prs import (
    has_duplicate_guard_ack,
    has_non_empty_body_field,
    has_no_change_verification_ack,
    has_stale_branch_cleanup_ack,
    has_scratch_file_cleanup_ack,
    has_overlap_reference,
)


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

    def test_indented_key_value_lines_count_as_field_content(self):
        body = """
- Risk:
  - Impact: low
- Rollback notes:
  Command: git revert HEAD
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


class PrAnalyzerNoChangeVerificationGuardTests(unittest.TestCase):
    def test_unchecked_template_no_change_guard_is_not_acknowledgement(self):
        body = "- [ ] **No-change verification:** If this PR claims no change..."

        self.assertFalse(has_no_change_verification_ack(body))

    def test_checked_template_no_change_guard_is_acknowledgement(self):
        body = "- [x] **No-change verification:** If this PR claims no change..."

        self.assertTrue(has_no_change_verification_ack(body))

    def test_filled_no_change_field_is_acknowledgement(self):
        body = "- no-change verification: checked using gh pr view --json files"

        self.assertTrue(has_no_change_verification_ack(body))


class PrAnalyzerStaleBranchCleanupGuardTests(unittest.TestCase):
    def test_unchecked_template_stale_branch_guard_is_not_acknowledgement(self):
        body = "- [ ] **Stale branch cleanup:** I am not salvaging a stale broad branch in-place."

        self.assertFalse(has_stale_branch_cleanup_ack(body))

    def test_checked_template_stale_branch_guard_is_acknowledgement(self):
        body = "- [x] **Stale branch cleanup:** I am not salvaging a stale broad branch in-place."

        self.assertTrue(has_stale_branch_cleanup_ack(body))

    def test_filled_stale_branch_field_is_acknowledgement(self):
        body = "- stale branch cleanup: closed stale branch and recreated."

        self.assertTrue(has_stale_branch_cleanup_ack(body))


class PrAnalyzerScratchFileCleanupGuardTests(unittest.TestCase):
    def test_unchecked_template_scratch_file_guard_is_not_acknowledgement(self):
        body = "- [ ] **Scratch file cleanup:** I have run `git status`..."

        self.assertFalse(has_scratch_file_cleanup_ack(body))

    def test_checked_template_scratch_file_guard_is_acknowledgement(self):
        body = "- [X] **Scratch file cleanup:** I have run `git status`..."

        self.assertTrue(has_scratch_file_cleanup_ack(body))

    def test_filled_scratch_file_field_is_acknowledgement(self):
        body = "- scratch file cleanup: ran git diff --check and git status."

        self.assertTrue(has_scratch_file_cleanup_ack(body))


class PrAnalyzerOverlapReferenceTests(unittest.TestCase):
    def test_overlap_reference_with_hash(self):
        body = "This is a no-change PR overlapping with #1234."
        self.assertTrue(has_overlap_reference(body))

    def test_overlap_reference_with_url(self):
        body = "Overlap with https://github.com/owner/repo/pull/5678."
        self.assertTrue(has_overlap_reference(body))

    def test_missing_overlap_reference(self):
        body = "This is a no-change PR but lacks exact PR numbers."
        self.assertFalse(has_overlap_reference(body))

if __name__ == "__main__":
    unittest.main()
