"""Tests for the #5464 (#5071 T5) relay-authority promotion report.

The report is the AC3 promotion gate's only reader, so its failure modes are
promotion decisions. These fix the three that r1 got wrong: a log missing every
stream and loop-exit record still passed (legB P1-2), a fingerprint re-entered
after a different dial position was summed into one continuous window (legB
P1-3), and unusable lines were a warning rather than a verdict.

r3 adds the three the r2c review reproduced: a dial excursion SHORTER than the
segment gap still merged two rollout windows (legA/legB r2c P1-1), the
`line_integrity` criterion was the one criterion scoped to the whole input so
history diluted the target window's own losses (legB r2c P1-2), and a turn lost
while stranded was invisible in the canon, which `publish_reason` now records
(legA r2c P1-3).

rc adds four more. Two are the half of that dilution the per-file scope still
let through — daily files are named by PUBLISH day and the dial moves mid-day, so
the previous segment's records cohabit the target's own first file (legA/legB r3c
P1-1): one pins the verdict on r3's own F -> G -> F counterexample, the other pins
that the verdict does not MOVE with the cohabiting volume, across r3's 1,350-line
flip threshold. The third is a self-inflicted defect standing since r1: a JSON
line that parses to a bare scalar raised `AttributeError` and aborted the run
before any criterion was evaluated, on exactly the interleaved input
`MALFORMED_LINE_CEILING` documents as expected (legA r3c P2-2). The fourth
asserts the criteria key set is exactly the six axis-A gates, because r3 claimed
that was pinned when only one negative membership check existed (legA r3c P2-7).

rc r2 adds no test, but `out_of_scope_unusable_lines` — the display field for the
one residual of the scope that is FAIL-OPEN rather than false-red (legA rc P1-1)
— is asserted inside the corrupt-history regression, which is that residual's own
shape.
"""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from datetime import datetime, timedelta
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "relay_authority_rollout_report.py"
_spec = importlib.util.spec_from_file_location("relay_authority_rollout_report", SCRIPT)
assert _spec and _spec.loader
report = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = report
_spec.loader.exec_module(report)

FINGERPRINT = "observe:100:c0ffee"
BASE = datetime(2026, 8, 1, 9, 0, 0)


def event(
    *,
    site: str,
    turn: int,
    observed: datetime,
    published: datetime | None = None,
    fingerprint: str = FINGERPRINT,
    axis_a: dict | None = None,
    publish_reason: str = "loop_exit",
) -> dict:
    """One emitted record, in the shape `authority_observation::encode` writes."""

    return {
        "schema": report.SCHEMA,
        "ts": (published or observed).isoformat(),
        "publish_reason": publish_reason,
        "host": "fixture",
        "api_port": 8790,
        "process_generation": 7,
        "runtime_ptr": "0x1",
        "provider": "codex",
        "channel_id": 4_259_300,
        "turn_id": turn,
        "observed_at": observed.strftime("%Y-%m-%d %H:%M:%S"),
        "cohort_fingerprint": fingerprint,
        "site": site,
        "axis_a": axis_a if axis_a is not None else default_axis_a(site),
    }


def completion_event(
    *,
    site: str,
    turn: int,
    observed: datetime,
    scope: str,
    scope_reason: str,
    fingerprint: str = FINGERPRINT,
) -> dict:
    item = event(site=site, turn=turn, observed=observed, fingerprint=fingerprint)
    item.pop("axis_a")
    item["publish_reason"] = "post_flush"
    item["scope"] = scope
    item["scope_reason"] = scope_reason
    return item


def default_axis_a(site: str) -> dict:
    if site == "bridge_entry":
        return {
            "guarded_save": "saved",
            "old": "continue",
            "new": "continue",
            "rowless_continuation": False,
        }
    if site == "stream_loop":
        return {
            "ticks": 3,
            "old_ended_lifecycle": 0,
            "new_ended_lifecycle": 0,
            "diff": 0,
            "new_stricter": 0,
        }
    return {"lease_range_shape": "advancing"}


