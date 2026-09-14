"""Exercise the real JSONL reader, report and CLI against replay/unknown evidence."""
from __future__ import annotations

import copy
import importlib.util
import json
import random
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timedelta
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "relay_authority_rollout_report.py"
spec = importlib.util.spec_from_file_location("report_under_test", SCRIPT)
assert spec and spec.loader
report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(report)
SITES = ("bridge_entry", "stream_loop", "loop_exit")
FIELDS = ("ticks", "old_ended_lifecycle", "new_ended_lifecycle", "diff", "new_stricter")


def fixture(turn=1, day=0, site="bridge_entry"):
    stamp = (datetime(2026, 8, 1, 9) + timedelta(days=day, seconds=turn)).isoformat()
    payloads = {
        "bridge_entry": {"old": "continue", "new": "continue", "guarded_save": "saved",
                         "rowless_continuation": False},
        "stream_loop": dict(zip(FIELDS, (3, 0, 0, 0, 0))),
        "loop_exit": {"lease_range_shape": "advancing"},
    }
    return {"schema": report.SCHEMA, "ts": stamp, "observed_at": stamp,
            "host": "fixture", "api_port": 8790, "process_generation": 7,
            "runtime_ptr": "0x1", "provider": "codex", "channel_id": 100, "turn_id": turn,
            "cohort_fingerprint": "observe:100:fixture", "publish_reason": "loop_exit",
            "site": site, "axis_a": payloads[site]}


def clean(count=210):
    return [fixture(turn=i + 1, day=i % 7, site=site) for i in range(count) for site in SITES]


def run_report(events, *, extra=(), copies=False, cli=False):
    with tempfile.TemporaryDirectory() as tmp:
        directory = Path(tmp) / "relay_authority"
        directory.mkdir()
        text = "\n".join([*(json.dumps(event) for event in events), *extra]) + "\n"
        (directory / "2026-08-07.jsonl").write_text(text, encoding="utf-8")
        if copies:
            (directory / "2026-08-08.jsonl").write_text(text, encoding="utf-8")
        loaded, warnings, files = report.load_events(directory)
        summary = report.summarize(loaded, 1, files)
        report.render(summary, warnings)
        if cli:
            result = subprocess.run([sys.executable, str(SCRIPT), "--root", tmp, "--json"],
                                    capture_output=True, text=True, check=False)
            return summary, result
        return summary


