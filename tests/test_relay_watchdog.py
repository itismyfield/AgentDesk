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
import subprocess
import tempfile
import time
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest import mock

import scripts.relay_watchdog as relay_watchdog
from scripts.relay_watchdog import (
    COVERAGE_CONFIRM_TICKS,
    COVERAGE_COVERED,
    COVERAGE_UNCOVERED,
    COVERAGE_UNKNOWN,
    PG_OK,
    PG_STATE_KEY,
    PG_TOPOLOGY_DIRECT,
    PG_TUNNEL_DOWN,
    PG_UNCLASSIFIED_DOWN,
    PG_UNKNOWN,
    PG_UPSTREAM_DOWN,
    SELECTOR_DIVERGED,
    SELECTOR_SYNCED,
    SELECTOR_UNKNOWN,
    STATE_GAP,
    STATE_LAGGING,
    STATE_OK,
    ChannelConfig,
    Config,
    ConfigError,
    Runtime,
    TranscriptCandidate,
    WatcherStateProbe,
    assistant_blocks_from_lines,
    channel_project_dirs,
    delivered,
    evaluate,
    evaluate_coverage,
    evaluate_pg_health,
    evaluate_selector_sync,
    expected_tmux_session_name,
    load_state,
    main_channel_project_re,
    newest_transcript,
    norm,
    parse_config,
    parse_transcript_ts,
    project_slug,
    save_state,
    select_watch_transcript,
    selector_divergence_confirmed,
    tick_channel,
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
        )
        self.assertEqual(selected, growing.path)

    def test_no_growth_falls_back_to_newest_mtime(self):
        older = TranscriptCandidate(Path("/tmp/older.jsonl"), 100, 100.0)
        newer = TranscriptCandidate(Path("/tmp/newer.jsonl"), 200, 200.0)
        selected = select_watch_transcript(
            [older, newer], {str(older.path): 100, str(newer.path): 200}
        )
        self.assertEqual(selected, newer.path)

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


class CoverageEvaluationTests(unittest.TestCase):
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

    def test_dead_expected_session_is_left_to_stall_watchdog(self):
        verdict = evaluate_coverage(False, 200, False, True, 1)
        self.assertEqual(verdict.state, COVERAGE_COVERED)
        self.assertEqual(verdict.reason, "tmux_not_expected")
        self.assertFalse(verdict.confirmed)

    def test_malformed_200_is_unknown_not_uncovered(self):
        verdict = evaluate_coverage(True, 200, None, None, 1)
        self.assertEqual(verdict.state, COVERAGE_UNKNOWN)
        self.assertEqual(verdict.consecutive_uncovered, 0)


