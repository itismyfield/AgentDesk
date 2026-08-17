from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "refresh_giant_file_issue_metadata.py"

_SPEC = importlib.util.spec_from_file_location(
    "refresh_giant_file_issue_metadata", SCRIPT_PATH
)
REFRESH = importlib.util.module_from_spec(_SPEC)
assert _SPEC.loader is not None
sys.modules[_SPEC.name] = REFRESH
_SPEC.loader.exec_module(REFRESH)


class RefreshGiantFileIssueMetadataTest(unittest.TestCase):
    def test_refresh_preserves_scope_and_updates_live_fields(self) -> None:
        payload = {
            "schema_version": 2,
            "refreshed_at": "2026-08-01T00:00:00Z",
            "ratchets": {
                "closed_deadline_entries": 1,
                "transition_list_entries": 1,
            },
            "issues": [
                {
                    "number": 42,
                    "state": "open",
                    "title": "old title",
                    "owners": ["team"],
                    "files": ["src/a.rs"],
                }
            ],
        }
        live = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout=json.dumps(
                {"number": 42, "state": "CLOSED", "title": "live title"}
            ),
            stderr="",
        )
        with patch.object(REFRESH.subprocess, "run", return_value=live) as run:
            refreshed = REFRESH.refreshed_snapshot(
                payload,
                "owner/repo",
                datetime(2026, 8, 12, 10, 30, tzinfo=timezone.utc),
            )
        self.assertEqual(refreshed["refreshed_at"], "2026-08-12T10:30:00Z")
        self.assertEqual(refreshed["issues"][0]["state"], "closed")
        self.assertEqual(refreshed["issues"][0]["title"], "live title")
        self.assertEqual(refreshed["issues"][0]["owners"], ["team"])
        self.assertEqual(refreshed["issues"][0]["files"], ["src/a.rs"])
        self.assertEqual(
            refreshed["ratchets"],
            {"closed_deadline_entries": 1, "transition_list_entries": 1},
        )
        self.assertIn("owner/repo", run.call_args.args[0])

    def test_writer_lowers_both_ratchets_after_shrink(self) -> None:
        payload = {
            "schema_version": 2,
            "refreshed_at": "2026-08-12T10:30:00Z",
            "ratchets": {
                "closed_deadline_entries": 3,
                "transition_list_entries": 3,
            },
            "issues": [
                {"number": 1, "state": "closed"},
                {"number": 2, "state": "open"},
            ],
        }
        entries = [
            {
                "file": "src/closed.rs",
                "owner": "team",
                "deadline": "2026-12-31",
                "decompose_issue": "#1",
            },
            {
                "file": "src/open.rs",
                "owner": "team",
                "deadline": "2026-12-31",
                "decompose_issue": "#2",
            },
        ]

        lowered = REFRESH.lower_ratchet_baselines(
            payload, entries, {"src/closed.rs"}
        )

        self.assertEqual(
            lowered["ratchets"],
            {"closed_deadline_entries": 1, "transition_list_entries": 1},
        )

    def test_writer_never_raises_ratchets(self) -> None:
        payload = {
            "schema_version": 2,
            "refreshed_at": "2026-08-12T10:30:00Z",
            "ratchets": {
                "closed_deadline_entries": 0,
                "transition_list_entries": 0,
            },
            "issues": [{"number": 1, "state": "closed"}],
        }
        entries = [
            {
                "file": "src/closed.rs",
                "owner": "team",
                "deadline": "2026-12-31",
                "decompose_issue": "#1",
            }
        ]

        unchanged = REFRESH.lower_ratchet_baselines(
            payload, entries, {"src/closed.rs"}
        )

        self.assertEqual(
            unchanged["ratchets"],
            {"closed_deadline_entries": 0, "transition_list_entries": 0},
        )

    def test_refresh_fails_closed_on_invalid_live_state(self) -> None:
        live = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout=json.dumps({"number": 42, "state": "MERGED", "title": "title"}),
            stderr="",
        )
        with patch.object(REFRESH.subprocess, "run", return_value=live):
            with self.assertRaises(REFRESH.RefreshError):
                REFRESH.github_issue(42, "owner/repo")

    def test_refresh_fails_closed_when_gh_fails(self) -> None:
        failure = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="", stderr="authentication failed"
        )
        with patch.object(REFRESH.subprocess, "run", return_value=failure):
            with self.assertRaises(REFRESH.RefreshError) as ctx:
                REFRESH.github_issue(42, "owner/repo")
        self.assertIn("authentication failed", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
