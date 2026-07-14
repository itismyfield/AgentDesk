#!/usr/bin/env python3
"""Focused tests for the weekly regression-churn audit (#4265)."""

from __future__ import annotations

import os
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
ROUTINE_DIR = ROOT / "routines" / "monitoring"
sys.path.insert(0, str(ROUTINE_DIR))

import weekly_churn_audit  # noqa: E402
from log_digest_issue_drafts import OpenIssue  # noqa: E402
from weekly_churn_audit import (  # noqa: E402
    GitCommit,
    analyze_churn,
    candidate_drafts,
    compute_issue_lineages,
    is_fix_commit_subject,
    issue_references,
    maybe_post_weekly_channel,
)


def commit(
    subject: str,
    files: tuple[str, ...] = ("src/services/discord/example.rs",),
    *,
    sha: str = "a" * 40,
    body: str = "",
) -> GitCommit:
    return GitCommit(sha=sha, subject=subject, body=body, files=files)


class WeeklyChurnAuditTests(unittest.TestCase):
    def test_fix_commit_classifier_is_precise(self) -> None:
        for subject in (
            "fix: stop duplicate relay",
            "fix(discord): stop duplicate relay",
        ):
            with self.subTest(subject=subject):
                self.assertTrue(is_fix_commit_subject(subject))

        for subject in (
            "chore: stop duplicate relay",
            "feat(discord): stop duplicate relay",
            "refactor: stop duplicate relay",
            "docs: explain duplicate relay",
            "test: reproduce duplicate relay",
            "prefix: fix: embedded text is not a fix subject",
        ):
            with self.subTest(subject=subject):
                self.assertFalse(is_fix_commit_subject(subject))

    def test_threshold_includes_n_but_not_n_minus_one(self) -> None:
        repeated = "src/services/discord/repeated.rs"
        below = "src/services/discord/below.rs"
        commits = [
            commit("fix: first", (repeated, below), sha="1" * 40),
            commit("fix(scope): second", (repeated, below), sha="2" * 40),
            commit("fix: third", (repeated,), sha="3" * 40),
            commit("feat: not counted", (below,), sha="4" * 40),
        ]

        audit = analyze_churn(commits, threshold=3)

        self.assertEqual(audit.file_counts[repeated], 3)
        self.assertEqual(audit.file_counts[below], 2)
        self.assertEqual([candidate.file for candidate in audit.candidates], [repeated])
        self.assertEqual(audit.module_counts["src/services/discord"], 3)

    def test_squash_subject_parses_all_issue_references_in_order(self) -> None:
        subject = (
            "fix(deploy): #4262 post-deploy scope (#4511) (#4523)"
        )

        self.assertTrue(is_fix_commit_subject(subject))
        self.assertEqual(issue_references(subject), (4262, 4511, 4523))

    def test_issue_lineage_generation_count_spans_commit_text_edges(self) -> None:
        commits = [
            commit("fix: first regression (#100) (#200)"),
            commit("fix: follow-up (#200)", body="Regression-of cross-reference: #300"),
            commit("fix: independent (#900)"),
        ]

        lineages = compute_issue_lineages(commits)

        self.assertEqual(lineages[0].issues, (100, 200, 300))
        self.assertEqual(lineages[0].generations, 3)
        self.assertIn((900,), [lineage.issues for lineage in lineages])

    def test_open_issue_dedup_suppresses_matching_candidate_draft(self) -> None:
        candidate = analyze_churn(
            [
                commit(f"fix: regression {index}", sha=str(index) * 40)
                for index in range(1, 4)
            ],
            threshold=3,
        ).candidates[0]
        matching = OpenIssue(
            number=4265,
            title=(
                "ops(process): repeated fix churn redesign candidate "
                "src/services/discord/example.rs"
            ),
        )

        with patch.object(
            weekly_churn_audit,
            "issue_matches_signature",
            wraps=weekly_churn_audit.issue_matches_signature,
        ) as matcher:
            drafts, matches = candidate_drafts(
                [candidate], [matching], since="7 days", threshold=3
            )

        self.assertEqual(drafts, [])
        self.assertEqual(matches, [(candidate, matching)])
        matcher.assert_called_once()

    def test_channel_post_gate_is_default_off_and_confirmed_is_idempotent(self) -> None:
        calls: list[str] = []
        with tempfile.TemporaryDirectory() as temp:
            state = Path(temp) / "post-state.json"
            disabled = maybe_post_weekly_channel(
                "report",
                "off",
                "123",
                state,
                calls.append,
            )
            first = maybe_post_weekly_channel(
                "report",
                "confirmed",
                "123",
                state,
                calls.append,
            )
            repeated = maybe_post_weekly_channel(
                "report",
                "confirmed",
                "123",
                state,
                calls.append,
            )

        self.assertEqual(disabled, (False, "weekly ops channel post disabled"))
        self.assertEqual(first, (True, "weekly ops channel report posted"))
        self.assertEqual(repeated, (False, "identical weekly report already posted"))
        self.assertEqual(calls, ["report"])

    def test_main_default_off_has_no_issue_or_channel_side_effect(self) -> None:
        audit_commits = [
            commit(f"fix: repeat {index}", sha=str(index) * 40)
            for index in range(1, 4)
        ]
        with tempfile.TemporaryDirectory() as temp:
            output = StringIO()
            with (
                patch.object(
                    sys,
                    "argv",
                    [
                        "weekly_churn_audit.py",
                        "--repo-root",
                        str(ROOT),
                        "--runtime-root",
                        temp,
                    ],
                ),
                patch.dict(os.environ, {}, clear=True),
                patch.object(
                    weekly_churn_audit,
                    "collect_git_commits",
                    return_value=audit_commits,
                ),
                patch.object(weekly_churn_audit, "load_open_issues") as load_open,
                patch.object(weekly_churn_audit, "write_pending_drafts") as write_drafts,
                patch.object(
                    weekly_churn_audit, "maybe_post_approved_drafts"
                ) as create_issues,
                patch.object(weekly_churn_audit, "_post_report") as post_channel,
                redirect_stdout(output),
            ):
                rc = weekly_churn_audit.main()

            runtime_files = list(Path(temp).rglob("*"))

        self.assertEqual(rc, 0)
        load_open.assert_not_called()
        write_drafts.assert_not_called()
        create_issues.assert_not_called()
        post_channel.assert_not_called()
        self.assertEqual(runtime_files, [])
        self.assertIn("재설계 후보 (1)", output.getvalue())
        self.assertIn("issue drafts dry-run only", output.getvalue())


if __name__ == "__main__":
    unittest.main()
