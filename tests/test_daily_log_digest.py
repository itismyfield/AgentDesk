#!/usr/bin/env python3
"""Focused tests for the reusable daily log-digest draft pipeline (#4263)."""

from __future__ import annotations

import os
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ROUTINE_DIR = ROOT / "routines" / "monitoring"
sys.path.insert(0, str(ROUTINE_DIR))

from log_digest_issue_drafts import (  # noqa: E402
    IssueDraft,
    OpenIssue,
    SignatureCount,
    aggregate_normalized_signatures,
    decide_issue_drafts,
    exceeds_threshold,
    format_daily_summary,
    issue_matches_signature,
    maybe_post_approved_drafts,
    normalize_signature,
    write_pending_drafts,
)
from daily_log_digest import dcserver_log_paths, recent_log_lines  # noqa: E402


class SignatureNormalizationTests(unittest.TestCase):
    def test_runtime_log_paths_include_internal_stdout_and_launchd_stderr(self) -> None:
        paths = dcserver_log_paths(Path("/srv/agentdesk"))
        self.assertIn(Path("/srv/agentdesk/logs/dcserver.stdout.log"), paths)
        self.assertIn(Path("/srv/agentdesk/logs/dcserver.stdout.log.1"), paths)
        self.assertIn(Path("/srv/agentdesk/logs/dcserver.launchd.stderr.log"), paths)

    def test_recent_window_filters_old_and_undated_stdout_lines(self) -> None:
        now = datetime(2026, 7, 14, 0, 0, tzinfo=timezone.utc)
        since = now - timedelta(days=1)
        with tempfile.TemporaryDirectory() as temp:
            logs = Path(temp)
            stdout = logs / "dcserver.stdout.log"
            launchd_stderr = logs / "dcserver.launchd.stderr.log"
            stdout.write_text(
                "2026-07-12T23:59:00Z ERROR stale failure id=1\n"
                "2026-07-13T12:00:00Z WARN recent timeout id=2\n"
                "ERROR undated stale stdout line\n",
                encoding="utf-8",
            )
            launchd_stderr.write_text("WARN undated recent launchd bootstrap\n", encoding="utf-8")
            timestamp = now.timestamp()
            stdout.touch()
            launchd_stderr.touch()
            # touch() uses wall clock; explicitly pin both mtimes to the test window.
            os.utime(stdout, (timestamp, timestamp))
            os.utime(launchd_stderr, (timestamp, timestamp))

            lines, warnings = recent_log_lines([stdout, launchd_stderr], since, now)

        self.assertEqual(warnings, [])
        self.assertEqual(
            lines,
            [
                "2026-07-13T12:00:00Z WARN recent timeout id=2",
                "WARN undated recent launchd bootstrap",
            ],
        )

    def test_different_ids_hashes_and_timestamps_collapse(self) -> None:
        first = (
            "2026-07-13T01:02:03.123Z WARN sqlx pool timed out while acquiring "
            "id=123 request_id=req-a9f3 token=secret-one commit=deadbeef"
        )
        second = (
            "2026-07-14T04:05:06.987Z WARN sqlx pool timed out while acquiring "
            "id=456 request_id=req-b7d1 token=secret-two commit=cafebabe"
        )

        self.assertEqual(normalize_signature(first), normalize_signature(second))
        patterns = aggregate_normalized_signatures([first, second])
        self.assertEqual(len(patterns), 1)
        self.assertEqual(patterns[0].severity, "WARN")
        self.assertEqual(patterns[0].count, 2)

    def test_semantically_distinct_patterns_stay_distinct(self) -> None:
        patterns = aggregate_normalized_signatures(
            [
                "2026-07-14T01:00:00Z ERROR postgres pool timed out id=123",
                "2026-07-14T01:00:01Z ERROR discord gateway connection refused id=456",
            ]
        )

        self.assertEqual(len(patterns), 2)
        self.assertNotEqual(patterns[0].signature, patterns[1].signature)


class DraftDecisionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.pattern = SignatureCount(
            severity="ERROR",
            signature="postgres pool timed out while acquiring connection id=<id>",
            count=51,
            sample="ERROR postgres pool timed out while acquiring connection id=9234",
        )

    def test_threshold_crosses_only_above_named_limit(self) -> None:
        self.assertFalse(exceeds_threshold(49, 50))
        self.assertFalse(exceeds_threshold(50, 50))
        self.assertTrue(exceeds_threshold(51, 50))

        below = SignatureCount(**{**self.pattern.__dict__, "count": 50})
        self.assertEqual(decide_issue_drafts([below], [], threshold=50), [])
        crossed = decide_issue_drafts([self.pattern], [], threshold=50)
        self.assertEqual(len(crossed), 1)
        self.assertIsNotNone(crossed[0].draft)
        self.assertNotIn("9234", crossed[0].draft.body)
        self.assertIn("id=<id>", crossed[0].draft.body)

    def test_matching_open_issue_suppresses_draft(self) -> None:
        issue = OpenIssue(
            number=4249,
            title="fix(db): postgres pool timed out while acquiring connection",
            body="Repeated pool acquisition timeouts are visible in dcserver.",
            url="https://github.com/itismyfield/AgentDesk/issues/4249",
        )

        self.assertTrue(issue_matches_signature(self.pattern.signature, issue))
        decisions = decide_issue_drafts([self.pattern], [issue], threshold=50)
        self.assertEqual(len(decisions), 1)
        self.assertEqual(decisions[0].matching_issue, issue)
        self.assertIsNone(decisions[0].draft)

    def test_open_issue_dedup_considers_body_beyond_first_500_characters(self) -> None:
        issue = OpenIssue(
            number=4250,
            title="ops: recurring database degradation",
            body="unrelated preface " * 40 + "postgres pool timed out while acquiring connection",
        )

        self.assertTrue(issue_matches_signature(self.pattern.signature, issue))

    def test_unavailable_dedup_fails_closed_without_draft(self) -> None:
        decisions = decide_issue_drafts(
            [self.pattern],
            [],
            threshold=50,
            dedup_available=False,
        )
        self.assertEqual(len(decisions), 1)
        self.assertIsNone(decisions[0].draft)

    def test_issue_creation_is_default_off_mutation_guard(self) -> None:
        """Removing the shared approval check makes this mutation-style test fail."""

        calls: list[str] = []
        draft = IssueDraft(
            severity=self.pattern.severity,
            signature=self.pattern.signature,
            count=self.pattern.count,
            title="draft title",
            body="draft body",
        )

        decision = maybe_post_approved_drafts(
            [draft],
            "off",
            lambda item: calls.append(item.title) or "https://example.test/1",
        )

        self.assertFalse(decision.attempted)
        self.assertEqual(calls, [], "default mode must never invoke gh issue creation")
        with tempfile.TemporaryDirectory() as temp:
            written = write_pending_drafts([draft], Path(temp))[0]
            unreviewed = maybe_post_approved_drafts(
                [written],
                "confirmed",
                lambda item: calls.append(item.title) or "https://example.test/1",
            )
            self.assertFalse(unreviewed.attempted)
            self.assertEqual(calls, [], "confirmation alone cannot bypass per-draft review")

            Path(f"{written.path}.approved").touch()
            confirmed = maybe_post_approved_drafts(
                [written],
                "confirmed",
                lambda item: calls.append(item.title) or "https://example.test/1",
            )
            self.assertTrue(confirmed.attempted)
            self.assertEqual(calls, ["draft title"])

    def test_daily_summary_lists_top_patterns_crossings_and_drafts(self) -> None:
        decisions = decide_issue_drafts([self.pattern], [], threshold=50)
        with tempfile.TemporaryDirectory() as temp:
            drafts = write_pending_drafts(
                [decision.draft for decision in decisions if decision.draft],
                Path(temp),
            )
            summary = format_daily_summary(
                [self.pattern],
                decisions,
                drafts,
                threshold=50,
                window_label="2026-07-13 00:00–2026-07-14 00:00 UTC",
            )

        self.assertIn("ERROR top: 51× postgres pool timed out", summary)
        self.assertIn("WARN top: none", summary)
        self.assertIn("Threshold >50: 1 crossed", summary)
        self.assertIn("Crossed: 51× ERROR postgres pool timed out", summary)
        self.assertIn("Pending drafts:", summary)
        self.assertNotIn("Pending drafts: none", summary)


if __name__ == "__main__":
    unittest.main()