class EvidenceReportTest(unittest.TestCase):
    def test_clean_report_and_cli_pass(self):
        summary, result = run_report(clean(), cli=True)
        self.assertTrue(summary["promotion_ready"])
        self.assertEqual(result.returncode, 0)
        self.assertEqual(summary["target_segment"]["turn_samples"], 210)
        self.assertEqual(set(summary["criteria"]), {"window_days", "turn_samples", "new_stricter",
                         "loop_exit_coverage", "stream_coverage", "line_integrity"})

    def test_repeated_same_turn_cannot_fill_entry_only_population(self):
        entries = [fixture(i + 1, i % 7) for i in range(210)]
        minority = [fixture(1, 0, site) for site in ("stream_loop", "loop_exit")]
        baseline = run_report(entries + minority)
        repeated = run_report(entries + minority * 500)
        self.assertEqual(baseline["criteria"], repeated["criteria"])
        self.assertFalse(repeated["promotion_ready"])
        for name in ("loop_exit_coverage", "stream_coverage"):
            self.assertEqual(repeated["criteria"][name]["value"], 1)
            self.assertEqual(repeated["criteria"][name]["of"], 210)

    def test_exact_replay_is_idempotent_for_counts_and_criteria(self):
        events = clean()
        first, repeat = run_report(events), run_report(events * 3, copies=True)
        self.assertEqual(first["criteria"], repeat["criteria"])
        for field in ("turn_samples", "sites", "days", "stream_gate", "publish_reasons"):
            self.assertEqual(first["target_segment"][field], repeat["target_segment"][field])
        self.assertEqual(repeat["target_segment"]["duplicate_records"], 3150)

    def test_orphan_turn_sites_do_not_cover_other_entries(self):
        entries = [fixture(i + 1, i % 7) for i in range(210)]
        orphans = [fixture(i + 1001, i % 7, site) for i in range(210)
                   for site in ("stream_loop", "loop_exit")]
        summary = run_report(entries + orphans)
        self.assertFalse(summary["promotion_ready"])
        self.assertEqual(summary["target_segment"]["turn_samples"], 210)
        self.assertEqual(summary["criteria"]["stream_coverage"]["value"], 0)
        self.assertEqual(summary["criteria"]["loop_exit_coverage"]["value"], 0)
        self.assertEqual(summary["target_segment"]["orphan_episode_sites"], 420)

    def test_distinct_episode_stamps_are_not_stitched(self):
        for field, value in (("host", "other"), ("api_port", 8791), ("process_generation", 8),
                             ("runtime_ptr", "0x2"), ("provider", "claude"), ("channel_id", 101),
                             ("turn_id", 2), ("observed_at", "2026-08-02T09:00:01")):
            with self.subTest(field=field):
                entry = fixture()
                stream = fixture(site="stream_loop")
                stream[field] = value
                counts = report.tally([entry, stream])
                self.assertEqual(counts["sites"].get("stream_loop", 0), 0)
                self.assertEqual(counts["turn_samples"], 1)

    def test_two_real_episodes_with_same_native_turn_id_both_count(self):
        events = [fixture(1, day, site) for day in (0, 1) for site in SITES]
        counts = report.tally(events)
        self.assertEqual(counts["turn_samples"], 2)
        self.assertEqual(counts["sites"], {site: 2 for site in SITES})

    def test_invalid_counter_each_field_never_passes_or_crashes(self):
        for field in FIELDS:
            for value in (None, False, True, -1, 0.5, 0.0, "0", "bad", "", [], {}, 2**32):
                with self.subTest(field=field, value=value):
                    events = clean()
                    events[1]["axis_a"][field] = value
                    summary = run_report(events)
                    self.assertFalse(summary["promotion_ready"])
                    self.assertIsNone(summary["criteria"]["new_stricter"]["value"])
                    self.assertFalse(summary["criteria"]["new_stricter"]["met"])

    def test_missing_counter_each_field_is_unknown(self):
        for field in FIELDS:
            with self.subTest(field=field):
                events = clean()
                del events[1]["axis_a"][field]
                summary = run_report(events)
                self.assertFalse(summary["promotion_ready"])
                self.assertIsNone(summary["criteria"]["new_stricter"]["value"])

    def test_entire_missing_or_invalid_axis_payload_cannot_disappear(self):
        for value in (None, [], "", False):
            with self.subTest(value=value):
                events = clean()
                events[1]["axis_a"] = value
                self.assertFalse(run_report(events)["promotion_ready"])
        events = clean()
        del events[1]["axis_a"]
        self.assertFalse(run_report(events)["promotion_ready"])

    def test_invalid_identity_is_explicit_and_does_not_crash_inventory(self):
        for field, value in (("host", None), ("runtime_ptr", {}), ("channel_id", []),
                             ("turn_id", True), ("process_generation", -1),
                             ("api_port", 2**16), ("observed_at", "bad")):
            with self.subTest(field=field):
                events = clean()
                events[1][field] = value
                summary = run_report(events)
                self.assertFalse(summary["promotion_ready"])
                self.assertTrue(summary["target_segment"]["evidence_errors"])

    def test_zero_is_known_but_no_stream_observations_is_unknown(self):
        self.assertEqual(run_report(clean())["criteria"]["new_stricter"]["value"], 0)
        summary = run_report([event for event in clean() if event["site"] != "stream_loop"])
        self.assertIsNone(summary["criteria"]["new_stricter"]["value"])
        self.assertFalse(summary["criteria"]["new_stricter"]["met"])
        self.assertFalse(run_report([])["promotion_ready"])

    def test_positive_counter_and_its_replay_are_counted_once(self):
        events = clean()
        events[1]["axis_a"].update(new_stricter=1, new_ended_lifecycle=1, diff=1)
        summary = run_report(events + [events[1]] * 100)
        self.assertEqual(summary["criteria"]["new_stricter"]["value"], 1)
        self.assertFalse(summary["promotion_ready"])

    def test_conflicting_zero_and_positive_is_unknown_with_positive_witness(self):
        events = clean()
        conflict = copy.deepcopy(events[1])
        conflict["axis_a"].update(new_stricter=1, new_ended_lifecycle=1, diff=1)
        for ordered in (events + [conflict], [conflict] + list(reversed(events))):
            summary = run_report(ordered)
            self.assertFalse(summary["promotion_ready"])
            self.assertIsNone(summary["criteria"]["new_stricter"]["value"])
            self.assertEqual(summary["criteria"]["new_stricter"]["observed_lower_bound"], 1)
            self.assertEqual(summary["criteria"]["stream_coverage"]["value"], 209)

    def test_invalid_sibling_cannot_hide_a_positive_witness(self):
        events = clean()
        events[1]["axis_a"].update(new_stricter=1, diff=1)
        events[4]["axis_a"]["new_stricter"] = None
        summary = run_report(events)
        self.assertIsNone(summary["criteria"]["new_stricter"]["value"])
        self.assertEqual(summary["criteria"]["new_stricter"]["observed_lower_bound"], 1)
        self.assertFalse(summary["promotion_ready"])

    def test_duplicate_clean_lines_cannot_dilute_corruption(self):
        events = clean()
        first = run_report(events, extra=["broken"] * 10)
        replay = run_report(events * 100, extra=["broken"] * 10)
        self.assertFalse(first["criteria"]["line_integrity"]["met"])
        self.assertEqual(first["criteria"], replay["criteria"])
        self.assertEqual(replay["line_integrity"]["duplicate_target_lines"], 630 * 99)

    def test_reordering_and_publication_timestamp_do_not_change_evidence(self):
        events = clean()
        changed = copy.deepcopy(events)
        for event in changed:
            event["ts"] = "2026-08-10T12:00:00"
        random.Random(5).shuffle(changed)
        self.assertEqual(run_report(events)["criteria"], run_report(events + changed)["criteria"])

    def test_invalid_counter_cli_is_nonzero_and_json_unknown(self):
        events = clean()
        del events[1]["axis_a"]["new_stricter"]
        summary, result = run_report(events, cli=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        parsed = json.loads(result.stdout)["summary"]
        self.assertIsNone(parsed["criteria"]["new_stricter"]["value"])
        self.assertEqual(parsed["criteria"], summary["criteria"])

    def test_no_raw_line_count_can_exceed_full_coverage(self):
        summary = run_report(clean() * 10)
        for name in ("loop_exit_coverage", "stream_coverage"):
            self.assertEqual(summary["criteria"][name]["share"], 1)


if __name__ == "__main__":
    unittest.main()
