#!/usr/bin/env python3
"""Focused tests for the ratchet cap-admission guard (#4269)."""

from __future__ import annotations

import contextlib
import io
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts.ratchet_admission import (
    ADMISSION_WARN_THRESHOLD,
    WIRING_SLACK_LINES,
    AdmissionEvent,
    admission_warning_messages,
    validate_admission_delta,
)

# check_hotfile_ratchet imports ratchet_admission as a top-level module.
SCRIPTS_DIR = Path(__file__).resolve().parent
if str(SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_DIR))
import check_hotfile_ratchet  # noqa: E402


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


class HotfileWiringSlackTest(unittest.TestCase):
    CEILING = 100

    def _run(self, grown: int) -> tuple[int, str, str]:
        """Run the hot-file check on a fixture tree; the first file is ``grown``
        lines over its ceiling, the second is at it, the third one line under."""

        grown_rel, at_rel, under_rel = check_hotfile_ratchet.REQUIRED_HOTFILES
        sizes = {grown_rel: self.CEILING + grown, at_rel: self.CEILING,
                 under_rel: self.CEILING - 1}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for rel, lines in sizes.items():
                (root / rel).parent.mkdir(parents=True, exist_ok=True)
                (root / rel).write_text("x\n" * lines, encoding="utf-8")
            manifest = root / "scripts" / "hotfile_ratchet.toml"
            manifest.parent.mkdir(parents=True, exist_ok=True)
            manifest.write_text(
                "[hotfile_ratchet]\n"
                + "".join(f'"{rel}" = {self.CEILING}\n' for rel in sizes),
                encoding="utf-8",
            )
            (root / "scripts" / "ratchet_admission_history.toml").write_text(
                "schema_version = 1\nadmission = []\n", encoding="utf-8"
            )
            out, err = io.StringIO(), io.StringIO()
            with mock.patch.object(check_hotfile_ratchet, "REPO_ROOT", root), \
                    mock.patch.object(check_hotfile_ratchet, "MANIFEST", manifest), \
                    contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                rc = check_hotfile_ratchet.main()
        return rc, out.getvalue(), err.getvalue()

    def test_growth_within_wiring_slack_passes_without_a_lock_in_note(self) -> None:
        self.assertEqual(WIRING_SLACK_LINES, 30)
        self.assertEqual(check_hotfile_ratchet.WIRING_SLACK_LINES, WIRING_SLACK_LINES)
        grown_rel, _, under_rel = check_hotfile_ratchet.REQUIRED_HOTFILES
        for grown in (1, WIRING_SLACK_LINES):
            with self.subTest(grown=grown):
                rc, out, err = self._run(grown)
                self.assertEqual(rc, 0, err)
                self.assertNotIn("FAIL", err)
                self.assertIn(
                    f"OK: {grown_rel} = {self.CEILING + grown} lines "
                    f"(ceiling {self.CEILING} + slack {WIRING_SLACK_LINES}",
                    out,
                )
                self.assertNotIn(f"NOTE: {grown_rel}", out)
                # Shrink lock-in still compares against the frozen ceiling.
                self.assertIn(f"Lower ceiling to {self.CEILING - 1}", out)
                self.assertEqual(out.count("NOTE:"), 1)

    def test_growth_past_wiring_slack_fails_naming_ceiling_and_slack(self) -> None:
        grown_rel = check_hotfile_ratchet.REQUIRED_HOTFILES[0]
        rc, _, err = self._run(WIRING_SLACK_LINES + 1)
        self.assertEqual(rc, 1)
        self.assertIn(
            f"FAIL: {grown_rel} grew to {self.CEILING + WIRING_SLACK_LINES + 1} "
            f"lines > ceiling {self.CEILING} + slack {WIRING_SLACK_LINES}.",
            err,
        )


if __name__ == "__main__":
    unittest.main()
