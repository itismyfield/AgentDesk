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
import tempfile
import time
import unittest
from datetime import datetime, timezone
from pathlib import Path

from scripts.relay_watchdog import (
    STATE_GAP,
    STATE_LAGGING,
    STATE_OK,
    ConfigError,
    assistant_blocks_from_lines,
    channel_project_dirs,
    delivered,
    evaluate,
    load_state,
    main_channel_project_re,
    newest_transcript,
    norm,
    parse_config,
    parse_transcript_ts,
    project_slug,
    save_state,
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
        v = self._eval([(self.NOW - 4000, "never arrived")], "")
        self.assertEqual(v.state, STATE_GAP)
        self.assertEqual(v.gap_secs, float("inf"))

    def test_no_blocks_is_ok(self):
        v = self._eval([], "")
        self.assertEqual(v.state, STATE_OK)


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
                "github_repo": "owner/repo",
            }
        )
        self.assertEqual(cfg.gap_alert_secs, 1200)
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


class DeploymentWiringTests(unittest.TestCase):
    """#4372 lesson: a test that CI never runs is a graveyard, and a script the
    deploy never ships evaporates (the 06-29 relay-gap-watch, the 07-09
    prototype). Pin the wiring itself."""

    def test_ci_script_checks_runs_this_suite(self):
        script = (REPO_ROOT / "scripts" / "ci-script-checks.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("tests.test_relay_watchdog", script)

    def test_deploy_release_ships_watchdog_and_plist(self):
        deploy = (REPO_ROOT / "scripts" / "deploy-release.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("scripts/relay_watchdog.py", deploy)
        self.assertIn("com.agentdesk.relay-watchdog", deploy)
        # Deploy-window suppression contract: deploy must touch the marker the
        # watchdog checks before restarting dcserver.
        self.assertIn("relay-watchdog.deploy-marker", deploy)

    def test_watchdog_is_portable_path_linted(self):
        checker = (REPO_ROOT / "scripts" / "check-portable-paths.py").read_text(
            encoding="utf-8"
        )
        self.assertIn("scripts/relay_watchdog.py", checker)


if __name__ == "__main__":
    unittest.main()
