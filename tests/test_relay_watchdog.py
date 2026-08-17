"""Unit tests for scripts/relay_watchdog.py (#4381).

These nail the judgment logic that the 2026-07-09 incident proved must never
regress silently:

- project-dir resolution is DYNAMIC (no pinned worktree/session) and EXCLUDES
  thread sessions, which relay to a different channel;
- the LOST/GAP verdict uses the last-good-delivery watermark with grace and
  gap-alert boundaries exactly as calibrated during the incident.
"""

from __future__ import annotations

import json
import os
import stat
import subprocess
import tempfile
import time
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest import mock

import scripts.relay_watchdog as relay_watchdog
from scripts.relay_watchdog import (
    COVERAGE_ACTIVITY_FRESH_SECS,
    COVERAGE_CONFIRM_TICKS,
    COVERAGE_COVERED,
    COVERAGE_UNCOVERED,
    COVERAGE_UNKNOWN,
    DELIVERED_WATERMARKS_KEY,
    DISCORD_READ_LIMIT,
    EXTERNAL_VERDICT_EPOCH_KEY,
    MAX_DELIVERED_WATERMARKS,
    PG_OK,
    PG_STATE_KEY,
    PG_TOPOLOGY_DIRECT,
    PG_TUNNEL_DOWN,
    PG_UNCLASSIFIED_DOWN,
    PG_UNKNOWN,
    PG_UPSTREAM_DOWN,
    SELECTOR_DIVERGED,
    SELECTED_TRANSCRIPT_KEY,
    SELECTOR_SYNCED,
    SELECTOR_UNKNOWN,
    STATE_GAP,
    STATE_LAGGING,
    STATE_OK,
    ChannelConfig,
    Config,
    ConfigError,
    CoverageActivityProbe,
    CoverageTranscriptProbe,
    Runtime,
    TranscriptCandidate,
    Verdict,
    WatcherStateProbe,
    advance_delivered_watermark,
    assistant_blocks_from_lines,
    bounded_read_is_complete,
    build_external_verdict_record,
    channel_project_dirs,
    delivered,
    delivered_flags,
    delivered_watermark_for_path,
    delivered_watermarks,
    evaluate,
    evaluate_active_foreground_coverage,
    evaluate_coverage,
    evaluate_pg_health,
    evaluate_selector_sync,
    expected_tmux_session_name,
    is_harness_control_assistant_record,
    load_state,
    main_channel_project_re,
    newest_transcript,
    next_external_verdict_epoch,
    norm,
    parse_config,
    parse_watcher_state_probe,
    parse_transcript_ts,
    permanent_loss_tombstones,
    permanent_loss_total,
    project_slug,
    recheck_selected_transcript,
    revalidate_incarnation_identity,
    save_state,
    select_watch_transcript,
    select_watch_transcript_with_reason,
    selector_divergence_confirmed,
    session_incarnation_identity,
    tick_channel,
    tick_coverage,
    tick_pg_tunnel,
)

REPO_ROOT = Path(__file__).resolve().parents[1]

WORKTREE_ROOT = "/Users/alice/.adk/release/worktrees"
PREFIX = "claude-adk-cc"


def make_re():
    return main_channel_project_re(WORKTREE_ROOT, PREFIX)


class ProjectSlugTests(unittest.TestCase):
    def test_slashes_and_dots_become_dashes(self):
        self.assertEqual(
            project_slug("/Users/alice/.adk/release/worktrees"),
            "-Users-alice--adk-release-worktrees",
        )


class ProjectDirMatchingTests(unittest.TestCase):
    """The 07-09 hotfix invariants. If these fail, the watchdog either goes
    blind (pinned dir) or manufactures false LOST blocks (thread sessions)."""

    def test_main_channel_worktree_matches(self):
        self.assertIsNotNone(
            make_re().match(
                "-Users-alice--adk-release-worktrees-claude-adk-cc-20260709-140500"
            )
        )

    def test_thread_session_dirs_are_excluded(self):
        # INVARIANT (#4381): thread worktrees (`<prefix>-t<thread_id>-…`) relay
        # to a DIFFERENT Discord channel. Comparing their transcripts against
        # the main channel's messages would manufacture false LOST blocks, so
        # they must NEVER match the main-channel pattern.
        self.assertIsNone(
            make_re().match(
                "-Users-alice--adk-release-worktrees-claude-adk-cc-"
                "t1391234567890123456-20260709-140500"
            )
        )

    def test_short_thread_segment_is_still_excluded(self):
        self.assertIsNone(
            make_re().match(
                "-Users-alice--adk-release-worktrees-claude-adk-cc-t1-20260709-140500"
            )
        )

    def test_other_prefix_families_are_excluded(self):
        self.assertIsNone(
            make_re().match(
                "-Users-alice--adk-release-worktrees-codex-adk-20260709-140500"
            )
        )

    def test_suffix_noise_is_excluded(self):
        self.assertIsNone(
            make_re().match(
                "-Users-alice--adk-release-worktrees-claude-adk-cc-20260709-140500-x"
            )
        )

    def test_non_worktree_project_dirs_are_excluded(self):
        self.assertIsNone(make_re().match("-Users-alice-src-someproject"))

    def test_date_time_shape_is_required(self):
        # Not 8-digit date / 6-digit time → not a main-channel worktree.
        self.assertIsNone(
            make_re().match(
                "-Users-alice--adk-release-worktrees-claude-adk-cc-2026079-140500"
            )
        )

    def test_pattern_is_derived_from_home_not_hardcoded(self):
        # Portability (#4381): the operator username must come from the given
        # worktree root, never be baked into the module.
        pattern = main_channel_project_re("/Users/bob/.adk/release/worktrees", PREFIX)
        self.assertIsNotNone(
            pattern.match(
                "-Users-bob--adk-release-worktrees-claude-adk-cc-20260709-140500"
            )
        )
        self.assertIsNone(
            pattern.match(
                "-Users-alice--adk-release-worktrees-claude-adk-cc-20260709-140500"
            )
        )


class TranscriptResolutionTests(unittest.TestCase):
    def test_growth_beats_newer_mtime(self):
        growing = TranscriptCandidate(Path("/tmp/growing.jsonl"), 101, 100.0)
        newer_stagnant = TranscriptCandidate(
            Path("/tmp/newer-stagnant.jsonl"), 200, 200.0
        )
        selected = select_watch_transcript(
            [growing, newer_stagnant],
            {str(growing.path): 100, str(newer_stagnant.path): 200},
            semantic_growth_paths={str(growing.path)},
        )
        self.assertEqual(selected, growing.path)

    def test_no_growth_falls_back_to_newest_mtime(self):
        older = TranscriptCandidate(Path("/tmp/older.jsonl"), 100, 100.0)
        newer = TranscriptCandidate(Path("/tmp/newer.jsonl"), 200, 200.0)
        selected = select_watch_transcript(
            [older, newer], {str(older.path): 100, str(newer.path): 200}
        )
        self.assertEqual(selected, newer.path)

    def test_invariant_4435_no_growth_retains_previous_selection(self):
        current = TranscriptCandidate(Path("/tmp/current.jsonl"), 100, 100.0)
        touched_old = TranscriptCandidate(Path("/tmp/old.jsonl"), 200, 200.0)
        selected, reason = select_watch_transcript_with_reason(
            [current, touched_old],
            {str(current.path): 100, str(touched_old.path): 200},
            str(current.path),
        )
        self.assertEqual(selected, current.path)
        self.assertEqual(reason, "sticky")

    def test_invariant_4435_missing_previous_selection_bootstraps_newest(self):
        older = TranscriptCandidate(Path("/tmp/older.jsonl"), 100, 100.0)
        newer = TranscriptCandidate(Path("/tmp/newer.jsonl"), 200, 200.0)
        selected, reason = select_watch_transcript_with_reason(
            [older, newer],
            {str(older.path): 100, str(newer.path): 200},
            "/tmp/disappeared.jsonl",
        )
        self.assertEqual(selected, newer.path)
        self.assertEqual(reason, "prior_missing")

    def test_invariant_4435_corrupt_previous_selection_fails_open(self):
        older = TranscriptCandidate(Path("/tmp/older.jsonl"), 100, 100.0)
        newer = TranscriptCandidate(Path("/tmp/newer.jsonl"), 200, 200.0)
        selected, reason = select_watch_transcript_with_reason(
            [older, newer],
            {str(older.path): 100, str(newer.path): 200},
            True,
        )
        self.assertEqual(selected, newer.path)
        self.assertEqual(reason, "bootstrap")

    def test_first_observation_uses_mtime_fallback(self):
        older = TranscriptCandidate(Path("/tmp/older.jsonl"), 100, 100.0)
        newer = TranscriptCandidate(Path("/tmp/newer.jsonl"), 1, 200.0)
        self.assertEqual(select_watch_transcript([older, newer], {}), newer.path)

    def test_newest_transcript_ignores_thread_dirs_and_picks_latest(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            main_old = root / (
                "-Users-alice--adk-release-worktrees-claude-adk-cc-20260629-120235"
            )
            main_new = root / (
                "-Users-alice--adk-release-worktrees-claude-adk-cc-20260709-140500"
            )
            thread = root / (
                "-Users-alice--adk-release-worktrees-claude-adk-cc-"
                "t139123-20260710-000000"
            )
            for d in (main_old, main_new, thread):
                d.mkdir()
            old = main_old / "a.jsonl"
            new = main_new / "b.jsonl"
            threads = thread / "c.jsonl"
            for f in (old, new, threads):
                f.write_text("{}\n", encoding="utf-8")
            now = time.time()
            os.utime(old, (now - 300, now - 300))
            os.utime(new, (now - 100, now - 100))
            # The thread transcript is the NEWEST file overall; it must still
            # lose because thread dirs are filtered out before mtime ranking.
            os.utime(threads, (now, now))

            dirs = channel_project_dirs(root, make_re())
            self.assertEqual(
                sorted(d.name for d in dirs),
                sorted([main_old.name, main_new.name]),
            )
            self.assertEqual(newest_transcript(dirs), new)

    def test_no_dirs_yields_none(self):
        self.assertIsNone(newest_transcript([]))


class TranscriptRecheckTests(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.pattern = make_re()
        self.project = self.root / (
            "-Users-alice--adk-release-worktrees-claude-adk-cc-20260709-140500"
        )
        self.project.mkdir()
        self.transcript = self.project / "valid.jsonl"
        self.transcript.write_text("{}\n", encoding="utf-8")

    def test_untracked_valid_shaped_path_is_rejected(self):
        self.assertIsNone(
            recheck_selected_transcript(
                str(self.transcript), self.root, self.pattern, set()
            )
        )

    def test_tracked_noncanonical_path_is_rejected(self):
        value = str(
            self.root
            / self.project.name
            / ".."
            / self.project.name
            / self.transcript.name
        )
        self.assertIsNone(
            recheck_selected_transcript(value, self.root, self.pattern, {value})
        )

    def test_tracked_path_outside_root_or_deeper_nested_is_rejected(self):
        other_tmp = tempfile.TemporaryDirectory()
        self.addCleanup(other_tmp.cleanup)
        other_root = Path(other_tmp.name)
        outside_project = other_root / self.project.name
        outside_project.mkdir()
        outside = outside_project / "outside.jsonl"
        outside.write_text("{}\n", encoding="utf-8")

        nested_project = self.root / "extra" / self.project.name
        nested_project.mkdir(parents=True)
        nested = nested_project / "nested.jsonl"
        nested.write_text("{}\n", encoding="utf-8")

        for value in (str(outside), str(nested)):
            with self.subTest(value=value):
                self.assertIsNone(
                    recheck_selected_transcript(
                        value, self.root, self.pattern, {value}
                    )
                )

    def test_tracked_wrong_suffix_is_rejected(self):
        wrong = self.project / "wrong.txt"
        wrong.write_text("{}\n", encoding="utf-8")
        self.assertIsNone(
            recheck_selected_transcript(
                str(wrong), self.root, self.pattern, {str(wrong)}
            )
        )

    def test_tracked_pattern_mismatch_is_rejected(self):
        wrong_project = self.root / "not-a-channel-project"
        wrong_project.mkdir()
        wrong = wrong_project / "wrong.jsonl"
        wrong.write_text("{}\n", encoding="utf-8")
        self.assertIsNone(
            recheck_selected_transcript(
                str(wrong), self.root, self.pattern, {str(wrong)}
            )
        )

    def test_valid_tracked_existing_path_is_recovered(self):
        candidate = recheck_selected_transcript(
            str(self.transcript),
            self.root,
            self.pattern,
            {str(self.transcript)},
        )
        self.assertIsNotNone(candidate)
        self.assertEqual(candidate.path, self.transcript)
        self.assertEqual(candidate.size, self.transcript.stat().st_size)

    def test_malformed_tracked_path_stat_errors_are_rejected(self):
        for name in ("bad\0.jsonl", "bad\ud800.jsonl"):
            value = str(self.project / name)
            with self.subTest(name=repr(name)):
                try:
                    candidate = recheck_selected_transcript(
                        value, self.root, self.pattern, {value}
                    )
                except (OSError, ValueError, UnicodeError) as exc:
                    self.fail(f"malformed persisted path escaped recheck: {exc!r}")
                self.assertIsNone(candidate)

    def test_directory_fstat_errors_fail_closed_as_unsafe_paths(self):
        real_fstat = relay_watchdog.os.fstat
        for failing_directory_index in (1, 2):
            with self.subTest(failing_directory_index=failing_directory_index):
                directory_calls = 0

                def fail_selected_directory_fstat(descriptor):
                    nonlocal directory_calls
                    opened = real_fstat(descriptor)
                    if stat.S_ISDIR(opened.st_mode):
                        directory_calls += 1
                        if directory_calls == failing_directory_index:
                            raise OSError("injected directory fstat failure")
                    return opened

                with mock.patch.object(
                    relay_watchdog.os,
                    "fstat",
                    side_effect=fail_selected_directory_fstat,
                ):
                    result = relay_watchdog.assistant_blocks(self.transcript)

                self.assertEqual(directory_calls, failing_directory_index)
                self.assertEqual(result.blocks, [])
                self.assertEqual(result.error, "UnsafePath")

    def test_symlink_escape_is_rejected_across_recheck_discovery_and_read(self):
        other_tmp = tempfile.TemporaryDirectory()
        self.addCleanup(other_tmp.cleanup)
        outside = Path(other_tmp.name) / "outside.jsonl"
        outside.write_text("{}\n", encoding="utf-8")
        linked = self.project / "linked.jsonl"
        try:
            linked.symlink_to(outside)
        except OSError as exc:
            self.skipTest(f"symlink unavailable: {exc}")

        self.assertIsNone(
            recheck_selected_transcript(
                str(linked), self.root, self.pattern, {str(linked)}
            )
        )
        self.assertNotIn(
            linked,
            [
                candidate.path
                for candidate in relay_watchdog.transcript_candidates([self.project])
            ],
        )
        self.assertIsNotNone(relay_watchdog.assistant_blocks(linked).error)
        with mock.patch.object(
            relay_watchdog,
            "_regular_file_stat_without_symlink",
            return_value=outside.stat(),
        ):
            self.assertIsNotNone(
                relay_watchdog.assistant_blocks(linked).error,
                "descriptor open must reject a symlink swapped in after precheck",
            )

        discovery_root = self.root / "discovery"
        discovery_root.mkdir()
        linked_project = discovery_root / self.project.name
        linked_project.symlink_to(self.project, target_is_directory=True)
        self.assertEqual(
            channel_project_dirs(discovery_root, self.pattern),
            [],
        )

    def test_parent_symlink_swap_before_open_cannot_escape_dirfd(self):
        outside_tmp = tempfile.TemporaryDirectory()
        self.addCleanup(outside_tmp.cleanup)
        outside_project = Path(outside_tmp.name) / self.project.name
        outside_project.mkdir()
        outside = outside_project / self.transcript.name
        outside.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": "2026-07-11T07:00:00Z",
                    "message": {
                        "content": [
                            {"type": "text", "text": "escaped-parent-read"}
                        ]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        moved_project = self.root / "original-project-after-swap"
        real_open = relay_watchdog.os.open
        swapped = False

        def swap_parent_then_open(path, flags, *args, **kwargs):
            nonlocal swapped
            if not swapped and os.fspath(path) == self.project.name:
                swapped = True
                self.project.rename(moved_project)
                self.project.symlink_to(outside_project, target_is_directory=True)
            return real_open(path, flags, *args, **kwargs)

        with mock.patch.object(
            relay_watchdog.os, "open", side_effect=swap_parent_then_open
        ):
            result = relay_watchdog.assistant_blocks(self.transcript)

        self.assertTrue(swapped, "test must swap after nofollow prechecks")
        self.assertEqual(result.error, "UnsafePath")
        self.assertEqual(
            result.blocks,
            [],
            "a path-string reopen must never read through the swapped parent",
        )

    def test_projects_root_symlink_swap_after_discovery_cannot_escape(self):
        """Pin every ancestor below the trusted root, not just file.parent."""
        trusted_home = self.root / "trusted-home"
        projects_root = trusted_home / "projects"
        project = projects_root / self.project.name
        project.mkdir(parents=True)
        transcript = project / "ancestor-swap.jsonl"
        transcript.write_text("{}\n", encoding="utf-8")
        candidates = relay_watchdog.transcript_candidates([project])
        self.assertEqual([candidate.path for candidate in candidates], [transcript])

        outside_tmp = tempfile.TemporaryDirectory()
        self.addCleanup(outside_tmp.cleanup)
        outside_projects = Path(outside_tmp.name) / "projects"
        outside_project = outside_projects / project.name
        outside_project.mkdir(parents=True)
        outside = outside_project / transcript.name
        outside.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": "2026-07-11T07:00:00Z",
                    "message": {
                        "content": [
                            {"type": "text", "text": "escaped-ancestor-read"}
                        ]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        moved_projects = trusted_home / "projects-after-discovery"
        projects_root.rename(moved_projects)
        projects_root.symlink_to(outside_projects, target_is_directory=True)

        # Independent semantic mutation oracle: the old immediate-parent-only
        # implementation follows the swapped projects ancestor and reads the
        # outside same-shaped transcript.  Keep this proof local to the test;
        # the production helper below must reject the exact same filesystem.
        def narrow_parent_only_open(path, flags, _trusted_root):
            expected_parent = path.parent.stat(follow_symlinks=False)
            expected_file = path.stat(follow_symlinks=False)
            parent_fd = os.open(
                path.parent,
                os.O_RDONLY
                | getattr(os, "O_CLOEXEC", 0)
                | getattr(os, "O_DIRECTORY", 0)
                | getattr(os, "O_NOFOLLOW", 0)
                | getattr(os, "O_NONBLOCK", 0),
            )
            descriptor = -1
            try:
                opened_parent = os.fstat(parent_fd)
                if not relay_watchdog._same_file_identity(
                    expected_parent, opened_parent
                ):
                    return None
                descriptor = os.open(path.name, flags, dir_fd=parent_fd)
                opened_file = os.fstat(descriptor)
                if not stat.S_ISREG(opened_file.st_mode) or not (
                    relay_watchdog._same_file_identity(expected_file, opened_file)
                ):
                    return None
                opened = descriptor
                descriptor = -1
                return opened
            finally:
                if descriptor >= 0:
                    os.close(descriptor)
                os.close(parent_fd)

        with mock.patch.object(
            relay_watchdog,
            "_open_regular_file_beneath_parent",
            side_effect=narrow_parent_only_open,
        ):
            legacy_result = relay_watchdog.assistant_blocks(
                transcript, trusted_root=projects_root
            )
        self.assertEqual(
            [text for _, text in legacy_result.blocks], ["escaped-ancestor-read"]
        )

        result = relay_watchdog.assistant_blocks(
            transcript, trusted_root=projects_root
        )
        self.assertEqual(result.error, "UnsafePath")
        self.assertEqual(result.blocks, [])

    def test_trusted_root_boundary_allows_symlink_only_above_it(self):
        """The explicit root may live below a legitimate platform symlink."""
        real_home = self.root / "real-home"
        real_projects = real_home / "projects"
        real_project = real_projects / self.project.name
        real_project.mkdir(parents=True)
        transcript_name = "trusted-boundary.jsonl"
        (real_project / transcript_name).write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": "2026-07-11T07:00:00Z",
                    "message": {
                        "content": [{"type": "text", "text": "trusted-read"}]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        alias_home = self.root / "home-alias"
        alias_home.symlink_to(real_home, target_is_directory=True)
        alias_projects = alias_home / "projects"
        alias_transcript = alias_projects / real_project.name / transcript_name

        result = relay_watchdog.assistant_blocks(
            alias_transcript, trusted_root=alias_projects
        )

        self.assertIsNone(result.error)
        self.assertEqual([text for _, text in result.blocks], ["trusted-read"])

    def test_component_open_requires_absolute_path_beneath_trusted_root(self):
        relative = relay_watchdog.assistant_blocks(
            Path("relative.jsonl"), trusted_root=self.root
        )
        outside_tmp = tempfile.TemporaryDirectory()
        self.addCleanup(outside_tmp.cleanup)
        outside = Path(outside_tmp.name) / "outside.jsonl"
        outside.write_text("{}\n", encoding="utf-8")
        escaped = relay_watchdog.assistant_blocks(
            outside, trusted_root=self.root
        )

        self.assertEqual(relative.error, "UnsafePath")
        self.assertEqual(escaped.error, "UnsafePath")

    def test_component_open_not_implemented_fails_closed(self):
        real_open = relay_watchdog.os.open

        def unsupported_openat(path, flags, *args, **kwargs):
            if kwargs.get("dir_fd") is not None:
                raise NotImplementedError("dir_fd is unavailable")
            return real_open(path, flags, *args, **kwargs)

        with mock.patch.object(
            relay_watchdog.os, "open", side_effect=unsupported_openat
        ):
            result = relay_watchdog.assistant_blocks(
                self.transcript, trusted_root=self.root
            )

        self.assertEqual(result.error, "UnsafePath")
        self.assertEqual(result.blocks, [])

    @unittest.skipUnless(hasattr(os, "mkfifo"), "FIFO unavailable on this platform")
    def test_fifo_swap_before_open_is_rejected_without_blocking(self):
        """A non-regular swap must not block before the descriptor fstat."""
        fifo = self.project / "swapped.jsonl"
        os.mkfifo(fifo)
        probe = (
            "from pathlib import Path\n"
            "from unittest import mock\n"
            "import scripts.relay_watchdog as rw\n"
            f"with mock.patch.object(rw, '_regular_file_stat_without_symlink', "
            f"return_value=rw.os.stat({str(self.transcript)!r})):\n"
            f"    result = rw.assistant_blocks(Path({str(fifo)!r}))\n"
            "raise SystemExit(0 if result.error == 'UnsafePath' else 1)\n"
        )
        try:
            completed = subprocess.run(
                [os.environ.get("PYTHON", "python3"), "-c", probe],
                cwd=REPO_ROOT,
                timeout=2,
                check=False,
            )
        except subprocess.TimeoutExpired:
            self.fail("assistant_blocks blocked opening a FIFO swapped after precheck")
        self.assertEqual(completed.returncode, 0)

    @unittest.skipUnless(hasattr(os, "mkfifo"), "FIFO unavailable on this platform")
    def test_regular_to_fifo_race_between_stat_and_open_is_nonblocking(self):
        """Swap after the final lstat; O_NONBLOCK must make fstat rejection bounded."""
        raced = self.project / "raced-after-stat.jsonl"
        raced.write_text("{}\n", encoding="utf-8")
        probe = (
            "import os\n"
            "from pathlib import Path\n"
            "from unittest import mock\n"
            "import scripts.relay_watchdog as rw\n"
            f"target = Path({str(raced)!r})\n"
            f"trusted = Path({str(self.root)!r})\n"
            "real_open = rw.os.open\n"
            "swapped = False\n"
            "def swap_then_open(path, flags, *args, **kwargs):\n"
            "    global swapped\n"
            "    if (not swapped and kwargs.get('dir_fd') is not None "
            "and os.fspath(path) == target.name):\n"
            "        target.unlink()\n"
            "        os.mkfifo(target)\n"
            "        swapped = True\n"
            "    return real_open(path, flags, *args, **kwargs)\n"
            "with mock.patch.object(rw.os, 'open', side_effect=swap_then_open):\n"
            "    result = rw.assistant_blocks(target, trusted_root=trusted)\n"
            "raise SystemExit(0 if swapped and result.error == 'UnsafePath' else 1)\n"
        )
        try:
            completed = subprocess.run(
                [os.environ.get("PYTHON", "python3"), "-c", probe],
                cwd=REPO_ROOT,
                timeout=2,
                check=False,
            )
        except subprocess.TimeoutExpired:
            self.fail("regular-to-FIFO race blocked at final open without O_NONBLOCK")
        self.assertEqual(completed.returncode, 0)


class TimestampTests(unittest.TestCase):
    def test_transcript_timestamps_parse_as_utc(self):
        # `mktime(...) - time.timezone` (the prototype) breaks under DST; the
        # parse must be pure UTC regardless of local timezone.
        expected = datetime(2026, 7, 9, 2, 57, 18, tzinfo=timezone.utc).timestamp()
        self.assertEqual(parse_transcript_ts("2026-07-09T02:57:18.123Z"), expected)

    def test_garbage_timestamp_is_none(self):
        self.assertIsNone(parse_transcript_ts("not-a-timestamp"))
        self.assertIsNone(parse_transcript_ts(""))


class TranscriptParsingTests(unittest.TestCase):
    def test_extracts_only_assistant_text_blocks(self):
        lines = [
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": "2026-07-09T02:00:00Z",
                    "message": {
                        "content": [
                            {"type": "text", "text": "hello world"},
                            {"type": "tool_use", "name": "Bash"},
                            {"type": "text", "text": "   "},
                        ]
                    },
                }
            ),
            json.dumps({"type": "user", "timestamp": "2026-07-09T02:00:01Z"}),
            "not json at all",
            json.dumps({"type": "assistant", "message": {"content": []}}),
        ]
        blocks = assistant_blocks_from_lines(lines)
        self.assertEqual(len(blocks), 1)
        self.assertEqual(blocks[0][1], "hello world")

    def test_invariant_4435_structural_synthetic_rate_limit_is_excluded(self):
        record = {
            "type": "assistant",
            "timestamp": "2026-07-09T02:00:00Z",
            "isApiErrorMessage": True,
            "apiErrorStatus": 429,
            "error": "rate_limit",
            "message": {
                "model": "<synthetic>",
                "content": [
                    {"type": "text", "text": "You've hit your session limit"}
                ],
            },
        }
        self.assertTrue(is_harness_control_assistant_record(record))
        self.assertEqual(assistant_blocks_from_lines([json.dumps(record)]), [])

    def test_invariant_4435_all_synthetic_harness_error_shapes_are_excluded(self):
        cases = [
            (
                "server_error_none",
                {"error": "server_error", "apiErrorStatus": None},
                "server error",
            ),
            (
                "authentication_failed",
                {"error": "authentication_failed", "apiErrorStatus": 401},
                "auth failed",
            ),
            (
                "overloaded_529",
                {"error": "overloaded", "apiErrorStatus": 529},
                "overloaded",
            ),
            ("unknown_idle_timeout", {"error": "idle_timeout"}, "unknown idle timeout"),
            (
                "non_api_no_response",
                {"isApiErrorMessage": False, "apiErrorStatus": None},
                "No response requested.",
            ),
            (
                "rate_limit_none",
                {"error": "rate_limit", "apiErrorStatus": None},
                "rate limited",
            ),
        ]
        for name, metadata, prose in cases:
            with self.subTest(name=name):
                record = {
                    "type": "assistant",
                    "timestamp": "2026-07-09T02:00:00Z",
                    **metadata,
                    "message": {
                        "model": "<synthetic>",
                        "content": [{"type": "text", "text": prose}],
                    },
                }
                self.assertTrue(is_harness_control_assistant_record(record))
                self.assertEqual(assistant_blocks_from_lines([json.dumps(record)]), [])

                real_record = {
                    **record,
                    "message": {**record["message"], "model": "claude-opus-4-1"},
                }
                self.assertFalse(is_harness_control_assistant_record(real_record))
                self.assertEqual(
                    [text for _, text in assistant_blocks_from_lines([json.dumps(real_record)])],
                    [prose],
                )

    def test_invariant_4435_identical_normal_assistant_text_is_retained(self):
        record = {
            "type": "assistant",
            "timestamp": "2026-07-09T02:00:00Z",
            "isApiErrorMessage": False,
            "apiErrorStatus": 200,
            "message": {
                "model": "claude-opus-4-1",
                "content": [
                    {"type": "text", "text": "You've hit your session limit"}
                ],
            },
        }
        self.assertFalse(is_harness_control_assistant_record(record))
        blocks = assistant_blocks_from_lines([json.dumps(record)])
        self.assertEqual([text for _, text in blocks], ["You've hit your session limit"])


class DeliveredTests(unittest.TestCase):
    def test_short_text_requires_exact_normalized_substring(self):
        self.assertTrue(delivered("done!", norm("prefix done! suffix")))
        self.assertFalse(delivered("done!", norm("prefix nope suffix")))

    def test_whitespace_is_normalized(self):
        self.assertTrue(delivered("a  b\n\nc", "x a b c y"))

    def test_chunked_delivery_counts_via_any_probe(self):
        text = ("H" * 80) + ("M" * 80) + ("T" * 80)
        # Only the tail chunk landed (relay chunking/edit): still delivered.
        self.assertTrue(delivered(text, "T" * 80))
        self.assertFalse(delivered(text, "Z" * 200))


class EvaluateBoundaryTests(unittest.TestCase):
    """LOST/GAP boundaries. GRACE=600/GAP=900 were calibrated live on 07-09."""

    GRACE = 600
    GAP = 900
    NOW = 1_800_000_000.0

    def _eval(self, blocks, hay):
        return evaluate(blocks, hay, self.NOW, self.GRACE, self.GAP)

    def test_all_delivered_is_ok(self):
        v = self._eval([(self.NOW - 2000, "alpha block")], "alpha block")
        self.assertEqual(v.state, STATE_OK)
        self.assertEqual(v.lost, 0)

    def test_young_undelivered_block_is_within_grace(self):
        # The relay flushes on turn/tool boundaries; a block younger than GRACE
        # is not evidence of anything (07-09 05:30Z false positive at 300s).
        v = self._eval([(self.NOW - self.GRACE, "undelivered")], "")
        self.assertEqual(v.stale, 0)
        self.assertEqual(v.state, STATE_OK)

    def test_block_one_second_past_grace_is_stale(self):
        v = self._eval([(self.NOW - self.GRACE - 1, "undelivered block here")], "")
        self.assertEqual(v.stale, 1)
        self.assertEqual(v.lost, 1)

    def test_historic_gap_before_watermark_never_realerts(self):
        # A lost block OLDER than the last successful delivery is a historic,
        # already-recovered gap — the watermark must silence it forever.
        lost_old = (self.NOW - 5000, "vanished long ago")
        delivered_new = (self.NOW - 120, "this one landed fine")
        v = self._eval([lost_old, delivered_new], "this one landed fine")
        self.assertEqual(v.lost, 0)
        self.assertEqual(v.state, STATE_OK)

    def test_block_sharing_the_watermark_timestamp_is_not_lost(self):
        # `e > delivered_ts` is strict: an undelivered block with the SAME
        # second-resolution timestamp as the delivered watermark block does not
        # count as lost (transcripts often stamp adjacent blocks identically).
        ts = self.NOW - 2000
        v = self._eval(
            [(ts, "delivered payload"), (ts, "missing payload")],
            "delivered payload",
        )
        self.assertEqual(v.lost, 0)
        self.assertEqual(v.state, STATE_OK)

    def test_lost_with_recent_watermark_is_lagging_not_gap(self):
        delivered_block = (self.NOW - self.GAP, "delivered payload")
        lost_block = (self.NOW - self.GRACE - 60, "missing payload")
        v = self._eval([delivered_block, lost_block], "delivered payload")
        self.assertEqual(v.lost, 1)
        # gap_secs == GAP exactly: strictly-greater is required to alert.
        self.assertEqual(v.state, STATE_LAGGING)

    def test_lost_with_old_watermark_is_gap(self):
        delivered_block = (self.NOW - self.GAP - 1, "delivered payload")
        lost_block = (self.NOW - self.GRACE - 60, "missing payload")
        v = self._eval([delivered_block, lost_block], "delivered payload")
        self.assertEqual(v.state, STATE_GAP)

    def test_no_delivery_ever_with_stale_lost_is_gap(self):
        # The VERDICT is unchanged: a relay that died before it ever delivered
        # must still alert. What changed with #5190 is that the verdict no
        # longer carries `inf` as if it were a measured elapsed time — the two
        # claims ("never observed" vs "infinitely stale") had collapsed into
        # one value that auto-passed every threshold and outranked every real
        # measurement in tick_channel's verdict ordering.
        v = self._eval([(self.NOW - 4000, "never arrived")], "")
        self.assertEqual(v.state, STATE_GAP)
        self.assertEqual(v.gap_secs, relay_watchdog.GAP_SECS_UNOBSERVED)
        self.assertTrue(relay_watchdog.gap_is_unobserved(v))
        self.assertNotEqual(v.gap_secs, float("inf"))

    def test_5190_a_measured_gap_is_never_reported_as_unobserved(self):
        delivered_block = (self.NOW - self.GAP - 1, "delivered payload")
        lost_block = (self.NOW - self.GRACE - 60, "missing payload")
        v = self._eval([delivered_block, lost_block], "delivered payload")
        self.assertEqual(v.state, STATE_GAP)
        self.assertFalse(relay_watchdog.gap_is_unobserved(v))
        self.assertAlmostEqual(v.gap_secs, self.GAP + 1)

    def test_no_blocks_is_ok(self):
        v = self._eval([], "")
        self.assertEqual(v.state, STATE_OK)

    def test_invariant_4435_prior_watermark_survives_bounded_haystack_loss(self):
        anchor = self.NOW - 2000
        v = evaluate(
            [(anchor, "previously delivered")],
            "",
            self.NOW,
            self.GRACE,
            self.GAP,
            prior_delivered_ts=anchor,
        )
        self.assertEqual(v.state, STATE_OK)
        self.assertEqual(v.lost, 0)
        self.assertEqual(v.delivered_ts, anchor)

    def test_invariant_4435_older_current_match_cannot_retreat_watermark(self):
        older = self.NOW - 3000
        prior = self.NOW - 1000
        v = evaluate(
            [(older, "old delivery")],
            "old delivery",
            self.NOW,
            self.GRACE,
            self.GAP,
            prior_delivered_ts=prior,
        )
        self.assertEqual(v.delivered_ts, prior)


class CoverageEvaluationTests(unittest.TestCase):
    NOW_MS = 1_800_000_000_000

    @classmethod
    def active_foreground(cls, **overrides) -> CoverageActivityProbe:
        fields = {
            "relay_stall_state": "active_foreground_stream",
            "active_turn": "foreground",
            "queue_depth": 0,
            "tmux_alive": True,
            "watcher_attached": True,
            "watcher_attached_stale": False,
            "watcher_owns_live_relay": True,
            "last_outbound_activity_ms": cls.NOW_MS - 1,
            "last_relay_ts_ms": cls.NOW_MS - 2,
            "desynced": True,
        }
        fields.update(overrides)
        return CoverageActivityProbe(**fields)

    def test_live_expected_session_with_healthy_watcher_is_covered(self):
        verdict = evaluate_coverage(True, 200, True, False, 1)
        self.assertEqual(verdict.state, COVERAGE_COVERED)
        self.assertEqual(verdict.consecutive_uncovered, 0)
        self.assertFalse(verdict.confirmed)

    def test_authoritative_404_confirms_on_exactly_second_tick(self):
        first = evaluate_coverage(True, 404, None, None, 0)
        self.assertEqual(first.state, COVERAGE_UNCOVERED)
        self.assertEqual(first.consecutive_uncovered, 1)
        self.assertFalse(first.confirmed)

        second = evaluate_coverage(
            True, 404, None, None, first.consecutive_uncovered
        )
        self.assertEqual(second.consecutive_uncovered, COVERAGE_CONFIRM_TICKS)
        self.assertTrue(second.confirmed)

    def test_dcserver_unreachable_is_unknown_and_resets_confirmation(self):
        verdict = evaluate_coverage(True, None, None, None, 1)
        self.assertEqual(verdict.state, COVERAGE_UNKNOWN)
        self.assertEqual(verdict.reason, "dcserver_unreachable")
        self.assertEqual(verdict.consecutive_uncovered, 0)
        self.assertFalse(verdict.confirmed)

    def test_single_detached_tick_never_confirms(self):
        verdict = evaluate_coverage(True, 200, False, False, 0)
        self.assertEqual(verdict.state, COVERAGE_UNCOVERED)
        self.assertEqual(verdict.consecutive_uncovered, 1)
        self.assertFalse(verdict.confirmed)

    def test_attached_but_desynced_is_phantom_coverage(self):
        verdict = evaluate_coverage(True, 200, True, True, 1)
        self.assertEqual(verdict.state, COVERAGE_UNCOVERED)
        self.assertEqual(verdict.reason, "attached_but_desynced")
        self.assertTrue(verdict.confirmed)

    def test_recent_active_foreground_stream_covers_transient_desync(self):
        verdict = evaluate_coverage(
            True,
            200,
            True,
            True,
            1,
            self.active_foreground(),
            self.NOW_MS,
        )
        self.assertEqual(verdict.state, COVERAGE_COVERED)
        self.assertEqual(verdict.reason, "active_foreground_recent_activity")
        self.assertEqual(verdict.consecutive_uncovered, 0)
        self.assertFalse(verdict.confirmed)

    def test_relay_timestamp_is_valid_freshness_fallback(self):
        verdict = evaluate_active_foreground_coverage(
            self.active_foreground(
                last_outbound_activity_ms=None,
                last_relay_ts_ms=self.NOW_MS - 1,
            ),
            self.NOW_MS,
        )
        self.assertEqual(verdict.state, COVERAGE_COVERED)

    def test_activity_freshness_boundary_is_strict(self):
        inside = evaluate_active_foreground_coverage(
            self.active_foreground(
                last_outbound_activity_ms=(
                    self.NOW_MS - COVERAGE_ACTIVITY_FRESH_SECS * 1000 + 1
                ),
                last_relay_ts_ms=None,
            ),
            self.NOW_MS,
        )
        boundary = evaluate_active_foreground_coverage(
            self.active_foreground(
                last_outbound_activity_ms=(
                    self.NOW_MS - COVERAGE_ACTIVITY_FRESH_SECS * 1000
                ),
                last_relay_ts_ms=None,
            ),
            self.NOW_MS,
        )
        self.assertEqual(inside.state, COVERAGE_COVERED)
        self.assertEqual(boundary.state, COVERAGE_UNCOVERED)
        self.assertEqual(boundary.reason, "active_foreground_activity_stale")

    def test_future_activity_timestamp_cannot_suppress_desync(self):
        activity = self.active_foreground(
            last_outbound_activity_ms=self.NOW_MS + 1,
            last_relay_ts_ms=None,
        )
        activity_verdict = evaluate_active_foreground_coverage(
            activity,
            self.NOW_MS,
        )
        self.assertEqual(activity_verdict.state, COVERAGE_UNCOVERED)
        self.assertEqual(
            activity_verdict.reason,
            "active_foreground_activity_future",
        )

        coverage_verdict = evaluate_coverage(
            True,
            200,
            True,
            True,
            1,
            activity,
            self.NOW_MS,
        )
        self.assertEqual(coverage_verdict.state, COVERAGE_UNCOVERED)
        self.assertEqual(coverage_verdict.reason, "attached_but_desynced")
        self.assertTrue(coverage_verdict.confirmed)

    def test_oversized_activity_timestamp_fails_closed_without_throwing(self):
        activity = self.active_foreground(
            last_outbound_activity_ms=10**400,
            last_relay_ts_ms=None,
        )
        activity_verdict = evaluate_active_foreground_coverage(
            activity,
            self.NOW_MS,
        )
        self.assertEqual(activity_verdict.state, COVERAGE_UNCOVERED)
        self.assertEqual(
            activity_verdict.reason,
            "active_foreground_activity_invalid",
        )

        coverage_verdict = evaluate_coverage(
            True,
            200,
            True,
            True,
            1,
            activity,
            self.NOW_MS,
        )
        self.assertEqual(coverage_verdict.state, COVERAGE_UNCOVERED)
        self.assertEqual(coverage_verdict.reason, "attached_but_desynced")
        self.assertTrue(coverage_verdict.confirmed)

    def test_invalid_freshness_clock_cannot_suppress_desync(self):
        for now_ms, freshness_secs in (
            (None, 60),
            (self.NOW_MS, 0),
            (10**400, 60),
        ):
            with self.subTest(now_ms=now_ms, freshness_secs=freshness_secs):
                verdict = evaluate_coverage(
                    True,
                    200,
                    True,
                    True,
                    1,
                    self.active_foreground(),
                    now_ms,
                    freshness_secs,
                )
                self.assertEqual(verdict.state, COVERAGE_UNCOVERED)
                self.assertEqual(verdict.reason, "attached_but_desynced")
                self.assertTrue(verdict.confirmed)

    def test_explicit_stall_evidence_never_uses_foreground_bypass(self):
        cases = {
            "queue": {"queue_depth": 1},
            "dead tmux": {"tmux_alive": False},
            "detached nested watcher": {"watcher_attached": False},
            "stale attachment": {"watcher_attached_stale": True},
            "lost live relay ownership": {"watcher_owns_live_relay": False},
            "relay-dead classifier": {
                "relay_stall_state": "tmux_alive_relay_dead"
            },
            "no activity": {
                "last_outbound_activity_ms": None,
                "last_relay_ts_ms": None,
            },
            "stale activity": {
                "last_outbound_activity_ms": (
                    self.NOW_MS - COVERAGE_ACTIVITY_FRESH_SECS * 1000
                ),
                "last_relay_ts_ms": None,
            },
        }
        for name, overrides in cases.items():
            with self.subTest(name=name):
                verdict = evaluate_coverage(
                    True,
                    200,
                    True,
                    True,
                    1,
                    self.active_foreground(**overrides),
                    self.NOW_MS,
                )
                self.assertEqual(verdict.state, COVERAGE_UNCOVERED)
                self.assertEqual(verdict.reason, "attached_but_desynced")
                self.assertTrue(verdict.confirmed)

    def test_partial_or_malformed_active_schema_cannot_suppress_desync(self):
        partial = CoverageActivityProbe(
            relay_stall_state="active_foreground_stream",
            active_turn="foreground",
        )
        malformed = self.active_foreground(malformed=True)
        for activity in (partial, malformed):
            with self.subTest(activity=activity):
                verdict = evaluate_coverage(
                    True,
                    200,
                    True,
                    True,
                    1,
                    activity,
                    self.NOW_MS,
                )
                self.assertEqual(verdict.state, COVERAGE_UNCOVERED)
                self.assertEqual(verdict.reason, "attached_but_desynced")
                self.assertEqual(
                    verdict.consecutive_uncovered, COVERAGE_CONFIRM_TICKS
                )
                self.assertTrue(verdict.confirmed)

    def test_nested_desync_disagreement_cannot_suppress_top_level_desync(self):
        verdict = evaluate_coverage(
            True,
            200,
            True,
            True,
            1,
            self.active_foreground(desynced=False),
            self.NOW_MS,
        )
        self.assertEqual(verdict.state, COVERAGE_UNCOVERED)
        self.assertEqual(verdict.reason, "attached_but_desynced")
        self.assertTrue(verdict.confirmed)

    def test_active_evidence_cannot_override_detached_or_404(self):
        activity = self.active_foreground()
        detached = evaluate_coverage(
            True, 200, False, True, 1, activity, self.NOW_MS
        )
        missing = evaluate_coverage(
            True, 404, None, None, 1, activity, self.NOW_MS
        )
        self.assertEqual((detached.state, detached.reason), (
            COVERAGE_UNCOVERED,
            "detached",
        ))
        self.assertEqual((missing.state, missing.reason), (
            COVERAGE_UNCOVERED,
            "watcher_state_404",
        ))

    def test_dead_expected_session_is_left_to_stall_watchdog(self):
        verdict = evaluate_coverage(False, 200, False, True, 1)
        self.assertEqual(verdict.state, COVERAGE_COVERED)
        self.assertEqual(verdict.reason, "tmux_not_expected")
        self.assertFalse(verdict.confirmed)

    def test_malformed_200_is_unknown_not_uncovered(self):
        verdict = evaluate_coverage(True, 200, None, None, 1)
        self.assertEqual(verdict.state, COVERAGE_UNKNOWN)
        self.assertEqual(verdict.consecutive_uncovered, 0)


class WatcherStateParserTests(unittest.TestCase):
    NOW_MS = CoverageEvaluationTests.NOW_MS

    @classmethod
    def payload(cls, **relay_overrides) -> dict:
        relay_health = {
            "active_turn": "foreground",
            "queue_depth": 0,
            "tmux_alive": True,
            "watcher_attached": True,
            "watcher_attached_stale": False,
            "watcher_owns_live_relay": True,
            "last_outbound_activity_ms": cls.NOW_MS - 1,
            "last_relay_ts_ms": cls.NOW_MS - 2,
            "desynced": True,
        }
        relay_health.update(relay_overrides)
        return {
            "attached": True,
            "desynced": True,
            "bound_output_path": "/tmp/live.jsonl",
            "relay_stall_state": "active_foreground_stream",
            "relay_health": relay_health,
        }

    def test_parses_actual_active_foreground_schema(self):
        probe = parse_watcher_state_probe(200, self.payload())
        self.assertEqual(
            probe,
            WatcherStateProbe(
                200,
                True,
                True,
                "/tmp/live.jsonl",
                CoverageActivityProbe(
                    relay_stall_state="active_foreground_stream",
                    active_turn="foreground",
                    queue_depth=0,
                    tmux_alive=True,
                    watcher_attached=True,
                    watcher_attached_stale=False,
                    watcher_owns_live_relay=True,
                    last_outbound_activity_ms=self.NOW_MS - 1,
                    last_relay_ts_ms=self.NOW_MS - 2,
                    desynced=True,
                ),
            ),
        )

    def test_parses_local_inflight_updated_at(self):
        payload = self.payload()
        payload["inflight_updated_at"] = "2026-07-24 10:43:58"

        probe = parse_watcher_state_probe(200, payload)

        self.assertEqual(
            probe.inflight_updated_at,
            time.mktime(time.strptime("2026-07-24 10:43:58", "%Y-%m-%d %H:%M:%S")),
        )

    def test_malformed_inflight_updated_at_fails_closed(self):
        payload = self.payload()
        payload["inflight_updated_at"] = "2026-07-24T10:43:58Z"

        probe = parse_watcher_state_probe(200, payload)

        self.assertIsNone(probe.inflight_updated_at)

    def test_nullable_activity_timestamps_are_valid_schema(self):
        probe = parse_watcher_state_probe(
            200,
            self.payload(
                last_outbound_activity_ms=None,
                last_relay_ts_ms=self.NOW_MS - 1,
            ),
        )
        self.assertIsNotNone(probe.relay_activity)
        self.assertFalse(probe.relay_activity.malformed)
        self.assertIsNone(probe.relay_activity.last_outbound_activity_ms)
        self.assertEqual(probe.relay_activity.last_relay_ts_ms, self.NOW_MS - 1)

    def test_exact_types_reject_bool_integer_and_string_boolean(self):
        probe = parse_watcher_state_probe(
            200,
            self.payload(
                queue_depth=True,
                watcher_attached_stale="false",
                last_outbound_activity_ms="1800000000000",
            ),
        )
        self.assertIsNotNone(probe.relay_activity)
        self.assertTrue(probe.relay_activity.malformed)
        self.assertIsNone(probe.relay_activity.queue_depth)
        self.assertIsNone(probe.relay_activity.watcher_attached_stale)
        self.assertIsNone(probe.relay_activity.last_outbound_activity_ms)

    def test_partial_active_schema_preserves_legacy_desync_detection(self):
        probe = parse_watcher_state_probe(
            200,
            {
                "attached": True,
                "desynced": True,
                "relay_stall_state": "active_foreground_stream",
                "relay_health": {"active_turn": "foreground"},
            },
        )
        verdict = evaluate_coverage(
            True,
            probe.status,
            probe.attached,
            probe.desynced,
            1,
            probe.relay_activity,
            self.NOW_MS,
        )
        self.assertEqual(verdict.state, COVERAGE_UNCOVERED)
        self.assertEqual(verdict.reason, "attached_but_desynced")
        self.assertTrue(verdict.confirmed)

    def test_legacy_schema_retains_original_desync_detection(self):
        probe = parse_watcher_state_probe(
            200, {"attached": True, "desynced": True}
        )
        self.assertIsNone(probe.relay_activity)
        verdict = evaluate_coverage(
            True,
            probe.status,
            probe.attached,
            probe.desynced,
            1,
            probe.relay_activity,
            self.NOW_MS,
        )
        self.assertEqual(verdict.state, COVERAGE_UNCOVERED)
        self.assertTrue(verdict.confirmed)

    def test_non_mapping_and_non_200_never_invent_activity(self):
        self.assertEqual(
            parse_watcher_state_probe(200, []),
            WatcherStateProbe(200, response_malformed=True),
        )
        self.assertEqual(
            parse_watcher_state_probe(404, self.payload()), WatcherStateProbe(404)
        )


class SelectorSyncEvaluationTests(unittest.TestCase):
    """Pure I1 judgment (#4408 phase 2): B (watcher-state bound_output_path) vs
    F (growth-aware transcript pick)."""

    @staticmethod
    def provider_path(name: str) -> str:
        return str(relay_watchdog.projects_root() / "selector-tests" / name)

    def test_absent_bind_is_unknown_fail_closed(self):
        verdict = evaluate_selector_sync(None, "/tmp/f.jsonl", True)
        self.assertEqual(verdict.state, SELECTOR_UNKNOWN)
        self.assertEqual(verdict.reason, "bound_output_path_absent")
        self.assertFalse(verdict.diverged)

    def test_no_transcript_is_unknown(self):
        verdict = evaluate_selector_sync("/tmp/b.jsonl", None, True)
        self.assertEqual(verdict.state, SELECTOR_UNKNOWN)
        self.assertEqual(verdict.reason, "no_transcript")
        self.assertFalse(verdict.diverged)

    def test_matching_bind_is_synced(self):
        verdict = evaluate_selector_sync("/tmp/f.jsonl", "/tmp/f.jsonl", True)
        self.assertEqual(verdict.state, SELECTOR_SYNCED)
        self.assertEqual(verdict.reason, "selector_synced")
        self.assertFalse(verdict.diverged)

    def test_mismatch_without_growth_is_not_actionable(self):
        verdict = evaluate_selector_sync(
            self.provider_path("b.jsonl"), self.provider_path("f.jsonl"), False
        )
        self.assertEqual(verdict.state, SELECTOR_SYNCED)
        self.assertEqual(verdict.reason, "f_not_growing")
        self.assertFalse(verdict.diverged)

    def test_mismatch_with_growing_f_is_diverged(self):
        verdict = evaluate_selector_sync(
            self.provider_path("b.jsonl"), self.provider_path("f.jsonl"), True
        )
        self.assertEqual(verdict.state, SELECTOR_DIVERGED)
        self.assertEqual(verdict.reason, "selector_diverged")
        self.assertTrue(verdict.diverged)

    def test_invariant_4435_runtime_mirror_is_unknown_uncomparable(self):
        mirror = str(
            relay_watchdog.adk_root()
            / "runtime"
            / "sessions"
            / "agentdesk-channel.jsonl"
        )
        verdict = evaluate_selector_sync(
            mirror, self.provider_path("f.jsonl"), True
        )
        self.assertEqual(verdict.state, SELECTOR_UNKNOWN)
        self.assertEqual(verdict.reason, "runtime_session_mirror_uncomparable")
        self.assertFalse(verdict.diverged)

    def test_invariant_4435_arbitrary_unequal_paths_are_not_comparable(self):
        verdict = evaluate_selector_sync(
            "/tmp/not-a-provider.jsonl", self.provider_path("f.jsonl"), True
        )
        self.assertEqual(verdict.state, SELECTOR_UNKNOWN)
        self.assertEqual(verdict.reason, "selector_paths_uncomparable")
        self.assertFalse(verdict.diverged)

    def test_invariant_4435_nonexistent_tilde_user_is_unknown_uncomparable(self):
        verdict = evaluate_selector_sync(
            "~agentdesk-user-that-does-not-exist-4435/b.jsonl",
            self.provider_path("f.jsonl"),
            True,
        )
        self.assertEqual(verdict.state, SELECTOR_UNKNOWN)
        self.assertEqual(verdict.reason, "selector_paths_uncomparable")
        self.assertFalse(verdict.diverged)


class SelectorConfirmBoundaryTests(unittest.TestCase):
    """Mutation-2 target: removing the swap-confirm age gate in
    ``selector_divergence_confirmed`` makes the below-threshold case FAIL."""

    def test_below_swap_confirm_is_not_confirmed(self):
        self.assertFalse(selector_divergence_confirmed(True, 299.0, 300))

    def test_exactly_at_swap_confirm_is_confirmed(self):
        self.assertTrue(selector_divergence_confirmed(True, 300.0, 300))

    def test_not_diverged_never_confirms(self):
        self.assertFalse(selector_divergence_confirmed(False, 10_000.0, 300))


class PgHealthEvaluationTests(unittest.TestCase):
    def test_db_true_is_authoritative_even_without_listener_probe(self):
        self.assertEqual(evaluate_pg_health(True, None).state, PG_OK)

    def test_db_false_and_closed_identifies_tunnel_down(self):
        verdict = evaluate_pg_health(False, False)
        self.assertEqual(verdict.state, PG_TUNNEL_DOWN)
        self.assertFalse(verdict.tunnel_open)

    def test_db_false_and_open_identifies_half_dead_or_pg(self):
        verdict = evaluate_pg_health(False, True)
        self.assertEqual(verdict.state, PG_UPSTREAM_DOWN)
        self.assertTrue(verdict.tunnel_open)

    def test_db_false_survives_nc_classifier_failure(self):
        self.assertEqual(
            evaluate_pg_health(False, None).state, PG_UNCLASSIFIED_DOWN
        )

    def test_missing_or_malformed_db_is_unknown_not_down(self):
        for value in (None, "false", 0, {}, []):
            with self.subTest(value=value):
                self.assertEqual(evaluate_pg_health(value, False).state, PG_UNKNOWN)


class ConfigTests(unittest.TestCase):
    def test_minimal_config_parses_with_defaults(self):
        cfg = parse_config(
            {
                "channels": [
                    {
                        "channel_id": "123",
                        "sendmessage_key": "discord_abc",
                        "worktree_root": WORKTREE_ROOT,
                    }
                ]
            }
        )
        self.assertEqual(cfg.channels[0].channel_id, "123")
        self.assertEqual(cfg.channels[0].worktree_prefix, "claude-adk-cc")
        self.assertEqual(cfg.grace_secs, 600)
        self.assertEqual(cfg.gap_alert_secs, 900)
        self.assertEqual(cfg.github_repo, "")
        self.assertEqual(cfg.pg_alert_after_secs, 300)
        self.assertEqual(cfg.pg_realert_secs, 900)
        self.assertEqual(cfg.swap_confirm_secs, 300)

    def test_overrides_apply(self):
        cfg = parse_config(
            {
                "channels": [
                    {
                        "channel_id": "123",
                        "sendmessage_key": "k",
                        "worktree_root": WORKTREE_ROOT,
                        "announce_to": "project-agentdesk",
                    }
                ],
                "gap_alert_secs": 1200,
                "pg_alert_after_secs": 60,
                "pg_realert_secs": 180,
                "swap_confirm_secs": 45,
                "github_repo": "owner/repo",
            }
        )
        self.assertEqual(cfg.gap_alert_secs, 1200)
        self.assertEqual(cfg.pg_alert_after_secs, 60)
        self.assertEqual(cfg.pg_realert_secs, 180)
        self.assertEqual(cfg.swap_confirm_secs, 45)
        self.assertEqual(cfg.github_repo, "owner/repo")
        self.assertEqual(cfg.channels[0].announce_to, "project-agentdesk")

    def test_empty_channels_is_an_error(self):
        with self.assertRaises(ConfigError):
            parse_config({"channels": []})
        with self.assertRaises(ConfigError):
            parse_config({})

    def test_missing_required_channel_key_is_an_error(self):
        with self.assertRaises(ConfigError):
            parse_config({"channels": [{"channel_id": "123"}]})

    # r4 review (PR #4399): a non-numeric numeric field raised bare ValueError,
    # which main()'s retry loop does not catch → process death → KeepAlive
    # crash-loop every ~30s. It must surface as ConfigError so main() logs and
    # retries instead of dying.
    def test_non_numeric_field_is_config_error_not_valueerror(self):
        base = {
            "channels": [
                {
                    "channel_id": "123",
                    "sendmessage_key": "k",
                    "worktree_root": WORKTREE_ROOT,
                }
            ]
        }
        with self.assertRaises(ConfigError):
            parse_config({**base, "poll_secs": "bad"})
        with self.assertRaises(ConfigError):
            parse_config({**base, "gap_alert_secs": None})
        with self.assertRaises(ConfigError):
            parse_config({**base, "pg_alert_after_secs": 0})
        with self.assertRaises(ConfigError):
            parse_config({**base, "pg_realert_secs": -1})
        with self.assertRaises(ConfigError):
            parse_config({**base, "swap_confirm_secs": 0})

    def test_load_config_surfaces_bad_numeric_as_config_error(self):
        # File-level proof that main()'s `except ConfigError` retry path (the
        # crash-loop avoidance) covers the exact config an operator could typo.
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "relay-watchdog.json"
            p.write_text(
                json.dumps(
                    {
                        "channels": [
                            {
                                "channel_id": "123",
                                "sendmessage_key": "k",
                                "worktree_root": WORKTREE_ROOT,
                            }
                        ],
                        "poll_secs": "bad",
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(ConfigError):
                relay_watchdog.load_config(p)


class PgTopologyConfigTests(unittest.TestCase):
    BASE = {
        "channels": [
            {
                "channel_id": "123",
                "sendmessage_key": "k",
                "worktree_root": WORKTREE_ROOT,
            }
        ]
    }

    def test_topology_defaults_to_tunnel_for_existing_configs(self):
        self.assertEqual(parse_config(self.BASE).pg_topology, "tunnel")

    def test_direct_topology_is_accepted(self):
        cfg = parse_config({**self.BASE, "pg_topology": "direct"})
        self.assertEqual(cfg.pg_topology, PG_TOPOLOGY_DIRECT)

    def test_unknown_topology_is_config_error(self):
        with self.assertRaises(ConfigError):
            parse_config({**self.BASE, "pg_topology": "auto"})


class DiscordHaystackShapeTests(unittest.TestCase):
    """r4 review (PR #4399): `agentdesk discord read` returning rc=0 with VALID
    but non-list/dict JSON (`null`, a bare number/string) raised AttributeError
    — read_failures never incremented, so the 'watchdog blind' escalation was
    silently skipped. Such shapes must join the read-failure path (None)."""

    def _probe(self, stdout: str) -> "tuple[str | None, str]":
        """Returns (haystack result, watchdog log contents)."""

        def fake_run(argv, **kwargs):
            return subprocess.CompletedProcess(argv, 0, stdout=stdout, stderr="")

        with tempfile.TemporaryDirectory() as tmp:
            rt = Runtime(Config(channels=(TICK_CHANNEL,)), Path(tmp))
            with mock.patch.object(
                relay_watchdog.subprocess, "run", side_effect=fake_run
            ):
                out = rt.discord_haystack("999")
            try:
                log = rt.log_path.read_text(encoding="utf-8")
            except OSError:
                log = ""
        return out, log

    def _haystack_for(self, stdout: str) -> "str | None":
        return self._probe(stdout)[0]

    def test_valid_but_non_collection_json_is_read_failure(self):
        for stdout in ("null", "123", '"str"'):
            with self.subTest(stdout=stdout):
                self.assertIsNone(self._haystack_for(stdout))

    def test_dict_with_non_list_messages_is_read_failure(self):
        self.assertIsNone(self._haystack_for('{"messages": 5}'))

    # r5 review (PR #4399): a NON-EMPTY list with zero dict entries used to
    # collapse to '' — a "successful" empty read that never incremented
    # read_failures, bypassing the watchdog-blind escalation. It must be a
    # read failure (None).
    def test_all_malformed_entries_is_read_failure(self):
        self.assertIsNone(self._haystack_for("[null, 7]"))

    def test_all_malformed_entries_in_dict_wrapper_is_read_failure(self):
        self.assertIsNone(self._haystack_for('{"messages": [null, 7]}'))

    def test_empty_list_is_a_normal_empty_channel(self):
        out, log = self._probe("[]")
        self.assertEqual(out, "")
        self.assertNotIn("schema drift", log)

    def test_mixed_entries_parse_dicts_and_log_schema_drift(self):
        # Partial data beats blindness, but the drift must leave a trace.
        out, log = self._probe(
            '[null, 7, {"author": {"bot": true}, "content": "hello  world"}]'
        )
        self.assertEqual(out, "hello world")
        self.assertIn("skipped 2 malformed message entries", log)
        self.assertIn("schema drift", log)

    # r6 review (PR #4399): entries that ARE dicts but carry a non-dict
    # `author` (`{"author": "bot"}` → AttributeError) or a non-string
    # `content` (TypeError at join) used to raise instead of classifying —
    # a generic tick error that bypassed the read_failures escalation, the
    # same failure class r4/r5 closed for non-dict shapes.
    def test_all_malformed_dict_entries_is_read_failure_not_exception(self):
        for stdout in (
            '[{"author": "bot", "content": "x"}]',
            '[{"author": {"bot": true}, "content": [1, 2]}]',
            '[{"author": "bot"}, {"author": {"bot": true}, "content": 7}]',
        ):
            with self.subTest(stdout=stdout):
                self.assertIsNone(self._haystack_for(stdout))

    def test_malformed_dict_entry_mixed_with_valid_is_skipped_with_trace(self):
        out, log = self._probe(
            '[{"author": "bot", "content": "bad"},'
            ' {"author": {"bot": true}, "content": "good"}]'
        )
        self.assertEqual(out, "good")
        self.assertIn("skipped 1 malformed message entries", log)
        self.assertIn("schema drift", log)

    def test_absent_author_and_absent_content_stay_well_formed(self):
        # A dict entry missing `author`/`content` was always tolerated (a
        # non-bot or empty message) — r6 must not reclassify it as drift.
        out, log = self._probe(
            '[{}, {"author": {"bot": true}, "content": "ok"},'
            ' {"author": {"bot": true}}]'
        )
        self.assertEqual(out, "ok")
        self.assertNotIn("schema drift", log)

    def test_valid_shapes_still_parse(self):
        self.assertEqual(
            self._haystack_for('[{"author": {"bot": true}, "content": "ok"}]'),
            "ok",
        )
        self.assertEqual(
            self._haystack_for(
                '{"messages": [{"author": {"bot": true}, "content": "ok"}]}'
            ),
            "ok",
        )


class StateTests(unittest.TestCase):
    def test_round_trip(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "state.json"
            save_state(p, {"123": {"last_alert": 1.0, "alerting": True}})
            self.assertEqual(
                load_state(p), {"123": {"last_alert": 1.0, "alerting": True}}
            )

    def test_corrupt_state_yields_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "state.json"
            p.write_text("garbage{", encoding="utf-8")
            self.assertEqual(load_state(p), {})
            self.assertEqual(load_state(Path(tmp) / "missing.json"), {})

    def test_invariant_4435_malformed_watermarks_are_ignored_fail_open(self):
        state = {
            DELIVERED_WATERMARKS_KEY: {
                "/bool": {"delivered_ts": True, "updated_at": 1.0},
                "/negative": {"delivered_ts": -1.0, "updated_at": 1.0},
                "/nan": {"delivered_ts": float("nan"), "updated_at": 1.0},
                "/inf": {"delivered_ts": 1.0, "updated_at": float("inf")},
                "/not-object": 123,
                "/valid": {"delivered_ts": 12.5, "updated_at": 13.0},
            }
        }
        self.assertEqual(delivered_watermarks(state), {"/valid": (12.5, 13.0)})
        self.assertEqual(delivered_watermark_for_path(state, "/nan"), 0.0)
        self.assertEqual(delivered_watermark_for_path(state, "/valid"), 12.5)

    def test_invariant_4435_watermark_map_is_deterministically_bounded(self):
        state: dict = {}
        for index in range(MAX_DELIVERED_WATERMARKS + 1):
            self.assertTrue(
                advance_delivered_watermark(
                    state,
                    f"/transcript-{index:02}.jsonl",
                    100.0 + index,
                    1000.0 + index,
                )
            )
        entries = delivered_watermarks(state)
        self.assertEqual(len(entries), MAX_DELIVERED_WATERMARKS)
        self.assertNotIn("/transcript-00.jsonl", entries)
        self.assertEqual(
            list(entries)[0],
            f"/transcript-{MAX_DELIVERED_WATERMARKS:02}.jsonl",
        )

    def test_invariant_4435_watermark_advancement_is_path_scoped_and_monotonic(self):
        state: dict = {}
        self.assertTrue(advance_delivered_watermark(state, "/a.jsonl", 20.0, 30.0))
        self.assertFalse(advance_delivered_watermark(state, "/a.jsonl", 19.0, 31.0))
        self.assertEqual(delivered_watermark_for_path(state, "/a.jsonl"), 20.0)
        self.assertEqual(delivered_watermark_for_path(state, "/b.jsonl"), 0.0)

    def test_invariant_4435_selected_watermark_is_pinned_under_cap_pressure(self):
        selected = "/z-selected.jsonl"
        state: dict = {SELECTED_TRANSCRIPT_KEY: selected}
        self.assertTrue(advance_delivered_watermark(state, selected, 50.0, 100.0))
        for index in range(MAX_DELIVERED_WATERMARKS):
            self.assertTrue(
                advance_delivered_watermark(
                    state,
                    f"/a-{index:02d}.jsonl",
                    60.0 + index,
                    100.0,
                )
            )
        self.assertEqual(len(delivered_watermarks(state)), MAX_DELIVERED_WATERMARKS)
        self.assertEqual(delivered_watermark_for_path(state, selected), 50.0)

    def test_invariant_4435_all_active_pending_watermarks_are_pinned(self):
        selected = "/selected.jsonl"
        pending = [
            f"/pending-{index:02d}.jsonl"
            for index in range(relay_watchdog.MAX_PENDING_TRANSCRIPTS)
        ]
        state: dict = {
            SELECTED_TRANSCRIPT_KEY: selected,
            "pending_transcripts": pending,
        }
        for index, path in enumerate([selected, *pending]):
            self.assertTrue(
                advance_delivered_watermark(state, path, 10.0 + index, 20.0)
            )
        for index in range(MAX_DELIVERED_WATERMARKS + 5):
            advance_delivered_watermark(
                state,
                f"/unrelated-{index:02d}.jsonl",
                100.0 + index,
                1000.0 + index,
            )

        retained = delivered_watermarks(state)
        self.assertEqual(len(retained), MAX_DELIVERED_WATERMARKS)
        self.assertTrue(set([selected, *pending]) <= set(retained))

    def test_invariant_4435_watermark_cap_fits_full_authority_union(self):
        selected = "/selected.jsonl"
        pending = [
            f"/pending-{index:02d}.jsonl"
            for index in range(relay_watchdog.MAX_PENDING_TRANSCRIPTS)
        ]
        gap_owners = [
            f"/gap-owner-{index:02d}.jsonl"
            for index in range(relay_watchdog.MAX_PENDING_TRANSCRIPTS + 1)
        ]
        recovered = [
            f"/recovered-{index:03d}.jsonl"
            for index in range(relay_watchdog.MAX_RECOVERED_GAP_GUARDS)
        ]
        authorities = [selected, *pending, *gap_owners, *recovered]
        unrelated = "/unrelated-newer.jsonl"
        state = {
            SELECTED_TRANSCRIPT_KEY: selected,
            relay_watchdog.PENDING_TRANSCRIPTS_KEY: pending,
            relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY: gap_owners,
            relay_watchdog.RECOVERED_GAP_GUARDS_KEY: {
                path: {
                    "size": index,
                    "confirmed_at": 100.0,
                    "last_seen_at": 100.0,
                    "absent_since": None,
                }
                for index, path in enumerate(recovered)
            },
            DELIVERED_WATERMARKS_KEY: {
                path: {"delivered_ts": 10.0 + index, "updated_at": 100.0}
                for index, path in enumerate(authorities)
            }
            | {
                unrelated: {
                    "delivered_ts": 999.0,
                    "updated_at": 999.0,
                }
            },
        }

        retained = delivered_watermarks(state)

        self.assertEqual(
            MAX_DELIVERED_WATERMARKS,
            1
            + relay_watchdog.MAX_PENDING_TRANSCRIPTS
            + (relay_watchdog.MAX_PENDING_TRANSCRIPTS + 1)
            + relay_watchdog.MAX_RECOVERED_GAP_GUARDS,
        )
        self.assertEqual(len(retained), MAX_DELIVERED_WATERMARKS)
        self.assertTrue(set(authorities) <= set(retained))
        self.assertNotIn(unrelated, retained)

    def test_invariant_4435_gap_owner_validation_stays_deduped_and_bounded(self):
        valid = [
            f"/owner-{index:02d}.jsonl"
            for index in range(relay_watchdog.MAX_PENDING_TRANSCRIPTS + 5)
        ]
        state = {
            relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY: [
                valid[0],
                "",
                None,
                True,
                valid[0],
                *valid[1:],
            ],
            relay_watchdog.GAP_TRANSCRIPT_KEY: valid[-1],
        }

        owners = relay_watchdog._validated_gap_owner_transcripts(state)

        self.assertEqual(
            owners,
            valid[: relay_watchdog.MAX_PENDING_TRANSCRIPTS + 1],
        )
        self.assertEqual(len(owners), len(set(owners)))

    def test_invariant_4435_recovered_guard_validation_is_strict_and_bounded(self):
        valid = {
            f"/guard-{index:03d}.jsonl": {
                "size": index,
                "confirmed_at": 10.0,
                "last_seen_at": 11.0,
                "absent_since": None,
            }
            for index in range(relay_watchdog.MAX_RECOVERED_GAP_GUARDS + 5)
        }
        state = {
            relay_watchdog.RECOVERED_GAP_GUARDS_KEY: {
                "/bad-bool.jsonl": {
                    "size": True,
                    "confirmed_at": 10.0,
                    "last_seen_at": 11.0,
                    "absent_since": None,
                },
                "/bad-clock.jsonl": {
                    "size": 1,
                    "confirmed_at": 10.0,
                    "last_seen_at": 20.0,
                    "absent_since": 19.0,
                },
                **valid,
            }
        }

        guards = relay_watchdog._validated_recovered_gap_guards(state)

        self.assertEqual(len(guards), relay_watchdog.MAX_RECOVERED_GAP_GUARDS)
        self.assertNotIn("/bad-bool.jsonl", guards)
        self.assertNotIn("/bad-clock.jsonl", guards)

    def test_invariant_4435_recovered_guard_capacity_outlives_one_gap_wave(self):
        guards: dict = {}
        for index in range(relay_watchdog.MAX_GAP_OWNER_TRANSCRIPTS + 1):
            guards, admitted = relay_watchdog._upsert_recovered_gap_guard(
                guards, f"/guard-{index:03d}.jsonl", index, 100.0 + index
            )
            self.assertTrue(admitted)
        self.assertGreater(
            len(guards), relay_watchdog.MAX_GAP_OWNER_TRANSCRIPTS
        )

        for index in range(
            len(guards), relay_watchdog.MAX_RECOVERED_GAP_GUARDS
        ):
            guards, admitted = relay_watchdog._upsert_recovered_gap_guard(
                guards, f"/guard-{index:03d}.jsonl", index, 100.0 + index
            )
            self.assertTrue(admitted)
        before = dict(guards)
        guards, admitted = relay_watchdog._upsert_recovered_gap_guard(
            guards, "/overflow.jsonl", 1, 999.0
        )
        self.assertFalse(admitted)
        self.assertEqual(guards, before)


class RuntimePgProbeTests(unittest.TestCase):
    def test_health_detail_db_false_is_classified_by_nc_closed(self):
        calls: list[list[str]] = []

        def fake_run(argv, **kwargs):
            calls.append(list(argv))
            if argv[0] == "curl":
                return subprocess.CompletedProcess(
                    argv, 0, stdout='{"db": false}', stderr=""
                )
            return subprocess.CompletedProcess(argv, 1, stdout=b"", stderr=b"")

        with tempfile.TemporaryDirectory() as tmp:
            rt = Runtime(Config(channels=(TICK_CHANNEL,)), Path(tmp))
            with mock.patch.object(
                relay_watchdog.subprocess, "run", side_effect=fake_run
            ):
                verdict = rt.pg_health()
        self.assertEqual(verdict.state, PG_TUNNEL_DOWN)
        self.assertIn("/api/health/detail", calls[0][-1])
        self.assertNotIn("-f", calls[0], "non-2xx health JSON must remain readable")
        self.assertEqual(calls[1][-2:], ["127.0.0.1", "15432"])

    def test_unknown_health_does_not_consult_nc_or_claim_tunnel_down(self):
        calls: list[list[str]] = []

        def fake_run(argv, **kwargs):
            calls.append(list(argv))
            return subprocess.CompletedProcess(argv, 0, stdout="not-json", stderr="")

        with tempfile.TemporaryDirectory() as tmp:
            rt = Runtime(Config(channels=(TICK_CHANNEL,)), Path(tmp))
            with mock.patch.object(
                relay_watchdog.subprocess, "run", side_effect=fake_run
            ):
                verdict = rt.pg_health()
        self.assertEqual(verdict.state, PG_UNKNOWN)
        self.assertEqual(len(calls), 1)

    def test_dcserver_alert_stamp_is_read_fail_open(self):
        with tempfile.TemporaryDirectory() as tmp:
            rt = Runtime(
                Config(channels=(TICK_CHANNEL,), pg_realert_secs=900), Path(tmp)
            )
            rt.dcserver_pg_alert_state.parent.mkdir(parents=True)
            rt.dcserver_pg_alert_state.write_text("1000", encoding="utf-8")
            self.assertTrue(rt.recent_dcserver_pg_alert(1899))
            self.assertFalse(rt.recent_dcserver_pg_alert(1900))
            self.assertFalse(rt.recent_dcserver_pg_alert(999))
            rt.dcserver_pg_alert_state.write_text("rolled-back", encoding="utf-8")
            self.assertFalse(rt.recent_dcserver_pg_alert(1001))


class RuntimeCoverageProbeTests(unittest.TestCase):
    def make_rt(self) -> Runtime:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        return Runtime(Config(channels=(TICK_CHANNEL,)), Path(self._tmp.name))

    def test_tmux_enumeration_includes_only_sessions_with_live_panes(self):
        stdout = (
            "AgentDesk-claude-adk-cc\t0\n"
            "AgentDesk-codex-adk-cdx\t1\n"
            "AgentDesk-mixed\t1\n"
            "AgentDesk-mixed\t0\n"
        )
        completed = subprocess.CompletedProcess(
            ["tmux"], 0, stdout=stdout, stderr=""
        )
        with mock.patch.object(relay_watchdog.subprocess, "run", return_value=completed):
            sessions = self.make_rt().live_tmux_sessions()
        self.assertEqual(
            sessions, {"AgentDesk-claude-adk-cc", "AgentDesk-mixed"}
        )

    def test_tmux_probe_failure_is_unknown(self):
        completed = subprocess.CompletedProcess(
            ["tmux"], 2, stdout="", stderr="permission denied"
        )
        with mock.patch.object(relay_watchdog.subprocess, "run", return_value=completed):
            self.assertIsNone(self.make_rt().live_tmux_sessions())

    def test_no_tmux_server_is_authoritative_empty_set(self):
        completed = subprocess.CompletedProcess(
            ["tmux"], 1, stdout="", stderr="no server running on /tmp/tmux"
        )
        with mock.patch.object(relay_watchdog.subprocess, "run", return_value=completed):
            self.assertEqual(self.make_rt().live_tmux_sessions(), set())

    def test_watcher_state_404_is_preserved_as_uncovered_evidence(self):
        completed = subprocess.CompletedProcess(
            ["curl"],
            0,
            stdout='{"error":"missing"}\n404',
            stderr="",
        )
        with mock.patch.object(relay_watchdog.subprocess, "run", return_value=completed):
            probe = self.make_rt().watcher_state("999")
        self.assertEqual(probe, WatcherStateProbe(404))

    def test_watcher_state_unreachable_is_unknown(self):
        completed = subprocess.CompletedProcess(
            ["curl"], 7, stdout="\n000", stderr="connect failed"
        )
        with mock.patch.object(relay_watchdog.subprocess, "run", return_value=completed):
            probe = self.make_rt().watcher_state("999")
        self.assertEqual(probe, WatcherStateProbe(None))

    def test_watcher_state_parses_exact_boolean_contract(self):
        completed = subprocess.CompletedProcess(
            ["curl"],
            0,
            stdout='{"attached":true,"desynced":false}\n200',
            stderr="",
        )
        with mock.patch.object(relay_watchdog.subprocess, "run", return_value=completed):
            probe = self.make_rt().watcher_state("999")
        self.assertEqual(probe, WatcherStateProbe(200, True, False))

    def test_watcher_state_wires_production_active_foreground_shape(self):
        completed = subprocess.CompletedProcess(
            ["curl"],
            0,
            stdout=json.dumps(WatcherStateParserTests.payload()) + "\n200",
            stderr="",
        )
        with mock.patch.object(relay_watchdog.subprocess, "run", return_value=completed):
            probe = self.make_rt().watcher_state("999")
        self.assertEqual(probe.status, 200)
        self.assertTrue(probe.attached)
        self.assertTrue(probe.desynced)
        self.assertEqual(
            probe.relay_activity,
            CoverageEvaluationTests.active_foreground(),
        )

    def test_production_activity_clock_anomalies_fail_closed(self):
        cases = (
            (
                "future",
                CoverageEvaluationTests.NOW_MS + 1,
                "active_foreground_activity_future",
            ),
            (
                "oversized",
                10**400,
                "active_foreground_activity_invalid",
            ),
        )
        for name, timestamp, reason in cases:
            with self.subTest(name=name):
                completed = subprocess.CompletedProcess(
                    ["curl"],
                    0,
                    stdout=(
                        json.dumps(
                            WatcherStateParserTests.payload(
                                last_outbound_activity_ms=timestamp,
                                last_relay_ts_ms=None,
                            )
                        )
                        + "\n200"
                    ),
                    stderr="",
                )
                with mock.patch.object(
                    relay_watchdog.subprocess,
                    "run",
                    return_value=completed,
                ):
                    probe = self.make_rt().watcher_state("999")
                activity_verdict = evaluate_active_foreground_coverage(
                    probe.relay_activity,
                    CoverageEvaluationTests.NOW_MS,
                )
                self.assertEqual(activity_verdict.state, COVERAGE_UNCOVERED)
                self.assertEqual(activity_verdict.reason, reason)
                coverage_verdict = evaluate_coverage(
                    True,
                    probe.status,
                    probe.attached,
                    probe.desynced,
                    1,
                    probe.relay_activity,
                    CoverageEvaluationTests.NOW_MS,
                )
                self.assertEqual(coverage_verdict.state, COVERAGE_UNCOVERED)
                self.assertEqual(
                    coverage_verdict.reason,
                    "attached_but_desynced",
                )
                self.assertTrue(coverage_verdict.confirmed)

    def test_watcher_state_malformed_200_keeps_status_but_not_claims(self):
        completed = subprocess.CompletedProcess(
            ["curl"], 0, stdout='{"attached":"true"}\n200', stderr=""
        )
        with mock.patch.object(relay_watchdog.subprocess, "run", return_value=completed):
            probe = self.make_rt().watcher_state("999")
        self.assertEqual(
            probe, WatcherStateProbe(200, None, None, response_malformed=True)
        )

    def test_direct_node_snapshot_does_not_report_tunnel_closed(self):
        calls: list[list[str]] = []

        def fake_run(argv, **kwargs):
            calls.append(list(argv))
            if argv[0] == "curl":
                return subprocess.CompletedProcess(
                    argv,
                    0,
                    stdout='{"db":false,"degraded":true}',
                    stderr="",
                )
            if argv[0] == "/bin/ps":
                return subprocess.CompletedProcess(argv, 0, stdout="", stderr="")
            raise AssertionError(f"unexpected direct-node probe: {argv}")

        with tempfile.TemporaryDirectory() as tmp:
            rt = Runtime(
                Config(
                    channels=(TICK_CHANNEL,), pg_topology=PG_TOPOLOGY_DIRECT
                ),
                Path(tmp),
            )
            with mock.patch.object(
                relay_watchdog.subprocess, "run", side_effect=fake_run
            ):
                snapshot = rt.dcserver_snapshot()
        self.assertIn("pg-topology DIRECT", snapshot)
        self.assertNotIn("pg-tunnel CLOSED", snapshot)
        self.assertFalse(any(call[0] == "nc" for call in calls))


TICK_CHANNEL = ChannelConfig(
    channel_id="999",
    sendmessage_key="k",
    worktree_root=WORKTREE_ROOT,
)


class FakeRuntime(Runtime):
    """Runtime with every subprocess/network edge stubbed; tick_channel logic
    (including the REAL in_deploy_window file check) runs unmodified."""

    def __init__(self, cfg: Config, root: Path) -> None:
        super().__init__(cfg, root)
        self.alerts: list[tuple[str, bool]] = []
        self.log_lines: list[str] = []
        self.haystack: str | None = ""
        self.issue_calls = 0
        # #5190: what the watchdog would put in the auto-filed issue. `None`
        # means "no delivery on record"; the defect was rendering that as 999.
        self.issue_gap_mins: list[int | None] = []
        self.alert_succeeds = True
        self.live_sessions: set[str] | None = set()
        self.watcher_probe = WatcherStateProbe(None)
        self.watcher_calls = 0

    def log(self, msg: str) -> None:
        self.log_lines.append(msg)

    def discord_haystack(self, channel_id: str) -> str | None:
        return self.haystack

    def live_tmux_sessions(self) -> set[str] | None:
        return self.live_sessions

    def watcher_state(self, channel_id: str) -> WatcherStateProbe:
        self.watcher_calls += 1
        return self.watcher_probe

    def dcserver_snapshot(self) -> str:
        return "stub-snapshot"

    def alert(self, ch, body: str, trigger_turn: bool = True) -> bool:
        self.alerts.append((body, trigger_turn))
        return self.alert_succeeds

    def file_github_issue(self, ch, gap_min: int | None, lost: int) -> str:
        self.issue_calls += 1
        self.issue_gap_mins.append(gap_min)
        return f"https://example.test/issues/{self.issue_calls}"


class FakePgRuntime(Runtime):
    def __init__(
        self,
        verdict,
        *,
        after: int = 300,
        cooldown: int = 900,
        topology: str = "tunnel",
    ) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        cfg = Config(
            channels=(TICK_CHANNEL,),
            pg_alert_after_secs=after,
            pg_realert_secs=cooldown,
            pg_topology=topology,
        )
        super().__init__(cfg, Path(self._tmp.name))
        self.verdict = verdict
        self.dcserver_recent = False
        self.alerts: list[tuple[str, bool]] = []
        self.log_lines: list[str] = []

    def cleanup(self) -> None:
        self._tmp.cleanup()

    def pg_health(self):
        return self.verdict

    def recent_dcserver_pg_alert(self, now: float) -> bool:
        return self.dcserver_recent

    def dcserver_snapshot(self) -> str:
        return "stub-pg-snapshot"

    def alert(self, ch, body: str, trigger_turn: bool = True) -> bool:
        self.alerts.append((body, trigger_turn))
        return True

    def log(self, msg: str) -> None:
        self.log_lines.append(msg)


class TickPgTunnelTests(unittest.TestCase):
    NOW = 10_000.0

    def make_rt(self, verdict, **kwargs) -> FakePgRuntime:
        rt = FakePgRuntime(verdict, **kwargs)
        self.addCleanup(rt.cleanup)
        return rt

    def test_alerts_only_at_persistence_boundary(self):
        rt = self.make_rt(evaluate_pg_health(False, False))
        state: dict = {}
        tick_pg_tunnel(rt, state, self.NOW)
        tick_pg_tunnel(rt, state, self.NOW + 299)
        self.assertEqual(rt.alerts, [])
        tick_pg_tunnel(rt, state, self.NOW + 300)
        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("CLOSED", rt.alerts[0][0])
        self.assertEqual(state[PG_STATE_KEY]["last_alert"], self.NOW + 300)

    def test_open_listener_reports_half_dead_or_upstream(self):
        rt = self.make_rt(evaluate_pg_health(False, True))
        state = {PG_STATE_KEY: {"unhealthy_since": self.NOW - 300}}
        tick_pg_tunnel(rt, state, self.NOW)
        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("OPEN", rt.alerts[0][0])
        self.assertIn("half-dead", rt.alerts[0][0])

    def test_direct_node_closed_wording_does_not_blame_ssh_supervisor(self):
        rt = self.make_rt(
            evaluate_pg_health(False, False), topology=PG_TOPOLOGY_DIRECT
        )
        state = {PG_STATE_KEY: {"unhealthy_since": self.NOW - 300}}
        tick_pg_tunnel(rt, state, self.NOW)
        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("CLOSED", rt.alerts[0][0])
        self.assertIn("direct-node topology", rt.alerts[0][0])
        self.assertNotIn("SSH -L supervisor 재기동 루프 실패", rt.alerts[0][0])
        self.assertEqual(state[PG_STATE_KEY]["cause"], "direct_postgres_down")
        self.assertTrue(any("cause=direct_postgres_down" in l for l in rt.log_lines))

        rt.verdict = evaluate_pg_health(True, None)
        tick_pg_tunnel(rt, state, self.NOW + 1)
        self.assertIn("direct_postgres_down", rt.alerts[1][0])
        self.assertNotIn("tunnel_down", rt.alerts[1][0])

    def test_realert_cooldown_boundary_is_exact(self):
        verdict = evaluate_pg_health(False, False)
        rt = self.make_rt(verdict)
        state = {
            PG_STATE_KEY: {
                "unhealthy_since": self.NOW - 1000,
                "last_alert": self.NOW - 899,
                "alerting": True,
            }
        }
        tick_pg_tunnel(rt, state, self.NOW)
        self.assertEqual(rt.alerts, [])
        tick_pg_tunnel(rt, state, self.NOW + 1)
        self.assertEqual(len(rt.alerts), 1)

    def test_recovery_notifies_and_keeps_antiflap_cooldown(self):
        rt = self.make_rt(evaluate_pg_health(True, None))
        state = {
            PG_STATE_KEY: {
                "unhealthy_since": self.NOW - 600,
                "last_alert": self.NOW - 60,
                "alerting": True,
                "cause": PG_TUNNEL_DOWN,
            }
        }
        tick_pg_tunnel(rt, state, self.NOW)
        self.assertEqual(len(rt.alerts), 1)
        body, trigger_turn = rt.alerts[0]
        self.assertIn("복구", body)
        self.assertFalse(trigger_turn)
        self.assertEqual(state[PG_STATE_KEY], {"last_alert": self.NOW - 60})

    def test_recent_dcserver_alert_defers_exactly_one_tick(self):
        rt = self.make_rt(evaluate_pg_health(False, False))
        rt.dcserver_recent = True
        state = {PG_STATE_KEY: {"unhealthy_since": self.NOW - 300}}
        tick_pg_tunnel(rt, state, self.NOW)
        self.assertEqual(rt.alerts, [])
        self.assertTrue(state[PG_STATE_KEY]["dedup_deferred"])
        tick_pg_tunnel(rt, state, self.NOW + rt.cfg.poll_secs)
        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("1 tick 보류", rt.alerts[0][0])

    def test_unknown_health_breaks_pending_timer_but_not_active_alert(self):
        rt = self.make_rt(evaluate_pg_health(None, False))
        pending = {PG_STATE_KEY: {"unhealthy_since": self.NOW - 299}}
        tick_pg_tunnel(rt, pending, self.NOW)
        self.assertNotIn("unhealthy_since", pending[PG_STATE_KEY])

        active = {
            PG_STATE_KEY: {
                "unhealthy_since": self.NOW - 1000,
                "last_alert": self.NOW - 10,
                "alerting": True,
            }
        }
        tick_pg_tunnel(rt, active, self.NOW)
        self.assertTrue(active[PG_STATE_KEY]["alerting"])
        self.assertEqual(rt.alerts, [])


class TickChannelTests(unittest.TestCase):
    """Orchestration-level behavior: suppression windows, cooldown, recovery,
    issue dedup, read-failure escalation. These exercise tick_channel itself —
    the pure-judgment tests above cannot catch a broken wiring of it (adversarial
    review finding on PR #4399: neutering in_deploy_window left 35/35 green)."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        (self.root / "logs").mkdir()
        self.projects = self.root / "projects"
        self.proj_dir = self.projects / (
            "-Users-alice--adk-release-worktrees-claude-adk-cc-20260709-140500"
        )
        self.proj_dir.mkdir(parents=True)
        env = mock.patch.dict(
            os.environ, {"CLAUDE_PROJECTS_ROOT": str(self.projects)}
        )
        env.start()
        self.addCleanup(env.stop)
        self.now = time.time()

    def write_transcript(self, blocks: list[tuple[float, str]]) -> None:
        lines = []
        for index, (epoch, text) in enumerate(blocks):
            ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch))
            lines.append(
                json.dumps(
                    {
                        "type": "assistant",
                        "uuid": f"00000000-0000-4000-8000-{index:012d}",
                        "timestamp": ts,
                        "message": {"content": [{"type": "text", "text": text}]},
                    }
                )
            )
        (self.proj_dir / "s.jsonl").write_text(
            "\n".join(lines) + "\n", encoding="utf-8"
        )

    def make_rt(self, **cfg_overrides) -> FakeRuntime:
        cfg = Config(channels=(TICK_CHANNEL,), **cfg_overrides)
        return FakeRuntime(cfg, self.root)

    def gap_rt(self, **cfg_overrides) -> FakeRuntime:
        # One stale undelivered block, nothing ever delivered → GAP verdict.
        self.write_transcript([(self.now - 2000, "never delivered block")])
        rt = self.make_rt(**cfg_overrides)
        rt.haystack = ""
        return rt

    def arm_coverage(
        self, rt: FakeRuntime, probe: WatcherStateProbe
    ) -> None:
        rt.live_sessions = {expected_tmux_session_name(TICK_CHANNEL)}
        rt.watcher_probe = probe

    def active_foreground_probe(self, **overrides) -> WatcherStateProbe:
        inflight_updated_at = overrides.pop("inflight_updated_at", None)
        fields = {
            "relay_stall_state": "active_foreground_stream",
            "active_turn": "foreground",
            "queue_depth": 0,
            "tmux_alive": True,
            "watcher_attached": True,
            "watcher_attached_stale": False,
            "watcher_owns_live_relay": True,
            "last_outbound_activity_ms": int(self.now * 1000) - 1,
            "last_relay_ts_ms": int(self.now * 1000) - 2,
            "desynced": True,
        }
        fields.update(overrides)
        return WatcherStateProbe(
            status=200,
            attached=True,
            desynced=True,
            relay_activity=CoverageActivityProbe(**fields),
            inflight_updated_at=inflight_updated_at,
        )

    def test_metadata_only_growth_cannot_steal_live_selection(self):
        stale_compact = self.proj_dir / "stale-compact.jsonl"
        stagnant_dir = self.projects / (
            "-Users-alice--adk-release-worktrees-claude-adk-cc-20260710-140500"
        )
        stagnant_dir.mkdir()
        current = stagnant_dir / "current.jsonl"

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        stale_compact.write_text(
            "".join(
                record(self.now - 3000 - index, f"old missing block {index}") + "\n"
                for index in range(490)
            ),
            encoding="utf-8",
        )
        current.write_text(
            record(self.now - 30, "current live block landed") + "\n",
            encoding="utf-8",
        )
        os.utime(stale_compact, (self.now - 100, self.now - 100))
        os.utime(current, (self.now, self.now))
        rt = self.make_rt()
        rt.haystack = norm("current live block landed")
        state: dict = {}

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(rt.alerts, [], "first tick must use mtime fallback")
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(current))

        with stale_compact.open("a", encoding="utf-8") as f:
            f.write("\n" + json.dumps({"type": "queue-operation"}) + "\n")
        # This is the live #4435 recurrence: compact/dead transcript metadata
        # grows and gets a fresh mtime, but no deliverable assistant row exists.
        os.utime(stale_compact, (self.now + 1, self.now + 1))
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        self.assertEqual(rt.alerts, [])
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(current))
        self.assertTrue(
            any(
                f"transcript-growth-ignored reason=non-semantic path={stale_compact}"
                in line
                for line in rt.log_lines
            )
        )

    def test_unchanged_transcript_above_replay_floor_is_not_growth(self):
        # Live #5072 recurrence: a dead transcript whose replay floor sits below
        # its own semantic end re-reported "growth" on every tick, pinning the
        # selector to a worktree that had not been written to in days and
        # holding the I1 selector-sync alert open against a healthy relay.
        dead = self.proj_dir / "dead.jsonl"
        live_dir = self.projects / (
            "-Users-alice--adk-release-worktrees-claude-adk-cc-20260710-140500"
        )
        live_dir.mkdir()
        live = live_dir / "live.jsonl"

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        # The trailing block is undelivered, so the gap-recovery re-arm at the
        # end of the tick is refused (fresh_undelivered > 0) — the production
        # shape in which the floor never advances on its own.
        dead.write_text(
            "".join(
                record(self.now - 5000 - index, f"dead block {index}") + "\n"
                for index in range(7)
            )
            + record(self.now - 200, "dead block 7") + "\n",
            encoding="utf-8",
        )
        live.write_text(record(self.now - 30, "live block landed") + "\n", "utf-8")
        os.utime(dead, (self.now - 4000, self.now - 4000))
        os.utime(live, (self.now, self.now))

        dead_size = dead.stat().st_size
        rt = self.make_rt()
        rt.haystack = norm(
            " ".join(f"dead block {index}" for index in range(7))
            + " live block landed"
        )
        # Guard armed at a recovery point below the transcript's semantic end:
        # exactly the production shape (floor 4025250 vs file 4070983).
        state: dict = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(dead),
                relay_watchdog.TRANSCRIPT_SIZES_KEY: {str(dead): dead_size},
                relay_watchdog.RECOVERED_GAP_GUARDS_KEY: {
                    str(dead): {
                        "size": dead_size - 100,
                        "confirmed_at": self.now - 4500,
                        "last_seen_at": self.now - 10,
                        "absent_since": None,
                    }
                },
            }
        }

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertFalse(
            any(
                f"transcript-select reason=growth path={dead}" in line
                for line in rt.log_lines
            ),
            "an unchanged transcript must not report growth off its replay floor",
        )

        rt.log_lines.clear()
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        self.assertFalse(
            any(
                f"transcript-select reason=growth path={dead}" in line
                for line in rt.log_lines
            ),
            "the false growth signal must not return on later ticks either",
        )
        self.assertEqual(
            state["999"][relay_watchdog.RECOVERED_GAP_GUARDS_KEY][str(dead)]["size"],
            dead_size - 100,
            "the replay floor must stay below the undelivered block (#4435)",
        )

    def test_timestamped_assistant_growth_can_switch_selection(self):
        prior = self.proj_dir / "prior.jsonl"
        current_dir = self.projects / (
            "-Users-alice--adk-release-worktrees-claude-adk-cc-20260710-140500"
        )
        current_dir.mkdir()
        current = current_dir / "current.jsonl"

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        prior.write_text(record(self.now - 60, "prior delivered") + "\n", encoding="utf-8")
        current.write_text(record(self.now - 30, "current delivered") + "\n", encoding="utf-8")
        os.utime(prior, (self.now - 100, self.now - 100))
        os.utime(current, (self.now, self.now))
        rt = self.make_rt()
        rt.haystack = norm("prior delivered current delivered semantic growth delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        with prior.open("a", encoding="utf-8") as stream:
            stream.write(record(self.now + 1, "semantic growth delivered") + "\n")
        os.utime(prior, (self.now - 50, self.now - 50))
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(prior))
        self.assertTrue(
            any("transcript-select reason=growth" in line for line in rt.log_lines)
        )

    def _grow_selected_transcript(self) -> None:
        tr = self.proj_dir / "s.jsonl"
        with tr.open("a", encoding="utf-8") as f:
            f.write(
                json.dumps(
                    {
                        "type": "assistant",
                        "timestamp": time.strftime(
                            "%Y-%m-%dT%H:%M:%SZ", time.gmtime(self.now)
                        ),
                        "message": {
                            "content": [{"type": "text", "text": "selector growth"}]
                        },
                    }
                )
                + "\n"
            )
        os.utime(tr, (self.now, self.now))

    def test_selector_divergence_alerts_only_after_swap_confirm(self):
        # One delivered block → gap verdict stays OK, isolating the selector path.
        self.write_transcript([(self.now - 30, "delivered block one")])
        rt = self.make_rt(swap_confirm_secs=300)
        rt.haystack = norm("delivered block one")
        # dcserver asserts a bind to a DIFFERENT transcript than F (s.jsonl).
        stale_bind = str(self.projects / "other-provider" / "stale-bind.jsonl")
        rt.watcher_probe = WatcherStateProbe(200, True, False, stale_bind)
        state: dict = {}

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(
            [a for a in rt.alerts if "셀렉터" in a[0]],
            [],
            "first tick has no growth proof; selector probe is skipped",
        )

        self._grow_selected_transcript()
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        self.assertEqual(
            [a for a in rt.alerts if "셀렉터" in a[0]],
            [],
            "a divergence within the swap-confirm window is not alarmed",
        )
        self.assertIn("selector_diverged_since", state["999"])

        self._grow_selected_transcript()
        tick_channel(rt, TICK_CHANNEL, state, self.now + 300)
        self.assertEqual(
            [a for a in rt.alerts if "셀렉터" in a[0]],
            [],
            "299 seconds of divergence is still below the boundary",
        )

        self._grow_selected_transcript()
        tick_channel(rt, TICK_CHANNEL, state, self.now + 301)
        selector_alerts = [a for a in rt.alerts if "셀렉터 동기화" in a[0]]
        self.assertEqual(len(selector_alerts), 1)
        body, trigger_turn = selector_alerts[0]
        self.assertIn(stale_bind, body)
        self.assertIn("/api/inflight/rebind", body)
        self.assertIn("sessions", body)
        self.assertTrue(
            trigger_turn,
            "actionable selector alerts must trigger an agent turn "
            "(send-to-agent handoff), not bot-direct delivery",
        )

    def test_selector_quiet_tool_tick_preserves_divergence_window(self):
        self.write_transcript([(self.now - 30, "delivered selector anchor")])
        rt = self.make_rt(swap_confirm_secs=300)
        rt.haystack = norm("delivered selector anchor")
        stale_bind = str(self.projects / "other-provider" / "stale-bind.jsonl")
        rt.watcher_probe = WatcherStateProbe(200, True, False, stale_bind)
        state: dict = {}

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self._grow_selected_transcript()
        first_divergence = self.now + 1
        tick_channel(rt, TICK_CHANNEL, state, first_divergence)
        self.assertEqual(
            state["999"].get("selector_diverged_since"), first_divergence
        )

        # A normal long tool phase has no new assistant text.  It pauses F
        # evidence but must not erase the already-proven stuck-bind window.
        tick_channel(rt, TICK_CHANNEL, state, self.now + 200)
        self.assertEqual(
            state["999"].get("selector_diverged_since"), first_divergence
        )
        self.assertTrue(
            any("retained divergence window" in line for line in rt.log_lines)
        )

        self._grow_selected_transcript()
        tick_channel(rt, TICK_CHANNEL, state, self.now + 301)
        self.assertEqual(
            len([body for body, _ in rt.alerts if "셀렉터 동기화" in body]),
            1,
        )

    def test_invariant_4435_runtime_mirror_never_starts_swap_timer_or_alerts(self):
        self.write_transcript([(self.now - 30, "delivered mirror case")])
        rt = self.make_rt(swap_confirm_secs=300)
        rt.haystack = norm("delivered mirror case")
        mirror = str(
            relay_watchdog.adk_root()
            / "runtime"
            / "sessions"
            / "agentdesk-999.jsonl"
        )
        rt.watcher_probe = WatcherStateProbe(200, True, False, mirror)
        state: dict = {}

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        state["999"]["selector_diverged_since"] = self.now - 1000

        self._grow_selected_transcript()
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        self.assertNotIn("selector_diverged_since", state["999"])

        self._grow_selected_transcript()
        tick_channel(rt, TICK_CHANNEL, state, self.now + 601)
        self.assertEqual([a for a in rt.alerts if "셀렉터" in a[0]], [])
        self.assertNotIn("selector_diverged_since", state["999"])
        self.assertTrue(
            any("runtime_session_mirror_uncomparable" in line for line in rt.log_lines)
        )

    def test_selector_bind_absent_is_fail_closed(self):
        self.write_transcript([(self.now - 30, "delivered block one")])
        rt = self.make_rt(swap_confirm_secs=1)
        rt.haystack = norm("delivered block one")
        # Old server: HTTP 200 but no bound_output_path field → probe carries None.
        rt.watcher_probe = WatcherStateProbe(200, True, False, None)
        state: dict = {}

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self._grow_selected_transcript()
        tick_channel(rt, TICK_CHANNEL, state, self.now + 100)

        self.assertEqual([a for a in rt.alerts if "셀렉터" in a[0]], [])
        self.assertNotIn("selector_diverged_since", state["999"])

    def test_coverage_404_alerts_only_after_two_consecutive_ticks(self):
        rt = self.make_rt()
        self.arm_coverage(rt, WatcherStateProbe(404))
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(rt.alerts, [])
        self.assertEqual(state["999"]["coverage_uncovered_ticks"], 1)

        tick_channel(rt, TICK_CHANNEL, state, self.now + rt.cfg.poll_secs)
        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("커버리지 불변식 위반", rt.alerts[0][0])
        self.assertIn("watcher_state_404", rt.alerts[0][0])
        self.assertIn("read-only", rt.alerts[0][0])
        self.assertTrue(
            rt.alerts[0][1],
            "actionable coverage alerts must trigger an agent turn "
            "(send-to-agent handoff), not bot-direct delivery",
        )

    def test_active_foreground_desync_does_not_advance_coverage_confirmation(self):
        rt = self.make_rt()
        self.arm_coverage(rt, self.active_foreground_probe())
        state = {
            "999": {
                "coverage_uncovered_ticks": 1,
                "coverage_desync_since": self.now - 60,
            }
        }

        tick_channel(rt, TICK_CHANNEL, state, self.now)

        self.assertEqual(rt.alerts, [])
        self.assertNotIn("coverage_uncovered_ticks", state["999"])
        self.assertNotIn("coverage_desync_since", state["999"])
        self.assertNotIn("last_coverage_alert", state["999"])

    def test_back_to_back_foreground_churn_never_accumulates_confirmation(self):
        rt = self.make_rt()
        state = {"999": {"coverage_uncovered_ticks": 1}}

        for tick_index in range(10):
            tick_at = self.now + tick_index * rt.cfg.poll_secs
            self.arm_coverage(
                rt,
                self.active_foreground_probe(
                    last_outbound_activity_ms=int(tick_at * 1000) - 1,
                    last_relay_ts_ms=int(tick_at * 1000) - 2,
                ),
            )
            tick_channel(rt, TICK_CHANNEL, state, tick_at)
            self.assertNotIn("coverage_uncovered_ticks", state["999"])

        self.assertEqual(rt.alerts, [])
        self.assertNotIn("last_coverage_alert", state["999"])

    def test_stale_foreground_desync_needs_delivery_gap_not_duration(self):
        rt = self.make_rt()
        stale_ms = int(self.now * 1000) - COVERAGE_ACTIVITY_FRESH_SECS * 1000
        self.arm_coverage(
            rt,
            self.active_foreground_probe(
                last_outbound_activity_ms=stale_ms,
                last_relay_ts_ms=None,
            ),
        )
        state = {
            "999": {
                "coverage_uncovered_ticks": 1,
                "coverage_desync_since": self.now - 600,
            }
        }

        tick_channel(rt, TICK_CHANNEL, state, self.now)

        self.assertEqual(rt.alerts, [])
        self.assertNotIn("last_coverage_alert", state["999"])
        self.assertTrue(any("uncorroborated" in line for line in rt.log_lines))

    def test_partial_foreground_schema_confirms_without_early_desync_alert(self):
        rt = self.make_rt()
        self.arm_coverage(
            rt,
            WatcherStateProbe(
                status=200,
                attached=True,
                desynced=True,
                relay_activity=CoverageActivityProbe(
                    relay_stall_state="active_foreground_stream",
                    active_turn="foreground",
                ),
            ),
        )
        state = {"999": {"coverage_uncovered_ticks": 1}}

        tick_channel(rt, TICK_CHANNEL, state, self.now)

        self.assertEqual(rt.alerts, [])
        self.assertNotIn("last_coverage_alert", state["999"])
        self.assertEqual(
            state["999"]["coverage_uncovered_ticks"], COVERAGE_CONFIRM_TICKS
        )

    def test_uncorroborated_idle_desync_never_alarms_on_duration_alone(self):
        self.write_transcript([(self.now - 30, "delivered block")])
        rt = self.make_rt(poll_secs=60)
        rt.haystack = norm("delivered block")
        tick_at = self.now + 600
        self.arm_coverage(
            rt,
            self.active_foreground_probe(
                queue_depth=1,
                last_outbound_activity_ms=None,
                last_relay_ts_ms=int(tick_at * 1000) - 1,
            ),
        )
        state = {
            "999": {
                "coverage_uncovered_ticks": COVERAGE_CONFIRM_TICKS - 1,
                "coverage_desync_since": self.now,
            }
        }

        tick_coverage(
            rt,
            TICK_CHANNEL,
            state["999"],
            tick_at,
            CoverageTranscriptProbe(growing=False, blocks=723, lost=0),
        )

        self.assertEqual(rt.alerts, [])
        self.assertNotIn("last_coverage_alert", state["999"])
        self.assertFalse(state["999"].get("gap_since"))
        self.assertFalse(state["999"].get("alerting"))
        self.assertTrue(
            any("zero loss and recent relay" in line for line in rt.log_lines)
        )

    def test_uncorroborated_desync_without_gap_stays_suppressed(self):
        rt = self.make_rt()
        tick_at = self.now + 600
        self.arm_coverage(
            rt,
            self.active_foreground_probe(
                queue_depth=1,
                last_outbound_activity_ms=None,
                last_relay_ts_ms=(
                    int(tick_at * 1000) - COVERAGE_ACTIVITY_FRESH_SECS * 1000
                ),
            ),
        )
        state = {
            "coverage_uncovered_ticks": COVERAGE_CONFIRM_TICKS - 1,
            "coverage_desync_since": self.now,
        }

        tick_coverage(
            rt,
            TICK_CHANNEL,
            state,
            tick_at,
            CoverageTranscriptProbe(growing=False, blocks=723, lost=0),
        )

        self.assertEqual(rt.alerts, [])
        self.assertNotIn("last_coverage_alert", state)
        self.assertTrue(any("uncorroborated" in line for line in rt.log_lines))

    def test_delivery_gap_corroborates_desync_coverage_alarm(self):
        rt = self.gap_rt()
        self.arm_coverage(rt, self.active_foreground_probe(queue_depth=1))
        state = {
            "999": {
                "coverage_uncovered_ticks": 1,
                "gap_since": self.now - rt.cfg.poll_secs,
            }
        }

        tick_channel(rt, TICK_CHANNEL, state, self.now)

        self.assertTrue(
            any("커버리지 불변식 위반" in body for body, _ in rt.alerts)
        )
        self.assertIn("last_coverage_alert", state["999"])

    def test_growth_lag_with_zero_loss_and_recent_relay_suppresses_alert(self):
        rt = self.make_rt()
        tick_at = self.now + 600
        probe = self.active_foreground_probe(
            queue_depth=1,
            last_outbound_activity_ms=None,
            last_relay_ts_ms=int(tick_at * 1000) - 1,
        )
        self.arm_coverage(rt, probe)
        state = {
            "coverage_uncovered_ticks": COVERAGE_CONFIRM_TICKS - 1,
            "coverage_desync_since": self.now,
        }

        tick_coverage(
            rt,
            TICK_CHANNEL,
            state,
            tick_at,
            CoverageTranscriptProbe(growing=True, blocks=723, lost=0),
        )

        self.assertEqual(rt.alerts, [])
        self.assertNotIn("last_coverage_alert", state)
        self.assertTrue(
            any("zero loss and recent relay" in line for line in rt.log_lines)
        )

    def test_growth_with_loss_still_alerts_desync_coverage(self):
        rt = self.make_rt()
        tick_at = self.now + 600
        probe = self.active_foreground_probe(
            queue_depth=1,
            last_outbound_activity_ms=None,
            last_relay_ts_ms=int(tick_at * 1000) - 1,
        )
        self.arm_coverage(rt, probe)
        state = {
            "coverage_uncovered_ticks": COVERAGE_CONFIRM_TICKS - 1,
            "coverage_desync_since": self.now,
            # #4961: transcript loss no longer corroborates a desync on its own.
            # Corroboration is delegated to `gap_since`, the delivery-gap
            # watermark that `evaluate()` already maintains monotonically, so
            # this fixture carries the sustained-loss evidence in that key.
            "gap_since": tick_at - rt.cfg.gap_alert_secs,
        }

        tick_coverage(
            rt,
            TICK_CHANNEL,
            state,
            tick_at,
            CoverageTranscriptProbe(growing=True, blocks=723, lost=1),
        )

        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("attached_but_desynced", rt.alerts[0][0])

    def test_real_desync_still_alarms_without_a_gap_corroborator(self):
        # #5155 coupling disclosure: `delivery_gap_active` reads `alerting` /
        # `gap_since`, so terminating a dead session's loss-state removes one of
        # the three corroborators for `attached_but_desynced`. The corroborator
        # it removes was a phantom — a five-day-dead session says nothing about
        # the live channel — and the independent one survives: a transcript that
        # is growing while nothing is being relayed and no inflight update is
        # advancing still raises COVERAGE ALERT with no gap state at all.
        rt = self.make_rt()
        tick_at = self.now + 600
        stale_ms = int(
            (tick_at - COVERAGE_ACTIVITY_FRESH_SECS * 3) * 1000
        )
        self.arm_coverage(
            rt,
            self.active_foreground_probe(
                queue_depth=1,
                last_outbound_activity_ms=stale_ms,
                last_relay_ts_ms=stale_ms,
            ),
        )
        state = {
            "coverage_uncovered_ticks": COVERAGE_CONFIRM_TICKS - 1,
            "coverage_desync_since": self.now,
        }
        self.assertNotIn("gap_since", state)
        self.assertNotIn("alerting", state)

        tick_coverage(
            rt,
            TICK_CHANNEL,
            state,
            tick_at,
            CoverageTranscriptProbe(growing=True, blocks=723, lost=0),
        )

        self.assertEqual(len(rt.alerts), 1, rt.log_lines)
        self.assertIn("attached_but_desynced", rt.alerts[0][0])
        self.assertTrue(state.get("coverage_alerting"))

    def _loss_corroboration_probe(self, tick_at):
        return self.active_foreground_probe(
            queue_depth=1,
            last_outbound_activity_ms=None,
            last_relay_ts_ms=int(tick_at * 1000) - 1,
        )

    def test_isolated_transcript_loss_without_gap_since_does_not_corroborate(self):
        # #4961 regression: two real false positives (ticks=83 gap=678s and
        # ticks=4 gap=799s, same channel) fired off a single lost>0 tick whose
        # neighbours both reported lost=0 — relay batching, not a dead relay.
        # A momentary `lost > 0` carries no duration evidence, so on its own it
        # must not corroborate the desync. Sustained loss is expressed by
        # `gap_since`, which is absent here.
        rt = self.make_rt()
        tick_at = self.now + 600
        self.arm_coverage(rt, self._loss_corroboration_probe(tick_at))
        state = {
            "coverage_uncovered_ticks": COVERAGE_CONFIRM_TICKS - 1,
            "coverage_desync_since": self.now,
        }

        tick_coverage(
            rt,
            TICK_CHANNEL,
            state,
            tick_at,
            CoverageTranscriptProbe(growing=True, blocks=723, lost=1),
        )

        self.assertEqual(rt.alerts, [])
        self.assertNotIn("last_coverage_alert", state)
        # tick_coverage observes only; it must not invent a corroboration key.
        self.assertNotIn("gap_since", state)
        self.assertTrue(
            any(
                "coverage desync uncorroborated" in line for line in rt.log_lines
            )
        )

    def test_transcript_loss_with_gap_since_corroborates_desync_coverage_alarm(self):
        # The real signal is preserved: once `evaluate()` has opened a delivery
        # gap (STATE_GAP -> `gap_since`), transcript loss corroborates the
        # desync and the alarm fires. `gap_since` is owned by the gap path, so
        # tick_coverage must read it without rewriting it.
        rt = self.make_rt()
        tick_at = self.now + 600
        self.arm_coverage(rt, self._loss_corroboration_probe(tick_at))
        gap_since = tick_at - rt.cfg.gap_alert_secs
        state = {
            "coverage_uncovered_ticks": COVERAGE_CONFIRM_TICKS - 1,
            "coverage_desync_since": self.now,
            "gap_since": gap_since,
        }

        tick_coverage(
            rt,
            TICK_CHANNEL,
            state,
            tick_at,
            CoverageTranscriptProbe(growing=True, blocks=723, lost=1),
        )

        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("attached_but_desynced", rt.alerts[0][0])
        self.assertEqual(state["gap_since"], gap_since)

    def test_growth_with_stale_relay_and_advanced_inflight_update_is_suppressed(self):
        rt = self.make_rt()
        tick_at = self.now + 600
        stale_relay_ms = (
            int(tick_at * 1000) - COVERAGE_ACTIVITY_FRESH_SECS * 1000
        )
        probe = self.active_foreground_probe(
            queue_depth=1,
            last_outbound_activity_ms=None,
            last_relay_ts_ms=stale_relay_ms,
            inflight_updated_at=tick_at - COVERAGE_ACTIVITY_FRESH_SECS - 1,
        )
        self.arm_coverage(rt, probe)
        state = {
            "coverage_uncovered_ticks": COVERAGE_CONFIRM_TICKS - 1,
            "coverage_desync_since": self.now,
            relay_watchdog.COVERAGE_INFLIGHT_UPDATED_AT_KEY: (
                tick_at - COVERAGE_ACTIVITY_FRESH_SECS - 2
            ),
        }

        tick_coverage(
            rt,
            TICK_CHANNEL,
            state,
            tick_at,
            CoverageTranscriptProbe(growing=True, blocks=723, lost=0),
        )

        self.assertEqual(rt.alerts, [])
        self.assertNotIn("last_coverage_alert", state)
        self.assertTrue(
            any(
                "live inflight update evidence=advanced" in line
                for line in rt.log_lines
            )
        )

    def test_growth_with_stale_relay_and_stalled_inflight_update_still_alerts(self):
        rt = self.make_rt()
        tick_at = self.now + 600
        stale_relay_ms = (
            int(tick_at * 1000) - COVERAGE_ACTIVITY_FRESH_SECS * 1000
        )
        stalled_inflight = tick_at - COVERAGE_ACTIVITY_FRESH_SECS - 1
        probe = self.active_foreground_probe(
            queue_depth=1,
            last_outbound_activity_ms=None,
            last_relay_ts_ms=stale_relay_ms,
            inflight_updated_at=stalled_inflight,
        )
        self.arm_coverage(rt, probe)
        state = {
            "coverage_uncovered_ticks": COVERAGE_CONFIRM_TICKS - 1,
            "coverage_desync_since": self.now,
            relay_watchdog.COVERAGE_INFLIGHT_UPDATED_AT_KEY: stalled_inflight,
        }

        tick_coverage(
            rt,
            TICK_CHANNEL,
            state,
            tick_at,
            CoverageTranscriptProbe(growing=True, blocks=723, lost=0),
        )

        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("attached_but_desynced", rt.alerts[0][0])

    def test_growth_with_stale_relay_and_absent_inflight_update_still_alerts(self):
        rt = self.make_rt()
        tick_at = self.now + 600
        stale_relay_ms = (
            int(tick_at * 1000) - COVERAGE_ACTIVITY_FRESH_SECS * 1000
        )
        probe = self.active_foreground_probe(
            queue_depth=1,
            last_outbound_activity_ms=None,
            last_relay_ts_ms=stale_relay_ms,
        )
        self.arm_coverage(rt, probe)
        state = {
            "coverage_uncovered_ticks": COVERAGE_CONFIRM_TICKS - 1,
            "coverage_desync_since": self.now,
        }

        tick_coverage(
            rt,
            TICK_CHANNEL,
            state,
            tick_at,
            CoverageTranscriptProbe(growing=True, blocks=723, lost=0),
        )

        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("attached_but_desynced", rt.alerts[0][0])

    def test_tick_channel_wires_growth_lag_evidence_into_coverage(self):
        self.write_transcript([(self.now - 30, "delivered initial")])
        rt = self.make_rt(poll_secs=60)
        rt.haystack = norm("delivered initial selector growth")
        tick_at = self.now + 600
        self.arm_coverage(
            rt,
            self.active_foreground_probe(
                queue_depth=1,
                last_outbound_activity_ms=None,
                last_relay_ts_ms=int(tick_at * 1000) - 1,
            ),
        )
        transcript = self.proj_dir / "s.jsonl"
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(transcript),
                "transcript_sizes": {str(transcript): transcript.stat().st_size},
                "coverage_uncovered_ticks": COVERAGE_CONFIRM_TICKS - 1,
                "coverage_desync_since": self.now,
            }
        }
        self._grow_selected_transcript()

        tick_channel(rt, TICK_CHANNEL, state, tick_at)

        self.assertEqual(rt.alerts, [])
        self.assertTrue(
            any("zero loss and recent relay" in line for line in rt.log_lines)
        )

    def test_active_foreground_evidence_cannot_suppress_detached_alert(self):
        rt = self.make_rt()
        probe = self.active_foreground_probe()
        self.arm_coverage(
            rt,
            WatcherStateProbe(
                status=200,
                attached=False,
                desynced=True,
                relay_activity=probe.relay_activity,
            ),
        )
        state = {"999": {"coverage_uncovered_ticks": 1}}

        tick_channel(rt, TICK_CHANNEL, state, self.now)

        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("커버리지 불변식 위반", rt.alerts[0][0])
        self.assertIn("detached", rt.alerts[0][0])

    def test_active_foreground_coverage_cannot_suppress_transcript_gap(self):
        rt = self.gap_rt()
        self.arm_coverage(rt, self.active_foreground_probe())
        state = {"999": {"coverage_uncovered_ticks": 1}}

        tick_channel(rt, TICK_CHANNEL, state, self.now)

        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("릴레이 갭 감지", rt.alerts[0][0])
        self.assertNotIn("last_coverage_alert", state["999"])
        self.assertIn("last_alert", state["999"])

    def test_coverage_alert_is_suppressed_during_deploy_window(self):
        rt = self.make_rt()
        self.arm_coverage(rt, WatcherStateProbe(404))
        rt.deploy_marker.touch()
        state = {"999": {"coverage_uncovered_ticks": 1}}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(rt.alerts, [])
        self.assertNotIn("last_coverage_alert", state["999"])
        self.assertTrue(any("deploy window" in l for l in rt.log_lines))

    def test_coverage_recovery_keeps_antiflap_cooldown(self):
        rt = self.make_rt()
        self.arm_coverage(rt, WatcherStateProbe(404))
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        first_alert_at = self.now + rt.cfg.poll_secs
        tick_channel(rt, TICK_CHANNEL, state, first_alert_at)
        self.assertEqual(len(rt.alerts), 1)

        rt.watcher_probe = WatcherStateProbe(200, True, False)
        tick_channel(rt, TICK_CHANNEL, state, first_alert_at + rt.cfg.poll_secs)
        self.assertEqual(state["999"]["last_coverage_alert"], first_alert_at)

        rt.watcher_probe = WatcherStateProbe(404)
        tick_channel(rt, TICK_CHANNEL, state, first_alert_at + 2 * rt.cfg.poll_secs)
        tick_channel(rt, TICK_CHANNEL, state, first_alert_at + 3 * rt.cfg.poll_secs)
        self.assertEqual(
            len(rt.alerts), 1, "flapping must not bypass the 900s cooldown"
        )

    def test_tmux_death_ends_expectation_without_claiming_recovery(self):
        rt = self.make_rt()
        state = {
            "999": {
                "coverage_alerting": True,
                "last_coverage_alert": self.now - 60,
                "coverage_uncovered_ticks": 2,
            }
        }
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertTrue(any("expectation ended" in l for l in rt.log_lines))
        self.assertFalse(any("coverage restored" in l for l in rt.log_lines))
        self.assertEqual(state["999"]["last_coverage_alert"], self.now - 60)

    def test_dcserver_unreachable_never_advances_coverage_alert(self):
        rt = self.make_rt()
        self.arm_coverage(rt, WatcherStateProbe(None))
        state = {"999": {"coverage_uncovered_ticks": 1}}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        tick_channel(rt, TICK_CHANNEL, state, self.now + rt.cfg.poll_secs)
        self.assertEqual(rt.alerts, [])
        self.assertNotIn("coverage_uncovered_ticks", state["999"])
        self.assertTrue(any("dcserver_unreachable" in l for l in rt.log_lines))

    def test_covered_tick_breaks_uncovered_continuity(self):
        rt = self.make_rt()
        self.arm_coverage(rt, WatcherStateProbe(404))
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        rt.watcher_probe = WatcherStateProbe(200, True, False)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        rt.watcher_probe = WatcherStateProbe(404)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        self.assertEqual(rt.alerts, [])
        self.assertEqual(state["999"]["coverage_uncovered_ticks"], 1)

    def test_coverage_alert_cannot_suppress_gap_alert_in_same_tick(self):
        rt = self.gap_rt()
        self.arm_coverage(rt, WatcherStateProbe(404))
        state = {"999": {"coverage_uncovered_ticks": 1}}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(len(rt.alerts), 2)
        bodies = [body for body, _ in rt.alerts]
        self.assertTrue(any("커버리지 불변식 위반" in body for body in bodies))
        self.assertTrue(any("릴레이 갭 감지" in body for body in bodies))
        self.assertIn("last_coverage_alert", state["999"])
        self.assertIn("last_alert", state["999"])

    def test_coverage_probe_exception_cannot_suppress_gap_alert(self):
        rt = self.gap_rt()
        with mock.patch.object(
            rt, "live_tmux_sessions", side_effect=RuntimeError("probe broke")
        ):
            tick_channel(rt, TICK_CHANNEL, {}, self.now)
        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("릴레이 갭 감지", rt.alerts[0][0])
        self.assertTrue(any("coverage tick error" in l for l in rt.log_lines))

    def test_corrupt_growth_state_fails_open_to_mtime_fallback(self):
        self.write_transcript([(self.now - 60, "fresh block")])
        rt = self.make_rt()
        rt.haystack = norm("fresh block")
        transcript = self.proj_dir / "s.jsonl"
        state = {
            "999": {
                "transcript_sizes": {str(transcript): "not-an-int"},
                SELECTED_TRANSCRIPT_KEY: True,
            }
        }
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(rt.alerts, [])
        self.assertIsInstance(state["999"]["transcript_sizes"][str(transcript)], int)
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(transcript))
        self.assertTrue(
            any("transcript-select reason=bootstrap" in line for line in rt.log_lines)
        )

    def test_invariant_4435_empty_discovery_tick_retains_previous_selection(self):
        previous = self.proj_dir / "temporarily-hidden.jsonl"
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(previous),
                "transcript_sizes": {str(previous): 100},
            }
        }
        rt = self.make_rt()

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(previous))
        self.assertEqual(
            state["999"]["transcript_sizes"], {str(previous): 100}
        )
        self.assertTrue(
            any("transcript-select reason=no_candidates" in line for line in rt.log_lines)
        )

        replacement = self.proj_dir / "replacement.jsonl"
        replacement.write_text("{}\n", encoding="utf-8")
        os.utime(replacement, (self.now + 1, self.now + 1))
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(replacement))
        self.assertTrue(
            any("transcript-select reason=bootstrap" in line for line in rt.log_lines)
        )

    def test_invariant_4435_all_stale_restart_keeps_delivered_anchor(self):
        anchor = float(int(self.now - 2000))
        self.write_transcript([(anchor, "old delivered anchor")])
        transcript = self.proj_dir / "s.jsonl"
        rt = self.make_rt()
        rt.haystack = norm("old delivered anchor")
        state: dict = {}

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(
            delivered_watermark_for_path(state["999"], transcript), anchor
        )

        state_path = self.root / "persisted-state.json"
        save_state(state_path, state)
        restarted_state = load_state(state_path)
        restarted = self.make_rt()
        restarted.haystack = ""
        tick_channel(restarted, TICK_CHANNEL, restarted_state, self.now + 1)

        self.assertEqual(restarted.alerts, [])
        self.assertEqual(
            delivered_watermark_for_path(restarted_state["999"], transcript), anchor
        )
        self.assertTrue(any("stale=1 lost=0" in line for line in restarted.log_lines))

    def test_invariant_4435_restart_ignores_old_transcript_mtime_only_touch(self):
        current = self.proj_dir / "current.jsonl"
        touched_old = self.proj_dir / "old.jsonl"
        anchor = float(int(self.now - 2000))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(
            record(anchor, "confirmed current delivery") + "\n", encoding="utf-8"
        )
        touched_old.write_text(
            record(anchor - 1000, "historic missing output") + "\n",
            encoding="utf-8",
        )
        os.utime(touched_old, (self.now - 100, self.now - 100))
        os.utime(current, (self.now, self.now))

        first = self.make_rt()
        first.haystack = norm("confirmed current delivery")
        state: dict = {}
        tick_channel(first, TICK_CHANNEL, state, self.now)
        self.assertEqual(first.alerts, [])
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(current))
        self.assertEqual(
            delivered_watermark_for_path(state["999"], current), anchor
        )

        state_path = self.root / "sticky-state.json"
        save_state(state_path, state)
        restarted_state = load_state(state_path)
        os.utime(touched_old, (self.now + 1, self.now + 1))

        restarted = self.make_rt()
        restarted.haystack = ""
        tick_channel(restarted, TICK_CHANNEL, restarted_state, self.now + 2)
        self.assertEqual(restarted.alerts, [])
        self.assertEqual(
            restarted_state["999"][SELECTED_TRANSCRIPT_KEY], str(current)
        )
        self.assertTrue(
            any("transcript-select reason=sticky" in line for line in restarted.log_lines)
        )

    def test_invariant_4435_newest_updated_watermark_bootstraps_selection(self):
        current = self.proj_dir / "current.jsonl"
        touched_old = self.proj_dir / "old.jsonl"
        anchor = float(int(self.now - 2000))
        current.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(anchor)
                    ),
                    "message": {
                        "content": [{"type": "text", "text": "confirmed current"}]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        touched_old.write_text(
            current.read_text(encoding="utf-8").replace("confirmed current", "old"),
            encoding="utf-8",
        )
        os.utime(current, (self.now - 100, self.now - 100))
        os.utime(touched_old, (self.now, self.now))
        state = {
            "999": {
                "transcript_sizes": {
                    str(current): current.stat().st_size,
                    str(touched_old): touched_old.stat().st_size,
                }
            }
        }
        advance_delivered_watermark(
            state["999"], touched_old, anchor - 100, self.now - 2
        )
        advance_delivered_watermark(state["999"], current, anchor, self.now - 1)

        rt = self.make_rt()
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        self.assertEqual(rt.alerts, [])
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(current))
        self.assertTrue(
            any(
                "transcript-select reason=watermark_bootstrap" in line
                for line in rt.log_lines
            )
        )

    def test_invariant_4435_watermark_only_restart_ignores_mtime_override(self):
        current = self.proj_dir / "current.jsonl"
        touched_old = self.proj_dir / "old.jsonl"
        anchor = float(int(self.now - 2000))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(
            record(anchor, "confirmed current") + "\n", encoding="utf-8"
        )
        touched_old.write_text(
            record(anchor - 100, "old delivered") + "\n", encoding="utf-8"
        )
        os.utime(current, (self.now - 100, self.now - 100))
        os.utime(touched_old, (self.now, self.now))
        state = {"999": {}}
        advance_delivered_watermark(
            state["999"], touched_old, anchor - 100, self.now - 2
        )
        advance_delivered_watermark(
            state["999"], current, anchor, self.now - 1
        )
        rt = self.make_rt()
        rt.haystack = norm("confirmed current old delivered")

        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)

        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(current))
        self.assertEqual(rt.alerts, [])
        self.assertTrue(
            any(
                "transcript-select reason=watermark_bootstrap" in line
                for line in rt.log_lines
            )
        )

    def test_invariant_4435_watermark_bootstrap_yields_to_positive_growth(self):
        current = self.proj_dir / "current.jsonl"
        growing = self.proj_dir / "growing.jsonl"
        anchor = float(int(self.now - 2000))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(
            record(anchor, "current delivered") + "\n", encoding="utf-8"
        )
        growing.write_text(
            record(anchor - 100, "growing delivered") + "\n", encoding="utf-8"
        )
        state = {
            "999": {
                "transcript_sizes": {
                    str(current): current.stat().st_size,
                    str(growing): growing.stat().st_size,
                }
            }
        }
        with growing.open("a", encoding="utf-8") as stream:
            stream.write(record(anchor + 1, "positive growth delivered") + "\n")
        advance_delivered_watermark(
            state["999"], growing, anchor - 100, self.now - 2
        )
        advance_delivered_watermark(
            state["999"], current, anchor, self.now - 1
        )
        rt = self.make_rt()
        rt.haystack = norm(
            "current delivered growing delivered positive growth delivered"
        )

        tick_channel(rt, TICK_CHANNEL, state, self.now)

        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(growing))
        self.assertTrue(
            any("transcript-select reason=growth" in line for line in rt.log_lines)
        )

    def test_invariant_4435_partial_discovery_rechecks_tracked_selection(self):
        current = self.proj_dir / "current.jsonl"
        touched_old = self.proj_dir / "old.jsonl"
        anchor = float(int(self.now - 2000))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(
            record(anchor, "confirmed current") + "\n", encoding="utf-8"
        )
        touched_old.write_text(
            record(anchor - 1000, "historic old") + "\n", encoding="utf-8"
        )
        os.utime(current, (self.now - 100, self.now - 100))
        os.utime(touched_old, (self.now, self.now))
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(current),
                "transcript_sizes": {
                    str(current): current.stat().st_size,
                    str(touched_old): touched_old.stat().st_size,
                },
            }
        }
        advance_delivered_watermark(state["999"], current, anchor, self.now - 1)
        rt = self.make_rt()
        rt.haystack = norm("confirmed current historic old")
        partial = TranscriptCandidate(
            touched_old, touched_old.stat().st_size, touched_old.stat().st_mtime
        )

        with mock.patch.object(
            relay_watchdog, "transcript_candidates", return_value=[partial]
        ):
            tick_channel(rt, TICK_CHANNEL, state, self.now)

        self.assertEqual(rt.alerts, [])
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(current))
        self.assertTrue(
            any("transcript-recheck recovered" in line for line in rt.log_lines)
        )

        current.unlink()
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(touched_old))
        self.assertTrue(
            any("transcript-select reason=bootstrap" in line for line in rt.log_lines)
        )

    def test_invariant_4435_partial_discovery_preserves_nonselected_baseline(self):
        current = self.proj_dir / "current.jsonl"
        historical = self.proj_dir / "historical.jsonl"
        anchor = float(int(self.now - 2000))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(
            record(anchor, "confirmed current") + "\n", encoding="utf-8"
        )
        historical.write_text(
            record(anchor - 1000, "historic missing") + "\n", encoding="utf-8"
        )
        os.utime(historical, (self.now - 100, self.now - 100))
        os.utime(current, (self.now, self.now))
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(current),
                "transcript_sizes": {
                    str(current): current.stat().st_size,
                    str(historical): historical.stat().st_size,
                },
            }
        }
        advance_delivered_watermark(state["999"], current, anchor, self.now - 1)
        rt = self.make_rt()
        rt.haystack = norm("confirmed current")
        current_only = TranscriptCandidate(
            current, current.stat().st_size, current.stat().st_mtime
        )

        with mock.patch.object(
            relay_watchdog, "transcript_candidates", return_value=[current_only]
        ):
            tick_channel(rt, TICK_CHANNEL, state, self.now)

        self.assertIn(str(historical), state["999"]["transcript_sizes"])
        self.assertEqual(
            state["999"]["transcript_sizes"][str(historical)],
            historical.stat().st_size,
        )

        os.utime(historical, (self.now + 1, self.now + 1))
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        self.assertEqual(rt.alerts, [])
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(current))
        self.assertTrue(
            any("transcript-select reason=sticky" in line for line in rt.log_lines)
        )

    def test_invariant_4435_transcript_history_is_bounded(self):
        current = self.proj_dir / "current.jsonl"
        current.write_text("{}\n", encoding="utf-8")
        fresh = {
            f"/missing/fresh-{index}.jsonl": index
            for index in range(relay_watchdog.MAX_TRANSCRIPT_HISTORY + 10)
        }
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(current),
                "transcript_sizes": {
                    str(current): current.stat().st_size,
                    **fresh,
                },
                "transcript_seen_at": {
                    str(current): self.now,
                    **{path: self.now - 1 for path in fresh},
                },
            }
        }

        tick_channel(self.make_rt(), TICK_CHANNEL, state, self.now)

        self.assertEqual(
            len(state["999"]["transcript_sizes"]),
            relay_watchdog.MAX_TRANSCRIPT_HISTORY,
        )
        self.assertIn(str(current), state["999"]["transcript_sizes"])
        self.assertTrue(set(fresh) & set(state["999"]["transcript_sizes"]))

    def test_invariant_4435_transcript_history_evicts_stale_missing(self):
        current = self.proj_dir / "current.jsonl"
        current.write_text("{}\n", encoding="utf-8")
        stale_at = self.now - relay_watchdog.TRANSCRIPT_HISTORY_TTL_SECS - 1
        stale = {
            f"/missing/history-{index}.jsonl": index for index in range(3)
        }
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(current),
                "transcript_sizes": {
                    str(current): current.stat().st_size,
                    **stale,
                },
                "transcript_seen_at": {
                    str(current): self.now,
                    **{path: stale_at for path in stale},
                },
                "pending_transcripts": [next(iter(stale))],
            }
        }

        tick_channel(self.make_rt(), TICK_CHANNEL, state, self.now)

        self.assertFalse(set(stale) & set(state["999"]["transcript_sizes"]))
        self.assertEqual(state["999"]["pending_transcripts"], [])

    def test_invariant_4435_pending_debut_queue_is_bounded_on_read_failure(self):
        current = self.proj_dir / "current.jsonl"
        current.write_text("{}\n", encoding="utf-8")
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(current),
                "transcript_sizes": {str(current): current.stat().st_size},
            }
        }
        for index in range(relay_watchdog.MAX_PENDING_TRANSCRIPTS + 5):
            (self.proj_dir / f"debut-{index:02d}.jsonl").write_text(
                json.dumps(
                    {
                        "type": "assistant",
                        "timestamp": time.strftime(
                            "%Y-%m-%dT%H:%M:%SZ", time.gmtime(self.now - 1)
                        ),
                        "message": {
                            "content": [
                                {"type": "text", "text": f"debut {index}"}
                            ]
                        },
                    }
                )
                + "\n",
                encoding="utf-8",
            )
        rt = self.make_rt()
        rt.haystack = None

        tick_channel(rt, TICK_CHANNEL, state, self.now)

        self.assertEqual(
            len(state["999"]["pending_transcripts"]),
            relay_watchdog.MAX_PENDING_TRANSCRIPTS,
        )

    def test_invariant_4435_debut_queue_evaluates_all_while_growth_stays_selected(
        self,
    ):
        current = self.proj_dir / "current.jsonl"
        missed = self.proj_dir / "missed-final.jsonl"
        delivered_new = self.proj_dir / "delivered-final.jsonl"
        current_ts = float(int(self.now - 1000))
        missed_ts = float(int(self.now - 2000))
        delivered_ts = float(int(self.now - 1500))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(
            record(current_ts, "current delivered") + "\n", encoding="utf-8"
        )
        os.utime(current, (self.now, self.now))
        rt = self.make_rt()
        rt.haystack = norm("current delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        with current.open("a", encoding="utf-8") as stream:
            stream.write(record(current_ts + 1, "current growth delivered") + "\n")
        missed.write_text(
            record(missed_ts, "missed final block") + "\n", encoding="utf-8"
        )
        delivered_new.write_text(
            record(delivered_ts, "new final delivered") + "\n", encoding="utf-8"
        )
        os.utime(missed, (self.now - 2, self.now - 2))
        os.utime(delivered_new, (self.now - 1, self.now - 1))
        os.utime(current, (self.now + 1, self.now + 1))
        rt.haystack = norm(
            "current delivered current growth delivered new final delivered"
        )

        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(current))
        self.assertTrue(
            any("transcript-select reason=growth" in line for line in rt.log_lines)
        )
        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("릴레이 갭 감지", rt.alerts[0][0])
        for path in (missed, delivered_new):
            self.assertTrue(
                any(
                    f"transcript-debut-eval path={path}" in line
                    for line in rt.log_lines
                )
            )
        self.assertEqual(
            delivered_watermark_for_path(state["999"], delivered_new), delivered_ts
        )
        self.assertEqual(delivered_watermark_for_path(state["999"], missed), 0.0)
        self.assertEqual(state["999"]["pending_transcripts"], [str(missed)])

    def test_invariant_4435_debut_survives_blind_tick_until_first_evaluation(self):
        current = self.proj_dir / "current.jsonl"
        missed = self.proj_dir / "missed-after-blind-tick.jsonl"
        current_ts = float(int(self.now - 1000))
        missed_ts = float(int(self.now - 2000))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(
            record(current_ts, "current delivered") + "\n", encoding="utf-8"
        )
        os.utime(current, (self.now, self.now))
        rt = self.make_rt()
        rt.haystack = norm("current delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        missed.write_text(
            record(missed_ts, "missed during blind tick") + "\n", encoding="utf-8"
        )
        os.utime(missed, (self.now - 1, self.now - 1))
        rt.haystack = None
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        self.assertIn(str(missed), state["999"]["pending_transcripts"])

        rt.haystack = norm("current delivered")
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(current))
        self.assertEqual(len(rt.alerts), 1)
        self.assertTrue(
            any(
                f"transcript-debut-eval path={missed}" in line
                for line in rt.log_lines
            )
        )

    def test_invariant_4435_fresh_debut_stays_pending_until_mature_verdict(self):
        current = self.proj_dir / "current.jsonl"
        final = self.proj_dir / "swap-final.jsonl"

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(
            record(self.now - 1000, "current delivered") + "\n",
            encoding="utf-8",
        )
        os.utime(current, (self.now, self.now))
        rt = self.make_rt()
        rt.haystack = norm("current delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        final.write_text(
            record(self.now + 60, "swap final never relayed") + "\n",
            encoding="utf-8",
        )
        with current.open("a", encoding="utf-8") as stream:
            stream.write(record(self.now + 61, "current growth delivered") + "\n")
        os.utime(final, (self.now + 60, self.now + 60))
        os.utime(current, (self.now + 61, self.now + 61))
        rt.haystack = norm("current delivered current growth delivered")

        tick_channel(rt, TICK_CHANNEL, state, self.now + 120)

        self.assertIn(str(final), state["999"]["pending_transcripts"])
        self.assertEqual(
            len([body for body, _ in rt.alerts if "릴레이 갭 감지" in body]), 0
        )
        self.assertTrue(
            any(
                f"transcript-debut-eval path={final} state=ok" in line
                and "fresh_undelivered=1" in line
                for line in rt.log_lines
            )
        )

        with current.open("a", encoding="utf-8") as stream:
            stream.write(record(self.now + 899, "current keepalive delivered") + "\n")
        os.utime(current, (self.now + 900, self.now + 900))
        rt.haystack = norm(
            "current delivered current growth delivered current keepalive delivered"
        )
        tick_channel(rt, TICK_CHANNEL, state, self.now + 900)

        self.assertEqual(
            len([body for body, _ in rt.alerts if "릴레이 갭 감지" in body]), 1
        )
        self.assertIn(str(final), state["999"]["pending_transcripts"])
        self.assertEqual(
            sum(
                1
                for line in rt.log_lines
                if f"transcript-debut-eval path={final}" in line
            ),
            2,
        )

    def test_invariant_4435_touched_known_history_evictee_cannot_redebut(self):
        current = self.proj_dir / "current.jsonl"
        historic = self.proj_dir / "historic-old.jsonl"
        day = 24 * 60 * 60

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        historic.write_text(
            "\n".join(
                record(self.now - 3 * day + index, f"historic block {index}")
                for index in range(490)
            )
            + "\n",
            encoding="utf-8",
        )
        os.utime(historic, (self.now - 3 * day, self.now - 3 * day))
        for index in range(relay_watchdog.MAX_TRANSCRIPT_HISTORY):
            recent = self.proj_dir / f"recent-{index:03d}.jsonl"
            recent.write_text("{}\n", encoding="utf-8")
            os.utime(
                recent,
                (self.now - 5000 - index, self.now - 5000 - index),
            )
        current.write_text(
            record(self.now - 1000, "current delivered") + "\n",
            encoding="utf-8",
        )
        os.utime(current, (self.now, self.now))
        rt = self.make_rt()
        rt.haystack = norm("current delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertNotIn(str(historic), state["999"]["transcript_sizes"])
        self.assertIn("transcript_known_at", state["999"])
        self.assertIn(str(historic), state["999"]["transcript_known_at"])

        os.utime(historic, (self.now + 100, self.now + 100))
        log_start = len(rt.log_lines)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 120)

        second_tick_logs = rt.log_lines[log_start:]
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(current))
        self.assertEqual(
            len([body for body, _ in rt.alerts if "릴레이 갭 감지" in body]), 0
        )
        self.assertNotIn(str(historic), state["999"]["pending_transcripts"])
        self.assertFalse(
            any(
                f"transcript-debut-eval path={historic}" in line
                for line in second_tick_logs
            )
        )
        self.assertTrue(
            any(
                f"transcript-debut-skip reason=known_stale_content path={historic}"
                in line
                for line in second_tick_logs
            )
        )

    def test_invariant_4435_history_cap_evictees_do_not_redebut_when_idle(self):
        current = self.proj_dir / "current.jsonl"
        current_ts = float(int(self.now - 1000))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(
            record(current_ts, "current delivered") + "\n", encoding="utf-8"
        )
        os.utime(current, (self.now, self.now))
        rt = self.make_rt()
        idle_at = self.now - rt.cfg.idle_quiet_secs - 60
        dead_paths: list[Path] = []
        for index in range(relay_watchdog.MAX_TRANSCRIPT_HISTORY + 8):
            dead = self.proj_dir / f"dead-{index:03d}.jsonl"
            dead.write_text(
                record(self.now - 3000 - index, f"historic missing {index}") + "\n",
                encoding="utf-8",
            )
            os.utime(dead, (idle_at - index, idle_at - index))
            dead_paths.append(dead)

        rt.haystack = norm("current delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(rt.alerts, [])
        retained = set(state["999"]["transcript_sizes"])
        evicted = [path for path in dead_paths if str(path) not in retained]
        self.assertGreater(len(evicted), 0)

        with current.open("a", encoding="utf-8") as stream:
            stream.write(record(current_ts + 1, "current growth delivered") + "\n")
        live_debut = self.proj_dir / "live-final.jsonl"
        live_debut.write_text(
            record(self.now - 2000, "live final missing") + "\n",
            encoding="utf-8",
        )
        os.utime(live_debut, (self.now + 1, self.now + 1))
        os.utime(current, (self.now + 2, self.now + 2))
        rt.haystack = norm("current delivered current growth delivered")
        log_start = len(rt.log_lines)

        tick_channel(rt, TICK_CHANNEL, state, self.now + 3)

        second_tick_logs = rt.log_lines[log_start:]
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(current))
        self.assertEqual(
            len([body for body, _ in rt.alerts if "릴레이 갭 감지" in body]), 1
        )
        self.assertTrue(
            any(
                f"transcript-debut-eval path={live_debut}" in line
                for line in second_tick_logs
            )
        )
        for path in evicted:
            self.assertFalse(
                any(
                    f"transcript-debut-eval path={path}" in line
                    for line in second_tick_logs
                ),
                f"idle history evictee regained debut authority: {path}",
            )

    def test_invariant_4435_malformed_pending_does_not_abort_healthy_selected(self):
        current = self.proj_dir / "current.jsonl"
        malformed = self.proj_dir / "malformed.jsonl"
        current_ts = float(int(self.now - 1000))
        grown_ts = current_ts + 1

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(
            record(current_ts, "current delivered") + "\n", encoding="utf-8"
        )
        os.utime(current, (self.now, self.now))
        rt = self.make_rt()
        rt.haystack = norm("current delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        with current.open("a", encoding="utf-8") as stream:
            stream.write(record(grown_ts, "healthy selected growth") + "\n")
        malformed.write_bytes(b"\xff\n")
        os.utime(malformed, (self.now + 1, self.now + 1))
        os.utime(current, (self.now + 2, self.now + 2))
        state["999"]["alerting"] = True
        state["999"]["gap_since"] = self.now - 60
        rt.haystack = norm("current delivered healthy selected growth")

        try:
            tick_channel(rt, TICK_CHANNEL, state, self.now + 3)
        except UnicodeError as exc:
            self.fail(f"malformed pending transcript aborted channel tick: {exc!r}")

        self.assertEqual(
            delivered_watermark_for_path(state["999"], current), grown_ts
        )
        self.assertIn(str(malformed), state["999"]["pending_transcripts"])
        self.assertEqual(rt.alerts, [])
        self.assertTrue(state["999"]["alerting"])
        self.assertTrue(
            any(
                f"transcript-read-error path={malformed} error=UnicodeDecodeError"
                in line
                for line in rt.log_lines
            )
        )
        self.assertFalse(
            any(
                f"transcript-debut-eval path={malformed}" in line
                for line in rt.log_lines
            )
        )

    def test_invariant_4435_pending_overflow_keeps_newest_and_alerts(self):
        current = self.proj_dir / "current.jsonl"
        current_ts = float(int(self.now - 1000))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(
            record(current_ts, "current delivered") + "\n", encoding="utf-8"
        )
        os.utime(current, (self.now, self.now))
        rt = self.make_rt()
        rt.haystack = norm("current delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        debuts: list[Path] = []
        delivered_texts: list[str] = []
        for index in range(relay_watchdog.MAX_PENDING_TRANSCRIPTS + 5):
            path = self.proj_dir / f"overflow-{index:02d}.jsonl"
            text = f"overflow debut {index}"
            path.write_text(
                record(self.now - 2000 - index, text) + "\n", encoding="utf-8"
            )
            os.utime(
                path,
                (self.now + 1 + index / 100, self.now + 1 + index / 100),
            )
            debuts.append(path)
            delivered_texts.append(text)
        newest = debuts[-1]
        oldest = debuts[0]
        with current.open("a", encoding="utf-8") as stream:
            stream.write(record(current_ts + 1, "current growth delivered") + "\n")
        os.utime(current, (self.now + 2, self.now + 2))
        rt.haystack = None

        tick_channel(rt, TICK_CHANNEL, state, self.now + 3)

        pending = state["999"]["pending_transcripts"]
        self.assertEqual(len(pending), relay_watchdog.MAX_PENDING_TRANSCRIPTS)
        self.assertIn(str(newest), pending)
        self.assertNotIn(str(oldest), pending)
        self.assertEqual(
            state["999"]["pending_transcript_overflow"]["dropped"], 5
        )
        self.assertTrue(
            any(
                "transcript-debut-overflow kept=32 dropped=5" in line
                for line in rt.log_lines
            )
        )
        self.assertEqual(
            len([body for body, _ in rt.alerts if "평가 큐 포화" in body]), 1
        )

        rt.haystack = norm(
            "current delivered current growth delivered "
            + " ".join(delivered_texts[:-1])
        )
        log_start = len(rt.log_lines)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 4)

        self.assertTrue(
            any(
                f"transcript-debut-eval path={newest}" in line
                for line in rt.log_lines[log_start:]
            )
        )
        self.assertEqual(
            len([body for body, _ in rt.alerts if "릴레이 갭 감지" in body]), 1
        )

    def test_invariant_4435_truncated_debut_stays_pending_until_completed(self):
        current = self.proj_dir / "current.jsonl"
        final = self.proj_dir / "torn-final.jsonl"

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        anchor = float(int(self.now - 1000))
        current.write_text(record(anchor, "current delivered") + "\n", encoding="utf-8")
        os.utime(current, (self.now, self.now))
        rt = self.make_rt()
        rt.haystack = norm("current delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        final_record = record(self.now - 2000, "completed final was never relayed")
        final.write_text(final_record[:-9], encoding="utf-8")
        with current.open("a", encoding="utf-8") as stream:
            stream.write(record(anchor + 1, "current growth delivered") + "\n")
        os.utime(final, (self.now + 1, self.now + 1))
        os.utime(current, (self.now + 2, self.now + 2))
        rt.haystack = norm("current delivered current growth delivered")

        tick_channel(rt, TICK_CHANNEL, state, self.now + 3)

        self.assertIn(str(final), state["999"]["pending_transcripts"])
        self.assertNotIn(
            str(final),
            state["999"]["transcript_sizes"],
            "an incomplete debut must not commit a safe growth baseline",
        )
        self.assertTrue(
            any(
                f"transcript-read-incomplete path={final}" in line
                for line in rt.log_lines
            )
        )

        with mock.patch.object(
            relay_watchdog, "transcript_candidates", return_value=[]
        ):
            tick_channel(rt, TICK_CHANNEL, state, self.now + 4)
        self.assertIn(
            str(final),
            state["999"]["pending_transcripts"],
            "a torn debut must retain authority across a discovery miss",
        )
        self.assertNotIn(str(final), state["999"]["transcript_sizes"])

        final.write_text(final_record + "\n", encoding="utf-8")
        with current.open("a", encoding="utf-8") as stream:
            stream.write(record(anchor + 2, "current second growth delivered") + "\n")
        os.utime(final, (self.now + 4, self.now + 4))
        os.utime(current, (self.now + 5, self.now + 5))
        rt.haystack = norm(
            "current delivered current growth delivered current second growth delivered"
        )

        tick_channel(rt, TICK_CHANNEL, state, self.now + 6)

        self.assertEqual(
            len([body for body, _ in rt.alerts if "릴레이 갭 감지" in body]), 1
        )
        self.assertTrue(
            any(
                f"transcript-debut-eval path={final} state=gap" in line
                for line in rt.log_lines
            )
        )

    def test_invariant_4435_torn_debut_miss_reentry_still_escalates(self):
        current = self.proj_dir / "current.jsonl"
        torn = self.proj_dir / "torn-unreadable.jsonl"
        anchor = float(int(self.now - 30))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(
            record(anchor, "current delivered") + "\n", encoding="utf-8"
        )
        rt = self.make_rt(read_fail_alert_after=3)
        rt.haystack = norm("current delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        torn.write_text('{"type":"assistant"', encoding="utf-8")
        os.utime(torn, (self.now + 1, self.now + 1))
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        self.assertEqual(
            state["999"]["pending_transcript_failures"][str(torn)], 1
        )
        state["999"][relay_watchdog.ORPHAN_STRANDED_SINCE_KEY] = {
            str(torn): self.now + 2
        }

        with mock.patch.object(
            relay_watchdog, "transcript_candidates", return_value=[]
        ):
            tick_channel(rt, TICK_CHANNEL, state, self.now + 3)
        self.assertIn(str(torn), state["999"]["pending_transcripts"])
        self.assertEqual(
            state["999"]["pending_transcript_failures"][str(torn)], 1
        )

        tick_channel(rt, TICK_CHANNEL, state, self.now + 4)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 5)

        self.assertEqual(
            len([body for body, _ in rt.alerts if "평가 불능 에스컬레이션" in body]),
            1,
        )
        self.assertIn(str(torn), state["999"]["retired_transcripts"])
        self.assertNotIn(str(torn), state["999"]["pending_transcripts"])
        self.assertNotIn(
            str(torn),
            state["999"].get(relay_watchdog.ORPHAN_STRANDED_SINCE_KEY, {}),
        )

    def test_invariant_4435_corrupt_pending_escalates_once_then_unwedges(self):
        current = self.proj_dir / "current.jsonl"
        malformed = self.proj_dir / "permanently-malformed.jsonl"
        anchor = float(int(self.now - 1000))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(record(anchor, "current delivered") + "\n", encoding="utf-8")
        os.utime(current, (self.now, self.now))
        rt = self.make_rt(read_fail_alert_after=3)
        rt.haystack = norm("current delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        malformed.write_bytes(b"\xff\n")
        os.utime(malformed, (self.now + 1, self.now + 1))
        state["999"]["alerting"] = True
        state["999"]["gap_since"] = self.now - 60
        state["999"][relay_watchdog.GAP_TRANSCRIPT_KEY] = str(malformed)

        for offset in range(2, 5):
            tick_channel(rt, TICK_CHANNEL, state, self.now + offset)

        escalation_alerts = [
            body for body, _ in rt.alerts if "평가 불능 에스컬레이션" in body
        ]
        self.assertEqual(len(escalation_alerts), 1)
        self.assertNotIn(str(malformed), state["999"]["pending_transcripts"])
        self.assertFalse(state["999"].get("alerting", False))
        self.assertFalse(any("릴레이 갭 해소" in body for body, _ in rt.alerts))

        for offset in range(5, 9):
            with malformed.open("ab") as stream:
                stream.write(b"\xff")
            os.utime(malformed, (self.now + offset, self.now + offset))
            tick_channel(rt, TICK_CHANNEL, state, self.now + offset)
        self.assertEqual(
            len([body for body, _ in rt.alerts if "평가 불능 에스컬레이션" in body]),
            1,
        )
        self.assertIn(
            str(malformed), state["999"]["retired_transcripts"]
        )
        self.assertFalse(any("릴레이 갭 해소" in body for body, _ in rt.alerts))

    def test_invariant_4435_selected_read_failure_escalates_and_quarantines(self):
        selected = self.proj_dir / "selected.jsonl"
        anchor = float(int(self.now - 30))
        selected.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(anchor)
                    ),
                    "message": {
                        "content": [{"type": "text", "text": "selected delivered"}]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        rt = self.make_rt(read_fail_alert_after=3)
        rt.haystack = norm("selected delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        with selected.open("ab") as stream:
            stream.write(b"\xff\n")
        for offset in range(1, 4):
            os.utime(selected, (self.now + offset, self.now + offset))
            tick_channel(rt, TICK_CHANNEL, state, self.now + offset)

        self.assertEqual(
            len([body for body, _ in rt.alerts if "평가 불능 에스컬레이션" in body]),
            1,
        )
        self.assertIn(str(selected), state["999"]["retired_transcripts"])
        self.assertNotEqual(
            state["999"].get(SELECTED_TRANSCRIPT_KEY), str(selected)
        )

    def test_invariant_4435_full_pending_cap_keeps_selected_failure_authority(self):
        selected = self.proj_dir / "selected-cap-anchor.jsonl"
        anchor = float(int(self.now - 30))
        selected.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(anchor)
                    ),
                    "message": {
                        "content": [{"type": "text", "text": "cap anchor landed"}]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        rt = self.make_rt(read_fail_alert_after=4)
        rt.haystack = norm("cap anchor landed")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(selected))

        pending: list[str] = []
        for index in range(relay_watchdog.MAX_PENDING_TRANSCRIPTS):
            path = self.proj_dir / f"pending-corrupt-{index:02d}.jsonl"
            path.write_bytes(b"\xff\n")
            os.utime(path, (self.now + 1, self.now + 1))
            pending.append(str(path))
        with selected.open("ab") as stream:
            stream.write(b"\xff\n")
        os.utime(selected, (self.now + 1, self.now + 1))
        state["999"]["pending_transcripts"] = pending
        state["999"]["pending_transcript_since"] = {
            path: self.now + 1 for path in pending
        }

        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        chs = state["999"]
        self.assertEqual(
            len(chs["pending_transcripts"]),
            relay_watchdog.MAX_PENDING_TRANSCRIPTS,
        )
        self.assertNotIn(
            str(selected),
            chs["pending_transcripts"],
            "selected owns a separate failure slot, not pending capacity",
        )
        self.assertEqual(
            chs.get("pending_transcript_failures", {}).get(str(selected)), 1
        )

        tick_channel(rt, TICK_CHANNEL, state, self.now + 3)
        self.assertEqual(
            state["999"].get("pending_transcript_failures", {}).get(str(selected)),
            2,
            "a full cap must not reset the established selection each tick",
        )

    def test_invariant_4435_legacy_without_known_at_tracks_torn_debut(self):
        selected = self.proj_dir / "legacy-selected.jsonl"
        torn = self.proj_dir / "legacy-new-torn.jsonl"
        anchor = float(int(self.now - 30))
        selected.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(anchor)
                    ),
                    "message": {
                        "content": [{"type": "text", "text": "legacy landed"}]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        torn.write_text('{"type":"assistant"', encoding="utf-8")
        os.utime(selected, (self.now - 1, self.now - 1))
        os.utime(torn, (self.now, self.now))
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(selected),
                "transcript_sizes": {str(selected): selected.stat().st_size},
            }
        }
        rt = self.make_rt(read_fail_alert_after=3)
        rt.haystack = norm("legacy landed")

        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        chs = state["999"]
        self.assertIn(str(torn), chs["pending_transcripts"])
        self.assertEqual(chs["pending_transcript_failures"][str(torn)], 1)
        self.assertIn("transcript_known_at", chs)
        self.assertFalse(
            any("unproven_stale_content" in line for line in rt.log_lines)
        )

        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        self.assertEqual(
            state["999"]["pending_transcript_failures"][str(torn)], 2
        )
        tick_channel(rt, TICK_CHANNEL, state, self.now + 3)
        self.assertIn(str(torn), state["999"]["retired_transcripts"])
        self.assertEqual(
            len([body for body, _ in rt.alerts if "평가 불능" in body]), 1
        )

    def test_invariant_4435_zero_assistant_debut_keeps_pending_authority(self):
        current = self.proj_dir / "zero-block-current.jsonl"
        debut = self.proj_dir / "zero-block-debut.jsonl"

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(
            record(self.now - 30, "zero block anchor landed") + "\n",
            encoding="utf-8",
        )
        rt = self.make_rt()
        rt.haystack = norm("zero block anchor landed")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        debut.write_text(
            json.dumps(
                {
                    "type": "user",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(self.now + 1)
                    ),
                    "message": {"content": "prompt only"},
                }
            )
            + "\n",
            encoding="utf-8",
        )
        os.utime(debut, (self.now + 1, self.now + 1))
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        self.assertIn(
            str(debut),
            state["999"]["pending_transcripts"],
            "vacuous OK without one assistant block cannot consume debut authority",
        )

        with debut.open("a", encoding="utf-8") as stream:
            stream.write(record(self.now - 2000, "later semantic block missing") + "\n")
        os.utime(debut, (self.now + 3, self.now + 3))
        tick_channel(rt, TICK_CHANNEL, state, self.now + 4)
        self.assertEqual(
            len([body for body, _ in rt.alerts if "릴레이 갭 감지" in body]), 1
        )

    def test_invariant_4435_stale_pending_gap_expires_without_realert_loop(self):
        current = self.proj_dir / "current.jsonl"
        missed = self.proj_dir / "missed-final.jsonl"
        anchor = float(int(self.now - 1000))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(record(anchor, "current delivered") + "\n", encoding="utf-8")
        os.utime(current, (self.now, self.now))
        rt = self.make_rt()
        rt.haystack = norm("current delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        missed.write_text(
            record(self.now - 2000, "dead session final missing") + "\n",
            encoding="utf-8",
        )
        os.utime(missed, (self.now + 1, self.now + 1))
        rt.haystack = norm("current delivered")
        tick_channel(rt, TICK_CHANNEL, state, self.now + 3)
        self.assertEqual(
            len([body for body, _ in rt.alerts if "릴레이 갭 감지" in body]), 1
        )
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(missed))

        expiry = self.now + 3 + rt.cfg.idle_quiet_secs + 1
        # Metadata-only activity is not content authority and must not extend
        # the pending lifetime indefinitely.
        os.utime(missed, (expiry, expiry))
        tick_channel(rt, TICK_CHANNEL, state, expiry)
        tick_channel(rt, TICK_CHANNEL, state, expiry + rt.cfg.realert_secs + 1)

        self.assertNotIn(str(missed), state["999"]["pending_transcripts"])
        self.assertEqual(
            len([body for body, _ in rt.alerts if "릴레이 갭 감지" in body]), 1
        )
        self.assertEqual(
            len([body for body, _ in rt.alerts if "평가 권한 만료" in body]), 1
        )
        self.assertFalse(any("릴레이 갭 해소" in body for body, _ in rt.alerts))

    def test_invariant_4435_retirement_alerts_share_realert_cooldown(self):
        current = self.proj_dir / "retirement-current.jsonl"
        current.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(self.now - 30)
                    ),
                    "message": {
                        "content": [
                            {"type": "text", "text": "retirement anchor landed"}
                        ]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        rt = self.make_rt()
        rt.haystack = norm("retirement anchor landed")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        chs = state["999"]

        def arm_expired(name: str, tick_at: float) -> None:
            path = self.proj_dir / name
            path.write_text("{}\n", encoding="utf-8")
            os.utime(path, (tick_at, tick_at))
            chs["transcript_sizes"][str(path)] = path.stat().st_size
            chs["transcript_seen_at"][str(path)] = tick_at
            chs["transcript_known_at"][str(path)] = tick_at
            chs["pending_transcripts"] = [str(path)]
            chs["pending_transcript_since"] = {
                str(path): tick_at - rt.cfg.idle_quiet_secs - 1
            }

        first_tick = self.now + 1
        arm_expired("retire-one.jsonl", first_tick)
        tick_channel(rt, TICK_CHANNEL, state, first_tick)
        self.assertEqual(
            len([body for body, _ in rt.alerts if "평가 권한 만료" in body]), 1
        )

        arm_expired("retire-two.jsonl", first_tick + 1)
        tick_channel(rt, TICK_CHANNEL, state, first_tick + 1)
        self.assertEqual(
            len([body for body, _ in rt.alerts if "평가 권한 만료" in body]),
            1,
            "a new crash-loop transcript inside cooldown must not spam",
        )
        self.assertTrue(
            any("retirement-alert suppressed" in line for line in rt.log_lines)
        )

        boundary = first_tick + rt.cfg.realert_secs
        arm_expired("retire-three.jsonl", boundary)
        tick_channel(rt, TICK_CHANNEL, state, boundary)
        self.assertEqual(
            len([body for body, _ in rt.alerts if "평가 권한 만료" in body]), 2
        )

    def test_invariant_4435_unrelated_pending_retirement_preserves_live_gap(self):
        rt = self.gap_rt()
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        chs = state["999"]
        selected = chs[SELECTED_TRANSCRIPT_KEY]
        self.assertTrue(chs.get("alerting"))
        self.assertEqual(chs[relay_watchdog.GAP_TRANSCRIPT_KEY], selected)
        chs["issue_url"] = "https://example.test/issues/existing"
        original_gap_since = chs["gap_since"]

        unrelated = self.proj_dir / "unrelated-pending.jsonl"
        unrelated.write_text("{}\n", encoding="utf-8")
        os.utime(unrelated, (self.now, self.now))
        chs["transcript_sizes"][str(unrelated)] = unrelated.stat().st_size
        chs["transcript_seen_at"][str(unrelated)] = self.now
        chs["pending_transcripts"] = [str(unrelated)]
        chs["pending_transcript_since"] = {
            str(unrelated): self.now - rt.cfg.idle_quiet_secs - 1
        }

        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)

        self.assertTrue(chs.get("alerting"))
        self.assertEqual(chs["gap_since"], original_gap_since)
        self.assertEqual(
            chs["issue_url"], "https://example.test/issues/existing"
        )
        self.assertEqual(chs[relay_watchdog.GAP_TRANSCRIPT_KEY], selected)
        self.assertFalse(any("릴레이 갭 해소" in body for body, _ in rt.alerts))
        self.assertTrue(
            any(
                "unrelated transcript retirement preserved live gap authority"
                in line
                for line in rt.log_lines
            )
        )

    def test_invariant_4435_young_debut_cannot_false_recover_prior_gap_owner(self):
        owner = self.proj_dir / "gap-owner-a.jsonl"
        debut = self.proj_dir / "young-debut-b.jsonl"

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        owner.write_text(
            record(self.now - 2000, "owner A remains missing") + "\n",
            encoding="utf-8",
        )
        rt = self.make_rt()
        rt.haystack = ""
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        chs = state["999"]
        self.assertTrue(chs.get("alerting"))
        self.assertIn(
            str(owner), chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY]
        )
        original_gap_since = chs["gap_since"]
        chs["issue_url"] = "https://example.test/issues/owner-a"

        debut.write_text(
            record(self.now + 1, "young B not delivered yet") + "\n",
            encoding="utf-8",
        )
        os.utime(debut, (self.now + 1, self.now + 1))
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        self.assertTrue(chs.get("alerting"))
        self.assertEqual(chs["gap_since"], original_gap_since)
        self.assertEqual(
            chs["issue_url"], "https://example.test/issues/owner-a"
        )
        self.assertEqual(chs[relay_watchdog.GAP_TRANSCRIPT_KEY], str(owner))
        self.assertIn(
            str(owner), chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY]
        )
        self.assertFalse(any("릴레이 갭 해소" in body for body, _ in rt.alerts))

    def test_4715_delivered_successor_retires_idle_historical_gap_owner(self):
        owner = self.proj_dir / "cleared-session-gap.jsonl"
        successor = (
            self.proj_dir / "ea50ec55-d965-4ae4-86f1-05a4209f2b7c.jsonl"
        )

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        owner.write_text(
            record(self.now - 2000, "historical block never delivered") + "\n",
            encoding="utf-8",
        )
        os.utime(owner, (self.now - 2000, self.now - 2000))
        rt = self.make_rt()
        rt.haystack = ""
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        chs = state["999"]
        self.assertTrue(chs.get("alerting"))
        self.assertIn(str(owner), chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY])
        chs["issue_url"] = "https://example.test/issues/4715"
        chs[relay_watchdog.ORPHAN_STRANDED_SINCE_KEY] = {
            str(owner): self.now
        }

        successor_at = self.now + rt.cfg.idle_quiet_secs + 1
        successor.write_text(
            record(successor_at, "current session delivery proof") + "\n",
            encoding="utf-8",
        )
        os.utime(successor, (successor_at, successor_at))
        rt.haystack = norm("current session delivery proof")
        rt.watcher_probe = WatcherStateProbe(
            200,
            attached=True,
            desynced=False,
            bound_output_path="/runtime/sessions/channel-mirror.jsonl",
            bound_session_id=successor.stem,
        )
        tick_channel(rt, TICK_CHANNEL, state, successor_at + 1)

        self.assertEqual(chs[SELECTED_TRANSCRIPT_KEY], str(successor))
        self.assertFalse(chs.get("alerting"), (chs, rt.log_lines))
        self.assertNotIn("gap_since", chs)
        self.assertNotIn(relay_watchdog.GAP_TRANSCRIPT_KEY, chs)
        self.assertNotIn(relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY, chs)
        self.assertIn(str(owner), chs[relay_watchdog.RETIRED_TRANSCRIPTS_KEY])
        self.assertNotIn(
            str(owner),
            chs.get(relay_watchdog.ORPHAN_STRANDED_SINCE_KEY, {}),
        )
        self.assertFalse(any("릴레이 갭 해소" in body for body, _ in rt.alerts))
        self.assertTrue(
            any("historical-gap-owner-retired" in line for line in rt.log_lines)
        )

        tick_channel(rt, TICK_CHANNEL, state, self.now + 3)
        self.assertNotIn(
            str(owner), chs.get(relay_watchdog.PENDING_TRANSCRIPTS_KEY, [])
        )
        self.assertNotIn(
            str(owner), chs.get(relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY, [])
        )
        self.assertFalse(chs.get("alerting"), (chs, rt.log_lines))

    def test_4715_malformed_matching_session_id_fails_closed(self):
        owner = self.proj_dir / "malformed-owner-gap.jsonl"
        successor = self.proj_dir / "not-a-session-uuid.jsonl"

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        owner.write_text(
            record(self.now - 2000, "malformed binding owner remains missing") + "\n",
            encoding="utf-8",
        )
        os.utime(owner, (self.now - 2000, self.now - 2000))
        rt = self.make_rt()
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        chs = state["999"]

        successor_at = self.now + rt.cfg.idle_quiet_secs + 1
        successor.write_text(
            record(successor_at, "malformed successor delivered") + "\n",
            encoding="utf-8",
        )
        os.utime(successor, (successor_at, successor_at))
        rt.haystack = norm("malformed successor delivered")
        rt.watcher_probe = WatcherStateProbe(
            200,
            attached=True,
            desynced=False,
            bound_output_path="/runtime/sessions/channel-mirror.jsonl",
            bound_session_id=successor.stem,
        )
        tick_channel(rt, TICK_CHANNEL, state, successor_at + 1)

        self.assertTrue(chs.get("alerting"), (chs, rt.log_lines))
        self.assertIn(str(owner), chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY])
        self.assertNotIn(str(owner), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {}))

    def test_4715_successor_without_runtime_binding_keeps_historical_gap_owner(self):
        owner = self.proj_dir / "parallel-owner-gap.jsonl"
        successor = self.proj_dir / "newer-delivered-unbound.jsonl"

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        owner.write_text(
            record(self.now - 2000, "parallel owner remains missing") + "\n",
            encoding="utf-8",
        )
        os.utime(owner, (self.now - 2000, self.now - 2000))
        rt = self.make_rt()
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        chs = state["999"]

        successor_at = self.now + rt.cfg.idle_quiet_secs + 1
        successor.write_text(
            record(successor_at, "unbound successor delivered") + "\n",
            encoding="utf-8",
        )
        os.utime(successor, (successor_at, successor_at))
        rt.haystack = norm("unbound successor delivered")
        rt.watcher_probe = WatcherStateProbe(
            200, attached=True, desynced=False, bound_output_path=str(owner)
        )
        tick_channel(rt, TICK_CHANNEL, state, successor_at + 1)

        self.assertTrue(chs.get("alerting"), (chs, rt.log_lines))
        self.assertIn(str(owner), chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY])
        self.assertNotIn(str(owner), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {}))

    def test_4715_active_bound_predecessor_is_not_retired_by_mtime_order(self):
        owner = self.proj_dir / "active-bound-owner.jsonl"
        successor = self.proj_dir / "newer-delivered.jsonl"

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        owner.write_text(
            record(self.now - 2000, "active owner remains missing") + "\n",
            encoding="utf-8",
        )
        rt = self.make_rt()
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        chs = state["999"]

        successor.write_text(
            record(self.now + 1, "newer delivery does not prove predecessor ended") + "\n",
            encoding="utf-8",
        )
        os.utime(successor, (self.now + 1, self.now + 1))
        rt.haystack = norm("newer delivery does not prove predecessor ended")
        rt.watcher_probe = WatcherStateProbe(
            200, attached=True, desynced=False, bound_output_path=str(successor)
        )
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        self.assertTrue(chs.get("alerting"), (chs, rt.log_lines))
        self.assertIn(str(owner), chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY])
        self.assertNotIn(str(owner), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {}))

    # ── #5190: an orphan transcript must not pin the channel in GAP ──────────
    #
    # A `/clear`ed session leaves a frozen transcript holding blocks that no
    # future delivery can recover: its delivery frontier cannot advance (so the
    # tombstone path, which needs two advances, never confirms), its worktree is
    # still alive (so dead-worktree retirement never fires), and STATE_OK can
    # never arrive (so the pending authority never releases). Live incident:
    # three identical alerts 16 minutes apart, each claiming "999 minutes since
    # the last delivery" about a session that had been frozen for half an hour
    # while the channel kept delivering normally on a different transcript.
    #
    # The counter-pressure runs through every test below: the orphan handling
    # buys its silence with positive proof of delivery elsewhere, so a relay
    # that is actually down never gets quieter.

    ORPHAN_STRANDED_KEY = relay_watchdog.ORPHAN_STRANDED_SINCE_KEY

    @staticmethod
    def _5190_record(epoch: float, text: str) -> str:
        return json.dumps(
            {
                "type": "assistant",
                "timestamp": time.strftime(
                    "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                ),
                "message": {"content": [{"type": "text", "text": text}]},
            }
        )

    def _5190_live_channel(self) -> tuple[FakeRuntime, dict, Path, Path]:
        """Tick 1: only the live session exists, and it is delivering."""
        live = self.proj_dir / "4648aa76-0239-47a7-9a39-8487c7fd216a.jsonl"
        orphan = self.proj_dir / "67f48e65-74dd-4751-92c2-405145f1370b.jsonl"
        live.write_text(
            self._5190_record(self.now - 60, "live session first block") + "\n",
            encoding="utf-8",
        )
        os.utime(live, (self.now, self.now))
        rt = self.make_rt()
        rt.haystack = norm("live session first block")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        return rt, state, live, orphan

    def _5190_strand_orphan(self, orphan: Path, frozen_at: float) -> None:
        """The blocks #5188 stranded, and then the session goes silent forever.

        The watchdog only meets this file after it froze, so whatever it once
        delivered has long rolled out of the bounded Discord window: from here
        on it has never been seen delivering anything.
        """
        orphan.write_text(
            self._5190_record(frozen_at - 600, "stranded orphan block one")
            + "\n"
            + self._5190_record(frozen_at - 500, "stranded orphan block two")
            + "\n",
            encoding="utf-8",
        )
        os.utime(orphan, (frozen_at, frozen_at))

    def _5190_live_delivery(
        self, rt: FakeRuntime, live: Path, at: float, text: str
    ) -> None:
        with live.open("a", encoding="utf-8") as stream:
            stream.write(self._5190_record(at - 30, text) + "\n")
        os.utime(live, (at, at))
        rt.haystack = norm(f"{rt.haystack} {text}")

    def _5190_retire_orphan(
        self, rt: FakeRuntime, state: dict, live: Path, orphan: Path
    ) -> float:
        """Run the channel forward until the stranded orphan is closed out."""
        discovered_at = self.now + rt.cfg.orphan_abandon_secs + 600
        self._5190_strand_orphan(orphan, self.now)
        self._5190_live_delivery(rt, live, discovered_at, "live block two")
        tick_channel(rt, TICK_CHANNEL, state, discovered_at)
        retire_at = (
            discovered_at + relay_watchdog.ORPHAN_STRANDED_CONFIRM_SECS
        )
        self._5190_live_delivery(rt, live, retire_at, "live block three")
        tick_channel(rt, TICK_CHANNEL, state, retire_at)
        return retire_at

    def test_5206_release_prunes_stranded_marker_with_pending_authority(self):
        released = "/tmp/released.jsonl"
        retained = "/tmp/retained.jsonl"
        chs = {
            relay_watchdog.PENDING_TRANSCRIPTS_KEY: [released, retained],
            relay_watchdog.PENDING_TRANSCRIPT_FAILURES_KEY: {
                released: 2,
                retained: 1,
            },
            relay_watchdog.PENDING_TRANSCRIPT_SINCE_KEY: {
                released: self.now - 20,
                retained: self.now - 10,
            },
            self.ORPHAN_STRANDED_KEY: {
                released: self.now - 20,
                retained: self.now - 10,
            },
        }

        pending, failures, pending_since, stranded_since = (
            relay_watchdog._release_pending_authority(
                chs,
                [released, retained],
                {released: 2, retained: 1},
                {released: self.now - 20, retained: self.now - 10},
                {released: self.now - 20, retained: self.now - 10},
                {released},
            )
        )

        self.assertEqual(pending, [retained])
        self.assertEqual(failures, {retained: 1})
        self.assertEqual(pending_since, {retained: self.now - 10})
        self.assertEqual(stranded_since, {retained: self.now - 10})
        self.assertEqual(
            chs[self.ORPHAN_STRANDED_KEY], {retained: self.now - 10}
        )

    def test_5206_stranded_cap_keeps_new_marker_and_evicts_oldest(self):
        cap = relay_watchdog.MAX_ORPHAN_STRANDED_PATHS
        # Path order is intentionally the reverse of the eviction decision:
        # the broken `dict(sorted(stranded.items())[:cap])` keeps `oldest` and
        # drops `newest`, while timestamp order must do the opposite.
        oldest = "/tmp/orphan-aaa-oldest.jsonl"
        newest = "/tmp/orphan-zzz-newest.jsonl"
        stranded = {
            oldest: self.now,
            **{
                f"/tmp/orphan-mid-{index:02d}.jsonl": self.now + index
                for index in range(1, cap)
            },
            newest: self.now + cap,
            "/tmp/orphan-missing-timestamp.jsonl": None,
            "/tmp/orphan-unparseable-timestamp.jsonl": "not-a-timestamp",
        }
        chs: dict = {}

        relay_watchdog._store_orphan_stranded_since(chs, stranded)

        stored = chs[self.ORPHAN_STRANDED_KEY]
        self.assertEqual(len(stored), cap)
        self.assertIn(newest, stored)
        self.assertNotIn(oldest, stored)
        self.assertNotIn("/tmp/orphan-missing-timestamp.jsonl", stored)
        self.assertNotIn("/tmp/orphan-unparseable-timestamp.jsonl", stored)

    def test_5206_idle_expiry_prunes_an_uncorroborated_stranded_marker(self):
        """Reproduce the review's marker leak after orphan retirement defers."""
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]
        marker_at = self.now + rt.cfg.orphan_abandon_secs + 600
        self._5190_strand_orphan(orphan, self.now)
        self._5190_live_delivery(rt, live, marker_at, "live marker opener")
        tick_channel(rt, TICK_CHANNEL, state, marker_at)
        self.assertIn(str(orphan), chs[self.ORPHAN_STRANDED_KEY])

        # No sibling block newer than marker_at is delivered, so orphan close
        # lacks fresh corroboration. Two-hour pending expiry retires the path
        # before evaluation; there is no later marker-cleared opportunity.
        expiry = marker_at + rt.cfg.idle_quiet_secs + 1
        tick_channel(rt, TICK_CHANNEL, state, expiry)

        self.assertIn(
            str(orphan), chs[relay_watchdog.RETIRED_TRANSCRIPTS_KEY]
        )
        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.PENDING_TRANSCRIPTS_KEY, [])
        )
        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY, [])
        )
        self.assertNotIn(
            str(orphan), chs.get(self.ORPHAN_STRANDED_KEY, {})
        )

    def test_5190_frozen_orphan_stops_pinning_a_delivering_channel_in_gap(self):
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]

        discovered_at = self.now + rt.cfg.orphan_abandon_secs + 600
        self._5190_strand_orphan(orphan, self.now)
        self._5190_live_delivery(rt, live, discovered_at, "live block two")
        tick_channel(rt, TICK_CHANNEL, state, discovered_at)

        # The channel is provably delivering on `live`, so `orphan`'s unmatched
        # blocks are not evidence of a relay gap and must not be alerted as one.
        self.assertEqual(chs[SELECTED_TRANSCRIPT_KEY], str(live))
        self.assertEqual(
            [body for body, _ in rt.alerts if "릴레이 갭 감지" in body],
            [],
            rt.log_lines,
        )
        self.assertIn(str(orphan), chs[self.ORPHAN_STRANDED_KEY])
        self.assertTrue(
            any("orphan-stranded-marker-opened" in line for line in rt.log_lines)
        )

        retire_at = discovered_at + relay_watchdog.ORPHAN_STRANDED_CONFIRM_SECS
        self._5190_live_delivery(rt, live, retire_at, "live block three")
        tick_channel(rt, TICK_CHANNEL, state, retire_at)

        # ...and once the finding has held through the confirmation window the
        # authority is closed out, which is the exit #5190 had none of.
        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.PENDING_TRANSCRIPTS_KEY, [])
        )
        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY, [])
        )
        self.assertIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertNotIn(str(orphan), chs.get(self.ORPHAN_STRANDED_KEY, {}))
        self.assertFalse(chs.get("alerting"), (chs, rt.log_lines))
        self.assertNotIn("gap_since", chs)
        retirement = [body for body, _ in rt.alerts if "고아 세션" in body]
        self.assertEqual(len(retirement), 1, rt.alerts)
        self.assertIn("회수 불가", retirement[0])
        # Retirement is not a delivery claim: no recovery notice may go out.
        self.assertFalse(any("릴레이 갭 해소" in body for body, _ in rt.alerts))
        self.assertTrue(
            any(
                "orphan-stranded-authority-retired" in line
                for line in rt.log_lines
            )
        )

        # A full re-alert cycle later the channel is still quiet: the 15-minute
        # forever-loop is gone, not merely delayed.
        quiet_at = retire_at + rt.cfg.realert_secs + 1
        self._5190_live_delivery(rt, live, quiet_at, "live block four")
        tick_channel(rt, TICK_CHANNEL, state, quiet_at)
        self.assertEqual(
            [body for body, _ in rt.alerts if "릴레이 갭 감지" in body], []
        )
        self.assertEqual(
            len([body for body, _ in rt.alerts if "고아 세션" in body]), 1
        )

    def test_5190_reverse_frozen_orphan_still_alerts_when_nothing_delivers(self):
        """THE reverse direction: no delivery anywhere ⇒ no silence anywhere.

        Same frozen orphan, same elapsed time — only the corroborating delivery
        is missing, because the relay is down. Nothing may be reclassified.
        """
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]

        outage_at = self.now + rt.cfg.orphan_abandon_secs + 600
        self._5190_strand_orphan(orphan, self.now)
        # The live session keeps writing and NOTHING arrives in Discord.
        with live.open("a", encoding="utf-8") as stream:
            stream.write(
                self._5190_record(outage_at - 700, "live block lost in outage")
                + "\n"
            )
        os.utime(live, (outage_at, outage_at))
        tick_channel(rt, TICK_CHANNEL, state, outage_at)

        gap_alerts = [body for body, _ in rt.alerts if "릴레이 갭 감지" in body]
        self.assertEqual(len(gap_alerts), 1, (rt.alerts, rt.log_lines))
        self.assertIn(live.name, gap_alerts[0])
        self.assertTrue(chs.get("alerting"))
        self.assertNotIn(str(orphan), chs.get(self.ORPHAN_STRANDED_KEY, {}))
        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )

    def test_5190_reverse_relay_death_after_orphan_retirement_still_alerts(self):
        """The retirement must not desensitize the channel it retired from."""
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]
        retire_at = self._5190_retire_orphan(rt, state, live, orphan)
        self.assertIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertEqual(
            [body for body, _ in rt.alerts if "릴레이 갭 감지" in body], []
        )

        outage_at = retire_at + 3600
        with live.open("a", encoding="utf-8") as stream:
            stream.write(
                self._5190_record(outage_at - 700, "post-retirement block lost")
                + "\n"
            )
        os.utime(live, (outage_at, outage_at))
        tick_channel(rt, TICK_CHANNEL, state, outage_at)

        gap_alerts = [body for body, _ in rt.alerts if "릴레이 갭 감지" in body]
        self.assertEqual(len(gap_alerts), 1, (rt.alerts, rt.log_lines))
        self.assertIn(live.name, gap_alerts[0])
        self.assertTrue(chs.get("alerting"))

    def test_5190_r1_undelivered_retirement_notice_keeps_the_authority_open(self):
        """The notice IS the retirement (#5190 R1).

        `_alert_pending_retirement` returns False when nothing left the box.
        Retiring anyway deletes the loss record with nobody ever told — the
        observation failure collapsed into a success this campaign keeps finding.
        """
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]
        discovered_at = self.now + rt.cfg.orphan_abandon_secs + 600
        self._5190_strand_orphan(orphan, self.now)
        self._5190_live_delivery(rt, live, discovered_at, "live block two")
        tick_channel(rt, TICK_CHANNEL, state, discovered_at)
        self.assertIn(str(orphan), chs[self.ORPHAN_STRANDED_KEY])

        retire_at = discovered_at + relay_watchdog.ORPHAN_STRANDED_CONFIRM_SECS
        self._5190_live_delivery(rt, live, retire_at, "live block three")
        rt.alert_succeeds = False
        tick_channel(rt, TICK_CHANNEL, state, retire_at)

        self.assertIn(
            str(orphan), chs.get(relay_watchdog.PENDING_TRANSCRIPTS_KEY, [])
        )
        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertIn(str(orphan), chs.get(self.ORPHAN_STRANDED_KEY, {}))
        self.assertTrue(
            any(
                "orphan-stranded-retirement-deferred" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )
        self.assertFalse(
            any(
                "orphan-stranded-authority-retired" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )

        # A deferral, not a dead end: the first tick that reaches a human
        # completes exactly the same retirement.
        rt.alert_succeeds = True
        retry_at = retire_at + 1
        self._5190_live_delivery(rt, live, retry_at, "live block four")
        tick_channel(rt, TICK_CHANNEL, state, retry_at)
        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.PENDING_TRANSCRIPTS_KEY, [])
        )
        self.assertIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertEqual(
            len([body for body, _ in rt.alerts if "회수 불가" in body]),
            2,
            rt.alerts,
        )

    def _5190_observed_orphan(
        self, rt: FakeRuntime, orphan: Path, frozen_at: float
    ) -> None:
        """An orphan that DID deliver once and then stranded the rest (R2).

        Its `gap_secs` is a real measurement, not the unobserved sentinel, which
        is exactly why silencing it needs a stronger bar than the never-seen case.
        """
        orphan.write_text(
            self._5190_record(frozen_at - 900, "orphan block that did arrive")
            + "\n"
            + self._5190_record(frozen_at - 700, "orphan block stranded one")
            + "\n"
            + self._5190_record(frozen_at - 600, "orphan block stranded two")
            + "\n",
            encoding="utf-8",
        )
        os.utime(orphan, (frozen_at, frozen_at))
        rt.haystack = norm(f"{rt.haystack} orphan block that did arrive")

    def _5190_observed_orphan_tick(
        self, *, bind_successor: bool, elapsed: float
    ) -> tuple[FakeRuntime, dict, dict, Path, Path]:
        for stale in self.proj_dir.glob("*.jsonl"):
            stale.unlink()
        rt, state, live, orphan = self._5190_live_channel()
        self._5190_observed_orphan(rt, orphan, self.now)
        at = self.now + elapsed
        self._5190_live_delivery(rt, live, at, "live block two")
        if bind_successor:
            rt.watcher_probe = WatcherStateProbe(
                200, attached=True, desynced=False, bound_output_path=str(live)
            )
        tick_channel(rt, TICK_CHANNEL, state, at)
        return rt, state, state["999"], live, orphan

    def test_5190_r2_observed_history_orphan_retires_on_stronger_evidence(self):
        """R2: delivery history does not make the same defect out of scope.

        A `/clear`ed session that delivered before stranding its tail is pinned
        by the identical loop — every exit blocked, re-alerting every 15 minutes
        until an unrelated idle timer expires. It is admitted, but only on
        evidence strictly stronger than the never-observed case.
        """
        doubled = (
            relay_watchdog.Config().orphan_abandon_secs
            * relay_watchdog.ORPHAN_OBSERVED_FREEZE_MULTIPLIER
        )
        rt, state, chs, live, orphan = self._5190_observed_orphan_tick(
            bind_successor=True, elapsed=doubled + 60
        )
        self.assertIn(str(orphan), chs[self.ORPHAN_STRANDED_KEY])
        self.assertTrue(
            any(
                "orphan-stranded-marker-opened" in line
                and "observed_history=True" in line
                and f"freeze_floor={doubled}s" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )

        discovered_at = self.now + doubled + 60
        retire_at = discovered_at + relay_watchdog.ORPHAN_STRANDED_CONFIRM_SECS
        self._5190_live_delivery(rt, live, retire_at, "live block three")
        tick_channel(rt, TICK_CHANNEL, state, retire_at)

        self.assertIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.PENDING_TRANSCRIPTS_KEY, [])
        )
        self.assertEqual(
            len([body for body, _ in rt.alerts if "회수 불가" in body]), 1
        )
        self.assertFalse(any("릴레이 갭 해소" in body for body, _ in rt.alerts))

    def test_5190_r2_reverse_a_measured_gap_survives_a_weaker_evidence_bar(self):
        """THE reverse direction for R2 — the bar is what keeps it loud.

        Drop either half of the extra evidence and the measured gap must keep
        alerting, still naming its elapsed time as a measurement. This is the
        pin against R2's fix quietly widening into "orphans are never reported".
        """
        cfg = relay_watchdog.Config()
        doubled = cfg.orphan_abandon_secs * (
            relay_watchdog.ORPHAN_OBSERVED_FREEZE_MULTIPLIER
        )
        cases = (
            ("dcserver never confirmed the channel moved on", False, doubled + 60),
            ("frozen past the unobserved floor only", True, cfg.orphan_abandon_secs + 60),
        )
        for label, bind_successor, elapsed in cases:
            with self.subTest(label):
                rt, _state, chs, _live, orphan = self._5190_observed_orphan_tick(
                    bind_successor=bind_successor, elapsed=elapsed
                )
                gap_alerts = [
                    body for body, _ in rt.alerts if "릴레이 갭 감지" in body
                ]
                self.assertEqual(len(gap_alerts), 1, (rt.alerts, rt.log_lines))
                self.assertIn(orphan.name, gap_alerts[0])
                # The measurement is reported AS a measurement.
                self.assertIn("마지막 정상 도달 이후", gap_alerts[0])
                self.assertNotIn("배달이 확인된 이력이 없습니다", gap_alerts[0])
                self.assertNotIn(str(orphan), chs.get(self.ORPHAN_STRANDED_KEY, {}))
                self.assertNotIn(
                    str(orphan),
                    chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {}),
                )

    def test_5190_young_orphan_gap_names_the_session_and_narrows_the_claim(self):
        """Below the freeze floor nothing is reclassified — #4435 lives here.

        A session swapped in moments ago and losing blocks right now is also
        unmatched, also not the selected transcript, and also has a delivering
        sibling. Only elapsed silence separates it from #5190's corpse, so the
        alert still fires; what changes is that it now says WHICH session it is
        talking about and stops claiming a live relay failure.

        #5190 R5: r1's version of this test pinned the WRONG rendering — it
        asserted that this very target was announced as a "고아 세션 / 과거
        세션". Non-selection is not pastness; only the freeze floor this target
        has not yet crossed could carry that claim.
        """
        rt, state, live, orphan = self._5190_live_channel()
        young_at = self.now + rt.cfg.orphan_abandon_secs - 600
        self._5190_strand_orphan(orphan, self.now)
        self._5190_live_delivery(rt, live, young_at, "live block two")
        tick_channel(rt, TICK_CHANNEL, state, young_at)

        gap_alerts = [body for body, _ in rt.alerts if "릴레이 갭 감지" in body]
        self.assertEqual(len(gap_alerts), 1, (rt.alerts, rt.log_lines))
        body = gap_alerts[0]
        self.assertIn(orphan.name, body)
        self.assertIn(live.name, body)
        # Not proved, therefore not claimed.
        self.assertNotIn("고아 세션", body)
        self.assertNotIn("과거 세션에 미도달 블록이", body)
        self.assertIn("아직 과거 세션이라고 단정할 수 없습니다", body)
        # Proved this tick, therefore stated: it is not the active session, and
        # a sibling really did deliver.
        self.assertIn("현재 활성 세션(`%s`)이 아닙니다" % live.name, body)
        self.assertIn("실제로 배달이 확인됐습니다", body)
        # R2: an unknown elapsed time is stated as unknown, not as 999 minutes.
        self.assertIn("배달이 확인된 이력이 없습니다", body)
        self.assertNotIn("999분", body)
        self.assertNotIn("**999**", body)
        self.assertNotIn(
            str(orphan), state["999"].get(self.ORPHAN_STRANDED_KEY, {})
        )

    def test_5190_r3_a_block_that_arrives_during_the_window_is_not_retired(self):
        """Maturity is a timer, not a finding (#5190 R3).

        The confirmation window exists so the finding can be falsified. If a
        stranded block finally reaches Discord inside it, "회수 불가" becomes a
        false statement about blocks that had already arrived — so the claim is
        re-checked against the current verdict before it is made.
        """
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]
        discovered_at = self.now + rt.cfg.orphan_abandon_secs + 600
        self._5190_strand_orphan(orphan, self.now)
        self._5190_live_delivery(rt, live, discovered_at, "live block two")
        tick_channel(rt, TICK_CHANNEL, state, discovered_at)
        self.assertIn(str(orphan), chs[self.ORPHAN_STRANDED_KEY])

        # The relay was slow, not dead: both stranded blocks land during the
        # confirmation window.
        recovered_at = (
            discovered_at + relay_watchdog.ORPHAN_STRANDED_CONFIRM_SECS
        )
        rt.haystack = norm(
            f"{rt.haystack} stranded orphan block one stranded orphan block two"
        )
        tick_channel(rt, TICK_CHANNEL, state, recovered_at)

        self.assertEqual([b for b, _ in rt.alerts if "회수 불가" in b], [])
        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertNotIn(str(orphan), chs.get(self.ORPHAN_STRANDED_KEY, {}))
        self.assertTrue(
            any(
                "orphan-stranded-maturity-voided" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )

    def test_5190_r4_a_stale_watermark_is_not_proof_of_live_delivery(self):
        """R4: the safety hinge must prove a match NOW, not a match once.

        `Verdict.delivered_ts` is `max(persisted watermark, current match)`, so
        an idle sibling that delivered minutes ago voted "the channel is fine"
        for a full `gap_alert_secs` on an empty haystack — and that vote is the
        only thing standing between a real gap and being marked stranded.

        R5 rides along: this orphan HAS crossed the freeze floor, so the "고아
        세션" wording is earned here, while the relay-health clause must stay
        honest because nothing was seen delivering.
        """
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]
        # A delivery recent enough that its watermark is younger than
        # `gap_alert_secs` at probe time.
        mid_at = self.now + rt.cfg.orphan_abandon_secs + 50
        self._5190_live_delivery(rt, live, mid_at, "live block two")
        tick_channel(rt, TICK_CHANNEL, state, mid_at)

        probe_at = mid_at + 100
        self._5190_strand_orphan(orphan, self.now)
        # The bounded Discord read window rolled over: nothing matches now.
        rt.haystack = ""
        tick_channel(rt, TICK_CHANNEL, state, probe_at)

        self.assertLessEqual(
            probe_at - relay_watchdog.delivered_watermark_for_path(chs, live),
            rt.cfg.gap_alert_secs,
            "watermark must still be fresh, or the test proves nothing",
        )
        self.assertNotIn(str(orphan), chs.get(self.ORPHAN_STRANDED_KEY, {}))
        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        gap_alerts = [body for body, _ in rt.alerts if "릴레이 갭 감지" in body]
        self.assertEqual(len(gap_alerts), 1, (rt.alerts, rt.log_lines))
        body = gap_alerts[0]
        self.assertIn(orphan.name, body)
        self.assertIn("고아 세션", body)
        self.assertIn("분째 새 기록이 없습니다", body)
        self.assertIn("릴레이 장애 가능성이 남아 있습니다", body)
        self.assertNotIn("실제로 배달이 확인됐습니다", body)

    # ── #5190 R3: a matured marker is a claim, and claims get re-checked ─────
    #
    # r2 re-verified STATE_GAP and semantic growth at maturity and stopped
    # there, so three of the four conditions that justified OPENING the marker
    # were never asked again before it was cashed in. Each test below is one of
    # the ways that let a retirement — a user-facing "회수 불가" statement — be
    # made on evidence nobody had re-read.

    def _5190_marker_open(
        self, rt: FakeRuntime, state: dict, live: Path, orphan: Path
    ) -> float:
        """Tick the channel to the point where the stranded marker is open."""
        discovered_at = self.now + rt.cfg.orphan_abandon_secs + 600
        self._5190_strand_orphan(orphan, self.now)
        self._5190_live_delivery(rt, live, discovered_at, "live block two")
        tick_channel(rt, TICK_CHANNEL, state, discovered_at)
        self.assertIn(str(orphan), state["999"][self.ORPHAN_STRANDED_KEY])
        return discovered_at

    def test_5190_r3_a_matured_marker_does_not_close_through_an_outage(self):
        """(a) The relay dies inside the confirmation window.

        The marker was opened on proof that a sibling was delivering. If that
        stops being true before the window elapses, the finding has lost the
        one piece of evidence that distinguished a dead session from a dead
        relay — and retiring anyway sends "회수 불가" about blocks whose only
        real problem is that the relay is down. This is the exact inversion the
        whole mechanism exists to prevent.
        """
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]
        discovered_at = self._5190_marker_open(rt, state, live, orphan)

        outage_at = (
            discovered_at + relay_watchdog.ORPHAN_STRANDED_CONFIRM_SECS + 400
        )
        with live.open("a", encoding="utf-8") as stream:
            stream.write(
                self._5190_record(outage_at - 700, "live block lost in outage")
                + "\n"
            )
        os.utime(live, (outage_at, outage_at))
        tick_channel(rt, TICK_CHANNEL, state, outage_at)

        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertEqual([b for b, _ in rt.alerts if "회수 불가" in b], [])
        # The finding is not disproved either — the marker survives, so the
        # first tick that can prove it again completes the retirement.
        self.assertIn(str(orphan), chs.get(self.ORPHAN_STRANDED_KEY, {}))
        self.assertTrue(
            any(
                "orphan-stranded-maturity-unconfirmed" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )
        # And the outage is loud, not silent.
        self.assertTrue(chs.get("alerting"))

    def test_5190_r3_a_pre_marker_delivery_cannot_corroborate_a_retirement(self):
        """(a') The write-epoch window — #5190 R3 P2-D's 900-second blind spot.

        `current_delivered_by_path` holds block WRITE epochs, not delivery
        times, so an idle sibling whose last block still matches keeps voting
        "the relay is alive" for a full `gap_alert_secs`. That is LONGER than
        the confirmation window it is carrying, so a relay that died the moment
        the marker opened was indistinguishable from a healthy one for the
        entire window. Nothing here writes a new block; the only evidence is a
        match that predates the finding.
        """
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]
        discovered_at = self._5190_marker_open(rt, state, live, orphan)

        retire_at = (
            discovered_at + relay_watchdog.ORPHAN_STRANDED_CONFIRM_SECS
        )
        # The sibling's newest matched block is the one from the marker tick,
        # and it is still inside the freshness bound — or this proves nothing.
        self.assertLess(
            retire_at - (discovered_at - 30),
            rt.cfg.gap_alert_secs,
            "stale match must still pass the freshness bound",
        )
        tick_channel(rt, TICK_CHANNEL, state, retire_at)

        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertEqual([b for b, _ in rt.alerts if "회수 불가" in b], [])
        self.assertIn(str(orphan), chs.get(self.ORPHAN_STRANDED_KEY, {}))
        self.assertTrue(
            any(
                "orphan-stranded-maturity-unconfirmed" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )

    def test_5190_r3_an_orphan_promoted_to_the_active_session_is_not_retired(
        self,
    ):
        """(b) The orphan IS the channel's current session by the time it matures.

        The worst of the three: retiring the selected transcript drops the
        channel's GAP ownership to zero, so the session that is running RIGHT
        NOW loses its own loss record and the incident closes with nobody told.
        `eligible` already rejects a selected path — what was missing is that
        maturity never consulted it.
        """
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]
        discovered_at = self._5190_marker_open(rt, state, live, orphan)

        # The live transcript is gone, so the selector falls back to the only
        # candidate left: the orphan itself.
        live.unlink()
        retire_at = (
            discovered_at + relay_watchdog.ORPHAN_STRANDED_CONFIRM_SECS
        )
        tick_channel(rt, TICK_CHANNEL, state, retire_at)

        self.assertEqual(chs.get(SELECTED_TRANSCRIPT_KEY), str(orphan))
        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertEqual([b for b, _ in rt.alerts if "회수 불가" in b], [])
        # Gap ownership did NOT drop to zero.
        self.assertIn(
            str(orphan), chs.get(relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY, [])
        )
        self.assertTrue(
            any(
                "orphan-stranded-marker-cleared" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )

    def test_5190_r3_a_marker_that_matured_unevaluated_still_needs_proof(self):
        """(c) The path was not judged for a tick and came back to an expired timer.

        The marker is persisted state and its clock does not care whether
        anyone was looking. A path that drops out of the candidate set for a
        tick — a transient enumeration or read failure — returns to find the
        window already elapsed, and the pre-fix code closed it out on the first
        tick that saw it again, without ④ ever having been asked since the
        finding was made.
        """
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]
        discovered_at = self._5190_marker_open(rt, state, live, orphan)
        content = orphan.read_text(encoding="utf-8")

        orphan.unlink()
        tick_channel(rt, TICK_CHANNEL, state, discovered_at + 60)
        self.assertIn(str(orphan), chs.get(self.ORPHAN_STRANDED_KEY, {}))

        orphan.write_text(content, encoding="utf-8")
        os.utime(orphan, (self.now, self.now))
        back_at = discovered_at + relay_watchdog.ORPHAN_STRANDED_CONFIRM_SECS
        tick_channel(rt, TICK_CHANNEL, state, back_at)

        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertEqual([b for b, _ in rt.alerts if "회수 불가" in b], [])

        # A deferral, not a dead end: a tick that CAN prove the channel is
        # delivering completes exactly the same retirement.
        proven_at = back_at + 60
        self._5190_live_delivery(rt, live, proven_at, "live block three")
        tick_channel(rt, TICK_CHANNEL, state, proven_at)
        self.assertIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertEqual(
            len([b for b, _ in rt.alerts if "회수 불가" in b]), 1, rt.alerts
        )

    def test_5190_r3_reverse_a_fully_corroborated_orphan_still_retires(self):
        """THE reverse direction for every re-check added above.

        Four new conditions now stand between a matured marker and a
        retirement, and a guard that can never be satisfied is not a guard —
        it is the #5190 defect with extra steps. A genuine orphan on a
        genuinely delivering channel must still close out, in one notice, with
        no unconfirmed deferral anywhere in the log.
        """
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]
        self._5190_retire_orphan(rt, state, live, orphan)

        self.assertIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertNotIn(str(orphan), chs.get(self.ORPHAN_STRANDED_KEY, {}))
        self.assertEqual(
            len([b for b, _ in rt.alerts if "회수 불가" in b]), 1, rt.alerts
        )
        self.assertFalse(
            any(
                "orphan-stranded-maturity-unconfirmed" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )
        self.assertTrue(
            any(
                "orphan-stranded-authority-retired" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )

    def test_5190_r3_a_marker_may_not_close_before_the_confirmation_window(
        self,
    ):
        """⑤ on its own: the window is what makes one anomalous tick harmless.

        Everything else the retirement needs is true here — the orphan is
        frozen, unselected, still in GAP, and the channel is delivering fresh
        blocks. Only the clock has not run out. Nothing may close.
        """
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]
        discovered_at = self._5190_marker_open(rt, state, live, orphan)

        early_at = (
            discovered_at + relay_watchdog.ORPHAN_STRANDED_CONFIRM_SECS - 60
        )
        self._5190_live_delivery(rt, live, early_at, "live block three")
        tick_channel(rt, TICK_CHANNEL, state, early_at)

        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertIn(str(orphan), chs.get(self.ORPHAN_STRANDED_KEY, {}))
        self.assertEqual([b for b, _ in rt.alerts if "회수 불가" in b], [])
        self.assertFalse(
            any(
                "orphan-stranded-authority-retired" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )

        # ...and the tick that DOES reach the window closes it, so this pins
        # the window rather than "nothing ever retires".
        retire_at = (
            discovered_at + relay_watchdog.ORPHAN_STRANDED_CONFIRM_SECS
        )
        self._5190_live_delivery(rt, live, retire_at, "live block four")
        tick_channel(rt, TICK_CHANNEL, state, retire_at)
        self.assertIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )

    def test_5190_r3_live_delivery_freshness_bound_rejects_a_stale_match(self):
        """② of `_channel_has_live_delivery`, exercised on its own.

        Every end-to-end test reaches this helper through the `matched_ts <=
        0.0` branch (an empty haystack), so the freshness clause — the
        difference between "this sibling matched something" and "this sibling
        matched something RECENTLY" — was never executed by any assertion.
        Deleting it changed no test result, which is the definition of an
        untested safeguard. The reachable production shape is an idle sibling
        whose old block is still inside the bounded Discord read window.
        """
        now = 1_000_000.0
        gap_alert_secs = 900
        sibling = relay_watchdog.TranscriptCandidate(
            path=Path("/tmp/sibling.jsonl"), size=10, mtime=now
        )
        verdict = relay_watchdog.Verdict(
            state=relay_watchdog.STATE_OK,
            blocks=1,
            stale=0,
            lost=0,
            delivered_ts=now - 10,
            gap_secs=10.0,
        )
        evaluated = [(sibling, verdict)]

        def probe(matched_ts: float, **kwargs) -> bool:
            return relay_watchdog._channel_has_live_delivery(
                evaluated,
                {},
                {str(sibling.path): matched_ts},
                "/tmp/orphan.jsonl",
                now,
                gap_alert_secs,
                **kwargs,
            )

        self.assertTrue(probe(now))
        self.assertTrue(probe(now - (gap_alert_secs - 1)))
        # The boundary is inclusive on the fresh side...
        self.assertTrue(probe(now - gap_alert_secs))
        # ...and one second past it the match is no longer evidence that
        # anything is delivering now.
        self.assertFalse(probe(now - (gap_alert_secs + 1)))

        # `newer_than` is a SEPARATE, stricter lower bound: it does not replace
        # the freshness bound, and neither one alone is sufficient.
        self.assertFalse(probe(now - 100, newer_than=now - 50))
        self.assertTrue(probe(now - 30, newer_than=now - 50))
        self.assertFalse(probe(now - (gap_alert_secs + 1), newer_than=0.0))

    def test_5190_r3_retirement_notice_reports_cooldown_apart_from_failure(
        self,
    ):
        """#5190 R3 P2-E: "nothing went out" has two causes, not one.

        The retirement notice shares ONE cooldown key across every reason, so
        an unrelated idle/read_failure/dead_worktree notice within
        `realert_secs` suppresses this one. The old code answered a bare bool
        and the caller logged `notice=undelivered` for both — a false statement
        about the relay written into the record every time the real cause was a
        cooldown.
        """
        rt = self.make_rt()
        chs: dict = {}
        now = self.now

        self.assertEqual(
            relay_watchdog._alert_pending_retirement(
                rt, TICK_CHANNEL, chs, [], now, reason="idle"
            ),
            "empty",
        )
        self.assertEqual(
            relay_watchdog._alert_pending_retirement(
                rt, TICK_CHANNEL, chs, ["/a.jsonl"], now, reason="idle"
            ),
            "sent",
        )
        # A DIFFERENT reason, moments later, on the shared key.
        self.assertEqual(
            relay_watchdog._alert_pending_retirement(
                rt,
                TICK_CHANNEL,
                chs,
                ["/b.jsonl"],
                now + 60,
                reason=relay_watchdog.ORPHAN_STRANDED_RETIREMENT_REASON,
            ),
            "cooldown",
        )
        self.assertFalse(
            any(
                "transcript-retirement-alert undelivered" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )

        rt.alert_succeeds = False
        self.assertEqual(
            relay_watchdog._alert_pending_retirement(
                rt,
                TICK_CHANNEL,
                chs,
                ["/c.jsonl"],
                now + rt.cfg.realert_secs + 1,
                reason="idle",
            ),
            "undelivered",
        )
        self.assertTrue(
            any(
                "transcript-retirement-alert undelivered" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )

    def test_5190_r3_orphan_retirement_deferred_by_cooldown_says_cooldown(self):
        """The same distinction where a human reads it: the deferral log line."""
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]
        discovered_at = self._5190_marker_open(rt, state, live, orphan)

        retire_at = (
            discovered_at + relay_watchdog.ORPHAN_STRANDED_CONFIRM_SECS
        )
        self._5190_live_delivery(rt, live, retire_at, "live block three")
        # An unrelated retirement notice went out moments ago on the shared key.
        chs[relay_watchdog.LAST_PENDING_TRANSCRIPT_RETIREMENT_ALERT_KEY] = (
            retire_at - 10
        )
        tick_channel(rt, TICK_CHANNEL, state, retire_at)

        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertTrue(
            any(
                "orphan-stranded-retirement-deferred" in line
                and "notice=cooldown" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )
        self.assertFalse(
            any("notice=undelivered" in line for line in rt.log_lines),
            rt.log_lines,
        )

    def test_5190_r3_a_rewound_mtime_cannot_manufacture_an_orphan(self):
        """② alone, where it is the ONLY thing holding the line (#5190 R3 P2-C).

        Safeguard ① (the doubled freeze floor) reads `mtime` — filesystem
        METADATA, which any restore can set to whatever it likes: `cp -p`,
        `rsync -t` and `tar -x` all stamp a file with an mtime older than the
        newest record it actually contains. Safeguard ② reads the block
        timestamps the watchdog parsed out of the file's CONTENT, so it is the
        one check a rewound mtime cannot fool.

        On honest metadata ② is slack — ① implies it, which is why forcing ②
        true changed no test result before this one existed. Here the metadata
        lies: the file claims to have been frozen for over an hour while its
        own records show it delivering 16 minutes ago. ① passes. Only ② stands
        between a live session and a "회수 불가" notice about its blocks.
        """
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]
        at = self.now + 4000
        floor = rt.cfg.orphan_abandon_secs * (
            relay_watchdog.ORPHAN_OBSERVED_FREEZE_MULTIPLIER
        )

        # Content: delivered 1000s ago, stranded a block 950s ago. That gap is
        # past `gap_alert_secs` (so STATE_GAP) but well inside
        # `orphan_abandon_secs` (so the frontier is NOT stale).
        orphan.write_text(
            self._5190_record(at - 1000, "orphan block that did arrive")
            + "\n"
            + self._5190_record(at - 950, "orphan block stranded")
            + "\n",
            encoding="utf-8",
        )
        rt.haystack = norm(f"{rt.haystack} orphan block that did arrive")
        # Metadata: rewound to look frozen for over an hour.
        os.utime(orphan, (at - 3700, at - 3700))
        self._5190_live_delivery(rt, live, at, "live block two")
        rt.watcher_probe = WatcherStateProbe(
            200, attached=True, desynced=False, bound_output_path=str(live)
        )
        tick_channel(rt, TICK_CHANNEL, state, at)

        # The preconditions this test exists to isolate: ① passes on the lie,
        # every other safeguard is satisfied, and ② is the sole objection.
        self.assertGreaterEqual(at - orphan.stat().st_mtime, floor)
        frontier = relay_watchdog.delivered_watermark_for_path(chs, orphan)
        self.assertGreater(frontier, 0.0)
        self.assertLess(at - frontier, rt.cfg.orphan_abandon_secs)
        self.assertGreater(at - frontier, rt.cfg.gap_alert_secs)

        # Therefore: no marker, no retirement, and the gap stays loud.
        self.assertNotIn(str(orphan), chs.get(self.ORPHAN_STRANDED_KEY, {}))
        self.assertNotIn(
            str(orphan), chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {})
        )
        self.assertEqual([b for b, _ in rt.alerts if "회수 불가" in b], [])
        gap_alerts = [body for body, _ in rt.alerts if "릴레이 갭 감지" in body]
        self.assertEqual(len(gap_alerts), 1, (rt.alerts, rt.log_lines))
        self.assertIn(orphan.name, gap_alerts[0])

    def test_5190_r3_reverse_an_honest_frontier_still_opens_the_marker(self):
        """The reverse of the above: ② must not block a genuine orphan.

        The freshest delivery frontier an observed orphan can honestly have is
        one where the last block it ever wrote is one that arrived. On truthful
        metadata ① already implies ②, so the marker opens — a guard that is
        slack in the normal case and binding only when `mtime` lies is exactly
        what it should be.
        """
        rt, state, live, orphan = self._5190_live_channel()
        chs = state["999"]
        floor = rt.cfg.orphan_abandon_secs * (
            relay_watchdog.ORPHAN_OBSERVED_FREEZE_MULTIPLIER
        )
        orphan.write_text(
            self._5190_record(self.now - 2, "orphan tail that did arrive")
            + "\n"
            + self._5190_record(self.now - 1, "orphan tail stranded")
            + "\n",
            encoding="utf-8",
        )
        os.utime(orphan, (self.now, self.now))
        rt.haystack = norm(f"{rt.haystack} orphan tail that did arrive")
        at = self.now + floor + 60
        self._5190_live_delivery(rt, live, at, "live block two")
        rt.watcher_probe = WatcherStateProbe(
            200, attached=True, desynced=False, bound_output_path=str(live)
        )
        tick_channel(rt, TICK_CHANNEL, state, at)

        self.assertIn(str(orphan), chs[self.ORPHAN_STRANDED_KEY])
        self.assertTrue(
            any(
                "orphan-stranded-marker-opened" in line
                and "observed_history=True" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )
        frontier = relay_watchdog.delivered_watermark_for_path(chs, orphan)
        self.assertLessEqual(frontier, orphan.stat().st_mtime)
        self.assertGreaterEqual(at - frontier, rt.cfg.orphan_abandon_secs)

    def _5190_no_selection_tick(self) -> tuple[FakeRuntime, dict, dict, Path]:
        """Drive the channel to a tick that alerts with NOTHING selected.

        Reachability, not hypothesis (#5190 R3 P2-F). The route, all of it
        ordinary production behaviour:

        1. `live` delivers, is selected, and therefore leaves the pending set.
        2. `live` strands a block. It becomes a GAP owner — and because it is
           not pending, no pending expiry can ever remove it.
        3. A brand-new transcript debuts with a later mtime and takes the
           selection on the `unseen_newer` rule. It has delivered nothing, so
           it stays pending.
        4. The channel goes quiet past `idle_quiet_secs`. The pending expiry
           fires on the SELECTED path and clears `tr` — and it does so upstream
           of the line that resolves `selected` from `tr`, so `selected` is None
           for the rest of the pass while `live` is still a GAP owner and still
           the alert subject.
        """
        rt, state, live, _orphan = self._5190_live_channel()
        chs = state["999"]
        anchor = self.now

        with live.open("a", encoding="utf-8") as stream:
            stream.write(
                self._5190_record(anchor + 300, "live block never delivered")
                + "\n"
            )
        os.utime(live, (anchor + 1000, anchor + 1000))
        tick_channel(rt, TICK_CHANNEL, state, anchor + 1000)
        self.assertIn(
            str(live), chs.get(relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY, [])
        )
        self.assertNotIn(
            str(live), chs.get(relay_watchdog.PENDING_TRANSCRIPTS_KEY, [])
        )

        successor = self.proj_dir / "aaaaaaaa-1111-2222-3333-444444444444.jsonl"
        successor.write_text(
            self._5190_record(anchor + 1950, "successor block undelivered")
            + "\n",
            encoding="utf-8",
        )
        os.utime(successor, (anchor + 2000, anchor + 2000))
        tick_channel(rt, TICK_CHANNEL, state, anchor + 2000)
        self.assertEqual(chs[SELECTED_TRANSCRIPT_KEY], str(successor))
        self.assertIn(
            str(successor), chs.get(relay_watchdog.PENDING_TRANSCRIPTS_KEY, [])
        )

        quiet_at = anchor + 2000 + rt.cfg.idle_quiet_secs + 60
        before = len(rt.alerts)
        tick_channel(rt, TICK_CHANNEL, state, quiet_at)
        return rt, state, chs, live, before

    def test_5190_r6_a_tick_with_no_active_session_never_claims_one(self):
        """No selection means no claim about selection (#5190 R3 P2-F).

        `not_selected` was `bool(selected_path) and path != selected_path`, so a
        tick that selected nothing collapsed into the "it IS the selection" arm
        and rendered "대상 세션: X (현재 활성 세션)" on a channel that had no
        active session at all. It is the mirror image of the overclaim R5
        removed: there an unproven pastness was asserted, here an unproven
        presentness.
        """
        rt, _state, chs, live, before = self._5190_no_selection_tick()

        # The precondition this test exists to exercise: nothing is selected.
        self.assertIsNone(chs.get(SELECTED_TRANSCRIPT_KEY))
        self.assertIn(
            str(live), chs.get(relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY, [])
        )
        gap_alerts = [
            body for body, _ in rt.alerts[before:] if "릴레이 갭 감지" in body
        ]
        self.assertEqual(len(gap_alerts), 1, (rt.alerts[before:], rt.log_lines))
        body = gap_alerts[0]
        self.assertIn(live.name, body)
        # The false claim, and the true statement that replaced it.
        self.assertNotIn("(현재 활성 세션)", body)
        self.assertIn("현재 활성 세션으로 판정된 트랜스크립트가 없습니다", body)
        self.assertIn("이번 점검으로 판정되지 않았습니다", body)
        # Pastness is not asserted either — non-selection cannot be read off a
        # tick that selected nothing.
        self.assertNotIn("고아 세션", body)
        self.assertNotIn("아니냐", body)
        # And the relay-health clause stays honest: nothing delivered here.
        self.assertIn("릴레이 장애 가능성이 남아 있습니다", body)

    def test_5190_unobserved_elapsed_time_never_renders_as_a_measurement(self):
        rt = self.gap_rt(github_repo="owner/repo")
        rt.watcher_probe = WatcherStateProbe(200, True, False)
        # A populated by-path delivery map with no entry for this transcript is
        # the production shape: the watchdog met the file after its deliveries
        # had already rolled out of the bounded Discord window.
        state = {
            "999": {
                "gap_since": self.now - rt.cfg.issue_after_secs - 1,
                relay_watchdog.LAST_ACTUAL_DELIVERY_BY_PATH_KEY: {
                    "/other/session.jsonl": self.now,
                },
            }
        }
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        body = rt.alerts[0][0]
        self.assertIn("릴레이 갭 감지", body)
        self.assertIn("배달이 확인된 이력이 없습니다", body)
        self.assertNotIn("999분", body)
        self.assertNotIn("**999**", body)
        # The sentinel must not reach the issue the watchdog files about itself.
        self.assertEqual(rt.issue_gap_mins, [None])

        channel = ChannelConfig(
            channel_id="555", sendmessage_key="k", worktree_root=WORKTREE_ROOT
        )
        calls: list[list[str]] = []

        def fake_run(argv, **kwargs):
            calls.append(list(argv))
            return subprocess.CompletedProcess(
                argv, 0, stdout="https://example.test/issues/9\n", stderr=""
            )

        with tempfile.TemporaryDirectory() as tmp:
            real = Runtime(
                Config(channels=(channel,), github_repo="owner/repo"), Path(tmp)
            )
            with mock.patch.object(
                relay_watchdog.subprocess, "run", side_effect=fake_run
            ):
                real.file_github_issue(channel, None, 2)

        # The runtime snapshot shells out first; find the `gh issue create`.
        create = next(argv for argv in calls if "--title" in argv)
        title = create[create.index("--title") + 1]
        self.assertNotIn("999", title)
        self.assertIn("no confirmed delivery on record", title)

    def test_invariant_4435_retiring_one_of_two_gap_owners_keeps_incident_clock(self):
        owner_a = self.proj_dir / "multi-gap-a.jsonl"
        owner_b = self.proj_dir / "multi-gap-b.jsonl"

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        owner_a.write_text(
            record(self.now - 2000, "multi gap A missing") + "\n",
            encoding="utf-8",
        )
        owner_b.write_text(
            record(self.now - 3000, "multi gap B missing") + "\n",
            encoding="utf-8",
        )
        os.utime(owner_a, (self.now, self.now))
        os.utime(owner_b, (self.now + 1, self.now + 1))
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(owner_a),
                "transcript_sizes": {
                    str(owner_a): owner_a.stat().st_size,
                    str(owner_b): owner_b.stat().st_size,
                },
                "transcript_seen_at": {
                    str(owner_a): self.now,
                    str(owner_b): self.now,
                },
                "transcript_known_at": {
                    str(owner_a): self.now,
                    str(owner_b): self.now,
                },
                "pending_transcripts": [str(owner_b)],
                "pending_transcript_since": {str(owner_b): self.now},
            }
        }
        rt = self.make_rt()
        rt.haystack = ""
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        chs = state["999"]
        self.assertEqual(chs[relay_watchdog.GAP_TRANSCRIPT_KEY], str(owner_b))
        self.assertEqual(
            set(chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY]),
            {str(owner_a), str(owner_b)},
        )
        original_gap_since = chs["gap_since"]
        chs["issue_url"] = "https://example.test/issues/multi-gap"
        chs["pending_transcript_since"][str(owner_b)] = (
            self.now - rt.cfg.idle_quiet_secs - 1
        )

        os.utime(owner_a, (self.now + 3, self.now + 3))
        tick_channel(rt, TICK_CHANNEL, state, self.now + 3)

        self.assertTrue(chs.get("alerting"))
        self.assertEqual(chs["gap_since"], original_gap_since)
        self.assertEqual(
            chs["issue_url"], "https://example.test/issues/multi-gap"
        )
        self.assertEqual(chs[relay_watchdog.GAP_TRANSCRIPT_KEY], str(owner_a))
        self.assertEqual(
            chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY], [str(owner_a)]
        )
        self.assertFalse(any("릴레이 갭 해소" in body for body, _ in rt.alerts))

    def test_invariant_4435_deleted_anchor_uses_watermark_over_touched_dead_path(self):
        deleted_anchor = self.proj_dir / "deleted-current.jsonl"
        safe = self.proj_dir / "safe-watermarked.jsonl"
        dead = self.proj_dir / "touched-dead.jsonl"
        unwatermarked = self.proj_dir / "unwatermarked-stale.jsonl"
        anchor = float(int(self.now - 2000))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        safe.write_text(record(anchor, "safe delivered anchor") + "\n", encoding="utf-8")
        dead.write_text(
            "\n".join(
                record(anchor - 1000 + index, f"historic missing block {index}")
                for index in range(490)
            )
            + "\n",
            encoding="utf-8",
        )
        unwatermarked.write_text("{}\n", encoding="utf-8")
        os.utime(safe, (self.now - 100, self.now - 100))
        os.utime(unwatermarked, (self.now - 1, self.now - 1))
        os.utime(dead, (self.now, self.now))
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(deleted_anchor),
                "transcript_sizes": {
                    str(deleted_anchor): 10,
                    str(safe): safe.stat().st_size,
                    str(dead): dead.stat().st_size,
                },
                "transcript_seen_at": {
                    str(deleted_anchor): self.now - 1,
                    str(safe): self.now - 1,
                    str(dead): self.now - 1,
                },
                "transcript_known_at": {
                    str(deleted_anchor): self.now - 1,
                    str(safe): self.now - 1,
                    str(dead): self.now - 1,
                },
            }
        }
        advance_delivered_watermark(state["999"], safe, anchor, self.now - 1)
        rt = self.make_rt()
        rt.haystack = ""

        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)

        self.assertEqual(rt.alerts, [])
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(safe))
        self.assertTrue(
            any(
                "transcript-select reason=watermark_anchor_recovery" in line
                for line in rt.log_lines
            )
        )

    def test_invariant_4435_selected_anchor_survives_many_delivered_debuts(self):
        current = self.proj_dir / "z-current.jsonl"
        anchor = float(int(self.now - 2000))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        current.write_text(record(anchor, "selected delivered") + "\n", encoding="utf-8")
        os.utime(current, (self.now, self.now))
        rt = self.make_rt()
        rt.haystack = norm("selected delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        delivered_texts = ["selected delivered", "selected growth delivered"]
        with current.open("a", encoding="utf-8") as stream:
            stream.write(record(anchor + 1, "selected growth delivered") + "\n")
        for index in range(relay_watchdog.MAX_PENDING_TRANSCRIPTS):
            path = self.proj_dir / f"a-{index:02d}.jsonl"
            text = f"delivered debut {index}"
            path.write_text(
                record(self.now - 1900 + index, text) + "\n", encoding="utf-8"
            )
            os.utime(path, (self.now + 1, self.now + 1))
            delivered_texts.append(text)
        os.utime(current, (self.now + 2, self.now + 2))
        rt.haystack = norm(" ".join(delivered_texts))

        tick_channel(rt, TICK_CHANNEL, state, self.now + 3)

        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(current))
        self.assertEqual(
            delivered_watermark_for_path(state["999"], current), anchor + 1
        )
        self.assertEqual(
            len(delivered_watermarks(state["999"])),
            relay_watchdog.MAX_PENDING_TRANSCRIPTS + 1,
        )

        rt.haystack = ""
        tick_channel(rt, TICK_CHANNEL, state, self.now + 4)
        self.assertEqual(
            len([body for body, _ in rt.alerts if "릴레이 갭 감지" in body]), 0
        )

    def test_invariant_4435_full_cap_gap_owner_recovery_survives_haystack_rolloff(self):
        anchor = float(int(self.now - 2000))
        selected = self.proj_dir / "selected-empty.jsonl"
        pending = [
            self.proj_dir / f"pending-empty-{index:02d}.jsonl"
            for index in range(relay_watchdog.MAX_PENDING_TRANSCRIPTS)
        ]
        target = self.proj_dir / "zz-recovered-gap-owner.jsonl"
        other_owners = [
            self.proj_dir / f"gap-owner-{index:02d}.jsonl"
            for index in range(relay_watchdog.MAX_PENDING_TRANSCRIPTS)
        ]
        gap_owners = [target, *other_owners]

        def record(text: str, epoch: float = anchor) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        selected.write_text("{}\n", encoding="utf-8")
        for path in pending:
            path.write_text("{}\n", encoding="utf-8")
        owner_texts: dict[str, str] = {}
        for index, path in enumerate(gap_owners):
            text = f"delivered full-cap gap owner {index:02d}"
            owner_texts[str(path)] = text
            path.write_text(record(text) + "\n", encoding="utf-8")
        for path in [selected, *pending, *other_owners]:
            os.utime(path, (self.now, self.now))
        # Keep the target in bounded transcript history while the 66 authority
        # paths are present; it is the sole file left for the rolloff tick.
        os.utime(target, (self.now + 1, self.now + 1))

        paths = [selected, *pending, *gap_owners]
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(selected),
                relay_watchdog.TRANSCRIPT_SIZES_KEY: {
                    str(path): path.stat().st_size for path in paths
                },
                relay_watchdog.TRANSCRIPT_SEEN_AT_KEY: {
                    str(path): self.now for path in paths
                },
                relay_watchdog.TRANSCRIPT_KNOWN_AT_KEY: {
                    str(path): self.now for path in paths
                },
                relay_watchdog.PENDING_TRANSCRIPTS_KEY: [
                    str(path) for path in pending
                ],
                relay_watchdog.PENDING_TRANSCRIPT_SINCE_KEY: {
                    str(path): self.now for path in pending
                },
                relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY: [
                    str(path) for path in gap_owners
                ],
                relay_watchdog.GAP_TRANSCRIPT_KEY: str(target),
                DELIVERED_WATERMARKS_KEY: {
                    str(path): {
                        "delivered_ts": anchor - 1,
                        "updated_at": self.now,
                    }
                    for path in [selected, *pending]
                }
                | {
                    f"/unrelated-newer-{index:03d}.jsonl": {
                        "delivered_ts": anchor - 1,
                        "updated_at": self.now + 100,
                    }
                    for index in range(
                        relay_watchdog.MAX_RECOVERED_GAP_GUARDS
                    )
                },
                "alerting": True,
                "gap_since": self.now - 2000,
                "last_alert": self.now - 10_000,
            }
        }
        rt = self.make_rt()
        rt.haystack = norm(" ".join(owner_texts.values()))
        state_path = self.root / "post-recovery-state.json"

        def restart(current_state: dict) -> dict:
            save_state(state_path, current_state)
            return load_state(state_path)

        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        state = restart(state)
        chs = state["999"]

        self.assertFalse(chs.get("alerting", False))
        self.assertNotIn(relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY, chs)
        self.assertEqual(
            delivered_watermark_for_path(chs, target),
            anchor,
            "a recovered GAP owner must retain its delivery authority at full cap",
        )
        self.assertEqual(
            len(delivered_watermarks(chs)), MAX_DELIVERED_WATERMARKS
        )

        growth_text = "delivered growth after GAP recovery"
        with target.open("a", encoding="utf-8") as stream:
            stream.write(record(growth_text, anchor + 1) + "\n")
        os.utime(target, (self.now + 3, self.now + 3))
        rt.haystack = norm(growth_text)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 3)
        state = restart(state)
        chs = state["999"]
        self.assertEqual(delivered_watermark_for_path(chs, target), anchor + 1)
        self.assertEqual(
            relay_watchdog._validated_recovered_gap_guards(chs)[str(target)][0],
            target.stat().st_size,
        )

        # Recovery clears GAP-owner state. Exercise the actual review finding:
        # two later full debut waves create enough newer watermarks to evict an
        # ex-owner unless recovery transferred it to durable replay authority.
        pressure_paths: list[Path] = []
        for wave in range(2):
            wave_texts: list[str] = []
            for index in range(relay_watchdog.MAX_PENDING_TRANSCRIPTS + 1):
                path = self.proj_dir / f"wave-{wave}-{index:02d}.jsonl"
                text = f"delivered pressure wave {wave} item {index:02d}"
                path.write_text(record(text) + "\n", encoding="utf-8")
                os.utime(path, (self.now + 4 + wave, self.now + 4 + wave))
                pressure_paths.append(path)
                wave_texts.append(text)
            rt.haystack = norm(" ".join(wave_texts))
            tick_channel(rt, TICK_CHANNEL, state, self.now + 4 + wave)
            state = restart(state)
            chs = state["999"]
            self.assertEqual(
                delivered_watermark_for_path(chs, target),
                anchor + 1,
                f"post-recovery pressure wave {wave} evicted replay authority",
            )

        for path in [selected, *pending, *other_owners, *pressure_paths]:
            path.unlink()
        rt.haystack = ""  # bounded Discord history has rolled off the delivery
        prior_gap_alerts = len(
            [body for body, _ in rt.alerts if "릴레이 갭 감지" in body]
        )

        tick_channel(rt, TICK_CHANNEL, state, self.now + 6)

        self.assertEqual(chs[SELECTED_TRANSCRIPT_KEY], str(target))
        self.assertEqual(
            len([body for body, _ in rt.alerts if "릴레이 갭 감지" in body]),
            prior_gap_alerts,
            "haystack rolloff must not re-alert a delivery already confirmed",
        )
        self.assertFalse(chs.get("alerting", False))

    def test_invariant_4435_recovered_guard_absence_ttl_starts_new_lifecycle(self):
        anchor = float(int(self.now - 2000))
        target = self.proj_dir / "absent-recovered.jsonl"
        target.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(anchor)
                    ),
                    "message": {
                        "content": [{"type": "text", "text": "old lifecycle"}]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        path = str(target)
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: path,
                relay_watchdog.TRANSCRIPT_SIZES_KEY: {path: target.stat().st_size},
                relay_watchdog.TRANSCRIPT_SEEN_AT_KEY: {path: self.now},
                relay_watchdog.TRANSCRIPT_KNOWN_AT_KEY: {path: self.now},
                relay_watchdog.RECOVERED_GAP_GUARDS_KEY: {
                    path: {
                        "size": target.stat().st_size,
                        "confirmed_at": self.now,
                        "last_seen_at": self.now,
                        "absent_since": None,
                    }
                },
                DELIVERED_WATERMARKS_KEY: {
                    path: {"delivered_ts": anchor, "updated_at": self.now}
                },
                relay_watchdog.ORPHAN_STRANDED_SINCE_KEY: {
                    path: self.now
                },
            }
        }
        state_path = self.root / "absence-lifecycle.json"

        def restart(current_state: dict) -> dict:
            save_state(state_path, current_state)
            return load_state(state_path)

        target.unlink()
        rt = self.make_rt()
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        state = restart(state)
        guard = relay_watchdog._validated_recovered_gap_guards(state["999"])[path]
        self.assertEqual(guard[3], self.now + 1)

        before_expiry = (
            self.now + 1 + relay_watchdog.RECOVERED_GAP_GUARD_TTL_SECS - 1
        )
        tick_channel(rt, TICK_CHANNEL, state, before_expiry)
        state = restart(state)
        self.assertIn(
            path, relay_watchdog._validated_recovered_gap_guards(state["999"])
        )
        self.assertEqual(delivered_watermark_for_path(state["999"], path), anchor)

        after_expiry = before_expiry + 2
        tick_channel(rt, TICK_CHANNEL, state, after_expiry)
        state = restart(state)
        chs = state["999"]
        self.assertNotIn(
            path, relay_watchdog._validated_recovered_gap_guards(chs)
        )
        self.assertEqual(delivered_watermark_for_path(chs, path), 0.0)
        self.assertNotIn(path, chs.get(relay_watchdog.TRANSCRIPT_SIZES_KEY, {}))
        self.assertNotIn(
            path, chs.get(relay_watchdog.ORPHAN_STRANDED_SINCE_KEY, {})
        )
        self.assertTrue(
            any("recovered-gap-guard-reclaimed" in line for line in rt.log_lines)
        )

        # Reuse after the proven-absence boundary is a new path lifecycle. The
        # old delivery floor must not authenticate its stale content.
        target.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(anchor)
                    ),
                    "message": {
                        "content": [{"type": "text", "text": "old lifecycle"}]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        os.utime(target, (after_expiry + 1, after_expiry + 1))
        rt.haystack = ""
        tick_channel(rt, TICK_CHANNEL, state, after_expiry + 1)
        self.assertTrue(state["999"].get("alerting"))
        self.assertEqual(state["999"][relay_watchdog.GAP_TRANSCRIPT_KEY], path)

    def test_invariant_4435_present_recovered_guard_does_not_expire(self):
        anchor = float(int(self.now - 2000))
        selected = self.proj_dir / "present-selected.jsonl"
        target = self.proj_dir / "present-recovered.jsonl"
        selected.write_text("{}\n", encoding="utf-8")
        target.write_text("{}\n", encoding="utf-8")
        target_path = str(target)
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(selected),
                relay_watchdog.TRANSCRIPT_SIZES_KEY: {
                    str(selected): selected.stat().st_size,
                    target_path: target.stat().st_size,
                },
                relay_watchdog.RECOVERED_GAP_GUARDS_KEY: {
                    target_path: {
                        "size": target.stat().st_size,
                        "confirmed_at": self.now,
                        "last_seen_at": self.now,
                        "absent_since": None,
                    }
                },
                DELIVERED_WATERMARKS_KEY: {
                    target_path: {
                        "delivered_ts": anchor,
                        "updated_at": self.now,
                    }
                },
            }
        }
        state_path = self.root / "present-lifecycle.json"
        save_state(state_path, state)
        state = load_state(state_path)
        tick_at = self.now + relay_watchdog.RECOVERED_GAP_GUARD_TTL_SECS + 1

        tick_channel(self.make_rt(), TICK_CHANNEL, state, tick_at)

        guard = relay_watchdog._validated_recovered_gap_guards(state["999"])[
            target_path
        ]
        self.assertEqual(guard[1], self.now)
        self.assertEqual(guard[2], tick_at)
        self.assertIsNone(guard[3])
        self.assertEqual(
            delivered_watermark_for_path(state["999"], target_path), anchor
        )

    def test_invariant_4435_ancestor_symlink_is_ambiguous_not_absent(self):
        target = self.proj_dir / "hidden-recovered.jsonl"
        target.write_text("{}\n", encoding="utf-8")
        path = str(target)
        state = {
            "999": {
                relay_watchdog.RECOVERED_GAP_GUARDS_KEY: {
                    path: {
                        "size": target.stat().st_size,
                        "confirmed_at": self.now,
                        "last_seen_at": self.now,
                        "absent_since": None,
                    }
                },
                DELIVERED_WATERMARKS_KEY: {
                    path: {
                        "delivered_ts": self.now - 2000,
                        "updated_at": self.now,
                    }
                },
            }
        }
        hidden = self.projects / "hidden-real-project"
        decoy = self.projects / "decoy-project"
        self.proj_dir.rename(hidden)
        decoy.mkdir()
        self.proj_dir.symlink_to(decoy, target_is_directory=True)
        state_path = self.root / "symlink-lifecycle.json"
        rt = self.make_rt()

        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        save_state(state_path, state)
        state = load_state(state_path)
        tick_channel(
            rt,
            TICK_CHANNEL,
            state,
            self.now + relay_watchdog.RECOVERED_GAP_GUARD_TTL_SECS + 2,
        )

        guard = relay_watchdog._validated_recovered_gap_guards(state["999"])[path]
        self.assertIsNone(guard[3])
        self.assertGreater(
            delivered_watermark_for_path(state["999"], path), 0.0
        )
        self.assertFalse(
            any("recovered-gap-guard-reclaimed" in line for line in rt.log_lines)
        )

    def test_invariant_4435_guard_directory_fstat_errors_keep_floor_and_tick_running(self):
        selected = self.proj_dir / "fstat-selected.jsonl"
        selected.write_text("{}\n", encoding="utf-8")
        hidden_project = self.projects / (
            "-Users-alice--adk-release-worktrees-claude-adk-cc-20260709-150500"
        )
        hidden_project.mkdir()
        target = hidden_project / "fstat-hidden-recovered.jsonl"
        target.write_text("{}\n", encoding="utf-8")
        path = str(target)
        real_fstat = relay_watchdog.os.fstat

        for failing_directory_index in (1, 2):
            with self.subTest(failing_directory_index=failing_directory_index):
                state = {
                    "999": {
                        relay_watchdog.RECOVERED_GAP_GUARDS_KEY: {
                            path: {
                                "size": target.stat().st_size,
                                "confirmed_at": self.now,
                                "last_seen_at": self.now,
                                "absent_since": None,
                            }
                        },
                        DELIVERED_WATERMARKS_KEY: {
                            path: {
                                "delivered_ts": self.now - 2000,
                                "updated_at": self.now,
                            }
                        },
                    }
                }
                directory_calls = 0

                def fail_selected_directory_fstat(descriptor):
                    nonlocal directory_calls
                    opened = real_fstat(descriptor)
                    if stat.S_ISDIR(opened.st_mode):
                        directory_calls += 1
                        if directory_calls == failing_directory_index:
                            raise OSError("injected recovered-guard directory fstat failure")
                    return opened

                with (
                    mock.patch.object(
                        relay_watchdog,
                        "channel_project_dirs",
                        return_value=[self.proj_dir],
                    ),
                    mock.patch.object(
                        relay_watchdog.os,
                        "fstat",
                        side_effect=fail_selected_directory_fstat,
                    ),
                ):
                    tick_channel(self.make_rt(), TICK_CHANNEL, state, self.now + 1)

                chs = state["999"]
                guard = relay_watchdog._validated_recovered_gap_guards(chs)[path]
                self.assertGreaterEqual(directory_calls, failing_directory_index)
                self.assertIsNone(guard[3])
                self.assertGreater(delivered_watermark_for_path(chs, target), 0.0)
                self.assertEqual(chs[SELECTED_TRANSCRIPT_KEY], str(selected))

    def test_invariant_4435_unchanged_guards_are_not_parsed_and_metadata_settles(self):
        selected = self.proj_dir / "guard-perf-selected.jsonl"
        selected.write_text("{}\n", encoding="utf-8")
        guards: dict[str, dict] = {}
        sizes = {str(selected): selected.stat().st_size}
        watermarks: dict[str, dict] = {}
        paths: list[Path] = []
        for index in range(relay_watchdog.MAX_RECOVERED_GAP_GUARDS):
            path = self.proj_dir / f"guard-perf-{index:03d}.jsonl"
            path.write_text("{}\n", encoding="utf-8")
            paths.append(path)
            sizes[str(path)] = path.stat().st_size
            guards[str(path)] = {
                "size": path.stat().st_size,
                "confirmed_at": self.now,
                "last_seen_at": self.now,
                "absent_since": None,
            }
            watermarks[str(path)] = {
                "delivered_ts": self.now - 2000,
                "updated_at": self.now,
            }
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(selected),
                relay_watchdog.TRANSCRIPT_SIZES_KEY: sizes,
                relay_watchdog.RECOVERED_GAP_GUARDS_KEY: guards,
                DELIVERED_WATERMARKS_KEY: watermarks,
            }
        }
        rt = self.make_rt()

        with mock.patch.object(
            relay_watchdog,
            "assistant_blocks",
            wraps=relay_watchdog.assistant_blocks,
        ) as parse:
            tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        self.assertEqual(
            [str(call.args[0]) for call in parse.call_args_list], [str(selected)]
        )

        target = paths[0]
        with target.open("a", encoding="utf-8") as stream:
            stream.write('{"type":"progress","message":"metadata only"}\n')
        os.utime(target, (self.now + 2, self.now + 2))
        with mock.patch.object(
            relay_watchdog,
            "assistant_blocks",
            wraps=relay_watchdog.assistant_blocks,
        ) as parse:
            tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        parsed = [str(call.args[0]) for call in parse.call_args_list]
        self.assertEqual(parsed.count(str(target)), 1)
        self.assertEqual(parsed.count(str(selected)), 1)
        guard = relay_watchdog._validated_recovered_gap_guards(state["999"])[
            str(target)
        ]
        self.assertEqual(guard[0], target.stat().st_size)
        self.assertEqual(guard[1], self.now)

        state_path = self.root / "guard-perf-state.json"
        save_state(state_path, state)
        state = load_state(state_path)
        with mock.patch.object(
            relay_watchdog,
            "assistant_blocks",
            wraps=relay_watchdog.assistant_blocks,
        ) as parse:
            tick_channel(rt, TICK_CHANNEL, state, self.now + 3)
        self.assertEqual(
            [str(call.args[0]) for call in parse.call_args_list], [str(selected)]
        )

    def test_invariant_4435_simultaneous_guard_growth_checks_every_growing_path(self):
        anchor = float(int(self.now - 4000))
        selected = self.proj_dir / "multi-growth-selected.jsonl"
        delivered_path = self.proj_dir / "multi-growth-delivered.jsonl"
        missing_path = self.proj_dir / "multi-growth-missing.jsonl"

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        selected.write_text("{}\n", encoding="utf-8")
        delivered_path.write_text(
            record(anchor, "old delivered A") + "\n", encoding="utf-8"
        )
        missing_path.write_text(
            record(anchor, "old delivered B") + "\n", encoding="utf-8"
        )
        old_sizes = {
            str(delivered_path): delivered_path.stat().st_size,
            str(missing_path): missing_path.stat().st_size,
        }
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(selected),
                relay_watchdog.TRANSCRIPT_SIZES_KEY: {
                    str(selected): selected.stat().st_size,
                    **old_sizes,
                },
                relay_watchdog.RECOVERED_GAP_GUARDS_KEY: {
                    path: {
                        "size": old_sizes[path],
                        "confirmed_at": self.now - 100,
                        "last_seen_at": self.now - 100,
                        "absent_since": None,
                    }
                    for path in old_sizes
                },
                DELIVERED_WATERMARKS_KEY: {
                    path: {
                        "delivered_ts": anchor,
                        "updated_at": self.now - 100,
                    }
                    for path in old_sizes
                },
            }
        }
        delivered_epoch = float(int(self.now - 2000))
        missing_epoch = float(int(self.now))
        with delivered_path.open("a", encoding="utf-8") as stream:
            stream.write(record(delivered_epoch, "new delivered A") + "\n")
        with missing_path.open("a", encoding="utf-8") as stream:
            stream.write(record(missing_epoch, "new missing B") + "\n")
        os.utime(delivered_path, (self.now, self.now))
        os.utime(missing_path, (self.now, self.now))
        rt = self.make_rt()
        rt.haystack = norm("new delivered A")

        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        state_path = self.root / "multi-growth-state.json"
        save_state(state_path, state)
        state = load_state(state_path)
        chs = state["999"]
        guards = relay_watchdog._validated_recovered_gap_guards(chs)
        self.assertEqual(guards[str(delivered_path)][0], delivered_path.stat().st_size)
        self.assertEqual(guards[str(missing_path)][0], old_sizes[str(missing_path)])
        self.assertEqual(
            delivered_watermark_for_path(chs, delivered_path), delivered_epoch
        )
        self.assertEqual(delivered_watermark_for_path(chs, missing_path), anchor)
        self.assertNotIn(relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY, chs)
        self.assertFalse(chs.get("alerting", False))

        stale_tick = self.now + rt.cfg.gap_alert_secs + 2
        rt.haystack = ""
        tick_channel(rt, TICK_CHANNEL, state, stale_tick)
        save_state(state_path, state)
        state = load_state(state_path)
        chs = state["999"]
        self.assertIn(
            str(missing_path), chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY]
        )
        self.assertTrue(chs.get("alerting"))

        rt.haystack = norm("new missing B")
        rt.watcher_probe = WatcherStateProbe(200, True, False)
        tick_channel(rt, TICK_CHANNEL, state, stale_tick + 1)
        chs = state["999"]
        self.assertFalse(chs.get("alerting", False))
        self.assertNotIn(relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY, chs)
        self.assertEqual(
            relay_watchdog._validated_recovered_gap_guards(chs)[str(missing_path)][
                0
            ],
            missing_path.stat().st_size,
        )

    def test_invariant_4435_multi_owner_partial_recovery_keeps_incident_open(self):
        anchor = float(int(self.now - 2000))
        owner_a = self.proj_dir / "partial-owner-a.jsonl"
        owner_b = self.proj_dir / "partial-owner-b.jsonl"

        def record(text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(anchor)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        owner_a.write_text(record("partial delivered A") + "\n", encoding="utf-8")
        owner_b.write_text(record("partial missing B") + "\n", encoding="utf-8")
        owners = [str(owner_a), str(owner_b)]
        original_gap_since = self.now - 5000
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(owner_a),
                relay_watchdog.TRANSCRIPT_SIZES_KEY: {
                    str(owner_a): owner_a.stat().st_size,
                    str(owner_b): owner_b.stat().st_size,
                },
                relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY: owners,
                relay_watchdog.GAP_TRANSCRIPT_KEY: str(owner_a),
                DELIVERED_WATERMARKS_KEY: {
                    path: {
                        "delivered_ts": anchor - 1,
                        "updated_at": self.now - 100,
                    }
                    for path in owners
                },
                "alerting": True,
                "gap_since": original_gap_since,
                "last_alert": self.now,
            }
        }
        rt = self.make_rt()
        rt.haystack = norm("partial delivered A")

        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        state_path = self.root / "partial-recovery-state.json"
        save_state(state_path, state)
        state = load_state(state_path)
        chs = state["999"]
        self.assertIn(
            str(owner_a), relay_watchdog._validated_recovered_gap_guards(chs)
        )
        self.assertNotIn(
            str(owner_a), chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY]
        )
        self.assertIn(str(owner_b), chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY])
        self.assertTrue(chs.get("alerting"))
        self.assertEqual(chs["gap_since"], original_gap_since)
        self.assertFalse(any("릴레이 갭 해소" in body for body, _ in rt.alerts))

    def test_invariant_4435_sequential_recovery_churn_exceeds_one_gap_wave(self):
        anchor = float(int(self.now - 2000))

        def record(text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(anchor)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        state: dict = {"999": {}}
        state_path = self.root / "sequential-recovery-state.json"
        rt = self.make_rt()
        recovery_count = relay_watchdog.MAX_GAP_OWNER_TRANSCRIPTS + 1
        for index in range(recovery_count):
            path = self.proj_dir / f"sequential-recovered-{index:03d}.jsonl"
            text = f"sequential delivered recovery {index:03d}"
            path.write_text(record(text) + "\n", encoding="utf-8")
            tick_at = self.now + index + 1
            os.utime(path, (tick_at, tick_at))
            chs = state["999"]
            chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY] = [str(path)]
            chs[relay_watchdog.GAP_TRANSCRIPT_KEY] = str(path)
            chs["alerting"] = True
            chs["gap_since"] = self.now - 5000
            chs["last_alert"] = self.now
            rt.haystack = norm(text)

            tick_channel(rt, TICK_CHANNEL, state, tick_at)
            save_state(state_path, state)
            state = load_state(state_path)
            guards = relay_watchdog._validated_recovered_gap_guards(state["999"])
            self.assertIn(str(path), guards)
            self.assertEqual(len(guards), index + 1)
            self.assertFalse(state["999"].get("alerting", False))

        self.assertGreater(
            len(relay_watchdog._validated_recovered_gap_guards(state["999"])),
            relay_watchdog.MAX_GAP_OWNER_TRANSCRIPTS,
        )
        self.assertFalse(
            any("recovered-gap-guard-capacity-blocked" in line for line in rt.log_lines)
        )

    def test_invariant_4435_full_recovered_guard_store_blocks_false_recovery(self):
        anchor = float(int(self.now - 2000))
        owner = self.proj_dir / "full-store-owner.jsonl"
        owner.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(anchor)
                    ),
                    "message": {
                        "content": [{"type": "text", "text": "owner delivered"}]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        guards = {
            str(self.proj_dir / f"absent-full-guard-{index:03d}.jsonl"): {
                "size": 1,
                "confirmed_at": self.now,
                "last_seen_at": self.now,
                "absent_since": None,
            }
            for index in range(relay_watchdog.MAX_RECOVERED_GAP_GUARDS)
        }
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: str(owner),
                relay_watchdog.TRANSCRIPT_SIZES_KEY: {
                    str(owner): owner.stat().st_size
                },
                relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY: [str(owner)],
                relay_watchdog.GAP_TRANSCRIPT_KEY: str(owner),
                relay_watchdog.RECOVERED_GAP_GUARDS_KEY: guards,
                DELIVERED_WATERMARKS_KEY: {
                    str(owner): {
                        "delivered_ts": anchor - 1,
                        "updated_at": self.now,
                    }
                },
                "alerting": True,
                "gap_since": self.now - 5000,
                "last_alert": self.now,
            }
        }
        rt = self.make_rt()
        rt.haystack = norm("owner delivered")

        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        state_path = self.root / "full-store-state.json"
        save_state(state_path, state)
        state = load_state(state_path)
        chs = state["999"]

        self.assertEqual(
            len(relay_watchdog._validated_recovered_gap_guards(chs)),
            relay_watchdog.MAX_RECOVERED_GAP_GUARDS,
        )
        self.assertIn(str(owner), chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY])
        self.assertTrue(chs.get("alerting"))
        self.assertTrue(
            any("recovered-gap-guard-capacity-blocked" in line for line in rt.log_lines)
        )
        self.assertFalse(any("릴레이 갭 해소" in body for body, _ in rt.alerts))

    def test_invariant_4435_invalid_persisted_selection_is_ignored_before_fallback(
        self,
    ):
        current = self.proj_dir / "current.jsonl"
        touched_old = self.proj_dir / "old.jsonl"
        anchor = float(int(self.now - 2000))
        current.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(anchor)
                    ),
                    "message": {
                        "content": [{"type": "text", "text": "confirmed current"}]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        touched_old.write_text(
            current.read_text(encoding="utf-8").replace(
                "confirmed current", "historic missing output"
            ),
            encoding="utf-8",
        )
        os.utime(current, (self.now - 100, self.now - 100))
        os.utime(touched_old, (self.now, self.now))
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: "relative/stale-selection.jsonl",
                "transcript_sizes": {
                    str(current): current.stat().st_size,
                    str(touched_old): touched_old.stat().st_size,
                },
            }
        }
        advance_delivered_watermark(state["999"], current, anchor, self.now - 1)

        rt = self.make_rt()
        rt.haystack = norm("confirmed current historic missing output")
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        self.assertEqual(rt.alerts, [])
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(touched_old))
        self.assertTrue(
            any(
                "transcript-select reason=bootstrap" in line
                for line in rt.log_lines
            )
        )

    def test_invariant_4435_partial_watermark_coverage_cannot_hide_newer_gap(self):
        delivered_old = self.proj_dir / "delivered-old.jsonl"
        missed_newer = self.proj_dir / "missed-newer.jsonl"
        old_anchor = float(int(self.now - 3000))
        missed_ts = float(int(self.now - 2000))

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        delivered_old.write_text(
            record(old_anchor, "delivered old") + "\n", encoding="utf-8"
        )
        missed_newer.write_text(
            record(missed_ts, "stale missed final block") + "\n", encoding="utf-8"
        )
        os.utime(delivered_old, (self.now - 100, self.now - 100))
        os.utime(missed_newer, (self.now, self.now))
        state = {
            "999": {
                SELECTED_TRANSCRIPT_KEY: "relative/corrupt-selection.jsonl",
                "transcript_sizes": {
                    str(delivered_old): delivered_old.stat().st_size,
                    str(missed_newer): missed_newer.stat().st_size,
                },
            }
        }
        advance_delivered_watermark(
            state["999"], delivered_old, old_anchor, self.now - 1
        )
        rt = self.make_rt()
        rt.haystack = norm("delivered old")

        tick_channel(rt, TICK_CHANNEL, state, self.now)

        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("릴레이 갭 감지", rt.alerts[0][0])
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(missed_newer))
        self.assertTrue(
            any("transcript-select reason=bootstrap" in line for line in rt.log_lines)
        )

    def test_invariant_4435_older_haystack_cannot_regress_persisted_anchor(self):
        older = float(int(self.now - 3000))
        newer = float(int(self.now - 1000))
        self.write_transcript(
            [(older, "older delivered text"), (newer, "newer delivered text")]
        )
        transcript = self.proj_dir / "s.jsonl"
        rt = self.make_rt()
        state: dict = {}

        rt.haystack = norm("newer delivered text")
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(
            delivered_watermark_for_path(state["999"], transcript), newer
        )

        rt.haystack = norm("older delivered text")
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        self.assertEqual(rt.alerts, [])
        self.assertEqual(
            delivered_watermark_for_path(state["999"], transcript), newer
        )

    def test_invariant_4435_unseen_newer_transcript_wins_without_growth_baseline(self):
        old_anchor = float(int(self.now - 1000))
        self.write_transcript([(old_anchor, "old path delivered")])
        old_transcript = self.proj_dir / "s.jsonl"
        rt = self.make_rt()
        rt.haystack = norm("old path delivered")
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        new_transcript = self.proj_dir / "new.jsonl"
        new_ts = float(int(self.now - 2000))
        new_transcript.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(new_ts)
                    ),
                    "message": {
                        "content": [{"type": "text", "text": "new path missing"}]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        os.utime(new_transcript, (self.now + 1, self.now + 1))
        rt.haystack = ""
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("릴레이 갭 감지", rt.alerts[0][0])
        self.assertEqual(state["999"][SELECTED_TRANSCRIPT_KEY], str(new_transcript))
        self.assertTrue(
            any("transcript-select reason=unseen_newer" in line for line in rt.log_lines)
        )
        self.assertEqual(
            delivered_watermark_for_path(state["999"], old_transcript), old_anchor
        )
        self.assertEqual(delivered_watermark_for_path(state["999"], new_transcript), 0.0)

    # (a) deploy-window suppression — REAL in_deploy_window runs against a real
    # marker file, so replacing it with `return False` fails this test.
    def test_fresh_deploy_marker_suppresses_gap_alert(self):
        rt = self.gap_rt()
        # Positive control first: without a marker the same scenario alerts.
        tick_channel(rt, TICK_CHANNEL, {}, self.now)
        self.assertEqual(len(rt.alerts), 1, "control: gap must alert sans marker")

        rt2 = self.gap_rt()
        rt2.deploy_marker.touch()
        state: dict = {}
        tick_channel(rt2, TICK_CHANNEL, state, self.now)
        self.assertEqual(rt2.alerts, [], "fresh deploy marker must suppress alerts")
        self.assertTrue(any("deploy window" in l for l in rt2.log_lines))
        self.assertNotIn("last_alert", state.get("999", {}))

    def test_stale_deploy_marker_does_not_suppress(self):
        rt = self.gap_rt()
        rt.deploy_marker.touch()
        old = self.now - rt.cfg.deploy_quiet_secs - 1
        os.utime(rt.deploy_marker, (old, old))
        tick_channel(rt, TICK_CHANNEL, {}, self.now)
        self.assertEqual(len(rt.alerts), 1)

    # (b) cooldown / re-alert boundary
    def test_cooldown_suppresses_realert_until_boundary(self):
        rt = self.gap_rt()
        state = {"999": {"last_alert": self.now - (rt.cfg.realert_secs - 1)}}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(rt.alerts, [])
        self.assertTrue(any("cooldown" in l for l in rt.log_lines))

        rt2 = self.gap_rt()
        state2 = {"999": {"last_alert": self.now - rt2.cfg.realert_secs}}
        tick_channel(rt2, TICK_CHANNEL, state2, self.now)
        self.assertEqual(len(rt2.alerts), 1)
        self.assertEqual(state2["999"]["last_alert"], self.now)
        self.assertTrue(state2["999"]["alerting"])

    # (c) recovery auto-clear
    def test_recovery_sends_notice_and_clears_alert_state(self):
        self.write_transcript([(self.now - 2000, "landed fine in discord")])
        rt = self.make_rt()
        rt.haystack = norm("landed fine in discord")
        state = {
            "999": {
                "alerting": True,
                "gap_since": self.now - 3000,
                "issue_url": "https://example.test/issues/7",
                "last_alert": self.now - 60,
            }
        }
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(len(rt.alerts), 1)
        body, trigger_turn = rt.alerts[0]
        self.assertIn("해소", body)
        self.assertIn("https://example.test/issues/7", body)
        self.assertFalse(trigger_turn, "recovery notice must not trigger a turn")
        for cleared in ("alerting", "gap_since", "issue_url"):
            self.assertNotIn(cleared, state["999"])

    def test_ok_without_prior_alert_sends_nothing(self):
        self.write_transcript([(self.now - 2000, "landed fine in discord")])
        rt = self.make_rt()
        rt.haystack = norm("landed fine in discord")
        tick_channel(rt, TICK_CHANNEL, {}, self.now)
        self.assertEqual(rt.alerts, [])

    def append_transcript_block(self, epoch: float, text: str) -> None:
        transcript = self.proj_dir / "s.jsonl"
        ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch))
        record_uuid = f"00000000-0000-4000-9000-{transcript.stat().st_size:012d}"
        with transcript.open("a", encoding="utf-8") as stream:
            stream.write(
                json.dumps(
                    {
                        "type": "assistant",
                        "uuid": record_uuid,
                        "timestamp": ts,
                        "message": {"content": [{"type": "text", "text": text}]},
                    }
                )
                + "\n"
            )

    def drive_distinct_delivery_advances(
        self,
        rt: FakeRuntime,
        state: dict,
        missing: tuple[float, str],
        first: tuple[float, str],
        second: tuple[float, str],
    ) -> None:
        self.write_transcript([missing, first])
        rt.haystack = norm(first[1])
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.append_transcript_block(*second)
        rt.haystack = norm(f"{first[1]} {second[1]}")
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)

    def test_bounded_window_rolloff_never_creates_tombstone_evidence(self):
        old = (self.now - 3000, "delivered response rolled out of limit 100")
        newer = (self.now - 2000, "newer delivered response still in window")
        self.write_transcript([old, newer])
        rt = self.make_rt()
        state: dict = {}

        rt.haystack = norm(f"{old[1]} {newer[1]}")
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        rt.haystack = norm(newer[1])
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        chs = state["999"]
        self.assertEqual(permanent_loss_total(chs), 0)
        self.assertNotIn(relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY, chs)
        self.assertEqual(rt.alerts, [])

    def test_single_frontier_skip_surfaces_suspected_loss_without_confirming(self):
        missing = (self.now - 3000, "missing response needs new evidence")
        successor = (self.now - 2000, "only delivered successor")
        self.write_transcript([missing, successor])
        rt = self.make_rt()
        rt.haystack = norm(successor[1])
        state: dict = {}

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        chs = state["999"]
        self.assertEqual(permanent_loss_total(chs), 0)
        self.assertEqual(chs[relay_watchdog.PERMANENT_LOSS_SUSPECTED_KEY], 1)
        self.assertTrue(
            any("permanent-loss-suspected count=1" in line for line in rt.log_lines)
        )
        observations = relay_watchdog._validated_loss_observations(chs)
        self.assertEqual(len(observations), 1)
        self.assertEqual(
            next(iter(observations.values()))["evidence_frontiers"],
            [float(int(successor[0]))],
        )

    def test_bounded_rolloff_does_not_surface_suspected_loss(self):
        old = (self.now - 3000, "delivered response rolled out of limit 100")
        newer = (self.now - 2000, "newer delivered response still in window")
        self.write_transcript([old, newer])
        rt = self.make_rt()
        state: dict = {}

        rt.haystack = norm(f"{old[1]} {newer[1]}")
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        rt.haystack = norm(newer[1])
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)

        self.assertNotIn(
            relay_watchdog.PERMANENT_LOSS_SUSPECTED_KEY, state["999"]
        )
        self.assertFalse(
            any("permanent-loss-suspected" in line for line in rt.log_lines)
        )

    def test_permanent_loss_requires_two_distinct_frontier_advances(self):
        missing = (self.now - 4000, "permanently skipped response")
        first = (self.now - 3000, "first delivered successor")
        second = (self.now - 2000, "second delivered successor")
        rt = self.make_rt(realert_secs=1)
        state: dict = {}

        self.drive_distinct_delivery_advances(rt, state, missing, first, second)

        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("새 영구 릴레이 유실 확정", rt.alerts[0][0])
        self.assertIn("현재 미도달 **0건**", rt.alerts[0][0])
        self.assertIn("영구 유실 **1건 누적**", rt.alerts[0][0])
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        self.assertEqual(len(rt.alerts), 1)

    def test_late_delivery_retracts_tombstone_and_total(self):
        missing = (self.now - 4000, "late response")
        first = (self.now - 3000, "first successor")
        second = (self.now - 2000, "second successor")
        rt = self.make_rt()
        state: dict = {}
        self.drive_distinct_delivery_advances(rt, state, missing, first, second)
        self.assertEqual(permanent_loss_total(state["999"]), 1)

        rt.haystack = norm(f"{missing[1]} {first[1]} {second[1]}")
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        self.assertEqual(permanent_loss_total(state["999"]), 0)
        self.assertNotIn(relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY, state["999"])
        self.assertTrue(any("permanent-loss-retracted" in line for line in rt.log_lines))

    def test_new_permanent_loss_after_prior_tombstone_alerts_again(self):
        first_missing = (self.now - 6000, "first skipped response")
        first_success = (self.now - 5000, "first delivered successor")
        second_success = (self.now - 4000, "second delivered successor")
        rt = self.make_rt(realert_secs=1)
        state: dict = {}
        self.drive_distinct_delivery_advances(
            rt, state, first_missing, first_success, second_success
        )

        next_missing = (self.now - 3000, "second skipped response")
        third_success = (self.now - 2000, "third delivered successor")
        fourth_success = (self.now - 1000, "fourth delivered successor")
        self.append_transcript_block(*next_missing)
        self.append_transcript_block(*third_success)
        rt.haystack = norm(
            f"{first_success[1]} {second_success[1]} {third_success[1]}"
        )
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        self.append_transcript_block(*fourth_success)
        rt.haystack = norm(
            f"{first_success[1]} {second_success[1]} {third_success[1]} {fourth_success[1]}"
        )
        tick_channel(rt, TICK_CHANNEL, state, self.now + 3)

        permanent_alerts = [
            body for body, _ in rt.alerts if "새 영구 릴레이 유실 확정" in body
        ]
        self.assertEqual(len(permanent_alerts), 2)
        self.assertIn("영구 유실 **2건 누적**", permanent_alerts[-1])

    def test_tombstone_survives_state_round_trip_and_restart(self):
        missing = (self.now - 4000, "restart-persistent skipped response")
        first = (self.now - 3000, "restart first successor")
        second = (self.now - 2000, "restart second successor")
        rt = self.make_rt()
        state: dict = {}
        self.drive_distinct_delivery_advances(rt, state, missing, first, second)
        save_state(rt.state_path, state)

        restarted_state = load_state(rt.state_path)
        restarted = self.make_rt()
        restarted.haystack = norm(f"{first[1]} {second[1]}")
        tick_channel(restarted, TICK_CHANNEL, restarted_state, self.now + 2)

        self.assertEqual(restarted.alerts, [])
        self.assertEqual(permanent_loss_total(restarted_state["999"]), 1)
        self.assertEqual(
            len(restarted_state["999"][relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY]),
            1,
        )

    def test_duplicate_delivery_is_cardinality_aware(self):
        ts = self.now - 2000
        blocks = [(ts, "same response"), (ts + 1, "same response")]
        verdict = evaluate(blocks, norm("same response"), self.now, 600, 900)
        self.assertEqual(verdict.delivered_ts, ts)
        self.assertEqual(delivered_flags(blocks, norm("same response")), [True, False])

    def test_failed_permanent_loss_alert_retries_next_tick(self):
        missing = (self.now - 4000, "alert retry skipped response")
        first = (self.now - 3000, "alert retry first successor")
        second = (self.now - 2000, "alert retry second successor")
        rt = self.make_rt()
        rt.alert_succeeds = False
        state: dict = {}
        self.drive_distinct_delivery_advances(rt, state, missing, first, second)
        self.assertEqual(
            state["999"][relay_watchdog.PERMANENT_LOSS_UNANNOUNCED_KEY], 1
        )

        rt.alert_succeeds = True
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        self.assertNotIn(
            relay_watchdog.PERMANENT_LOSS_UNANNOUNCED_KEY, state["999"]
        )
        permanent_alerts = [
            body for body, _ in rt.alerts if "새 영구 릴레이 유실 확정" in body
        ]
        self.assertEqual(len(permanent_alerts), 2)

    def test_transport_unreachable_tombstone_preserves_gap_incident(self):
        missing = (self.now - 4000, "transport skipped response")
        first = (self.now - 3000, "transport first successor")
        second = (self.now - 2000, "transport second successor")
        rt = self.make_rt(github_repo="owner/repo")
        rt.watcher_probe = WatcherStateProbe(None)
        state: dict = {}
        self.write_transcript([missing, first])
        rt.haystack = norm(first[1])
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        chs = state["999"]
        # Seed the exact #4947 incident established by a prior GAP tick.
        chs["alerting"] = True
        chs["gap_since"] = self.now - 3600
        chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY] = [
            str(self.proj_dir / "s.jsonl")
        ]
        chs[relay_watchdog.ISSUE_FILING_SUPPRESSION_REASON_KEY] = (
            relay_watchdog.ISSUE_FILING_DC_UNREACHABLE_REASON
        )
        chs[relay_watchdog.ISSUE_FILING_SUPPRESSION_SINCE_KEY] = self.now
        chs[relay_watchdog.ISSUE_FILING_REACHABLE_TICKS_KEY] = 0
        self.append_transcript_block(*second)
        rt.haystack = norm(f"{first[1]} {second[1]}")

        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)

        chs = state["999"]
        self.assertTrue(chs["alerting"])
        self.assertEqual(chs["gap_since"], self.now - 3600)
        self.assertEqual(
            chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY],
            [str(self.proj_dir / "s.jsonl")],
        )
        self.assertEqual(
            chs[relay_watchdog.ISSUE_FILING_SUPPRESSION_REASON_KEY],
            relay_watchdog.ISSUE_FILING_DC_UNREACHABLE_REASON,
        )

        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        chs = state["999"]
        self.assertTrue(chs["alerting"])
        self.assertEqual(chs["gap_since"], self.now - 3600)
        self.assertEqual(
            chs[relay_watchdog.ISSUE_FILING_SUPPRESSION_REASON_KEY],
            relay_watchdog.ISSUE_FILING_DC_UNREACHABLE_REASON,
        )

    def test_duplicate_same_timestamp_text_uses_stable_record_identities(self):
        duplicate = (self.now - 4000, "same duplicate obligation")
        first = (self.now - 3000, "duplicate first successor")
        second = (self.now - 2000, "duplicate second successor")
        self.write_transcript([duplicate, duplicate, first])
        rt = self.make_rt()
        state: dict = {}
        rt.haystack = norm(f"{duplicate[1]} {first[1]}")
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.append_transcript_block(*second)
        rt.haystack = norm(f"{duplicate[1]} {first[1]} {second[1]}")
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)

        tombstones = permanent_loss_tombstones(state["999"])
        self.assertEqual(len(tombstones), 1)
        self.assertEqual(permanent_loss_total(state["999"]), 1)

    def test_offset_fallback_warning_is_edge_triggered(self):
        transcript = self.proj_dir / "s.jsonl"
        ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(self.now - 2000))
        transcript.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "timestamp": ts,
                    "message": {
                        "content": [{"type": "text", "text": "uuid-less fallback"}]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        rt = self.make_rt()
        state: dict = {}

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)

        warnings = [
            line
            for line in rt.log_lines
            if "permanent-loss-identity-offset-fallback" in line
        ]
        self.assertEqual(len(warnings), 1)

    def test_record_uuid_identity_survives_actual_head_truncation(self):
        prefix = (self.now - 5000, "truncated delivered prefix")
        missing = (self.now - 4000, "late block retained after truncation")
        first = (self.now - 3000, "truncate first successor")
        second = (self.now - 2000, "truncate second successor")
        self.write_transcript([prefix, missing, first])
        rt = self.make_rt()
        state: dict = {}
        rt.haystack = norm(f"{prefix[1]} {first[1]}")
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.append_transcript_block(*second)
        rt.haystack = norm(f"{prefix[1]} {first[1]} {second[1]}")
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)

        before = set(permanent_loss_tombstones(state["999"]))
        self.assertEqual(len(before), 1)
        transcript = self.proj_dir / "s.jsonl"
        raw_lines = transcript.read_bytes().splitlines(keepends=True)
        transcript.write_bytes(b"".join(raw_lines[1:]))
        rt.haystack = norm(f"{missing[1]} {first[1]} {second[1]}")
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        self.assertEqual(permanent_loss_total(state["999"]), 0)
        self.assertTrue(any("permanent-loss-retracted" in line for line in rt.log_lines))

    def test_legacy_actual_delivery_scalar_migrates_to_selected_path(self):
        missing = (self.now - 4000, "legacy migration missing response")
        successor = (self.now - 3000, "legacy migration delivered successor")
        self.write_transcript([missing, successor])
        rt = self.make_rt(realert_secs=1)
        rt.haystack = norm(successor[1])
        legacy_observed_at = self.now - 120
        state = {
            "999": {
                relay_watchdog.LAST_ACTUAL_DELIVERY_AT_KEY: legacy_observed_at,
                DELIVERED_WATERMARKS_KEY: {
                    str(self.proj_dir / "s.jsonl"): {
                        "delivered_ts": successor[0],
                        "updated_at": self.now - 3000,
                    }
                },
            }
        }

        tick_channel(rt, TICK_CHANNEL, state, self.now)

        chs = state["999"]
        self.assertNotIn(relay_watchdog.LAST_ACTUAL_DELIVERY_AT_KEY, chs)
        self.assertEqual(
            chs[relay_watchdog.LAST_ACTUAL_DELIVERY_BY_PATH_KEY][
                str(self.proj_dir / "s.jsonl")
            ],
            legacy_observed_at,
        )

    def test_loss_state_overflow_is_logged_and_counted(self):
        path = str(self.proj_dir / "s.jsonl")
        state = {
            relay_watchdog.LOSS_OBSERVATIONS_KEY: {
                f"{index:064x}": {
                    "path": path,
                    "epoch": self.now - 4000,
                    "evidence_frontiers": [self.now - 3000],
                    "last_observed_at": self.now - index,
                }
                for index in range(relay_watchdog.MAX_LOSS_OBSERVATIONS)
            }
        }
        missing = (self.now - 2000, "overflow missing response")
        successor = (self.now - 1000, "overflow delivered successor")
        self.write_transcript([missing, successor])
        rt = self.make_rt()
        rt.haystack = norm(successor[1])

        tick_channel(rt, TICK_CHANNEL, {"999": state}, self.now)

        self.assertEqual(state[relay_watchdog.PERMANENT_LOSS_OVERFLOW_TOTAL_KEY], 1)
        self.assertTrue(
            any("permanent-loss-state-overflow" in line for line in rt.log_lines)
        )

    def test_corrupt_loss_state_logs_and_preserves_raw_evidence(self):
        path = str(self.proj_dir / "s.jsonl")
        corrupt = {"bad": "shape"}
        state = {
            "999": {
                relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY: corrupt.copy(),
                relay_watchdog.PERMANENT_LOSS_TOTAL_KEY: 9,
            }
        }
        self.write_transcript([(self.now - 2000, "corrupt state candidate")])
        rt = self.make_rt()

        tick_channel(rt, TICK_CHANNEL, state, self.now)

        chs = state["999"]
        self.assertEqual(chs[relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY], corrupt)
        self.assertEqual(permanent_loss_total(chs), 9)
        self.assertTrue(
            any("permanent-loss-state-corrupt" in line for line in rt.log_lines)
        )

    def test_corruption_warning_is_edge_triggered_by_content(self):
        chs = {
            relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY: {"bad": "shape"},
            relay_watchdog.PERMANENT_LOSS_TOTAL_KEY: 9,
        }
        state = {"999": chs}
        self.write_transcript([(self.now - 2000, "corrupt warning candidate")])
        rt = self.make_rt()

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)

        warnings = [
            line
            for line in rt.log_lines
            if "permanent-loss-state-corrupt" in line
        ]
        self.assertEqual(len(warnings), 1)
        chs[relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY] = {"changed": "shape"}

        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        warnings = [
            line
            for line in rt.log_lines
            if "permanent-loss-state-corrupt" in line
        ]
        self.assertEqual(len(warnings), 2)

    def test_corruption_warning_refires_between_absent_and_null(self):
        chs = {
            relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY: {"bad": "shape"},
            relay_watchdog.PERMANENT_LOSS_TOTAL_KEY: 9,
        }
        state = {"999": chs}
        self.write_transcript([(self.now - 2000, "absent null warning candidate")])
        rt = self.make_rt()

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        chs[relay_watchdog.LOSS_OBSERVATIONS_KEY] = None
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        chs.pop(relay_watchdog.LOSS_OBSERVATIONS_KEY)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        warnings = [
            line
            for line in rt.log_lines
            if "permanent-loss-state-corrupt" in line
        ]
        self.assertEqual(len(warnings), 3)

    def test_corrupt_tick_does_not_increment_overflow_projection(self):
        path = str(self.proj_dir / "s.jsonl")
        observations = {
            f"{index:064x}": {
                "path": path,
                "epoch": self.now - 4000,
                "evidence_frontiers": [self.now - 3000],
                "last_observed_at": self.now - index,
            }
            for index in range(relay_watchdog.MAX_LOSS_OBSERVATIONS)
        }
        observations["bad"] = "shape"
        state = {
            "999": {
                relay_watchdog.LOSS_OBSERVATIONS_KEY: observations,
                relay_watchdog.PERMANENT_LOSS_OVERFLOW_TOTAL_KEY: 7,
            }
        }
        self.write_transcript(
            [
                (self.now - 2000, "corrupt overflow candidate"),
                (self.now - 1000, "corrupt overflow successor"),
            ]
        )
        rt = self.make_rt()
        rt.haystack = norm("corrupt overflow successor")

        tick_channel(rt, TICK_CHANNEL, state, self.now)

        chs = state["999"]
        self.assertEqual(chs[relay_watchdog.PERMANENT_LOSS_OVERFLOW_TOTAL_KEY], 7)
        self.assertFalse(
            any("permanent-loss-state-overflow" in line for line in rt.log_lines)
        )

    def test_corrupt_state_suppresses_unpersisted_loss_confirmation(self):
        missing = (self.now - 4000, "corrupt unpersisted skipped response")
        first = (self.now - 3000, "corrupt first successor")
        second = (self.now - 2000, "corrupt second successor")
        rt = self.make_rt()
        state: dict = {}
        self.write_transcript([missing, first])
        rt.haystack = norm(first[1])
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        chs = state["999"]
        raw_tombstones = {"bad": "shape"}
        chs[relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY] = raw_tombstones.copy()
        self.append_transcript_block(*second)
        rt.haystack = norm(f"{first[1]} {second[1]}")

        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)

        self.assertEqual(
            chs[relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY], raw_tombstones
        )
        self.assertFalse(
            any("permanent-loss-confirmed" in line for line in rt.log_lines)
        )
        self.assertFalse(any("PERMANENT LOSS ALERT" in line for line in rt.log_lines))
        self.assertEqual(rt.alerts, [])
        self.assertTrue(
            any("permanent-loss-state-corrupt" in line for line in rt.log_lines)
        )

    def test_corrupt_tombstone_map_cannot_double_count_total(self):
        state = {
            relay_watchdog.PERMANENT_LOSS_TOTAL_KEY: 9,
            relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY: {"bad": "shape"},
        }
        self.assertEqual(permanent_loss_total(state), 9)

    def test_lifecycle_reclaim_preserves_and_escalates_corrupt_loss_state(self):
        target = self.proj_dir / "corrupt-reclaim.jsonl"
        target.write_text("{}\n", encoding="utf-8")
        path = str(target)
        raw_observations = {"bad": "shape"}
        raw_tombstones = {"also-bad": "shape"}
        state = {
            "999": {
                relay_watchdog.RECOVERED_GAP_GUARDS_KEY: {
                    path: {
                        "size": target.stat().st_size,
                        "confirmed_at": self.now,
                        "last_seen_at": self.now,
                        "absent_since": self.now + 1,
                    }
                },
                relay_watchdog.LOSS_OBSERVATIONS_KEY: raw_observations.copy(),
                relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY: raw_tombstones.copy(),
            }
        }
        target.unlink()
        rt = self.make_rt()

        tick_channel(
            rt,
            TICK_CHANNEL,
            state,
            self.now + relay_watchdog.RECOVERED_GAP_GUARD_TTL_SECS + 2,
        )

        chs = state["999"]
        self.assertEqual(chs[relay_watchdog.LOSS_OBSERVATIONS_KEY], raw_observations)
        self.assertEqual(
            chs[relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY], raw_tombstones
        )
        self.assertTrue(
            any(
                "permanent-loss-state-corrupt during lifecycle reclaim"
                in line
                for line in rt.log_lines
            )
        )

    def test_loss_state_is_bounded_ttl_pruned_and_lifecycle_cleaned(self):
        path = str(self.proj_dir / "s.jsonl")
        old = self.now - relay_watchdog.TRANSCRIPT_HISTORY_TTL_SECS - 1
        state = {
            relay_watchdog.LOSS_OBSERVATIONS_KEY: {
                f"{index:064x}": {
                    "path": path,
                    "epoch": old,
                    "evidence_frontiers": [old + 1],
                    "last_observed_at": old,
                }
                for index in range(relay_watchdog.MAX_LOSS_OBSERVATIONS + 10)
            },
            relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY: {
                f"{index + 1000:064x}": {
                    "path": path,
                    "epoch": old,
                    "confirmed_at": old,
                }
                for index in range(relay_watchdog.MAX_PERMANENT_LOSS_TOMBSTONES + 10)
            },
            relay_watchdog.LAST_ACTUAL_DELIVERY_BY_PATH_KEY: {path: self.now},
        }
        self.assertEqual(
            relay_watchdog._validated_loss_observations(state, self.now), {}
        )
        self.assertEqual(permanent_loss_tombstones(state, self.now), {})
        relay_watchdog._forget_reclaimed_recovered_gap_lifecycles(
            state, [path], loss_state_corrupted=False
        )
        self.assertNotIn(relay_watchdog.LAST_ACTUAL_DELIVERY_BY_PATH_KEY, state)

    # (d) persistent-gap issue auto-filing is deduplicated
    def test_persistent_gap_files_issue_exactly_once(self):
        rt = self.gap_rt(github_repo="owner/repo")
        rt.watcher_probe = WatcherStateProbe(200, True, False)
        state = {"999": {"gap_since": self.now - rt.cfg.issue_after_secs - 1}}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(rt.issue_calls, 1)
        self.assertEqual(state["999"]["issue_url"], "https://example.test/issues/1")
        self.assertIn("https://example.test/issues/1", rt.alerts[0][0])

        # Second tick, gap still open: issue_url in state must prevent a dupe.
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        self.assertEqual(rt.issue_calls, 1, "issue must be filed exactly once")

    def test_no_github_repo_configured_files_nothing(self):
        rt = self.gap_rt()  # github_repo defaults to ""
        state = {"999": {"gap_since": self.now - rt.cfg.issue_after_secs - 1}}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(rt.issue_calls, 0)

    def test_unreachable_gap_defers_issue_but_preserves_alert_and_evidence(self):
        rt = self.gap_rt(github_repo="owner/repo")
        rt.watcher_probe = WatcherStateProbe(None)
        incident_age = rt.cfg.issue_after_secs + 60
        transcript = self.proj_dir / "s.jsonl"
        recovered_guard = {
            "size": transcript.stat().st_size,
            "confirmed_at": self.now - 120,
            "last_seen_at": self.now - 60,
            "absent_since": None,
        }
        state = {
            "999": {
                "gap_since": self.now - incident_age,
                relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY: [str(transcript)],
                relay_watchdog.RECOVERED_GAP_GUARDS_KEY: {
                    str(transcript): recovered_guard
                },
            }
        }

        tick_channel(rt, TICK_CHANNEL, state, self.now)

        chs = state["999"]
        self.assertEqual(rt.issue_calls, 0)
        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("릴레이 갭 감지", rt.alerts[0][0])
        self.assertEqual(chs["gap_since"], self.now - incident_age)
        self.assertEqual(
            chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY],
            [str(self.proj_dir / "s.jsonl")],
        )
        self.assertEqual(
            chs[relay_watchdog.RECOVERED_GAP_GUARDS_KEY][str(transcript)][
                "confirmed_at"
            ],
            recovered_guard["confirmed_at"],
        )
        self.assertTrue(chs.get("alerting"))
        self.assertEqual(
            chs[relay_watchdog.ISSUE_FILING_SUPPRESSION_REASON_KEY],
            relay_watchdog.ISSUE_FILING_DC_UNREACHABLE_REASON,
        )
        self.assertEqual(
            chs[relay_watchdog.ISSUE_FILING_SUPPRESSION_SINCE_KEY], self.now
        )
        self.assertEqual(chs[relay_watchdog.ISSUE_FILING_REACHABLE_TICKS_KEY], 0)

    def test_repeated_unreachable_ticks_preserve_incident_clock(self):
        rt = self.gap_rt(github_repo="owner/repo", realert_secs=1)
        rt.watcher_probe = WatcherStateProbe(None)
        original_gap_since = self.now - rt.cfg.issue_after_secs - 600
        state = {"999": {"gap_since": original_gap_since}}

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        chs = state["999"]
        self.assertEqual(rt.issue_calls, 0)
        self.assertEqual(len(rt.alerts), 2)
        self.assertEqual(chs["gap_since"], original_gap_since)
        self.assertEqual(
            chs[relay_watchdog.ISSUE_FILING_SUPPRESSION_SINCE_KEY], self.now
        )
        self.assertEqual(chs[relay_watchdog.ISSUE_FILING_REACHABLE_TICKS_KEY], 0)

    def test_two_reachable_ticks_resume_old_incident_exactly_once(self):
        rt = self.gap_rt(github_repo="owner/repo")
        rt.watcher_probe = WatcherStateProbe(None)
        original_gap_since = self.now - rt.cfg.issue_after_secs - 300
        state = {"999": {"gap_since": original_gap_since}}
        tick_channel(rt, TICK_CHANNEL, state, self.now)

        rt.watcher_probe = WatcherStateProbe(200, True, False)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        self.assertEqual(rt.issue_calls, 0)
        self.assertEqual(
            state["999"][relay_watchdog.ISSUE_FILING_REACHABLE_TICKS_KEY], 1
        )
        self.assertEqual(state["999"]["gap_since"], original_gap_since)

        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        self.assertEqual(rt.issue_calls, 1)
        self.assertEqual(
            state["999"]["issue_url"], "https://example.test/issues/1"
        )
        self.assertEqual(state["999"]["gap_since"], original_gap_since)
        self.assertNotIn(
            relay_watchdog.ISSUE_FILING_SUPPRESSION_REASON_KEY, state["999"]
        )

        tick_channel(rt, TICK_CHANNEL, state, self.now + 3)
        self.assertEqual(rt.issue_calls, 1)

    def test_reachable_flap_resets_two_tick_counter(self):
        rt = self.gap_rt(github_repo="owner/repo")
        state = {
            "999": {
                "gap_since": self.now - rt.cfg.issue_after_secs - 1,
                relay_watchdog.ISSUE_FILING_SUPPRESSION_REASON_KEY: (
                    relay_watchdog.ISSUE_FILING_DC_UNREACHABLE_REASON
                ),
                relay_watchdog.ISSUE_FILING_SUPPRESSION_SINCE_KEY: self.now - 10,
                relay_watchdog.ISSUE_FILING_REACHABLE_TICKS_KEY: 0,
            }
        }

        rt.watcher_probe = WatcherStateProbe(200, True, False)
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        rt.watcher_probe = WatcherStateProbe(None)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        rt.watcher_probe = WatcherStateProbe(404)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        self.assertEqual(rt.issue_calls, 0)
        self.assertEqual(
            state["999"][relay_watchdog.ISSUE_FILING_REACHABLE_TICKS_KEY], 1
        )

        tick_channel(rt, TICK_CHANNEL, state, self.now + 3)
        self.assertEqual(rt.issue_calls, 1)

    def test_404_counts_as_reachable_for_issue_resume(self):
        rt = self.gap_rt(github_repo="owner/repo")
        state = {
            "999": {
                "gap_since": self.now - rt.cfg.issue_after_secs - 1,
                relay_watchdog.ISSUE_FILING_SUPPRESSION_REASON_KEY: (
                    relay_watchdog.ISSUE_FILING_DC_UNREACHABLE_REASON
                ),
                relay_watchdog.ISSUE_FILING_SUPPRESSION_SINCE_KEY: self.now - 10,
                relay_watchdog.ISSUE_FILING_REACHABLE_TICKS_KEY: 0,
            }
        }
        rt.watcher_probe = WatcherStateProbe(404)

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)

        self.assertEqual(rt.issue_calls, 1)

    def test_gap_owner_without_selected_transcript_still_probes_recovery(self):
        rt = self.gap_rt(github_repo="owner/repo")
        transcript = self.proj_dir / "s.jsonl"
        original_gap_since = self.now - rt.cfg.issue_after_secs - 60
        state = {
            "999": {
                "gap_since": original_gap_since,
                relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY: [str(transcript)],
                relay_watchdog.ISSUE_FILING_SUPPRESSION_REASON_KEY: (
                    relay_watchdog.ISSUE_FILING_DC_UNREACHABLE_REASON
                ),
                relay_watchdog.ISSUE_FILING_SUPPRESSION_SINCE_KEY: self.now - 10,
                relay_watchdog.ISSUE_FILING_REACHABLE_TICKS_KEY: 0,
            }
        }
        rt.watcher_probe = WatcherStateProbe(200, True, False)

        with mock.patch.object(
            relay_watchdog,
            "select_watch_transcript_with_reason",
            return_value=(None, "no_candidates"),
        ):
            tick_channel(rt, TICK_CHANNEL, state, self.now)
            self.assertEqual(rt.issue_calls, 0)
            self.assertEqual(
                state["999"][relay_watchdog.ISSUE_FILING_REACHABLE_TICKS_KEY], 1
            )
            tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
            tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

        self.assertGreaterEqual(rt.watcher_calls, 2)
        self.assertEqual(rt.issue_calls, 1)
        self.assertEqual(state["999"]["gap_since"], original_gap_since)

    def test_5xx_and_malformed_200_are_not_unreachable_suppression_authority(self):
        probes = (
            WatcherStateProbe(503),
            parse_watcher_state_probe(200, {"attached": "malformed"}),
        )
        for probe in probes:
            with self.subTest(probe=probe):
                rt = self.gap_rt(github_repo="owner/repo")
                rt.watcher_probe = probe
                state = {
                    "999": {
                        "gap_since": self.now - rt.cfg.issue_after_secs - 1
                    }
                }

                tick_channel(rt, TICK_CHANNEL, state, self.now)

                self.assertEqual(rt.issue_calls, 1)
                self.assertNotIn(
                    relay_watchdog.ISSUE_FILING_SUPPRESSION_REASON_KEY,
                    state["999"],
                )

    def test_bad_responses_keep_aged_incident_suppressed_until_two_healthy_ticks(self):
        probes = (
            WatcherStateProbe(503),
            parse_watcher_state_probe(200, {"attached": "malformed"}),
        )
        for probe in probes:
            with self.subTest(probe=probe):
                rt = self.gap_rt(github_repo="owner/repo")
                original_gap_since = self.now - rt.cfg.issue_after_secs - 100
                state = {"999": {"gap_since": original_gap_since}}

                rt.watcher_probe = WatcherStateProbe(None)
                tick_channel(rt, TICK_CHANNEL, state, self.now)
                rt.watcher_probe = WatcherStateProbe(200, True, False)
                tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
                rt.watcher_probe = probe
                tick_channel(rt, TICK_CHANNEL, state, self.now + 2)

                self.assertEqual(rt.issue_calls, 0)
                self.assertEqual(
                    state["999"][relay_watchdog.ISSUE_FILING_REACHABLE_TICKS_KEY],
                    0,
                )
                self.assertEqual(state["999"]["gap_since"], original_gap_since)

                rt.watcher_probe = WatcherStateProbe(200, True, False)
                tick_channel(rt, TICK_CHANNEL, state, self.now + 3)
                self.assertEqual(rt.issue_calls, 0)
                tick_channel(rt, TICK_CHANNEL, state, self.now + 4)
                self.assertEqual(rt.issue_calls, 1)
                tick_channel(rt, TICK_CHANNEL, state, self.now + 5)
                self.assertEqual(rt.issue_calls, 1)

    def test_planned_deploy_still_returns_before_gap_incident_state(self):
        rt = self.gap_rt(github_repo="owner/repo")
        rt.watcher_probe = WatcherStateProbe(None)
        rt.deploy_marker.touch()
        state: dict = {}

        tick_channel(rt, TICK_CHANNEL, state, self.now)

        chs = state["999"]
        self.assertEqual(rt.alerts, [])
        self.assertEqual(rt.issue_calls, 0)
        self.assertNotIn("gap_since", chs)
        self.assertNotIn(
            relay_watchdog.ISSUE_FILING_SUPPRESSION_REASON_KEY, chs
        )

    # (e) consecutive discord-read failures escalate to an alert
    def test_read_failure_threshold_escalates(self):
        self.write_transcript([(self.now - 60, "fresh block")])
        rt = self.make_rt()
        rt.haystack = None  # discord read failing
        state = {"999": {"read_failures": rt.cfg.read_fail_alert_after - 2}}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(rt.alerts, [], "below threshold must only log")
        self.assertEqual(
            state["999"]["read_failures"], rt.cfg.read_fail_alert_after - 1
        )
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(len(rt.alerts), 1, "threshold reached must alert")
        self.assertIn("연속 실패", rt.alerts[0][0])
        self.assertEqual(state["999"]["last_alert"], self.now)

    def test_read_success_resets_failure_counter(self):
        self.write_transcript([(self.now - 60, "fresh block")])
        rt = self.make_rt()
        rt.haystack = norm("fresh block")
        state = {"999": {"read_failures": 4}}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(state["999"]["read_failures"], 0)

    # r4 review (PR #4399), end-to-end for the AttributeError defect: with the
    # REAL discord_haystack wired in, `discord read` printing `null` (rc=0,
    # valid JSON) must increment read_failures instead of crashing the tick.
    def test_null_discord_json_increments_read_failures(self):
        self.write_transcript([(self.now - 60, "fresh block")])
        rt = self.make_rt()
        rt.discord_haystack = Runtime.discord_haystack.__get__(rt)

        def fake_run(argv, **kwargs):
            return subprocess.CompletedProcess(argv, 0, stdout="null", stderr="")

        state: dict = {}
        with mock.patch.object(
            relay_watchdog.subprocess, "run", side_effect=fake_run
        ):
            tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(state["999"]["read_failures"], 1)
        self.assertTrue(any("discord read failed" in l for l in rt.log_lines))

    # r2 review (PR #4399): save_state was the only unguarded call in the main
    # loop. A disk-full/unwritable-logs OSError there kills the process; the
    # plist's KeepAlive+ThrottleInterval=30 respawns it every ~30s with empty
    # in-memory state, and since the alert fires BEFORE the save, the cooldown
    # evaporates on each restart → ~2 alerts/min storm during a live gap
    # (amplified by announce-triggered agent turns), while gap_since never
    # persists so the auto-issue threshold can never fire.
    def test_unwritable_state_dir_does_not_kill_process_or_break_cooldown(self):
        rt = self.gap_rt()
        logs = self.root / "logs"
        self.addCleanup(os.chmod, logs, 0o755)
        os.chmod(logs, 0o555)  # every state save now raises OSError
        state: dict = {}
        # Three tick+save rounds sharing the SAME in-memory dict, exactly like
        # main(). An unguarded OSError escapes the loop and fails this test.
        for i in range(3):
            tick_channel(rt, TICK_CHANNEL, state, self.now + i)
            relay_watchdog.save_state_guarded(rt, state)
        self.assertEqual(
            len(rt.alerts), 1, "cooldown must survive failed state saves"
        )
        self.assertTrue(any("state save failed" in l for l in rt.log_lines))


class AlertFallbackTests(unittest.TestCase):
    """(f) Runtime.alert delivery chain: announce-bot primary, bot-token
    fallback. The fallback is the only path proven to survive the 07-09 outage;
    a broken handoff would silently swallow the alert."""

    CH = ChannelConfig(
        channel_id="999",
        sendmessage_key="key123",
        worktree_root=WORKTREE_ROOT,
        announce_to="project-agentdesk",
    )

    def _run_alert(self, announce_rc: int) -> list[list[str]]:
        calls: list[list[str]] = []

        def fake_run(argv, **kwargs):
            calls.append(list(argv))
            rc = announce_rc if "send-to-agent" in argv else 0
            return subprocess.CompletedProcess(argv, rc, stdout="", stderr="boom")

        with tempfile.TemporaryDirectory() as tmp:
            rt = Runtime(Config(channels=(self.CH,)), Path(tmp))
            with mock.patch.object(
                relay_watchdog.subprocess, "run", side_effect=fake_run
            ):
                rt.alert(self.CH, "alert body")
        return calls

    def test_announce_failure_falls_back_to_sendmessage(self):
        calls = self._run_alert(announce_rc=1)
        self.assertEqual(len(calls), 2)
        self.assertIn("send-to-agent", calls[0])
        # The unfulfillable-contract guard: --expect-reply must be false.
        self.assertIn("--expect-reply", calls[0])
        self.assertEqual(calls[0][calls[0].index("--expect-reply") + 1], "false")
        self.assertIn("discord-sendmessage", calls[1])
        self.assertIn("key123", calls[1])

    def test_announce_success_skips_fallback(self):
        calls = self._run_alert(announce_rc=0)
        self.assertEqual(len(calls), 1)
        self.assertIn("send-to-agent", calls[0])

    def test_direct_fallback_nonzero_returns_failure(self):
        ch = ChannelConfig(
            channel_id="999", sendmessage_key="key123", worktree_root=WORKTREE_ROOT
        )
        with tempfile.TemporaryDirectory() as tmp:
            rt = Runtime(Config(channels=(ch,)), Path(tmp))
            failed = subprocess.CompletedProcess(
                ["discord-sendmessage"], 1, stdout="", stderr="delivery failed"
            )
            with mock.patch.object(relay_watchdog.subprocess, "run", return_value=failed):
                self.assertFalse(rt.alert(ch, "alert body"))

    def test_no_announce_target_goes_straight_to_sendmessage(self):
        ch = ChannelConfig(
            channel_id="999", sendmessage_key="key123", worktree_root=WORKTREE_ROOT
        )
        calls: list[list[str]] = []

        def fake_run(argv, **kwargs):
            calls.append(list(argv))
            return subprocess.CompletedProcess(argv, 0, stdout="", stderr="")

        with tempfile.TemporaryDirectory() as tmp:
            rt = Runtime(Config(channels=(ch,)), Path(tmp))
            with mock.patch.object(
                relay_watchdog.subprocess, "run", side_effect=fake_run
            ):
                rt.alert(ch, "alert body")
        self.assertEqual(len(calls), 1)
        self.assertIn("discord-sendmessage", calls[0])


class DeploymentWiringTests(unittest.TestCase):
    """#4372 lesson: a test that CI never runs is a graveyard, and a script the
    deploy never ships evaporates (the 06-29 relay-gap-watch, the 07-09
    prototype). Pin the wiring itself."""

    def test_ci_script_checks_runs_this_suite(self):
        script = (REPO_ROOT / "scripts" / "ci-script-checks.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("tests.test_relay_watchdog", script)

    def test_main_loop_runs_independent_pg_tunnel_tick(self):
        script = (REPO_ROOT / "scripts/relay_watchdog.py").read_text(
            encoding="utf-8"
        )
        self.assertIn("tick_pg_tunnel(rt, state, now)", script)

    def test_deploy_release_ships_watchdog_and_plist(self):
        deploy = (REPO_ROOT / "scripts" / "deploy-release.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("scripts/relay_watchdog.py", deploy)
        self.assertIn("com.agentdesk.relay-watchdog", deploy)
        # Fail-open invariant (adversarial review, PR #4399): the watchdog block
        # runs after DEPLOY_OK, so a plist write failure must warn and continue
        # — never abort a healthy deploy or skip manifest/peer propagation.
        self.assertIn("_install_relay_watchdog_plist", deploy)
        self.assertIn("Relay watchdog plist write FAILED", deploy)
        # Planned restarts must not hide real transcript gaps. Deploy first
        # obtains the runtime's durable frontier acknowledgement, and it retains
        # an unchanged watchdog process instead of resetting observation state.
        self.assertIn("request_restart_drain_mode_or_fail", deploy)
        self.assertIn("wait_for_restart_persistence_or_fail", deploy)
        self.assertNotIn(
            'touch "$ADK_REL/logs/relay-watchdog.deploy-marker"', deploy
        )
        self.assertIn("WATCHDOG_SCRIPT_CHANGED", deploy)
        self.assertIn("WATCHDOG_PLIST_CHANGED", deploy)
        self.assertIn("durable authority uninterrupted", deploy)

    def test_watchdog_is_portable_path_linted(self):
        checker = (REPO_ROOT / "scripts" / "check-portable-paths.py").read_text(
            encoding="utf-8"
        )
        self.assertIn("scripts/relay_watchdog.py", checker)

    @staticmethod
    def _watchdog_block() -> str:
        """Extract the actual watchdog install block from deploy-release.sh so
        the harness below executes the SHIPPED code, not a copy."""
        lines = (
            (REPO_ROOT / "scripts" / "deploy-release.sh")
            .read_text(encoding="utf-8")
            .splitlines()
        )
        start = next(
            i
            for i, l in enumerate(lines)
            if 'WATCHDOG_LABEL="com.agentdesk.relay-watchdog"' in l
        )
        # The block's final `fi` is the line after the staging-FAILED warning.
        end = next(
            i for i, l in enumerate(lines) if "Relay watchdog staging FAILED" in l
        )
        return "\n".join(lines[start : end + 2])

    def _run_block(
        self,
        block: str,
        adk_rel: Path,
        home: Path,
        *,
        fake_bin: Path | None = None,
        spawns: bool = True,
    ):
        import shlex

        # Default (fake_bin=None) keeps the original contract: no fake on PATH,
        # so the real launchctl is asked about a nonexistent domain.
        fake_env = ""
        if fake_bin is not None:
            fake_env = (
                f"PATH={shlex.quote(str(fake_bin))}:$PATH\n"
                f"EVENT_LOG={shlex.quote(str(home / 'events.log'))}\n"
                f"WD_LOADED={shlex.quote(str(home / 'wd.loaded'))}\n"
                f"WD_PID={shlex.quote(str(home / 'wd.pid'))}\n"
                f"WD_SPAWNS={int(spawns)}\n"
                "export PATH EVENT_LOG WD_LOADED WD_PID WD_SPAWNS\n"
            )
        script = (
            "set -euo pipefail\n"
            f"REPO={shlex.quote(str(REPO_ROOT))}\n"
            f"ADK_REL={shlex.quote(str(adk_rel))}\n"
            f"HOME={shlex.quote(str(home))}\n"
            # Nonexistent domain: bootstrap must fail (fail-open ⚠ path) rather
            # than loading a test plist into the developer's real launchd.
            "LAUNCHD_DOMAIN=gui/999999\n" + fake_env + block + "\necho HARNESS-END\n"
        )
        return subprocess.run(
            ["bash", "-c", script], capture_output=True, text=True, timeout=60
        )

    @staticmethod
    def _fake_launchctl_bin(root: Path) -> Path:
        """Fake launchd shaped after the fake-launchctl harness in
        tests/test_pg_tunnel.py.

        It reproduces the #5153 defect exactly: `bootstrap` returns 0 and
        `print` reports the job as loaded, yet `launchctl list` shows '-' in the
        PID column because nothing was spawned. WD_SPAWNS=1 makes `kickstart`
        the thing that actually produces a PID — which is what the 2026-08-06
        manual recovery observed on the live node.
        """
        fake_bin = root / "fake-bin"
        fake_bin.mkdir(parents=True, exist_ok=True)
        (fake_bin / "launchctl").write_text(
            """#!/bin/sh
printf 'launchctl %s\\n' "$*" >> "$EVENT_LOG"
case "$1" in
  print) [ -f "$WD_LOADED" ] && exit 0; exit 1 ;;
  bootout) rm -f "$WD_LOADED" "$WD_PID"; exit 0 ;;
  bootstrap) : > "$WD_LOADED"; exit 0 ;;
  kickstart)
    [ "${WD_SPAWNS:-0}" != 1 ] || printf '4242\\n' > "$WD_PID"
    exit 0 ;;
  list)
    if [ -f "$WD_LOADED" ]; then
      pid='-'
      [ ! -f "$WD_PID" ] || IFS= read -r pid < "$WD_PID"
      printf '%s\\t0\\tcom.agentdesk.relay-watchdog\\n' "$pid"
    fi
    exit 0 ;;
esac
exit 0
""",
            encoding="utf-8",
        )
        (fake_bin / "sleep").write_text(
            "#!/bin/sh\nexec /bin/sleep 0.01\n", encoding="utf-8"
        )
        for name in ("launchctl", "sleep"):
            (fake_bin / name).chmod(0o755)
        return fake_bin

    @staticmethod
    def _watchdog_fixture(root: Path) -> tuple[Path, Path]:
        adk = root / "adk"
        for sub in ("bin", "config", "logs"):
            (adk / sub).mkdir(parents=True)
        (adk / "config" / "relay-watchdog.json").write_text("{}", encoding="utf-8")
        home = root / "home"
        (home / "Library").mkdir(parents=True)
        return adk, home

    # #5153: the deploy printed "✓ Relay watchdog armed" off the bootstrap
    # return code while `launchctl list` showed '-' and relay gap monitoring was
    # gone for about two minutes. The ✓ must follow the PID, not the rc.
    def test_bootstrap_without_spawn_never_prints_armed(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk, home = self._watchdog_fixture(Path(tmp))
            fake_bin = self._fake_launchctl_bin(Path(tmp))
            p = self._run_block(
                self._watchdog_block(), adk, home, fake_bin=fake_bin, spawns=False
            )
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
            self.assertIn("HARNESS-END", p.stdout, "fail-open must reach the end")
            events = (home / "events.log").read_text(encoding="utf-8")
            self.assertIn(
                "launchctl kickstart -k gui/999999/com.agentdesk.relay-watchdog",
                events,
                "bootstrap must be followed by an explicit kickstart (#5152 shape)",
            )
            self.assertNotIn(
                "✓ Relay watchdog armed",
                p.stdout,
                "a loaded-but-unspawned watchdog must never be reported armed",
            )
            self.assertIn("NOT spawned", p.stdout)

    def test_spawned_watchdog_is_reported_armed_with_its_pid(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk, home = self._watchdog_fixture(Path(tmp))
            fake_bin = self._fake_launchctl_bin(Path(tmp))
            p = self._run_block(
                self._watchdog_block(), adk, home, fake_bin=fake_bin, spawns=True
            )
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
            self.assertIn("HARNESS-END", p.stdout, "fail-open must reach the end")
            self.assertIn("✓ Relay watchdog armed", p.stdout)
            self.assertIn("pid 4242", p.stdout)
            self.assertNotIn("NOT spawned", p.stdout)

    def test_retained_fast_path_requires_a_running_pid_not_just_a_loaded_job(self):
        """`launchctl print` rc proves loaded, not running — same #5153 defect on
        the sibling branch. An unspawned job must fall through to the restart
        path instead of being silently retained."""
        deploy = (REPO_ROOT / "scripts" / "deploy-release.sh").read_text(
            encoding="utf-8"
        )
        block = deploy[deploy.index('WATCHDOG_LABEL="com.agentdesk.relay-watchdog"') :]
        retained = block.index("durable authority uninterrupted")
        self.assertNotIn(
            'if launchctl print "$LAUNCHD_DOMAIN/$WATCHDOG_LABEL" >/dev/null 2>&1; then',
            block[:retained],
            "the retained fast path must judge on the PID column, not on print rc",
        )
        self.assertIn("_wd_spawned_pid", block[:retained])

    # r4 review (PR #4399): plist values were raw-interpolated into the XML
    # heredoc, so an operator path containing &, <, or > produced an invalid
    # plist and a silently unarmed watchdog.
    def test_generated_plist_survives_xml_metachars_in_paths(self):
        import plistlib

        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk & <rel>"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            (adk / "config" / "relay-watchdog.json").write_text(
                "{}", encoding="utf-8"
            )
            home = Path(tmp) / "home"
            (home / "Library").mkdir(parents=True)

            p = self._run_block(self._watchdog_block(), adk, home)
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
            self.assertIn("HARNESS-END", p.stdout, "fail-open must reach the end")

            plist_path = (
                home / "Library/LaunchAgents/com.agentdesk.relay-watchdog.plist"
            )
            self.assertTrue(plist_path.is_file(), p.stdout + p.stderr)
            with plist_path.open("rb") as f:
                data = plistlib.load(f)  # raises on invalid XML → test fails
            # Escaping must round-trip: the parsed values are the RAW paths.
            self.assertEqual(
                data["ProgramArguments"][1], str(adk / "bin/relay-watchdog.py")
            )
            self.assertEqual(
                data["EnvironmentVariables"]["AGENTDESK_ROOT_DIR"], str(adk)
            )
            self.assertEqual(
                data["StandardOutPath"],
                str(adk / "logs/relay-watchdog.launchd.out.log"),
            )


class DeadWorktreeReverseMapTests(unittest.TestCase):
    """The liveness predicate is only sound if slug→worktree is exact."""

    def test_reverses_project_slug_to_the_owning_worktree_dir(self):
        pattern = make_re()
        transcript = (
            "/Users/alice/.claude/projects/"
            "-Users-alice--adk-release-worktrees-claude-adk-cc-20260731-220205/"
            "8cf48ae8-10e0-44a5-af3e-defc060fdf5f.jsonl"
        )
        self.assertEqual(
            relay_watchdog.worktree_dir_for_transcript(
                transcript, WORKTREE_ROOT, pattern
            ),
            Path(WORKTREE_ROOT) / "claude-adk-cc-20260731-220205",
        )

    def test_refuses_to_resolve_a_foreign_or_thread_project_dir(self):
        pattern = make_re()
        for parent in (
            "-Users-bob--adk-release-worktrees-claude-adk-cc-20260731-220205",
            "-Users-alice--adk-release-worktrees-claude-adk-cc-t123-20260731-220205",
            "-Users-alice--adk-release-worktrees-claude-adk-cc-20260731",
        ):
            with self.subTest(parent=parent):
                self.assertIsNone(
                    relay_watchdog.worktree_dir_for_transcript(
                        f"/p/{parent}/s.jsonl", WORKTREE_ROOT, pattern
                    )
                )

    def test_unreadable_directory_is_not_reported_as_absent(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        present = Path(tmp.name)
        self.assertIs(relay_watchdog.directory_presence(present), True)
        self.assertIs(
            relay_watchdog.directory_presence(present / "nope"), False
        )
        with mock.patch.object(
            relay_watchdog.os, "stat", side_effect=OSError(5, "I/O error")
        ):
            self.assertIsNone(relay_watchdog.directory_presence(present))


class DeadWorktreeRetirementTests(unittest.TestCase):
    """#5155: loss-state bound to a session that provably cannot come back.

    Shape mirrors the live 2026-08-05 state read off
    ``logs/relay-watchdog.state.json``: the transcript is a GAP owner and NOT
    pending (``pending_transcripts: []``), its worktree directory was deleted,
    and its last write was ~4.9 days before the tick — so nothing in the
    existing machinery (idle skip, pending expiry, supersession) retires it and
    the incident re-fires on its cooldown forever.
    """

    SESSION = "claude-adk-cc-20260731-220205"

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        (self.root / "logs").mkdir()
        self.worktrees = self.root / "worktrees"
        self.worktrees.mkdir()
        self.projects = self.root / "projects"
        self.proj_dir = self.projects / (
            project_slug(str(self.worktrees)) + "-" + self.SESSION
        )
        self.proj_dir.mkdir(parents=True)
        env = mock.patch.dict(
            os.environ, {"CLAUDE_PROJECTS_ROOT": str(self.projects)}
        )
        env.start()
        self.addCleanup(env.stop)
        self.channel = ChannelConfig(
            channel_id="999",
            sendmessage_key="k",
            worktree_root=str(self.worktrees),
        )
        self.now = time.time()
        self.transcript = self.proj_dir / "8cf48ae8.jsonl"

    def create_worktree(self) -> None:
        (self.worktrees / self.SESSION).mkdir()

    def write_stale_transcript(self, age_secs: float) -> None:
        epoch = self.now - age_secs
        record = json.dumps(
            {
                "type": "assistant",
                "uuid": "00000000-0000-4000-8000-000000000001",
                "timestamp": time.strftime(
                    "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                ),
                "message": {
                    "content": [
                        {"type": "text", "text": "undelivered block behind lost=1"}
                    ]
                },
            }
        )
        self.transcript.write_text(record + "\n", encoding="utf-8")
        os.utime(self.transcript, (epoch, epoch))

    def open_incident_state(self) -> dict:
        """Persisted shape of an already-open gap incident on this path."""
        path = str(self.transcript)
        return {
            "alerting": True,
            "gap_since": self.now - 4.9 * 86400,
            "issue_url": "https://example.test/issues/5072",
            "last_alert": 0.0,
            relay_watchdog.GAP_TRANSCRIPT_KEY: path,
            relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY: [path],
            relay_watchdog.PENDING_TRANSCRIPTS_KEY: [],
            SELECTED_TRANSCRIPT_KEY: path,
            relay_watchdog.TRANSCRIPT_SIZES_KEY: {
                path: self.transcript.stat().st_size
            },
            relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY: {
                f"{index:064x}": {
                    "confirmed_at": self.now - 5 * 86400,
                    "epoch": self.now - 5 * 86400,
                    "path": path,
                }
                for index in range(3)
            },
            relay_watchdog.PERMANENT_LOSS_TOTAL_KEY: 3,
        }

    def make_rt(self, **cfg_overrides) -> FakeRuntime:
        cfg = Config(channels=(self.channel,), **cfg_overrides)
        rt = FakeRuntime(cfg, self.root)
        rt.haystack = ""
        return rt

    def tick(self, rt: FakeRuntime, state: dict, at: float) -> None:
        tick_channel(rt, self.channel, state, at)

    # ── dead ────────────────────────────────────────────────────────────────

    def test_deleted_worktree_terminates_the_gap_incident(self):
        self.write_stale_transcript(4.9 * 86400)
        rt = self.make_rt()
        state = {"999": self.open_incident_state()}
        chs = state["999"]

        # Tick 1 only opens the absence window; the incident is still live.
        self.tick(rt, state, self.now)
        self.assertTrue(chs.get("alerting"), rt.log_lines)
        self.assertIn(
            str(self.transcript),
            chs.get(relay_watchdog.DEAD_WORKTREE_ABSENT_SINCE_KEY, {}),
        )
        chs[relay_watchdog.ORPHAN_STRANDED_SINCE_KEY] = {
            str(self.transcript): self.now
        }

        self.tick(
            rt, state, self.now + relay_watchdog.DEAD_WORKTREE_CONFIRM_SECS + 1
        )

        self.assertFalse(chs.get("alerting"), (chs, rt.log_lines))
        self.assertNotIn("gap_since", chs)
        self.assertNotIn("issue_url", chs)
        self.assertNotIn(relay_watchdog.GAP_TRANSCRIPT_KEY, chs)
        self.assertNotIn(relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY, chs)
        self.assertIn(
            str(self.transcript), chs[relay_watchdog.RETIRED_TRANSCRIPTS_KEY]
        )
        self.assertNotIn(
            str(self.transcript),
            chs.get(relay_watchdog.ORPHAN_STRANDED_SINCE_KEY, {}),
        )
        self.assertTrue(
            any(
                "dead-worktree-loss-state-retired" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )
        # Retirement is not a delivery claim.
        self.assertFalse(
            any("릴레이 갭 해소" in body for body, _ in rt.alerts), rt.alerts
        )
        self.assertTrue(
            any("죽은 세션" in body for body, _ in rt.alerts), rt.alerts
        )
        # The historical ledger is monotonic (#4954): retirement must not
        # decrease the projection nor drop a tombstone.
        self.assertEqual(permanent_loss_total(chs), 3)
        self.assertEqual(
            set(chs[relay_watchdog.PERMANENT_LOSS_TOMBSTONES_KEY]),
            {f"{index:064x}" for index in range(3)},
        )

    def test_terminated_incident_stops_re_alerting(self):
        self.write_stale_transcript(4.9 * 86400)
        rt = self.make_rt()
        state = {"999": self.open_incident_state()}
        at = self.now
        self.tick(rt, state, at)
        at += relay_watchdog.DEAD_WORKTREE_CONFIRM_SECS + 1
        self.tick(rt, state, at)
        alerts_after_retirement = len(rt.alerts)
        gap_alerts_before = sum(
            1 for body, _ in rt.alerts if "릴레이 갭 감지" in body
        )
        # Five further re-alert windows must produce no new alert of any kind.
        for _ in range(5):
            at += rt.cfg.realert_secs + 1
            self.tick(rt, state, at)
        self.assertEqual(len(rt.alerts), alerts_after_retirement, rt.alerts)
        self.assertEqual(
            sum(1 for body, _ in rt.alerts if "릴레이 갭 감지" in body),
            gap_alerts_before,
            rt.alerts,
        )

    def test_absence_shorter_than_confirm_window_keeps_the_incident_open(self):
        self.write_stale_transcript(4.9 * 86400)
        rt = self.make_rt()
        state = {"999": self.open_incident_state()}
        chs = state["999"]
        self.tick(rt, state, self.now)
        self.tick(
            rt, state, self.now + relay_watchdog.DEAD_WORKTREE_CONFIRM_SECS - 1
        )
        self.assertTrue(chs.get("alerting"), rt.log_lines)
        self.assertIn(
            str(self.transcript),
            chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY],
        )
        self.assertTrue(
            any(
                "dead-worktree-absence-pending" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )

    def test_selector_divergence_window_is_voided_with_the_dead_session(self):
        self.write_stale_transcript(4.9 * 86400)
        rt = self.make_rt()
        state = {"999": self.open_incident_state()}
        chs = state["999"]
        chs["selector_alerting"] = True
        chs["selector_diverged_since"] = self.now - 4.9 * 86400
        chs[relay_watchdog.SELECTOR_DIVERGED_TRANSCRIPT_KEY] = str(
            self.transcript
        )
        self.tick(rt, state, self.now)
        self.tick(
            rt, state, self.now + relay_watchdog.DEAD_WORKTREE_CONFIRM_SECS + 1
        )
        self.assertNotIn("selector_alerting", chs)
        self.assertNotIn("selector_diverged_since", chs)
        self.assertNotIn(relay_watchdog.SELECTOR_DIVERGED_TRANSCRIPT_KEY, chs)

    # ── alive ───────────────────────────────────────────────────────────────

    def test_live_but_idle_session_keeps_reporting_its_gap(self):
        """NON-SUPPRESSION: quiet is not dead.

        Identical to the dead case in every respect the watchdog can otherwise
        observe — same 4.9-day-idle transcript, same open incident, same number
        of ticks — except the worktree directory still exists. The gap must
        survive untouched and keep alerting.
        """
        self.create_worktree()
        self.write_stale_transcript(4.9 * 86400)
        rt = self.make_rt()
        state = {"999": self.open_incident_state()}
        chs = state["999"]
        at = self.now
        self.tick(rt, state, at)
        for _ in range(6):
            at += relay_watchdog.DEAD_WORKTREE_CONFIRM_SECS + 1
            self.tick(rt, state, at)

        self.assertTrue(chs.get("alerting"), (chs, rt.log_lines))
        self.assertIn(
            str(self.transcript),
            chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY],
        )
        self.assertNotIn(
            str(self.transcript),
            chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {}),
        )
        self.assertNotIn(relay_watchdog.DEAD_WORKTREE_ABSENT_SINCE_KEY, chs)
        self.assertFalse(
            any("dead-worktree" in line for line in rt.log_lines), rt.log_lines
        )
        self.assertTrue(
            any("릴레이 갭 감지" in body for body, _ in rt.alerts), rt.alerts
        )

    def test_a_transient_listing_failure_never_retires_a_fresh_transcript(self):
        """P1-A: a missing candidate must fail CLOSED, like the sibling guard.

        `channel_project_dirs` returns [] on ANY OSError from `root.iterdir()`,
        and `_regular_file_stat_without_symlink` returns None on a single failed
        stat. One transient listing failure therefore empties
        `candidate_by_path` — and a guard written as `candidate is not None and
        ...` would skip entirely and retire a transcript written seconds ago.
        """
        self.write_stale_transcript(4.9 * 86400)
        rt = self.make_rt()
        state = {"999": self.open_incident_state()}
        chs = state["999"]
        self.tick(rt, state, self.now)
        retire_at = self.now + relay_watchdog.DEAD_WORKTREE_CONFIRM_SECS + 1
        # The session is writing again as of this very tick (idle 0s)...
        os.utime(self.transcript, (retire_at, retire_at))
        # ...but this tick's directory listing failed, so nothing is observed.
        with mock.patch.object(
            relay_watchdog, "transcript_candidates", return_value=[]
        ):
            self.tick(rt, state, retire_at)

        self.assertNotIn(
            str(self.transcript),
            chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {}),
        )
        self.assertTrue(chs.get("alerting"), (chs, rt.log_lines))
        self.assertIn(
            str(self.transcript),
            chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY],
        )
        self.assertTrue(
            any(
                "dead-worktree-retirement-declined reason=transcript_unobserved"
                in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )

    def test_termination_defers_while_the_notice_is_swallowed_by_cooldown(self):
        """P1-B: loss-state never terminates in silence.

        `_alert_pending_retirement` shares ONE cooldown key across every reason,
        so an idle/read-failure retirement seconds earlier would otherwise
        swallow the dead-session notice while the incident closed anyway.
        """
        self.write_stale_transcript(4.9 * 86400)
        rt = self.make_rt()
        state = {"999": self.open_incident_state()}
        chs = state["999"]
        self.tick(rt, state, self.now)
        retire_at = self.now + relay_watchdog.DEAD_WORKTREE_CONFIRM_SECS + 1
        # An unrelated retirement 10s ago burned the shared cooldown.
        chs[relay_watchdog.LAST_PENDING_TRANSCRIPT_RETIREMENT_ALERT_KEY] = (
            retire_at - 10
        )
        self.tick(rt, state, retire_at)

        self.assertTrue(chs.get("alerting"), (chs, rt.log_lines))
        self.assertIn(
            str(self.transcript),
            chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY],
        )
        self.assertNotIn(
            str(self.transcript),
            chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {}),
        )
        self.assertFalse(
            any("죽은 세션" in body for body, _ in rt.alerts), rt.alerts
        )
        self.assertTrue(
            any(
                "dead-worktree-retirement-deferred" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )

        # Only once the notice actually goes out does the incident close.
        self.tick(rt, state, retire_at + rt.cfg.realert_secs + 1)
        self.assertTrue(
            any("죽은 세션" in body for body, _ in rt.alerts), rt.alerts
        )
        self.assertFalse(chs.get("alerting"), (chs, rt.log_lines))

    def test_termination_defers_when_the_notice_cannot_be_delivered(self):
        """P1-B: a failed post is 'not told', so the authority stays open."""
        self.write_stale_transcript(4.9 * 86400)
        rt = self.make_rt()
        state = {"999": self.open_incident_state()}
        chs = state["999"]
        self.tick(rt, state, self.now)
        rt.alert_succeeds = False
        retire_at = self.now + relay_watchdog.DEAD_WORKTREE_CONFIRM_SECS + 1
        self.tick(rt, state, retire_at)

        self.assertTrue(chs.get("alerting"), (chs, rt.log_lines))
        self.assertNotIn(
            str(self.transcript),
            chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {}),
        )
        self.assertTrue(
            any(
                "transcript-retirement-alert undelivered" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )
        # The failed attempt must not have burned the cooldown: the very next
        # tick after transport recovery closes it.
        rt.alert_succeeds = True
        self.tick(rt, state, retire_at + 1)
        self.assertFalse(chs.get("alerting"), (chs, rt.log_lines))

    def live_successor(self) -> Path:
        """A second, still-existing session family for this same channel."""
        name = "claude-adk-cc-20260805-200329"
        (self.worktrees / name).mkdir()
        live_dir = self.projects / (
            project_slug(str(self.worktrees)) + "-" + name
        )
        live_dir.mkdir(parents=True)
        return live_dir / "7e3abf13.jsonl"

    def desynced_probe(self, at: float) -> WatcherStateProbe:
        stale_ms = int((at - COVERAGE_ACTIVITY_FRESH_SECS * 3) * 1000)
        return WatcherStateProbe(
            status=200,
            attached=True,
            desynced=True,
            relay_activity=CoverageActivityProbe(
                relay_stall_state="active_foreground_stream",
                active_turn="foreground",
                queue_depth=1,
                tmux_alive=True,
                watcher_attached=True,
                watcher_attached_stale=False,
                watcher_owns_live_relay=True,
                last_outbound_activity_ms=stale_ms,
                last_relay_ts_ms=stale_ms,
                desynced=True,
            ),
            inflight_updated_at=None,
        )

    def test_retirement_hands_the_desync_alarm_to_the_live_successor(self):
        """P1-C: proven from the ACTUAL observed shape, not a convenient one.

        Live log, one tick, 2026-08-05T22:03:50-51Z::

            transcript-select reason=growth path=.../20260731-220205/8cf48ae8...
            COVERAGE ALERT reason=attached_but_desynced ticks=18
            gap persists path=.../20260731-220205/8cf48ae8... lost=1

        The selected transcript is the DEAD path carrying `lost=1`, so
        `zero_loss_observed` is False, `growing_relay_stall` is False, and that
        COVERAGE ALERT was corroborated by `alerting`/`gap_since` ALONE — the two
        keys this change pops. Starting from exactly that shape, this walks
        retirement -> reselection -> alarm and pins that the alarm survives on
        the independent corroborator once the selector reaches the live session.
        """
        self.write_stale_transcript(4.9 * 86400)
        successor = self.live_successor()

        def record(epoch: float, text: str) -> str:
            return json.dumps(
                {
                    "type": "assistant",
                    "uuid": f"00000000-0000-4000-8000-{abs(hash(text)) % 10**12:012d}",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch)
                    ),
                    "message": {"content": [{"type": "text", "text": text}]},
                }
            )

        successor.write_text(
            record(self.now - 30, "live block one delivered") + "\n",
            encoding="utf-8",
        )
        os.utime(successor, (self.now - 30, self.now - 30))

        rt = self.make_rt()
        rt.haystack = norm("live block one delivered")
        rt.live_sessions = {expected_tmux_session_name(self.channel)}
        rt.watcher_probe = self.desynced_probe(self.now)
        state = {"999": self.open_incident_state()}
        chs = state["999"]
        # The successor is already tracked (as in production), so it debuts as
        # a known path and the selector stays stuck on the dead one — which is
        # precisely the logged shape being reproduced.
        chs[relay_watchdog.TRANSCRIPT_SIZES_KEY][str(successor)] = (
            successor.stat().st_size
        )

        # Tick 1 reproduces the logged shape: dead path selected, lost=1, the
        # gap incident open, the desync accumulating toward confirmation.
        self.tick(rt, state, self.now)
        self.assertEqual(chs[SELECTED_TRANSCRIPT_KEY], str(self.transcript))
        self.assertTrue(chs.get("alerting"))
        self.assertTrue(
            any("**1건**" in body for body, _ in rt.alerts), rt.alerts
        )

        # Tick 2: the live session writes and is relayed; the dead session is
        # retired and the selector moves to the successor.
        at = self.now + rt.cfg.realert_secs + 1
        with successor.open("a", encoding="utf-8") as f:
            f.write(record(at - 5, "live block two delivered") + "\n")
        os.utime(successor, (at - 5, at - 5))
        rt.haystack = (
            norm("live block one delivered")
            + " "
            + norm("live block two delivered")
        )
        rt.watcher_probe = self.desynced_probe(at)
        rt.alerts.clear()
        rt.log_lines.clear()
        self.tick(rt, state, at)

        # The gap corroborators are gone...
        self.assertFalse(chs.get("alerting"), (chs, rt.log_lines))
        self.assertNotIn("gap_since", chs)
        self.assertEqual(chs[SELECTED_TRANSCRIPT_KEY], str(successor))
        # ...and the desync still alarms, on `growing_relay_stall` alone.
        self.assertTrue(
            any("attached_but_desynced" in body for body, _ in rt.alerts),
            (rt.alerts, rt.log_lines),
        )
        self.assertTrue(chs.get("coverage_alerting"))
        self.assertFalse(
            any(
                "coverage desync uncorroborated" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )

    def test_unreadable_worktree_root_never_retires_anything(self):
        self.write_stale_transcript(4.9 * 86400)
        rt = self.make_rt()
        state = {"999": self.open_incident_state()}
        chs = state["999"]
        self.tick(rt, state, self.now)
        self.assertIn(
            str(self.transcript),
            chs.get(relay_watchdog.DEAD_WORKTREE_ABSENT_SINCE_KEY, {}),
        )
        # The whole root disappears (dismount / rename): every worktree now
        # stats absent, which must NOT be read as every session being dead.
        self.worktrees.rmdir()
        self.tick(
            rt, state, self.now + relay_watchdog.DEAD_WORKTREE_CONFIRM_SECS + 1
        )
        self.assertTrue(chs.get("alerting"), (chs, rt.log_lines))
        self.assertIn(
            str(self.transcript),
            chs[relay_watchdog.GAP_OWNER_TRANSCRIPTS_KEY],
        )
        self.assertNotIn(relay_watchdog.DEAD_WORKTREE_ABSENT_SINCE_KEY, chs)
        self.assertTrue(
            any(
                "dead-worktree-probe-reset" in line for line in rt.log_lines
            ),
            rt.log_lines,
        )

    def test_a_still_writing_transcript_is_never_retired(self):
        """Belt-and-braces: even with the directory gone, a growing file stays."""
        self.write_stale_transcript(10.0)
        rt = self.make_rt()
        state = {"999": self.open_incident_state()}
        chs = state["999"]
        self.tick(rt, state, self.now)
        self.tick(
            rt, state, self.now + relay_watchdog.DEAD_WORKTREE_CONFIRM_SECS + 1
        )
        self.assertNotIn(
            str(self.transcript),
            chs.get(relay_watchdog.RETIRED_TRANSCRIPTS_KEY, {}),
        )
        self.assertTrue(
            any(
                "dead-worktree-retirement-declined" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )


class AnnounceSkipLoggingTests(unittest.TestCase):
    """#5155 correction 1: 'not attempted' must not read as 'attempted and failed'."""

    def test_empty_announce_to_logs_that_the_primary_was_skipped(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        (root / "logs").mkdir()
        ch = ChannelConfig(
            channel_id="999", sendmessage_key="k", worktree_root=WORKTREE_ROOT
        )
        rt = Runtime(Config(channels=(ch,)), root)
        lines: list[str] = []
        rt.log = lines.append  # type: ignore[method-assign]
        with mock.patch.object(
            relay_watchdog.subprocess,
            "run",
            return_value=subprocess.CompletedProcess([], 0, "", ""),
        ) as run:
            self.assertTrue(rt.alert(ch, "body"))
        self.assertEqual(run.call_count, 1, "primary must not be attempted")
        self.assertTrue(
            any("announce bot skipped" in line for line in lines), lines
        )


class BoundedReadCompletenessTests(unittest.TestCase):
    """4987 S6: `read_complete` is a claim about the BOUNDED READ, and the
    watchdog may only make it when the page it got back proves the read was not
    truncated. Everything else fails closed, because the reader grants authority
    on the strength of this one boolean."""

    def test_a_short_page_with_no_drops_is_complete(self):
        self.assertTrue(bounded_read_is_complete(99, 100, 0))
        self.assertTrue(bounded_read_is_complete(0, 100, 0))

    def test_a_full_page_is_not_complete(self):
        # Indistinguishable from "the page filled and older messages exist".
        self.assertFalse(bounded_read_is_complete(100, 100, 0))

    def test_an_over_full_page_is_not_complete(self):
        self.assertFalse(bounded_read_is_complete(101, 100, 0))

    def test_any_dropped_entry_defeats_completeness(self):
        # A skipped malformed entry is a message the matcher never compared.
        self.assertFalse(bounded_read_is_complete(50, 100, 1))

    def test_nonsensical_counts_fail_closed(self):
        for args in ((-1, 100, 0), (50, 0, 0), (50, -1, 0), (50, 100, -1)):
            with self.subTest(args=args):
                self.assertFalse(bounded_read_is_complete(*args))


class HaystackReadCompletenessTests(unittest.TestCase):
    """`Runtime.discord_haystack` must publish an honest `read_complete` for the
    read it just did, including on every failure path — a stale True from a
    previous tick would hand the reader authority it did not earn."""

    def _probe(self, stdout: str, returncode: int = 0):
        """Returns (haystack result, rt.last_read_complete, argv)."""
        seen: list[list[str]] = []

        def fake_run(argv, **kwargs):
            seen.append(list(argv))
            return subprocess.CompletedProcess(argv, returncode, stdout=stdout)

        with tempfile.TemporaryDirectory() as tmp:
            rt = Runtime(Config(channels=(TICK_CHANNEL,)), Path(tmp))
            # Arm the trap: a previous tick's success must not survive.
            rt.last_read_complete = True
            with mock.patch.object(
                relay_watchdog.subprocess, "run", side_effect=fake_run
            ):
                out = rt.discord_haystack("999")
            return out, rt.last_read_complete, seen[0] if seen else []

    def _page(self, count: int) -> str:
        return json.dumps(
            [
                {"author": {"bot": True}, "content": f"msg {index}"}
                for index in range(count)
            ]
        )

    def test_the_read_asks_for_exactly_the_named_bound(self):
        # `bounded_read_is_complete` compares against DISCORD_READ_LIMIT, so a
        # flag that drifted from the constant would silently mis-judge the page.
        _, _, argv = self._probe(self._page(1))
        self.assertIn("--limit", argv)
        self.assertEqual(argv[argv.index("--limit") + 1], str(DISCORD_READ_LIMIT))

    def test_a_short_page_marks_the_read_complete(self):
        out, complete, _ = self._probe(self._page(DISCORD_READ_LIMIT - 1))
        self.assertIsNotNone(out)
        self.assertTrue(complete)

    def test_a_full_page_leaves_the_read_incomplete(self):
        out, complete, _ = self._probe(self._page(DISCORD_READ_LIMIT))
        self.assertIsNotNone(out)
        self.assertFalse(complete)

    def test_a_mixed_payload_leaves_the_read_incomplete(self):
        payload = json.dumps([{"author": {"bot": True}, "content": "kept"}, None])
        out, complete, _ = self._probe(payload)
        self.assertIsNotNone(out, "partial data still beats blindness")
        self.assertFalse(complete)

    def test_a_dict_payload_carrying_neither_key_never_reads_as_complete(self):
        # `{}` passes every shape check by accident: `.get("messages",
        # .get("data", []))` hands back `[]`, which is indistinguishable from a
        # genuinely empty channel — returned=0, skipped=0, `read_complete=true`.
        # A response that broke the contract must not inherit an empty channel's
        # whole-history authority (r1 review, #5071 T4-B5).
        out, complete, _ = self._probe("{}")
        self.assertEqual(out, "", "the haystack stays best-effort")
        self.assertFalse(complete)

    def test_a_dict_payload_under_either_key_still_reads_as_complete(self):
        # The narrowing above is about the keys being ABSENT, not about the dict
        # form. Both accepted keys must keep earning an honest True.
        for key in ("messages", "data"):
            with self.subTest(key=key):
                page = json.loads(self._page(DISCORD_READ_LIMIT - 1))
                out, complete, _ = self._probe(json.dumps({key: page}))
                self.assertIsNotNone(out)
                self.assertTrue(complete)

    def test_entries_without_comparable_fields_leave_the_read_incomplete(self):
        # Each of these parses, so it survives to the matcher — as ''. An entry
        # compared as '' is an entry that was never compared, so it cannot count
        # toward "this page was judged in full" even though it raises nothing.
        for entry in (
            {"author": None, "content": None},
            {"author": {"bot": True}, "content": None},
            {"author": {"bot": True}},
            {"content": "no author"},
            {},
        ):
            with self.subTest(entry=entry):
                out, complete, _ = self._probe(
                    json.dumps([{"author": {"bot": True}, "content": "kept"}, entry])
                )
                self.assertEqual(out, "kept", "partial data still beats blindness")
                self.assertFalse(complete)

    def test_every_read_failure_path_resets_a_previous_true(self):
        for stdout, returncode in (
            ("", 1),
            ("not json", 0),
            ("null", 0),
            ('{"messages": 5}', 0),
            ("[null, 7]", 0),
        ):
            with self.subTest(stdout=stdout, returncode=returncode):
                out, complete, _ = self._probe(stdout, returncode)
                self.assertIsNone(out)
                self.assertFalse(complete)


class ExternalVerdictIdentityTests(unittest.TestCase):
    """4987 §-1.5 gates the reader on incarnation identity, so a partial or
    ambiguous identity has to become NO identity: the reader would otherwise
    match on whichever legs happened to arrive."""

    SESSION = "AgentDesk-claude-adk-cc"

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.sessions = self.root / "runtime" / "sessions"
        self.sessions.mkdir(parents=True)
        env = mock.patch.dict(
            os.environ, {"AGENTDESK_ROOT_DIR": str(self.root), "TMPDIR": str(self.root)}
        )
        env.start()
        self.addCleanup(env.stop)
        self.transcript = self.root / "s.jsonl"
        self.transcript.write_text("{}\n", encoding="utf-8")

    def marker(self, suffix: str, body: str = "", prefix: str = "agentdesk-abc-h") -> Path:
        path = self.sessions / f"{prefix}-{self.SESSION}.{suffix}"
        path.write_text(body, encoding="utf-8")
        return path

    def identity(self):
        return session_incarnation_identity(self.SESSION, self.transcript)

    def test_identity_carries_generation_nonce_and_file_id(self):
        generation = self.marker("generation")
        self.marker("spawn_nonce", "nonce-a\n")
        transcript_stat = self.transcript.stat()
        self.assertEqual(
            self.identity(),
            {
                "generation_mtime_ns": generation.stat().st_mtime_ns,
                "spawn_nonce": "nonce-a",
                "transcript_file_id": {
                    "dev": transcript_stat.st_dev,
                    "ino": transcript_stat.st_ino,
                },
            },
        )

    def test_a_missing_generation_marker_yields_no_identity(self):
        self.marker("spawn_nonce", "nonce-a")
        self.assertIsNone(self.identity())

    def test_an_ambiguous_generation_marker_yields_no_identity(self):
        # Two owner-marker hashes for one session name: which incarnation the
        # verdict belongs to is unknowable, so publish no identity at all.
        self.marker("generation", prefix="agentdesk-aaa-h")
        self.marker("generation", prefix="agentdesk-bbb-h")
        self.assertIsNone(self.identity())

    def test_an_absent_nonce_marker_is_a_none_leg_not_a_failure(self):
        # `read_spawn_nonce` returns None for a missing/empty marker, and the
        # reader compares that None for EQUALITY rather than as a wildcard.
        self.marker("generation")
        identity = self.identity()
        self.assertIsNotNone(identity)
        self.assertIsNone(identity["spawn_nonce"])

    def test_an_empty_nonce_marker_is_the_same_none_leg(self):
        self.marker("generation")
        self.marker("spawn_nonce", "   \n")
        identity = self.identity()
        self.assertIsNotNone(identity)
        self.assertIsNone(identity["spawn_nonce"])

    def test_an_ambiguous_nonce_marker_yields_no_identity(self):
        self.marker("generation")
        self.marker("spawn_nonce", "nonce-a", prefix="agentdesk-aaa-h")
        self.marker("spawn_nonce", "nonce-b", prefix="agentdesk-bbb-h")
        self.assertIsNone(self.identity())

    def test_a_missing_transcript_yields_no_identity(self):
        self.marker("generation")
        self.transcript.unlink()
        self.assertIsNone(self.identity())


class ExternalVerdictEpochTests(unittest.TestCase):
    def test_the_epoch_advances_once_per_publish(self):
        chs: dict = {}
        self.assertEqual(next_external_verdict_epoch(chs), 1)
        self.assertEqual(next_external_verdict_epoch(chs), 2)
        self.assertEqual(chs[EXTERNAL_VERDICT_EPOCH_KEY], 2)

    def test_a_lost_state_file_regresses_the_epoch_rather_than_inflating_it(self):
        # `load_state` returns {} for a missing/corrupt state, so a respawned
        # watchdog restarts at 1. The reader drops the regression, which costs
        # the external tier its authority instead of handing it a stale one.
        self.assertEqual(next_external_verdict_epoch({}), 1)

    def test_a_non_integer_or_negative_epoch_restarts_at_one(self):
        for stored in ("7", 7.5, True, -1, None, [7]):
            with self.subTest(stored=stored):
                self.assertEqual(
                    next_external_verdict_epoch(
                        {EXTERNAL_VERDICT_EPOCH_KEY: stored}
                    ),
                    1,
                )


class ExternalVerdictRecordTests(unittest.TestCase):
    IDENTITY = {
        "generation_mtime_ns": 1785321000000000000,
        "spawn_nonce": "nonce-a",
        "transcript_file_id": {"dev": 17, "ino": 4242},
    }

    def record(self, state: str, **overrides):
        defaults = dict(
            observed_at_epoch_ms=1785321543885,
            watchdog_epoch=7,
            read_complete=True,
            identity=self.IDENTITY,
        )
        defaults.update(overrides)
        return build_external_verdict_record(
            Verdict(
                state=state,
                blocks=4,
                stale=3,
                lost=3,
                delivered_ts=1785320700.0,
                gap_secs=843.885,
            ),
            **defaults,
        )

    def test_every_verdict_state_has_a_wire_spelling(self):
        for state, spelling in (
            (STATE_OK, "ok"),
            (STATE_LAGGING, "degraded"),
            (STATE_GAP, "unreachable"),
        ):
            with self.subTest(state=state):
                self.assertEqual(self.record(state)["verdict"], spelling)

    def test_an_unnamed_state_publishes_as_unknown_not_ok(self):
        # A state added to `evaluate` without a spelling here must not acquire
        # the mildest verdict by default.
        self.assertEqual(self.record("some_new_state")["verdict"], "unknown")

    def test_the_added_identity_and_completeness_fields_are_always_present(self):
        for identity in (self.IDENTITY, None):
            with self.subTest(identity=identity):
                record = self.record(STATE_GAP, identity=identity)
                for key in (
                    "generation_mtime_ns",
                    "spawn_nonce",
                    "transcript_file_id",
                    "watchdog_epoch",
                    "read_complete",
                ):
                    self.assertIn(key, record)

    def test_the_older_payload_keys_are_kept_unchanged(self):
        record = self.record(STATE_GAP)
        self.assertEqual(record["source"], "relay_watchdog")
        self.assertEqual(record["watchdog_state_version"], 1)
        self.assertEqual(record["reason"], STATE_GAP)
        self.assertEqual(record["lost_blocks"], 3)
        self.assertEqual(record["observed_at_epoch_ms"], 1785321543885)
        self.assertEqual(record["last_delivered_ts"], 1785320700000)

    def test_an_unobserved_delivery_publishes_a_zero_watermark(self):
        record = build_external_verdict_record(
            Verdict(
                state=STATE_GAP,
                blocks=1,
                stale=1,
                lost=1,
                delivered_ts=0.0,
                gap_secs=relay_watchdog.GAP_SECS_UNOBSERVED,
            ),
            1785321543885,
            1,
            True,
            None,
        )
        self.assertEqual(record["last_delivered_ts"], 0)

    def test_read_complete_and_identity_stay_independent(self):
        # The reader classifies an incomplete read and an unmatched incarnation
        # differently, so folding them together here would hide which failed.
        self.assertTrue(self.record(STATE_GAP, identity=None)["read_complete"])
        without_read = self.record(STATE_GAP, read_complete=False)
        self.assertFalse(without_read["read_complete"])
        self.assertEqual(without_read["generation_mtime_ns"], 1785321000000000000)


class ExternalVerdictSidecarTickTests(unittest.TestCase):
    """The wiring test: `tick_channel` must actually publish the sidecar with
    every field filled. The pure tests above cannot catch an unwired writer.

    Deliberately not a subclass of `TickChannelTests`: inheriting it would rerun
    that whole suite under this fixture for no coverage.
    """

    SESSION = "AgentDesk-claude-adk-cc"

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        (self.root / "logs").mkdir()
        self.projects = self.root / "projects"
        self.proj_dir = self.projects / (
            "-Users-alice--adk-release-worktrees-claude-adk-cc-20260709-140500"
        )
        self.proj_dir.mkdir(parents=True)
        self.sessions = self.root / "runtime" / "sessions"
        self.sessions.mkdir(parents=True)
        env = mock.patch.dict(
            os.environ,
            {
                "CLAUDE_PROJECTS_ROOT": str(self.projects),
                "AGENTDESK_ROOT_DIR": str(self.root),
                "TMPDIR": str(self.root),
            },
        )
        env.start()
        self.addCleanup(env.stop)
        self.now = time.time()
        self.generation = self.sessions / f"agentdesk-abc-h-{self.SESSION}.generation"
        self.generation.write_text("1", encoding="utf-8")
        (self.sessions / f"agentdesk-abc-h-{self.SESSION}.spawn_nonce").write_text(
            "nonce-a\n", encoding="utf-8"
        )
        self.transcript = self.proj_dir / "s.jsonl"

    def gap_rt(self) -> FakeRuntime:
        """One stale block that was never delivered → the channel ranks GAP."""
        self.transcript.write_text(
            json.dumps(
                {
                    "type": "assistant",
                    "uuid": "00000000-0000-4000-8000-000000000000",
                    "timestamp": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(self.now - 2000)
                    ),
                    "message": {
                        "content": [
                            {"type": "text", "text": "never delivered block"}
                        ]
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        rt = FakeRuntime(Config(channels=(TICK_CHANNEL,)), self.root)
        rt.haystack = ""
        return rt

    def sidecar(self, rt: FakeRuntime) -> dict:
        path = rt.external_verdict_path(TICK_CHANNEL.channel_id)
        return json.loads(path.read_text(encoding="utf-8"))

    def test_a_gap_tick_publishes_the_full_schema(self):
        rt = self.gap_rt()
        rt.last_read_complete = True
        tick_channel(rt, TICK_CHANNEL, {}, self.now)
        record = self.sidecar(rt)
        transcript_stat = self.transcript.stat()
        self.assertEqual(record["verdict"], "unreachable")
        self.assertEqual(record["reason"], STATE_GAP)
        self.assertEqual(record["source"], "relay_watchdog")
        self.assertEqual(record["watchdog_state_version"], 1)
        self.assertTrue(record["read_complete"])
        self.assertEqual(record["watchdog_epoch"], 1)
        self.assertEqual(
            record["generation_mtime_ns"], self.generation.stat().st_mtime_ns
        )
        self.assertEqual(record["spawn_nonce"], "nonce-a")
        self.assertEqual(
            record["transcript_file_id"],
            {"dev": transcript_stat.st_dev, "ino": transcript_stat.st_ino},
        )

    def test_an_incomplete_read_publishes_read_complete_false(self):
        rt = self.gap_rt()
        rt.last_read_complete = False
        tick_channel(rt, TICK_CHANNEL, {}, self.now)
        self.assertFalse(self.sidecar(rt)["read_complete"])

    def test_the_published_epoch_advances_across_ticks(self):
        rt = self.gap_rt()
        state: dict = {}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(self.sidecar(rt)["watchdog_epoch"], 1)
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        self.assertEqual(self.sidecar(rt)["watchdog_epoch"], 2)

    def test_a_missing_generation_marker_publishes_null_identity_legs(self):
        self.generation.unlink()
        rt = self.gap_rt()
        rt.last_read_complete = True
        tick_channel(rt, TICK_CHANNEL, {}, self.now)
        record = self.sidecar(rt)
        self.assertIsNone(record["generation_mtime_ns"])
        self.assertIsNone(record["transcript_file_id"])
        self.assertTrue(
            record["read_complete"],
            "the read is still complete; only the identity is unknown",
        )

    def test_a_publish_failure_does_not_break_the_tick(self):
        rt = self.gap_rt()
        with mock.patch.object(
            relay_watchdog.Path, "mkdir", side_effect=OSError("read-only")
        ):
            tick_channel(rt, TICK_CHANNEL, {}, self.now)
        self.assertTrue(
            any("external verdict publish failed" in line for line in rt.log_lines),
            rt.log_lines,
        )
        self.assertTrue(rt.alerts, "the gap alert must still go out")

    def _swap_transcript_during_judgment(self):
        """Replace the transcript with a fresh inode between read and publish.

        `evaluate` is the first thing the judgment loop does after reading the
        candidate and snapshotting its identity, so a swap here reproduces the
        window the publish site has to close: the verdict describes the inode that
        was read, while a path re-stat at publish time would see the new one.
        """
        real_evaluate = relay_watchdog.evaluate
        swapped: list[int] = []

        def swap_then_evaluate(*args, **kwargs):
            if not swapped:
                replacement = self.proj_dir / "s.jsonl.replacement"
                replacement.write_text(
                    self.transcript.read_text(encoding="utf-8"), encoding="utf-8"
                )
                replacement.replace(self.transcript)
                swapped.append(self.transcript.stat().st_ino)
            return real_evaluate(*args, **kwargs)

        return swapped, mock.patch.object(
            relay_watchdog, "evaluate", side_effect=swap_then_evaluate
        )

    def test_a_transcript_swapped_before_publish_loses_its_authority(self):
        rt = self.gap_rt()
        rt.last_read_complete = True
        judged_ino = self.transcript.stat().st_ino
        swapped, patch = self._swap_transcript_during_judgment()
        with patch:
            tick_channel(rt, TICK_CHANNEL, {}, self.now)
        self.assertNotEqual(
            swapped[0], judged_ino, "the fixture has to really replace the inode"
        )
        record = self.sidecar(rt)
        # The verdict itself is still published — this tier only ever loses
        # authority, never the observation.
        self.assertEqual(record["verdict"], "unreachable")
        self.assertIsNone(
            record["transcript_file_id"],
            "the new inode must not inherit the retired file's verdict",
        )
        self.assertIsNone(record["generation_mtime_ns"])
        self.assertIsNone(record["spawn_nonce"])
        self.assertFalse(
            record["read_complete"],
            "a verdict that cannot name its subject keeps no read authority",
        )
        self.assertTrue(
            any(
                "external-verdict-identity-superseded" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )

    def test_an_untouched_transcript_keeps_its_identity_through_publish(self):
        # The negative half: revalidation must not strip identity from the
        # ordinary tick, where nothing moves between judgment and publish.
        rt = self.gap_rt()
        rt.last_read_complete = True
        tick_channel(rt, TICK_CHANNEL, {}, self.now)
        record = self.sidecar(rt)
        self.assertEqual(
            record["transcript_file_id"],
            {
                "dev": self.transcript.stat().st_dev,
                "ino": self.transcript.stat().st_ino,
            },
        )
        self.assertTrue(record["read_complete"])
        self.assertFalse(
            any(
                "external-verdict-identity-superseded" in line
                for line in rt.log_lines
            ),
            rt.log_lines,
        )


class RevalidateIncarnationIdentityTests(unittest.TestCase):
    """`revalidate_incarnation_identity` has to separate "no identity known" from
    "the identity moved", because only the second one may also revoke
    `read_complete`."""

    SESSION = "AgentDesk-claude-adk-cc"
    SNAPSHOT = {
        "generation_mtime_ns": 1785321000000000000,
        "spawn_nonce": "nonce-a",
        "transcript_file_id": {"dev": 17, "ino": 4242},
    }

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.sessions = self.root / "runtime" / "sessions"
        self.sessions.mkdir(parents=True)
        env = mock.patch.dict(
            os.environ,
            {"AGENTDESK_ROOT_DIR": str(self.root), "TMPDIR": str(self.root)},
        )
        env.start()
        self.addCleanup(env.stop)
        (self.sessions / f"agentdesk-abc-h-{self.SESSION}.generation").write_text(
            "1", encoding="utf-8"
        )
        (self.sessions / f"agentdesk-abc-h-{self.SESSION}.spawn_nonce").write_text(
            "nonce-a\n", encoding="utf-8"
        )
        self.transcript = self.root / "s.jsonl"
        self.transcript.write_text("{}\n", encoding="utf-8")

    def live_snapshot(self):
        transcript_stat = self.transcript.stat()
        file_id = (transcript_stat.st_dev, transcript_stat.st_ino)
        snapshot = session_incarnation_identity(
            self.SESSION, self.transcript, expected_file_id=file_id
        )
        self.assertIsNotNone(snapshot, "fixture must produce a full identity")
        return file_id, snapshot

    def test_an_unchanged_incarnation_republishes_its_snapshot(self):
        file_id, snapshot = self.live_snapshot()
        result = revalidate_incarnation_identity(
            self.SESSION, self.transcript, file_id, snapshot
        )
        self.assertEqual(result.identity, snapshot)
        self.assertFalse(result.superseded)

    def test_an_absent_snapshot_is_unknown_and_not_superseded(self):
        result = revalidate_incarnation_identity(
            self.SESSION, self.transcript, (17, 4242), None
        )
        self.assertIsNone(result.identity)
        self.assertFalse(
            result.superseded, "nothing contradicted the bounded Discord read"
        )

    def test_a_replaced_transcript_inode_is_superseded(self):
        file_id, snapshot = self.live_snapshot()
        replacement = self.root / "s.jsonl.replacement"
        replacement.write_text("{}\n", encoding="utf-8")
        replacement.replace(self.transcript)
        self.assertNotEqual(self.transcript.stat().st_ino, file_id[1])
        result = revalidate_incarnation_identity(
            self.SESSION, self.transcript, file_id, snapshot
        )
        self.assertIsNone(result.identity)
        self.assertTrue(result.superseded)

    def test_a_restamped_generation_marker_is_superseded(self):
        # A respawn inside the tick moves the generation leg while the transcript
        # inode can stay put; identity is the three legs together, so this is a
        # different incarnation too.
        file_id, snapshot = self.live_snapshot()
        result = revalidate_incarnation_identity(
            self.SESSION,
            self.transcript,
            file_id,
            {**snapshot, "generation_mtime_ns": snapshot["generation_mtime_ns"] - 1},
        )
        self.assertIsNone(result.identity)
        self.assertTrue(result.superseded)

    def test_a_read_without_a_descriptor_identity_is_superseded(self):
        # `file_id is None` means the blocks came from a read that never reached a
        # descriptor, so there is nothing to pin the snapshot to. Fail closed.
        _, snapshot = self.live_snapshot()
        result = revalidate_incarnation_identity(
            self.SESSION, self.transcript, None, snapshot
        )
        self.assertIsNone(result.identity)
        self.assertTrue(result.superseded)

    def test_the_identity_leg_is_pinned_to_the_descriptor_that_was_read(self):
        # `session_incarnation_identity` reaches the transcript by PATH. Given a
        # descriptor identity that no longer matches that path, it must decline to
        # name the file rather than describe the replacement.
        transcript_stat = self.transcript.stat()
        self.assertIsNone(
            session_incarnation_identity(
                self.SESSION,
                self.transcript,
                expected_file_id=(transcript_stat.st_dev, transcript_stat.st_ino + 1),
            )
        )


if __name__ == "__main__":
    unittest.main()
