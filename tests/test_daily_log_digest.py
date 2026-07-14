#!/usr/bin/env python3
"""Focused tests for the reusable daily log-digest draft pipeline (#4263)."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from datetime import datetime, timedelta, timezone
from io import StringIO
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
ROUTINE_DIR = ROOT / "routines" / "monitoring"
sys.path.insert(0, str(ROUTINE_DIR))

from log_digest_issue_drafts import (  # noqa: E402
    _MEASURED_NUMBER_RE,
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
import daily_log_digest  # noqa: E402
from daily_log_digest import (  # noqa: E402
    OPEN_ISSUE_LIMIT,
    dcserver_log_paths,
    load_open_issues,
    recent_log_lines,
    runtime_root,
)


class SignatureNormalizationTests(unittest.TestCase):
    def test_runtime_root_keeps_canonical_override_before_deploy_fallback(self) -> None:
        with patch.dict(
            os.environ,
            {"AGENTDESK_ROOT_DIR": "/canonical", "ADK_REL": "/deploy-fallback"},
            clear=False,
        ):
            self.assertEqual(runtime_root(), Path("/canonical"))
        with patch.dict(os.environ, {"ADK_REL": "/deploy-fallback"}, clear=True):
            self.assertEqual(runtime_root(), Path("/deploy-fallback"))

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

    def test_first_undated_observation_baselines_then_counts_only_append(self) -> None:
        now = datetime(2026, 7, 14, 0, 0, tzinfo=timezone.utc)
        since = now - timedelta(days=1)
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            launchd_stderr = root / "dcserver.launchd.stderr.log"
            checkpoint = root / "runtime" / "undated-offsets.json"
            launchd_stderr.write_text(
                "".join(f"ERROR panic stale-{index}\n" for index in range(60)),
                encoding="utf-8",
            )
            old_run = now - timedelta(days=3)
            os.utime(launchd_stderr, (old_run.timestamp(), old_run.timestamp()))

            baseline, baseline_warnings = recent_log_lines(
                [launchd_stderr],
                old_run - timedelta(days=1),
                old_run,
                undated_checkpoint=checkpoint,
            )
            with launchd_stderr.open("a", encoding="utf-8") as stream:
                stream.write("WARN fresh append today\n")
            os.utime(launchd_stderr, (now.timestamp(), now.timestamp()))
            fresh, fresh_warnings = recent_log_lines(
                [launchd_stderr], since, now, undated_checkpoint=checkpoint
            )
            repeated, repeated_warnings = recent_log_lines(
                [launchd_stderr],
                now,
                now + timedelta(days=1),
                undated_checkpoint=checkpoint,
            )

        self.assertEqual(baseline, [])
        self.assertEqual(fresh, ["WARN fresh append today"])
        self.assertEqual(repeated, [])
        self.assertEqual(baseline_warnings + fresh_warnings + repeated_warnings, [])

    def test_corrupt_checkpoint_also_baselines_undated_history(self) -> None:
        now = datetime(2026, 7, 14, 0, 0, tzinfo=timezone.utc)
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            launchd_stderr = root / "dcserver.launchd.stderr.log"
            checkpoint = root / "runtime" / "undated-offsets.json"
            launchd_stderr.write_text("ERROR old undated history\n" * 60, encoding="utf-8")
            os.utime(launchd_stderr, (now.timestamp(), now.timestamp()))
            checkpoint.parent.mkdir(parents=True)
            checkpoint.write_text("{broken", encoding="utf-8")

            baseline, warnings = recent_log_lines(
                [launchd_stderr],
                now - timedelta(days=1),
                now,
                undated_checkpoint=checkpoint,
            )

        self.assertEqual(baseline, [])
        self.assertEqual(len(warnings), 1)
        self.assertIn("could not load undated-line checkpoint", warnings[0])

    def test_truncate_then_regrow_past_offset_resets_same_inode_watermark(self) -> None:
        now = datetime(2026, 7, 14, 0, 0, tzinfo=timezone.utc)
        since = now - timedelta(days=1)
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            launchd_stderr = root / "dcserver.launchd.stderr.log"
            checkpoint = root / "runtime" / "undated-offsets.json"
            launchd_stderr.write_text("ERROR old history\n" * 8, encoding="utf-8")
            os.utime(launchd_stderr, (now.timestamp(), now.timestamp()))
            baseline, _ = recent_log_lines(
                [launchd_stderr], since, now, undated_checkpoint=checkpoint
            )
            old_offset = launchd_stderr.stat().st_size
            old_inode = launchd_stderr.stat().st_ino

            with launchd_stderr.open("w", encoding="utf-8") as stream:
                stream.write("ERROR new content after truncation\n" * 10)
            os.utime(launchd_stderr, (now.timestamp(), now.timestamp()))
            fresh, warnings = recent_log_lines(
                [launchd_stderr], since, now, undated_checkpoint=checkpoint
            )
            new_offset = launchd_stderr.stat().st_size
            new_inode = launchd_stderr.stat().st_ino

        self.assertEqual(baseline, [])
        self.assertEqual(new_inode, old_inode)
        self.assertGreater(new_offset, old_offset)
        self.assertEqual(fresh, ["ERROR new content after truncation"] * 10)
        self.assertEqual(warnings, [])

    def test_truncate_regrow_with_same_tail_resets_then_plain_append_resumes(self) -> None:
        now = datetime(2026, 7, 14, 0, 0, tzinfo=timezone.utc)
        since = now - timedelta(days=1)
        tail = "ERROR tail unchanged marker\n" * 8
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            launchd_stderr = root / "dcserver.launchd.stderr.log"
            checkpoint = root / "runtime" / "undated-offsets.json"
            launchd_stderr.write_text("ERROR old prelude\n" + tail, encoding="utf-8")
            os.utime(launchd_stderr, (now.timestamp(), now.timestamp()))
            baseline, baseline_warnings = recent_log_lines(
                [launchd_stderr], since, now, undated_checkpoint=checkpoint
            )
            old_inode = launchd_stderr.stat().st_ino
            old_offset = launchd_stderr.stat().st_size

            rewritten = (
                "ERROR new prelude\n"
                + tail
                + "ERROR appended beyond old offset\n"
            )
            with launchd_stderr.open("w", encoding="utf-8") as stream:
                stream.write(rewritten)
            os.utime(launchd_stderr, (now.timestamp(), now.timestamp()))
            new_inode = launchd_stderr.stat().st_ino
            fresh, fresh_warnings = recent_log_lines(
                [launchd_stderr], since, now, undated_checkpoint=checkpoint
            )

            with launchd_stderr.open("a", encoding="utf-8") as stream:
                stream.write("ERROR plain append after rewrite\n")
            appended, append_warnings = recent_log_lines(
                [launchd_stderr], since, now, undated_checkpoint=checkpoint
            )

        self.assertEqual(baseline, [])
        self.assertEqual(new_inode, old_inode)
        self.assertGreater(len(rewritten.encode()), old_offset)
        self.assertEqual(
            fresh,
            ["ERROR new prelude"]
            + ["ERROR tail unchanged marker"] * 8
            + ["ERROR appended beyond old offset"],
        )
        self.assertEqual(appended, ["ERROR plain append after rewrite"])
        self.assertEqual(baseline_warnings + fresh_warnings + append_warnings, [])

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

    def test_short_embedded_request_ids_collapse_and_cross_threshold(self) -> None:
        ids = ("4f2a", "8c1d", "0b7e", "9d3c")
        lines = [
            f"ERROR failed to open /tmp/req-{ids[index % len(ids)]}/data"
            for index in range(100)
        ]
        patterns = aggregate_normalized_signatures(lines)

        self.assertEqual(len(patterns), 1)
        self.assertEqual(patterns[0].count, 100)
        self.assertIn("req-<id>", patterns[0].signature)
        self.assertTrue(exceeds_threshold(patterns[0].count, 50))

    def test_unit_suffixed_numbers_collapse_without_touching_identifiers(self) -> None:
        duration_patterns = aggregate_normalized_signatures(
            ["ERROR request took 123ms", "ERROR request took 456ms"]
        )
        size_patterns = aggregate_normalized_signatures(
            ["WARN payload reached 512kb", "WARN payload reached 1mb"]
        )

        self.assertEqual(len(duration_patterns), 1)
        self.assertEqual(duration_patterns[0].count, 2)
        self.assertIn("<dur>", duration_patterns[0].signature)
        self.assertEqual(len(size_patterns), 1)
        self.assertEqual(size_patterns[0].count, 2)
        self.assertIn("<size>", size_patterns[0].signature)
        for value, placeholder in (
            ("1.5s", "<dur>"),
            ("20us", "<dur>"),
            ("8gib", "<size>"),
            ("99%", "<rate>"),
            ("20/s", "<rate>"),
            ("30req/s", "<rate>"),
        ):
            with self.subTest(value=value):
                self.assertIn(placeholder, normalize_signature(f"ERROR measured {value}"))
        self.assertEqual(
            normalize_signature("ERROR error500handler failed"),
            "error500handler failed",
        )

    def test_measured_numbers_require_standalone_token_boundaries(self) -> None:
        for first, second in (
            ("cache_1mb_loader", "cache_2mb_loader"),
            ("cache-1mb-loader", "cache-2mb-loader"),
            ("cache.1mb.loader", "cache.2mb.loader"),
        ):
            with self.subTest(first=first, second=second):
                patterns = aggregate_normalized_signatures(
                    [f"ERROR {first} failed", f"ERROR {second} failed"]
                )
                self.assertEqual(len(patterns), 2)
                self.assertNotEqual(patterns[0].signature, patterns[1].signature)

        for first, second, expected_signature in (
            ("request took 123ms.", "request took 456ms.", "request took <dur>."),
            ("size 512kb.", "size 1mb.", "size <size>."),
            ("request took 123ms,", "request took 456ms,", "request took <dur>,"),
            ("request took (123ms)", "request took (456ms)", "request took (<dur>)"),
            ("request took 123ms;", "request took 456ms;", "request took <dur>;"),
            ("request took 123ms:", "request took 456ms:", "request took <dur>"),
        ):
            with self.subTest(first=first, second=second):
                patterns = aggregate_normalized_signatures(
                    [f"ERROR {first}", f"ERROR {second}"]
                )
                self.assertEqual(len(patterns), 1)
                self.assertEqual(patterns[0].count, 2)
                self.assertEqual(patterns[0].signature, expected_signature)

        for first, second, placeholder in (
            ("took 123ms", "took 456ms", "<dur>"),
            ("512kb", "1mb", "<size>"),
            ("took 1.5s", "took 2.5s", "<dur>"),
        ):
            with self.subTest(first=first, second=second):
                patterns = aggregate_normalized_signatures(
                    [f"ERROR {first}", f"ERROR {second}"]
                )
                self.assertEqual(len(patterns), 1)
                self.assertEqual(patterns[0].count, 2)
                self.assertIn(placeholder, patterns[0].signature)

        self.assertIsNone(_MEASURED_NUMBER_RE.search("0.144.1"))

    def test_http_status_codes_remain_distinct(self) -> None:
        self.assertNotEqual(
            normalize_signature("ERROR HTTP 500 upstream error"),
            normalize_signature("ERROR HTTP 404 upstream error"),
        )

    def test_port_numbers_remain_distinct(self) -> None:
        self.assertNotEqual(
            normalize_signature("ERROR connection to port 5432 refused"),
            normalize_signature("ERROR connection to port 6379 refused"),
        )


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

    def test_unrelated_long_issue_body_does_not_suppress_short_signature(self) -> None:
        pattern = SignatureCount(
            severity="WARN",
            signature="worker lease expired",
            count=60,
            sample="WARN worker lease expired",
        )
        issue = OpenIssue(
            number=4250,
            title="epic: routine runtime hardening",
            body=(
                "This broad epic discusses workers, scheduling, and resilience.\n"
                + "unrelated planning context " * 80
                + "worker ownership and lease cleanup after expired sessions"
            ),
        )

        self.assertFalse(issue_matches_signature(pattern.signature, issue))
        decisions = decide_issue_drafts([pattern], [issue], threshold=50)
        self.assertIsNotNone(decisions[0].draft)

    def test_genuine_short_signature_duplicate_is_suppressed(self) -> None:
        pattern = SignatureCount(
            severity="WARN",
            signature="worker lease expired",
            count=60,
            sample="WARN worker lease expired",
        )
        issue = OpenIssue(number=4251, title="fix(runtime): worker lease expired")

        self.assertTrue(issue_matches_signature(pattern.signature, issue))
        decisions = decide_issue_drafts([pattern], [issue], threshold=50)
        self.assertIsNone(decisions[0].draft)

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

        with tempfile.TemporaryDirectory() as temp:
            written = write_pending_drafts([draft], Path(temp))[0]
            Path(f"{written.path}.approved").touch()

            with patch.dict(os.environ, {}, clear=True):
                unset_mode = os.environ.get("AGENTDESK_LOG_DIGEST_CREATE_ISSUE", "off")
            for mode in (unset_mode, "off", "invalid"):
                disabled = maybe_post_approved_drafts(
                    [written],
                    mode,
                    lambda item: calls.append(item.title) or "https://example.test/1",
                )
                self.assertFalse(disabled.attempted)
                self.assertEqual(calls, [], f"mode {mode!r} must suppress an approved draft")

            Path(f"{written.path}.approved").unlink()
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
        self.assertIn("best-effort signatures; verify top patterns manually", summary)
        self.assertIn("Threshold >50: 1 crossed", summary)
        self.assertIn("Crossed: 51× ERROR postgres pool timed out", summary)
        self.assertIn("Pending drafts:", summary)
        self.assertNotIn("Pending drafts: none", summary)


class DailyDigestIntegrationTests(unittest.TestCase):
    def _write_threshold_crossing(self, root: Path) -> None:
        logs = root / "logs"
        logs.mkdir(parents=True)
        (logs / "dcserver.stdout.log").write_text(
            "".join(
                f"2026-07-13T12:00:{index % 60:02d}Z ERROR worker lease expired id={index}\n"
                for index in range(51)
            ),
            encoding="utf-8",
        )

    def test_main_gh_failure_wires_dedup_unavailable_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self._write_threshold_crossing(root)
            completed = subprocess.CompletedProcess(
                args=["gh"], returncode=1, stdout="", stderr="simulated gh failure"
            )
            output = StringIO()
            with (
                patch.object(
                    sys,
                    "argv",
                    [
                        "daily_log_digest.py",
                        "--root",
                        str(root),
                        "--now",
                        "2026-07-14T00:00:00Z",
                    ],
                ),
                patch.object(daily_log_digest.subprocess, "run", return_value=completed),
                patch.dict(os.environ, {"AGENTDESK_LOG_DIGEST_CREATE_ISSUE": "off"}, clear=False),
                redirect_stdout(output),
            ):
                rc = daily_log_digest.main()

            drafts = list(
                (root / "runtime" / "pending-issue-drafts" / "daily-log-digest").glob("*.md")
            )

        self.assertEqual(rc, 0)
        self.assertEqual(drafts, [])
        self.assertIn("dedup unavailable", output.getvalue())
        self.assertIn("drafts suppressed", output.getvalue())

    def test_invalid_threshold_env_warns_and_falls_back_to_50(self) -> None:
        for invalid in ("0", "-4", "not-a-number"):
            with self.subTest(invalid=invalid), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                self._write_threshold_crossing(root)
                output = StringIO()
                with (
                    patch.object(
                        sys,
                        "argv",
                        [
                            "daily_log_digest.py",
                            "--root",
                            str(root),
                            "--now",
                            "2026-07-14T00:00:00Z",
                        ],
                    ),
                    patch.object(daily_log_digest, "load_open_issues", return_value=([], None)),
                    patch.dict(
                        os.environ,
                        {
                            "AGENTDESK_LOG_DIGEST_THRESHOLD": invalid,
                            "AGENTDESK_LOG_DIGEST_CREATE_ISSUE": "off",
                        },
                        clear=False,
                    ),
                    redirect_stdout(output),
                ):
                    rc = daily_log_digest.main()

                self.assertEqual(rc, 0)
                self.assertIn("invalid AGENTDESK_LOG_DIGEST_THRESHOLD", output.getvalue())
                self.assertIn("using default 50", output.getvalue())
                self.assertIn("Threshold >50: 1 crossed", output.getvalue())

    def test_open_issue_cap_is_fail_closed_with_warning(self) -> None:
        payload = [
            {"number": index, "title": f"issue {index}", "body": "", "url": ""}
            for index in range(1, OPEN_ISSUE_LIMIT + 1)
        ]
        completed = SimpleNamespace(returncode=0, stdout=json.dumps(payload), stderr="")
        with patch.object(daily_log_digest.subprocess, "run", return_value=completed):
            issues, warning = load_open_issues("owner/repo")

        self.assertEqual(len(issues), OPEN_ISSUE_LIMIT)
        self.assertIsNotNone(warning)
        self.assertIn("may be truncated", warning)
        pattern = SignatureCount("ERROR", "brand new failure pattern", 51, "ERROR sample")
        decisions = decide_issue_drafts(
            [pattern], issues, threshold=50, dedup_available=warning is None
        )
        self.assertIsNone(decisions[0].draft)


if __name__ == "__main__":
    unittest.main()
