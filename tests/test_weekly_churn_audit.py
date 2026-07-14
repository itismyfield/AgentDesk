#!/usr/bin/env python3
"""Focused tests for the weekly regression-churn audit (#4265)."""

from __future__ import annotations

import os
import subprocess
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
from log_digest_issue_drafts import OpenIssue, stable_draft_filename  # noqa: E402
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
            "fix!: stop breaking duplicate relay",
            "fix(discord)!: stop breaking duplicate relay",
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

    def test_squash_pr_suffixes_do_not_create_issue_generations(self) -> None:
        subject = "fix(deploy): #4262 post-deploy scope (#4511) (#4523)"

        self.assertTrue(is_fix_commit_subject(subject))
        self.assertEqual(issue_references(subject), (4262,))
        self.assertEqual(compute_issue_lineages([commit(subject)])[0].issues, (4262,))
        self.assertEqual(issue_references("fix(deploy): #100 (#101) (#102)"), (100,))
        self.assertEqual(
            issue_references("fix: #100 see #99", "Regression-of: #98"),
            (100, 99, 98),
        )

    def test_issue_lineage_generation_count_spans_commit_text_edges(self) -> None:
        commits = [
            commit("fix: #100 first regression", body="Regression-of cross-reference: #200"),
            commit("fix: #200 follow-up", body="Regression-of cross-reference: #300"),
            commit("fix: independent #900"),
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
        created = weekly_churn_audit.build_candidate_draft(candidate, "7 days", 3)
        matching = OpenIssue(number=4265, title=created.title, body=created.body)

        drafts, matches = candidate_drafts(
            [candidate], [matching], since="7 days", threshold=3
        )

        self.assertEqual(drafts, [])
        self.assertEqual(matches, [(candidate, matching)])

    def test_created_issue_marker_prevents_duplicate_on_second_run(self) -> None:
        audit_commits = [
            commit(f"fix: repeat {index}", ("justfile",), sha=str(index) * 40)
            for index in range(1, 4)
        ]
        candidate = analyze_churn(audit_commits, threshold=3).candidates[0]
        draft = weekly_churn_audit.build_candidate_draft(candidate, "7 days", 3)
        open_issues: list[OpenIssue] = []

        def load_open(_repo: str) -> tuple[list[OpenIssue], None]:
            return list(open_issues), None

        def create_issue(_repo: str, approved) -> str:
            open_issues.append(
                OpenIssue(number=4265, title=approved.title, body=approved.body)
            )
            return "https://example.test/issues/4265"

        with tempfile.TemporaryDirectory() as temp:
            pending = (
                Path(temp)
                / "runtime"
                / "pending-issue-drafts"
                / "weekly-churn-audit"
            )
            pending.mkdir(parents=True)
            draft_name = stable_draft_filename(draft)
            Path(f"{pending / draft_name}.approved").touch()
            argv = [
                "weekly_churn_audit.py",
                "--repo-root",
                str(ROOT),
                "--runtime-root",
                temp,
            ]
            with (
                patch.object(sys, "argv", argv),
                patch.dict(
                    os.environ,
                    {"AGENTDESK_CHURN_AUDIT_CREATE_ISSUE": "confirmed"},
                    clear=True,
                ),
                patch.object(
                    weekly_churn_audit,
                    "collect_git_commits",
                    return_value=audit_commits,
                ),
                patch.object(
                    weekly_churn_audit, "load_open_issues", side_effect=load_open
                ),
                patch.object(
                    weekly_churn_audit, "create_github_issue", side_effect=create_issue
                ) as create,
                redirect_stdout(StringIO()),
            ):
                self.assertEqual(weekly_churn_audit.main(), 0)
                self.assertEqual(weekly_churn_audit.main(), 0)

        create.assert_called_once()

    def test_git_replaces_non_utf8_commit_text(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            subprocess.run(
                ["git", "-C", str(repo), "config", "user.name", "Audit Test"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", str(repo), "config", "user.email", "audit@example.test"],
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(repo),
                    "config",
                    "i18n.commitEncoding",
                    "ISO-8859-1",
                ],
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(repo),
                    "config",
                    "i18n.logOutputEncoding",
                    "ISO-8859-1",
                ],
                check=True,
            )
            (repo / "sample.txt").write_text("sample\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(repo), "add", "sample.txt"], check=True)
            subprocess.run(
                ["git", "-C", str(repo), "commit", "-q", "-F", "-"],
                input=b"fix: latin-1 \xff message\n",
                check=True,
            )

            commits = weekly_churn_audit.collect_git_commits(repo, "7 days")

        self.assertEqual(len(commits), 1)
        self.assertIn("\ufffd", commits[0].subject)
        self.assertEqual(commits[0].files, ("sample.txt",))

    def test_invalid_env_thresholds_fall_back_and_log(self) -> None:
        for value in ("abc", "0", "-1"):
            with (
                self.subTest(value=value),
                patch.object(sys, "argv", ["weekly_churn_audit.py"]),
                patch.dict(
                    os.environ,
                    {"AGENTDESK_CHURN_AUDIT_THRESHOLD": value},
                    clear=True,
                ),
                patch.object(weekly_churn_audit, "log") as audit_log,
            ):
                args = weekly_churn_audit.parse_args()

            self.assertEqual(args.threshold, weekly_churn_audit.DEFAULT_THRESHOLD)
            audit_log.assert_called_once()
            self.assertIn(value, audit_log.call_args.args[0])

    def test_dense_cyclic_lineage_search_is_bounded_and_logged(self) -> None:
        component = set(range(1, 8))
        edges = {node: component - {node} for node in component}
        with (
            patch.object(weekly_churn_audit, "LINEAGE_PATH_STATE_LIMIT", 25),
            patch.object(weekly_churn_audit, "log") as audit_log,
        ):
            lineage = weekly_churn_audit._longest_lineage(component, edges)

        self.assertTrue(lineage)
        self.assertLessEqual(len(lineage), len(component))
        audit_log.assert_called_once()
        self.assertIn("truncated at 25 path states", audit_log.call_args.args[0])

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

    def test_confirmed_issue_gate_still_requires_per_draft_approval(self) -> None:
        audit_commits = [
            commit(f"fix: repeat {index}", sha=str(index) * 40)
            for index in range(1, 4)
        ]
        with tempfile.TemporaryDirectory() as temp:
            argv = [
                "weekly_churn_audit.py",
                "--repo-root",
                str(ROOT),
                "--runtime-root",
                temp,
            ]
            with (
                patch.object(sys, "argv", argv),
                patch.dict(
                    os.environ,
                    {"AGENTDESK_CHURN_AUDIT_CREATE_ISSUE": "confirmed"},
                    clear=True,
                ),
                patch.object(
                    weekly_churn_audit,
                    "collect_git_commits",
                    return_value=audit_commits,
                ),
                patch.object(
                    weekly_churn_audit, "load_open_issues", return_value=([], None)
                ) as load_open,
                patch.object(
                    weekly_churn_audit,
                    "create_github_issue",
                    return_value="https://example.test/issues/1",
                ) as create_issue,
                redirect_stdout(StringIO()),
            ):
                self.assertEqual(weekly_churn_audit.main(), 0)
                draft = next(
                    (Path(temp) / "runtime" / "pending-issue-drafts").rglob("*.md")
                )
                create_issue.assert_not_called()
                Path(f"{draft}.approved").touch()
                self.assertEqual(weekly_churn_audit.main(), 0)

        self.assertEqual(load_open.call_count, 2)
        create_issue.assert_called_once()


if __name__ == "__main__":
    unittest.main()
