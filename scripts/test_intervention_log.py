#!/usr/bin/env python3
"""Focused tests for the manual-intervention recurrence recorder (#4264)."""

from __future__ import annotations

import contextlib
import fcntl
import io
import threading
import tempfile
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from scripts.intervention_log import (
    INTERVENTION_RECURRENCE_THRESHOLD,
    InterventionEvent,
    build_draft_body,
    crosses_threshold,
    main,
    parse_history,
    record_intervention,
)


class InterventionLogTest(unittest.TestCase):
    @staticmethod
    def _schema_history(value: str) -> str:
        return f"schema_version = {value}\n"

    def _seed(self, root: Path, count: int = 0) -> Path:
        path = root / "scripts" / "intervention_history.toml"
        path.parent.mkdir(parents=True)
        rows = ["schema_version = 1", ""]
        for value in range(1, count + 1):
            rows.extend(
                [
                    "[[intervention]]",
                    'type = "marker-clear"',
                    f'timestamp = "2026-06-18T09:1{value}:00Z"',
                    'node = "mac-mini"',
                    f'note = "seed {value}"',
                    f"count = {value}",
                    "",
                ]
            )
        path.write_text("\n".join(rows), encoding="utf-8")
        return path

    def _record(
        self,
        root: Path,
        history: Path,
        type: str,
        calls: list[list[str]],
        environ: dict[str, str] | None = None,
    ):
        def fake_runner(command, **_kwargs):
            calls.append(command)
            return SimpleNamespace(returncode=0, stdout="url", stderr="")

        return record_intervention(
            type=type,
            note=f"test {type}",
            node="mac-mini",
            issue=None,
            history_path=history,
            logs_dir=root / "logs",
            environ=environ or {},
            runner=fake_runner,
            timestamp="2026-07-14T00:00:00Z",
        )

    def test_counts_are_monotonic_and_isolated_per_type(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            history = self._seed(root)
            calls: list[list[str]] = []
            first = self._record(root, history, "marker-clear", calls)
            restart = self._record(root, history, "force-restart", calls)
            second = self._record(root, history, "marker-clear", calls)

            self.assertEqual(first.event.count, 1)
            self.assertEqual(restart.event.count, 1)
            self.assertEqual(second.event.count, 2)
            self.assertEqual([event.count for event in parse_history(history.read_text())], [1, 1, 2])

    def test_concurrent_records_serialize_read_count_append(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            history = self._seed(root)
            second_lock_attempted = threading.Event()
            counter_lock = threading.Lock()
            lock_attempts = 0
            first_parse = True
            original_flock = fcntl.flock
            original_parse = parse_history

            def observed_flock(file_descriptor: int, operation: int) -> None:
                nonlocal lock_attempts
                if operation == fcntl.LOCK_EX:
                    with counter_lock:
                        lock_attempts += 1
                        if lock_attempts == 2:
                            second_lock_attempted.set()
                original_flock(file_descriptor, operation)

            def parse_after_both_lock_attempts(text: str):
                nonlocal first_parse
                with counter_lock:
                    wait_for_second = first_parse
                    first_parse = False
                if wait_for_second and not second_lock_attempted.wait(timeout=5):
                    raise AssertionError("second record did not attempt the store lock")
                return original_parse(text)

            with (
                mock.patch(
                    "scripts.intervention_log.fcntl.flock", side_effect=observed_flock
                ),
                mock.patch(
                    "scripts.intervention_log.parse_history",
                    side_effect=parse_after_both_lock_attempts,
                ),
                ThreadPoolExecutor(max_workers=2) as executor,
            ):
                futures = [
                    executor.submit(
                        self._record, root, history, "marker-clear", []
                    )
                    for _ in range(2)
                ]
                results = [future.result(timeout=10) for future in futures]

            self.assertEqual(sorted(result.event.count for result in results), [1, 2])
            stored = history.read_text(encoding="utf-8")
            self.assertEqual(
                [event.count for event in parse_history(stored)],
                [1, 2],
            )
            self.assertIn("count = 2", stored)

    def test_out_of_sequence_count_is_repaired_and_recording_continues(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            history = self._seed(root, 2)
            corrupted = history.read_text(encoding="utf-8").replace(
                "count = 2", "count = 1"
            )
            history.write_text(corrupted, encoding="utf-8")

            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                repaired = parse_history(corrupted)
                result = self._record(root, history, "marker-clear", [])

            self.assertEqual([event.count for event in repaired], [1, 2])
            self.assertEqual(result.event.count, 3)
            self.assertIn("using positional count 2", stderr.getvalue())
            stored = history.read_text(encoding="utf-8")
            self.assertIn("count = 3", stored)
            self.assertEqual(
                [event.count for event in parse_history(stored)],
                [1, 2, 3],
            )

    def test_schema_version_true_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            parse_history(self._schema_history("true"))

    def test_schema_version_float_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            parse_history(self._schema_history("1.0"))

    def test_schema_version_string_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            parse_history(self._schema_history('"1"'))

    def test_schema_version_integer_is_accepted(self) -> None:
        self.assertEqual(parse_history(self._schema_history("1")), [])

    def test_threshold_crossing_builds_redesign_candidate_draft(self) -> None:
        below = INTERVENTION_RECURRENCE_THRESHOLD
        crossing = INTERVENTION_RECURRENCE_THRESHOLD + 1
        self.assertFalse(crosses_threshold(below))
        self.assertTrue(crosses_threshold(crossing))
        event = InterventionEvent(
            "marker-clear", "2026-07-14T00:00:00Z", "mac-mini", "test", None, crossing
        )
        draft = build_draft_body("marker-clear", crossing, [event])
        self.assertIn("marker-clear", draft)
        self.assertIn(str(crossing), draft)
        self.assertIn("재설계 후보", draft)

        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            history = self._seed(root, below - 1)
            result = self._record(root, history, "marker-clear", [])
            self.assertEqual(result.event.count, below)
            self.assertIsNone(result.draft_path)

    def test_gh_issue_creation_is_default_off_and_literal_confirmed_only(self) -> None:
        for environ in ({}, {"AGENTDESK_INTERVENTION_CREATE_ISSUE": "off"}):
            with self.subTest(environ=environ), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                history = self._seed(root, INTERVENTION_RECURRENCE_THRESHOLD)
                calls: list[list[str]] = []
                result = self._record(root, history, "marker-clear", calls, environ)
                self.assertIsNotNone(result.draft_path)
                self.assertTrue(result.draft_path.is_file())
                self.assertEqual(calls, [])

        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            history = self._seed(root, INTERVENTION_RECURRENCE_THRESHOLD)
            calls = []
            result = self._record(
                root,
                history,
                "marker-clear",
                calls,
                {"AGENTDESK_INTERVENTION_CREATE_ISSUE": "confirmed"},
            )
            self.assertTrue(result.draft_path.is_file())
            self.assertEqual(len(calls), 1)
            self.assertEqual(calls[0][:3], ["gh", "issue", "create"])

    def test_validation_and_history_round_trip(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            history = self._seed(root)
            self._record(root, history, "re-baseline", [])
            reparsed = parse_history(history.read_text(encoding="utf-8"))
            self.assertEqual(reparsed[0].type, "re-baseline")
            self.assertIn("schema_version = 1", history.read_text(encoding="utf-8"))

        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit):
            main(["record", "--type", "unknown", "--note", "test"])
        with contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit):
            main(
                [
                    "record",
                    "--type",
                    "marker-clear",
                    "--note",
                    "test",
                    "--issue",
                    "#4206",
                ]
            )
        with contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit):
            main(
                [
                    "record",
                    "--type",
                    "marker-clear",
                    "--note",
                    "test",
                    "--issue",
                    "not-an-int",
                ]
            )


if __name__ == "__main__":
    unittest.main()