class SelectorSyncEvaluationTests(unittest.TestCase):
    """Pure I1 judgment (#4408 phase 2): B (watcher-state bound_output_path) vs
    F (growth-aware transcript pick)."""

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
        verdict = evaluate_selector_sync("/tmp/b.jsonl", "/tmp/f.jsonl", False)
        self.assertEqual(verdict.state, SELECTOR_SYNCED)
        self.assertEqual(verdict.reason, "f_not_growing")
        self.assertFalse(verdict.diverged)

    def test_mismatch_with_growing_f_is_diverged(self):
        verdict = evaluate_selector_sync("/tmp/b.jsonl", "/tmp/f.jsonl", True)
        self.assertEqual(verdict.state, SELECTOR_DIVERGED)
        self.assertEqual(verdict.reason, "selector_diverged")
        self.assertTrue(verdict.diverged)


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

    def test_watcher_state_malformed_200_keeps_status_but_not_claims(self):
        completed = subprocess.CompletedProcess(
            ["curl"], 0, stdout='{"attached":"true"}\n200', stderr=""
        )
        with mock.patch.object(relay_watchdog.subprocess, "run", return_value=completed):
            probe = self.make_rt().watcher_state("999")
        self.assertEqual(probe, WatcherStateProbe(200, None, None))

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

    def alert(self, ch, body: str, trigger_turn: bool = True) -> None:
        self.alerts.append((body, trigger_turn))

    def file_github_issue(self, ch, gap_min: int, lost: int) -> str:
        self.issue_calls += 1
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

    def alert(self, ch, body: str, trigger_turn: bool = True) -> None:
        self.alerts.append((body, trigger_turn))

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
        for epoch, text in blocks:
            ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch))
            lines.append(
                json.dumps(
                    {
                        "type": "assistant",
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

    def test_growth_aware_selector_is_wired_into_tick(self):
        growing = self.proj_dir / "growing.jsonl"
        stagnant_dir = self.projects / (
            "-Users-alice--adk-release-worktrees-claude-adk-cc-20260710-140500"
        )
        stagnant_dir.mkdir()
        stagnant = stagnant_dir / "stagnant.jsonl"

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

        growing.write_text(
            record(self.now - 2000, "missing from older growing transcript") + "\n",
            encoding="utf-8",
        )
        stagnant.write_text(
            record(self.now - 2000, "newer stagnant block landed") + "\n",
            encoding="utf-8",
        )
        os.utime(growing, (self.now - 100, self.now - 100))
        os.utime(stagnant, (self.now, self.now))
        rt = self.make_rt()
        rt.haystack = norm("newer stagnant block landed")
        state: dict = {}

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(rt.alerts, [], "first tick must use mtime fallback")

        with growing.open("a", encoding="utf-8") as f:
            f.write("\n")
        # Keep the growing file's mtime older than the stagnant candidate: only
        # size growth can make it win on the second tick.
        os.utime(growing, (self.now - 50, self.now - 50))
        os.utime(stagnant, (self.now, self.now))
        tick_channel(rt, TICK_CHANNEL, state, self.now + 1)
        self.assertEqual(len(rt.alerts), 1)
        self.assertIn("릴레이 갭 감지", rt.alerts[0][0])

    def _grow_selected_transcript(self) -> None:
        tr = self.proj_dir / "s.jsonl"
        with tr.open("a", encoding="utf-8") as f:
            f.write("\n")
        os.utime(tr, (self.now, self.now))

    def test_selector_divergence_alerts_only_after_swap_confirm(self):
        # One delivered block → gap verdict stays OK, isolating the selector path.
        self.write_transcript([(self.now - 30, "delivered block one")])
        rt = self.make_rt(swap_confirm_secs=1)
        rt.haystack = norm("delivered block one")
        # dcserver asserts a bind to a DIFFERENT transcript than F (s.jsonl).
        rt.watcher_probe = WatcherStateProbe(200, True, False, "/tmp/stale-bind.jsonl")
        state: dict = {}

        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(
            [a for a in rt.alerts if "셀렉터" in a[0]],
            [],
            "first tick has no growth proof; selector probe is skipped",
        )

        self._grow_selected_transcript()
        tick_channel(rt, TICK_CHANNEL, state, self.now + 2)
        self.assertEqual(
            [a for a in rt.alerts if "셀렉터" in a[0]],
            [],
            "a divergence within the swap-confirm window is not alarmed",
        )
        self.assertIn("selector_diverged_since", state["999"])

        self._grow_selected_transcript()
        tick_channel(rt, TICK_CHANNEL, state, self.now + 4)
        selector_alerts = [a for a in rt.alerts if "셀렉터 동기화" in a[0]]
        self.assertEqual(len(selector_alerts), 1)
        body, trigger_turn = selector_alerts[0]
        self.assertIn("/tmp/stale-bind.jsonl", body)
        self.assertIn("/api/inflight/rebind", body)
        self.assertIn("sessions", body)
        self.assertTrue(
            trigger_turn,
            "actionable selector alerts must trigger an agent turn "
            "(send-to-agent handoff), not bot-direct delivery",
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
        state = {"999": {"transcript_sizes": {str(transcript): "not-an-int"}}}
        tick_channel(rt, TICK_CHANNEL, state, self.now)
        self.assertEqual(rt.alerts, [])
        self.assertIsInstance(state["999"]["transcript_sizes"][str(transcript)], int)

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

    # (d) persistent-gap issue auto-filing is deduplicated
    def test_persistent_gap_files_issue_exactly_once(self):
        rt = self.gap_rt(github_repo="owner/repo")
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
        # Deploy-window suppression contract: deploy must touch the marker the
        # watchdog checks before restarting dcserver.
        self.assertIn("relay-watchdog.deploy-marker", deploy)

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

    def _run_block(self, block: str, adk_rel: Path, home: Path):
        import shlex

        script = (
            "set -euo pipefail\n"
            f"REPO={shlex.quote(str(REPO_ROOT))}\n"
            f"ADK_REL={shlex.quote(str(adk_rel))}\n"
            f"HOME={shlex.quote(str(home))}\n"
            # Nonexistent domain: bootstrap must fail (fail-open ⚠ path) rather
            # than loading a test plist into the developer's real launchd.
            "LAUNCHD_DOMAIN=gui/999999\n" + block + "\necho HARNESS-END\n"
        )
        return subprocess.run(
            ["bash", "-c", script], capture_output=True, text=True, timeout=60
        )

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


if __name__ == "__main__":
    unittest.main()