def turns(
    count: int,
    *,
    days: int,
    sites: tuple[str, ...],
    start: datetime = BASE,
    fingerprint: str = FINGERPRINT,
    turn_base: int = 1_000,
    publish_reason: str = "loop_exit",
    spacing: timedelta = timedelta(minutes=1),
) -> list[dict]:
    """`count` turns spread over `days` consecutive days, each emitting `sites`.

    `spacing` is the within-day step between turns. It only matters when a run
    has to stay inside one calendar day and end before a later run starts, which
    is what the same-file cohabitation fixtures need.
    """

    events = []
    for index in range(count):
        observed = start + timedelta(days=index % days) + spacing * index
        for site in sites:
            events.append(
                event(
                    site=site,
                    turn=turn_base + index,
                    observed=observed,
                    fingerprint=fingerprint,
                    publish_reason=publish_reason,
                )
            )
    return events


class RolloutReportTest(unittest.TestCase):
    def run_report(self, events: list[dict], *, extra_lines: list[str] | None = None,
                   extra_day: str = "2026-08-01", argv: list[str] | None = None) -> dict:
        """Write `events` into per-publish-day files and summarize them.

        `extra_day` picks which day's file the raw `extra_lines` land in, because
        `line_integrity` is scoped by file: garbage in a file the target segment
        was not read from is deliberately outside the criterion.
        """

        with tempfile.TemporaryDirectory() as raw_root:
            root = Path(raw_root)
            log_dir = root / "relay_authority"
            log_dir.mkdir()
            by_day: dict[str, list[str]] = {}
            for item in events:
                day = item["ts"][:10]
                by_day.setdefault(day, []).append(json.dumps(item))
            for line in extra_lines or []:
                by_day.setdefault(extra_day, []).append(line)
            for day, lines in by_day.items():
                (log_dir / f"{day}.jsonl").write_text("\n".join(lines) + "\n", encoding="utf-8")
            events_read, warnings, by_file = report.load_events(log_dir)
            windowed = report.apply_window(events_read, None)
            summary = report.summarize(windowed, 1, by_file)
            # The rendered form has to survive every one of these shapes too: the
            # runbook reads it, not the JSON.
            report.render(summary, warnings)
            if argv is not None:
                self.assertIn(report.main(argv + ["--root", str(root)]), (0, 1))
            return summary

    def test_a_complete_window_is_promotion_ready(self):
        summary = self.run_report(
            turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        )
        self.assertTrue(summary["promotion_ready"], summary["criteria"])
        self.assertEqual(summary["criteria"]["window_days"]["value"], 7)
        self.assertEqual(summary["criteria"]["turn_samples"]["value"], 210)
        self.assertEqual(summary["criteria"]["loop_exit_coverage"]["share"], 1.0)
        self.assertEqual(len(summary["segments"]), 1)

    def test_completion_scope_distribution_displays_tui_direct_idle_and_foreign(self):
        observed = BASE
        events = turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        events.extend(
            [
                completion_event(
                    site="completion_r1",
                    turn=90_001,
                    observed=observed,
                    scope="idle",
                    scope_reason="mailbox_idle",
                ),
                completion_event(
                    site="completion_r2",
                    turn=90_001,
                    observed=observed,
                    scope="foreign",
                    scope_reason="foreign_episode",
                ),
            ]
        )

        summary = self.run_report(events)
        self.assertEqual(
            summary["target_segment"]["completion_scopes"],
            {
                "completion_r1:idle:mailbox_idle": 1,
                "completion_r2:foreign:foreign_episode": 1,
            },
        )
        self.assertTrue(summary["promotion_ready"], summary["criteria"])

    def test_completion_scopes_exclude_an_earlier_same_fingerprint_segment(self):
        first = turns(30, days=2, sites=("bridge_entry", "stream_loop", "loop_exit"))
        other = turns(
            10,
            days=1,
            sites=("bridge_entry", "stream_loop", "loop_exit"),
            start=BASE + timedelta(days=4),
            fingerprint="observe:50:c0ffee",
            turn_base=5_000,
        )
        target = turns(
            30,
            days=2,
            sites=("bridge_entry", "stream_loop", "loop_exit"),
            start=BASE + timedelta(days=7),
            turn_base=9_000,
        )
        events = first + other + target
        events.extend(
            [
                completion_event(
                    site="completion_r0",
                    turn=90_001,
                    observed=BASE + timedelta(days=1),
                    scope="idle",
                    scope_reason="mailbox_idle",
                ),
                completion_event(
                    site="completion_r0",
                    turn=90_002,
                    observed=BASE + timedelta(days=7),
                    scope="foreign",
                    scope_reason="foreign_episode",
                ),
            ]
        )

        summary = self.run_report(events)
        self.assertEqual(
            summary["target_segment"]["completion_scopes"],
            {"completion_r0:foreign:foreign_episode": 1},
        )

    def test_completion_records_neither_dilute_corruption_nor_change_verdict(self):
        """210 axis-A turns + 8 damaged lines judge identically with completion telemetry."""

        lifecycle = turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        completions = [
            completion_event(
                site=f"completion_r{site}",
                turn=90_000 + index,
                observed=BASE + timedelta(days=index % 7),
                scope="idle",
                scope_reason="mailbox_idle",
            )
            for index in range(210)
            for site in range(1, 5)
        ]
        without = self.run_report(lifecycle, extra_lines=["{not json"] * 8)
        with_completion = self.run_report(
            lifecycle + completions, extra_lines=["{not json"] * 8
        )

        self.assertEqual(without["promotion_ready"], with_completion["promotion_ready"])
        self.assertFalse(with_completion["promotion_ready"])
        self.assertEqual(without["criteria"], with_completion["criteria"])
        self.assertEqual(with_completion["line_integrity"]["lines"], 638)
        self.assertEqual(
            without["line_integrity_all_files"],
            with_completion["line_integrity_all_files"],
        )
        self.assertEqual(
            without["line_integrity"]["cohabiting_usable_lines"],
            with_completion["line_integrity"]["cohabiting_usable_lines"],
        )

    def test_completion_only_turn_keys_do_not_raise_axis_a_turn_samples(self):
        lifecycle = turns(199, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        completions = [
            completion_event(
                site="completion_r1",
                turn=99_000 + index,
                observed=BASE + timedelta(days=20 + index),
                scope="foreign",
                scope_reason="foreign_episode",
            )
            for index in range(4)
        ]

        summary = self.run_report(lifecycle + completions)
        self.assertEqual(summary["target_segment"]["turn_samples"], 199)
        self.assertEqual(summary["all_segments_turn_samples"], 199)
        self.assertEqual(summary["criteria"]["turn_samples"]["value"], 199)
        self.assertFalse(summary["promotion_ready"])

    def test_entry_only_turns_do_not_pass_on_the_turn_count(self):
        """legB P1-2: 200 turns, 7 days, and no stream or loop-exit record.

        Every turn-counting floor is satisfied and `new_stricter` is 0 only
        because no stream record exists to be nonzero. This is the false-green
        r1 produced, and it must now fail on both coverage floors.
        """

        summary = self.run_report(turns(210, days=7, sites=("bridge_entry",)))
        self.assertTrue(summary["criteria"]["window_days"]["met"])
        self.assertTrue(summary["criteria"]["turn_samples"]["met"])
        self.assertTrue(summary["criteria"]["new_stricter"]["met"])
        self.assertFalse(summary["criteria"]["stream_coverage"]["met"])
        self.assertFalse(summary["criteria"]["loop_exit_coverage"]["met"])
        self.assertFalse(summary["promotion_ready"])

    def test_a_partially_written_window_fails_the_coverage_floor(self):
        """The reachable sink failure: entry records land, later writes do not."""

        events = turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        survivors = [
            item
            for index, item in enumerate(events)
            if item["site"] == "bridge_entry" or index % 9 == 0
        ]
        summary = self.run_report(survivors)
        self.assertFalse(summary["criteria"]["loop_exit_coverage"]["met"])
        self.assertFalse(summary["promotion_ready"])

    def test_a_reentered_fingerprint_is_two_segments_judged_on_the_newest(self):
        """legB P1-3: F, then a different dial, then F again a week later.

        Summed into one bucket the two runs of F look like a 9-day, 240-turn
        window. They are two windows, and only the newest one is the case for
        promoting the dial that is live now.
        """

        first = turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        other = turns(
            10,
            days=1,
            sites=("bridge_entry", "stream_loop", "loop_exit"),
            start=BASE + timedelta(days=9),
            fingerprint="observe:50:c0ffee",
            turn_base=5_000,
        )
        again = turns(
            30,
            days=2,
            sites=("bridge_entry", "stream_loop", "loop_exit"),
            start=BASE + timedelta(days=14),
            turn_base=9_000,
        )
        summary = self.run_report(first + other + again)

        by_fingerprint = [segment["cohort_fingerprint"] for segment in summary["segments"]]
        self.assertEqual(by_fingerprint.count(FINGERPRINT), 2)
        target = summary["target_segment"]
        self.assertEqual(target["cohort_fingerprint"], FINGERPRINT)
        self.assertEqual(target["turn_samples"], 30)
        self.assertEqual(summary["all_segments_turn_samples"], 250)
        self.assertFalse(summary["criteria"]["window_days"]["met"])
        self.assertFalse(summary["promotion_ready"])

    def test_a_short_dial_excursion_splits_the_fingerprint_it_interrupts(self):
        """legA/legB r2c P1-1: the excursion is shorter than the segment gap.

        The reproduced counterexample: a complete 7-day/210-turn run at F, then a
        same-day switch to G for ten turns, then F again six hours later with only
        30 turns of its own. r2 bucketed by fingerprint before cutting runs, so
        F's own samples were 6h41m apart and merged into one 240-turn, 7-day
        window that passed every floor. The interleaved G samples are the input's
        own witness that the dial moved, and they are what splits it now.
        """

        first = turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        excursion_start = BASE + timedelta(days=6, minutes=250)
        other = turns(
            10,
            days=1,
            sites=("bridge_entry", "stream_loop", "loop_exit"),
            start=excursion_start,
            fingerprint="observe:50:c0ffee",
            turn_base=5_000,
        )
        again = turns(
            30,
            days=1,
            sites=("bridge_entry", "stream_loop", "loop_exit"),
            start=excursion_start + timedelta(hours=6),
            turn_base=9_000,
        )
        summary = self.run_report(first + other + again)

        gap = report.timedelta(hours=report.SEGMENT_GAP_HOURS)
        self.assertLess(
            datetime.fromisoformat(summary["segments"][-1]["first_observed"])
            - datetime.fromisoformat(summary["segments"][0]["last_observed"]),
            gap,
            "the counterexample requires the excursion to be shorter than the gap",
        )
        self.assertEqual(len(summary["segments"]), 3)
        self.assertEqual(
            [segment["cohort_fingerprint"] for segment in summary["segments"]],
            [FINGERPRINT, "observe:50:c0ffee", FINGERPRINT],
        )
        target = summary["target_segment"]
        self.assertEqual(target["cohort_fingerprint"], FINGERPRINT)
        self.assertEqual(target["turn_samples"], 30)
        self.assertEqual(len(target["days"]), 1)
        # What the merged reading would have been, and why it was a false green.
        self.assertEqual(summary["all_segments_turn_samples"], 250)
        self.assertFalse(summary["criteria"]["turn_samples"]["met"])
        self.assertFalse(summary["criteria"]["window_days"]["met"])
        self.assertFalse(summary["promotion_ready"])

    def test_a_gap_of_exactly_the_threshold_splits(self):
        """r2 compared with `>`, so a gap of exactly 48h merged (legB r2c P1-1)."""

        first = turns(30, days=1, sites=("bridge_entry",))
        later = turns(
            30,
            days=1,
            sites=("bridge_entry",),
            # Exactly the threshold after the newest sample of `first`, and at the
            # same fingerprint, so the gap rule is the only discriminator in play.
            start=BASE + timedelta(minutes=29, hours=report.SEGMENT_GAP_HOURS),
            turn_base=7_000,
        )
        summary = self.run_report(first + later)
        self.assertEqual(
            datetime.fromisoformat(summary["segments"][-1]["first_observed"])
            - datetime.fromisoformat(summary["segments"][0]["last_observed"]),
            report.timedelta(hours=report.SEGMENT_GAP_HOURS),
        )
        self.assertEqual(len(summary["segments"]), 2)
        self.assertEqual(summary["target_segment"]["turn_samples"], 30)

    def test_line_integrity_is_scoped_to_the_target_segment(self):
        """legB r2c P1-2, direction 1: history must not dilute the target's loss.

        The target window loses 20 of its own 650 lines (3.08%, over the 1%
        ceiling). r2 divided by every line in the directory, so 10,000 lines of
        older history read the same loss as 0.19% and passed.
        """

        history = turns(
            10_000,
            days=7,
            sites=("bridge_entry",),
            start=BASE,
            turn_base=200_000,
        )
        target_start = BASE + timedelta(days=200)
        target = turns(
            210,
            days=7,
            sites=("bridge_entry", "stream_loop", "loop_exit"),
            start=target_start,
        )
        summary = self.run_report(
            history + target,
            extra_lines=["{not json"] * 20,
            extra_day=target_start.date().isoformat(),
        )

        self.assertEqual(summary["target_segment"]["turn_samples"], 210)
        self.assertEqual(summary["line_integrity"]["unusable"], 20)
        self.assertEqual(summary["line_integrity"]["lines"], 650)
        self.assertFalse(summary["criteria"]["line_integrity"]["met"])
        self.assertFalse(summary["promotion_ready"])
        # The dilution that used to hide it is still reported, just not judged.
        whole = summary["line_integrity_all_files"]
        self.assertEqual(whole["lines"], 10_650)
        self.assertLess(whole["unusable"] / whole["lines"], report.MALFORMED_LINE_CEILING)

    def test_a_corrupt_history_does_not_fail_a_clean_target_segment(self):
        """legB r2c P1-2, direction 2: the false-red the same scope prevents."""

        history = turns(
            30,
            days=1,
            sites=("bridge_entry",),
            start=BASE,
            turn_base=200_000,
        )
        target_start = BASE + timedelta(days=200)
        target = turns(
            210,
            days=7,
            sites=("bridge_entry", "stream_loop", "loop_exit"),
            start=target_start,
        )
        summary = self.run_report(
            history + target,
            extra_lines=["{not json"] * 500,
            extra_day=BASE.date().isoformat(),
        )

        self.assertEqual(summary["line_integrity"]["unusable"], 0)
        self.assertTrue(summary["criteria"]["line_integrity"]["met"])
        self.assertTrue(summary["promotion_ready"], summary["criteria"])
        self.assertEqual(summary["line_integrity_all_files"]["unusable"], 500)
        # The same exclusion read the other way (legA rc P1-1): this direction is
        # fail-open, not false-red, and no criterion sees it — so the count that
        # left both sides of the ratio is displayed, symmetric with
        # `cohabiting_usable_lines`.
        self.assertEqual(summary["line_integrity"]["out_of_scope_unusable_lines"], 500)

    def test_a_cohabiting_segment_in_the_targets_own_file_does_not_dilute_it(self):
        """legA/legB r3c P1-1: the half the per-file scope still let through.

        Daily files are named by PUBLISH day and the dial moves mid-day, so the
        previous segment's records for that day sit in the target segment's own
        first file. r3 judged the criterion over that file's whole line count,
        which diluted the target's losses exactly as whole-input history had:
        20 of the target's own 650 lines is 3.08% and fails, but the same 20
        over 2,180 lines is 0.92% and passed, flipping promotion_ready to True.

        The shape is r3's own counterexample, F -> G -> F, all three parts of it
        sharing one day's file.
        """

        previous = turns(
            500,
            days=1,
            sites=("bridge_entry", "stream_loop", "loop_exit"),
            start=BASE,
            turn_base=500_000,
            spacing=timedelta(seconds=20),
        )
        excursion = turns(
            10,
            days=1,
            sites=("bridge_entry", "stream_loop", "loop_exit"),
            start=BASE + timedelta(hours=3),
            fingerprint="observe:50:c0ffee",
            turn_base=600_000,
        )
        target_start = BASE + timedelta(hours=6)
        target = turns(
            210,
            days=7,
            sites=("bridge_entry", "stream_loop", "loop_exit"),
            start=target_start,
        )
        summary = self.run_report(
            previous + excursion + target,
            extra_lines=["{not json"] * 20,
            extra_day=BASE.date().isoformat(),
        )

        # The fixture is only a counterexample if all three runs really do share
        # the target's first file, and if the runs really are three segments.
        self.assertEqual(
            [segment["cohort_fingerprint"] for segment in summary["segments"]],
            [FINGERPRINT, "observe:50:c0ffee", FINGERPRINT],
        )
        self.assertEqual(
            [segment["turns"] for segment in summary["segments"]], [500, 10, 210]
        )
        self.assertIn(f"{BASE.date().isoformat()}.jsonl", summary["line_integrity"]["files"])
        self.assertEqual(summary["line_integrity"]["cohabiting_usable_lines"], 1_530)

        self.assertEqual(summary["target_segment"]["turn_samples"], 210)
        self.assertEqual(summary["line_integrity"]["unusable"], 20)
        self.assertEqual(summary["line_integrity"]["lines"], 650)
        self.assertFalse(summary["criteria"]["line_integrity"]["met"])
        self.assertFalse(summary["promotion_ready"])
        # 20/2180 = 0.92%, the reading that used to pass, is still visible as
        # the whole-input tally — reported, never judged.
        whole = summary["line_integrity_all_files"]
        self.assertEqual(whole["lines"], 2_180)
        self.assertLess(whole["unusable"] / whole["lines"], report.MALFORMED_LINE_CEILING)

    def test_the_verdict_does_not_move_with_the_volume_cohabiting_beside_it(self):
        """The same P1 stated as the invariant, across r3's flip threshold.

        With a file-scoped denominator the verdict on a fixed target turned on
        how much unrelated traffic happened to share its file: 630 target lines
        and 20 unusable ones needed 1,350 cohabiting lines (450 turns) to cross
        from FAIL to PASS. Rolling a dial wide and then back narrow produces
        exactly that ratio, so the threshold sat in the middle of a normal
        rollout. The target's own loss is 3.08% at every volume below.
        """

        target_start = BASE + timedelta(hours=6)
        for cohabiting_turns in (0, 440, 450, 600):
            with self.subTest(cohabiting_turns=cohabiting_turns):
                previous = turns(
                    cohabiting_turns,
                    days=1,
                    sites=("bridge_entry", "stream_loop", "loop_exit"),
                    start=BASE,
                    fingerprint="observe:50:c0ffee",
                    turn_base=500_000,
                    spacing=timedelta(seconds=20),
                )
                target = turns(
                    210,
                    days=7,
                    sites=("bridge_entry", "stream_loop", "loop_exit"),
                    start=target_start,
                )
                summary = self.run_report(
                    previous + target,
                    extra_lines=["{not json"] * 20,
                    extra_day=BASE.date().isoformat(),
                )

                self.assertEqual(summary["target_segment"]["turn_samples"], 210)
                self.assertEqual(
                    summary["line_integrity"]["cohabiting_usable_lines"],
                    3 * cohabiting_turns,
                )
                self.assertEqual(summary["line_integrity"]["lines"], 650)
                self.assertAlmostEqual(
                    summary["criteria"]["line_integrity"]["share"], 20 / 650
                )
                self.assertFalse(summary["criteria"]["line_integrity"]["met"])
                self.assertFalse(summary["promotion_ready"])

    def test_a_json_line_that_is_not_an_object_is_counted_not_raised(self):
        """legA r3c P2-2, a self-inflicted defect standing since r1.

        A bare scalar or list is valid JSON, so it survives the decode and then
        met `.get` on an int, raising AttributeError and aborting the whole run
        before any criterion was evaluated. The input is the very one
        MALFORMED_LINE_CEILING is documented to expect — cross-process
        interleaving, where a fragment of one writer's line can parse alone.
        """

        target = turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        summary = self.run_report(
            target,
            extra_lines=["3", '"tail"', "true", "null", "[1, 2]"],
            extra_day=BASE.date().isoformat(),
        )

        self.assertEqual(summary["line_integrity"]["schema_mismatch"], 5)
        self.assertEqual(summary["line_integrity"]["unusable"], 5)
        self.assertEqual(summary["line_integrity"]["lines"], 635)
        # Absorbed, not fatal: the run still reaches a verdict, and the five
        # lines are too few to breach the ceiling on their own.
        self.assertEqual(summary["target_segment"]["turn_samples"], 210)
        self.assertTrue(summary["criteria"]["line_integrity"]["met"])
        self.assertTrue(summary["promotion_ready"], summary["criteria"])

    def test_the_criteria_key_set_is_exactly_the_six_axis_a_gates(self):
        """No seventh gate was added by this slice, asserted rather than stated.

        `evicted_publication_share` and the S7a fields are reported and not
        judged; the r3 report claimed the key set was pinned when only one
        negative membership check existed, so a seventh key could have appeared
        with no test failing (legA r3c P2-7).
        """

        summary = self.run_report(
            turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        )
        self.assertEqual(
            sorted(summary["criteria"]),
            [
                "line_integrity",
                "loop_exit_coverage",
                "new_stricter",
                "stream_coverage",
                "turn_samples",
                "window_days",
            ],
        )

    def test_the_evicted_publication_share_is_reported_and_not_gated(self):
        """legA r2c P1-3 scenarios A and B, which used to look identical.

        A: the entry-only measured population is in the log, published by its
        successors. B: the same turns were stranded and never published, which
        RAISES every coverage ratio. The provenance field is what tells them
        apart, and it is displayed rather than gated.
        """

        complete = turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        measured = turns(
            105,
            days=7,
            sites=("bridge_entry",),
            start=BASE + timedelta(minutes=1),
            turn_base=4_000,
            publish_reason="evicted",
        )

        with_population = self.run_report(complete + measured)
        self.assertEqual(
            with_population["target_segment"]["publish_reasons"],
            {"loop_exit": 210, "evicted": 105},
        )
        self.assertAlmostEqual(
            with_population["target_segment"]["evicted_publication_share"], 105 / 315
        )
        self.assertAlmostEqual(
            with_population["criteria"]["loop_exit_coverage"]["share"], 210 / 315
        )

        all_stranded = self.run_report(complete)
        self.assertEqual(
            all_stranded["target_segment"]["publish_reasons"], {"loop_exit": 210}
        )
        self.assertEqual(all_stranded["target_segment"]["evicted_publication_share"], 0.0)
        self.assertEqual(all_stranded["criteria"]["loop_exit_coverage"]["share"], 1.0)
        # Losing the measured population improves every gated criterion. That is
        # the finding, and the share above is the only thing that shows it.
        self.assertTrue(all_stranded["promotion_ready"])
        self.assertNotIn("evicted", all_stranded["criteria"])

    def test_a_record_without_provenance_is_not_assumed_to_be_a_flush(self):
        events = turns(30, days=1, sites=("bridge_entry",))
        for item in events:
            del item["publish_reason"]
        summary = self.run_report(events)
        self.assertEqual(
            summary["target_segment"]["publish_reasons"], {"unattributed": 30}
        )
        self.assertEqual(summary["target_segment"]["evicted_publication_share"], 0.0)

    def test_a_continuous_window_is_not_split_by_an_ordinary_idle_night(self):
        summary = self.run_report(
            turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        )
        self.assertEqual(len(summary["segments"]), 1)
        self.assertTrue(summary["promotion_ready"])

    def test_unusable_lines_above_the_ceiling_fail_instead_of_warning(self):
        events = turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        garbage = ["{not json", '{"schema": "relay_authority.axis_a.v1", "site": "bridge_entry"}']
        summary = self.run_report(events, extra_lines=garbage * 20)
        self.assertGreater(summary["line_integrity"]["unusable"], 0)
        self.assertFalse(summary["criteria"]["line_integrity"]["met"])
        self.assertFalse(summary["promotion_ready"])

    def test_a_few_unusable_lines_stay_under_the_ceiling(self):
        events = turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        summary = self.run_report(events, extra_lines=["{not json"])
        self.assertEqual(summary["line_integrity"]["unparseable"], 1)
        self.assertTrue(summary["criteria"]["line_integrity"]["met"])
        self.assertTrue(summary["promotion_ready"])

    def test_windows_key_on_observation_time_not_publish_time(self):
        """A stranded turn is published when its successor arrives, days later.

        Windowing on `ts` credited it to the successor's day, which is what
        E4-5 corrected. Here every record is published on one day and the window
        must still see the seven days the turns were observed on.
        """

        events = turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        published = (BASE + timedelta(days=30)).isoformat()
        for item in events:
            item["ts"] = published
        summary = self.run_report(events)
        self.assertEqual(summary["criteria"]["window_days"]["value"], 7)
        self.assertEqual(
            summary["target_segment"]["first_observed"][:10],
            BASE.date().isoformat(),
        )
        self.assertTrue(summary["promotion_ready"])

    def test_the_day_window_filters_on_observation_time(self):
        events = turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        kept = report.apply_window(events, 2)
        days = {item["observed_at"][:10] for item in kept}
        self.assertLessEqual(len(days), 3)
        self.assertIn((BASE + timedelta(days=6)).date().isoformat(), days)

    def test_completion_observed_at_does_not_move_the_axis_a_day_window(self):
        lifecycle = turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        future_completion = completion_event(
            site="completion_r4",
            turn=99_999,
            observed=BASE + timedelta(days=365),
            scope="foreign",
            scope_reason="foreign_episode",
        )

        kept = report.apply_window(lifecycle + [future_completion], 2)
        lifecycle_days = {
            item["observed_at"][:10] for item in kept if report.is_axis_a_event(item)
        }
        self.assertIn((BASE + timedelta(days=6)).date().isoformat(), lifecycle_days)
        self.assertGreater(len(lifecycle_days), 0)

    def test_an_empty_log_is_not_promotion_ready(self):
        summary = self.run_report([], argv=[])
        self.assertFalse(summary["promotion_ready"])
        self.assertIsNone(summary["target_segment"]["cohort_fingerprint"])
        self.assertFalse(summary["criteria"]["line_integrity"]["met"])

    def test_the_rowless_share_is_reported_as_s7a_owned(self):
        summary = self.run_report(
            turns(210, days=7, sites=("bridge_entry", "stream_loop", "loop_exit"))
        )
        self.assertIsNone(summary["rowless_no_range_share"])
        self.assertEqual(
            sorted(summary["target_segment"]["unmeasured_fields"]),
            sorted(report.UNMEASURED_UNTIL_S7A),
        )


if __name__ == "__main__":
    unittest.main()
