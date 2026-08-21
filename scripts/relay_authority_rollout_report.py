#!/usr/bin/env python3
"""Aggregate the #5464 (#5071 T5) relay-authority observation log.

The S2 slice writes one JSONL event per turn per gate to
``<agentdesk root>/relay_authority/YYYY-MM-DD.jsonl``. Design r3 §5.1 chose that
event log over an in-process rolling aggregate precisely so the aggregation
could live here, out of the production binary — this script is the whole cost
of that choice, and it adds **0 production LoC**.

Design §5.3 makes the promotion decision a human procedure reading this output,
not an automatic gate. The exit status therefore reports whether the axis-A
criteria are *met*, so a runbook step can branch on it, but nothing in the
running system consults it.

Time base: every promotion window, day count and segment boundary here is keyed
only on axis-A lifecycle records' ``observed_at`` (when the turn started), NOT on
``ts`` (when the record was published). Completion ``observed_at`` values are
telemetry-only and cannot move a promotion window. Publication and observation
times differ by design — a turn stranded by a bridge exit is published when the
*next* turn on its channel arrives, which for an idle channel is hours or days
later, and windowing on ``ts`` dated exactly the populations this slice exists to
measure to the wrong day (ERRATUM R3-E4/E4-5).

Completion R0–R4 records are displayed separately and never enter segmentation,
turn/day counts, publication provenance, coverage, or integrity denominators.
Their fresh reads are independent, non-atomic samples: a prefix may survive a
crash and different sites may describe different mailbox episodes, so the rows
must not be interpreted as one completion snapshot.

Promotion criteria (design §5.3, S2's share of the table). All of them are
evaluated over ONE segment — the newest contiguous run of samples at a single
cohort fingerprint — never over the whole input, so samples from two different
dial positions cannot be added up into one promotion case:

* ``window_days``          — ≥7 distinct days of samples
* ``turn_samples``         — ≥200 distinct turns (≥500 for stage 2)
* ``new_stricter``         — MUST be 0. A nonzero value means the AC2-R gate
                             would have ended a lifecycle the shipped gate kept,
                             which the monotone-relaxing contract forbids and
                             which blocks S4/S7a outright.
* ``loop_exit_coverage``   — loop-exit records per bridge-entry record, floored.
* ``stream_coverage``      — stream-loop records per bridge-entry record, floored.
* ``line_integrity``       — unusable lines, capped, as a share of the target
                             segment's own records plus them (see below).

The last three are what make an incomplete log fail instead of pass. The
JSONL sink is best-effort by contract (nothing may propagate back into the turn
that produced it), so a full-disk or half-written window is reachable, and a
turn's three site records are three separate writes. Counting turns alone cannot
tell that apart from a quiet window: 200 entry-only turns satisfied every
turn-counting floor while carrying no stream-gate evidence at all, which is the
only evidence S4 is promoted on (legB P1-2).

Segmentation: ``config_live_reload`` keeps no generation counter (§5.2), so two
rollout windows at the same dial position share a fingerprint. The primary
boundary is therefore not a clock threshold but the input's own witness that the
dial moved — a sample carrying a *different* fingerprint sitting between two
samples of one fingerprint. See [`segment_events`] for why that is the canonical
discriminator and what the gap threshold is and is not.

Scope of ``line_integrity``: an unusable line cannot be dated — that is what
makes it unusable — so it cannot be assigned to a segment by observation time,
and it is attributed to the *file* it sits in instead. The numerator is
therefore file-scoped; the denominator is NOT. It is the target segment's own
usable record count plus those unusable lines, never the files' whole line
count. Two rounds of the same dilution came from widening it: a whole-input
tally let 10,000 lines of history read a 3.08% loss as 0.19% (legB r2c P1-2),
and the file-scoped tally that replaced it let the previous segment's records
in the target's own first file — daily files are named by publish day, the dial
moves mid-day — read the same 3.08% as 0.92% and flip promotion (legA/legB r3c
P1-1). Only counting the target's own records closes both.

Both residuals of that choice are stated rather than hidden, and they do NOT
point the same way. Unusable lines belonging to a cohabiting segment are charged
to this target: that one is false-red, the direction a promotion gate may err in.
But a file containing NO usable target record is outside the scope entirely,
which drops its unusable lines from the numerator AND the denominator and so
*lowers* the measured share — that residual is FAIL-OPEN. A day of the target's
own window that is unusable end to end shows up only in
``line_integrity_all_files``, and no criterion can see it: a mixed-version schema
bump that left one day of a nine-day window unparseable read ``0/810 = 0.0%``
scoped while the whole input was 55.25% unusable, with ``promotion_ready`` True
(legA rc P1-1).

Neither exclusion is silent and neither is judged: ``cohabiting_usable_lines``
reports the usable lines the scope excluded, ``out_of_scope_unusable_lines`` the
unusable ones. Because the fail-open residual is invisible to all six criteria,
a runbook reading this output has to watch that second field and the
``line_integrity_all_files`` ratio itself before promoting on a green
``line_integrity``.

Known self-blocking limit of the coverage floors: see ``SITE_COVERAGE_FLOOR``.

Reported but deliberately NOT gated: ``publish_reasons`` and the evicted share
derived from them. A turn that leaves the bridge without reaching post-loop
finalize is published by its *successor's* entry gate, and if no successor comes
it is never published at all — a loss no coverage floor can see, because the
three site records of one turn are written in one call and vanish together
(``authority_observation``'s module docstring carries the full statement). The
evicted share is the visible half of that: the fraction of this window that was
one absent successor away from being lost. Whether it should become a floor is
S4's call, recorded in the §12-2 inventory; this script only shows it.

Read its direction carefully: it moves the *wrong* way under the loss it is
about. A turn actually lost while stranded is not in the log at all, so it
lowers the evicted share instead of raising it, and a window that lost every
stranded turn reports 0.0 — indistinguishable from a healthy window where no
turn was ever stranded. A low value is therefore not an all-clear; only a high
value carries information (legA r3c P2-6).

Reported but deliberately NOT gated, because S2 does not measure them:
``frontier_already_covers`` and ``unbound_anchor_left`` are S7a fields (that
slice computes the frontier for its own gate, so recording it there is free —
S2 will not add a durable read to the completion path just to observe), and
``rowless_no_range_share`` is S7a's too: a ``Missing``-at-entry turn is ended by
the shipped gate before the bridge loop starts, so rowless turns cannot reach a
loop exit until enforcement lands and the ratio has no denominator here
(ERRATUM R3-E4/E4-6).

Usage::

    scripts/relay_authority_rollout_report.py [--root DIR] [--days N]
                                              [--stage {1,2}] [--json]
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from collections import Counter
from datetime import datetime, timedelta
from pathlib import Path

SCHEMA = "relay_authority.axis_a.v3"
STAGE_TURN_FLOOR = {1: 200, 2: 500}
WINDOW_DAY_FLOOR = 7
# Per-site record counts, as a share of bridge-entry records. Both floors are
# below 1.0 on purpose: the two populations this slice measures legitimately skip
# post-loop finalize (`Missing` at entry is ended by the shipped gate, and
# `AuthorityLost` leaves the bridge mid-stream), and a turn that never performed a
# visible mutation legitimately has no stream record. But a window where more than
# half the observed turns are missing either record has no promotion case either
# way: it is a bridge failing at a rate that itself blocks S4, or a sink losing
# records. Both must stop the gate rather than pass on the turn count.
#
# The honest limit of that reasoning, stated because the threshold is not being
# changed: those two populations are *why* S4 exists, so if they ever exceed half
# the window this floor is a permanent FAIL and no amount of further observation
# can promote the fix for them. "The bridge is failing at a rate that blocks S4"
# and "S4 is needed badly" are the same measurement. The polarity is deliberately
# fail-closed — a window this incomplete has no promotion case on the evidence
# available here — so the escape hatch is not a lower number, it is S4 arriving
# with a narrower population or a per-population denominator (legA r2c P2-5).
SITE_COVERAGE_FLOOR = 0.5
# Unusable lines (unparseable, wrong schema, or undatable) as a share of every
# line read. Not zero: multiple processes append to one daily file with no
# cross-process line lock, so a rare interleaved line is expected. A rate above
# this means the log's completeness cannot be asserted at all.
MALFORMED_LINE_CEILING = 0.01
# Secondary segmentation heuristic, NOT a derived bound — see `segment_events`.
SEGMENT_GAP_HOURS = 48
# Which file a usable record came from, stapled on at load time so `line_integrity`
# can be scoped to the target segment. Internal to this script; never emitted.
SOURCE_FILE_KEY = "_source_file"
# Per-file line tally keys. `unusable` is derived from the three failure counters.
INTEGRITY_COUNTERS = ("lines", "unparseable", "schema_mismatch", "undatable")
COMPLETION_LINES = "_completion_lines"
# Design §4.3/§5.3 fields the S2 emitter cannot produce; re-assigned to S7a.
UNMEASURED_UNTIL_S7A = ("frontier_already_covers", "unbound_anchor_left")


def default_root() -> Path:
    override = os.environ.get("AGENTDESK_ROOT_DIR", "").strip()
    if override:
        return Path(override)
    return Path.home() / ".adk"


def event_time(event: dict) -> datetime | None:
    """When the turn was observed, not when its record was published.

    Falls back to ``ts`` only so a record that somehow lost ``observed_at`` is
    still placed in a window rather than dropped. Both stamps are the producing
    host's local clock, so the offset is discarded to keep every comparison here
    between naive local times.
    """

    for key in ("observed_at", "ts"):
        raw = event.get(key)
        if not isinstance(raw, str):
            continue
        try:
            return datetime.fromisoformat(raw).replace(tzinfo=None)
        except ValueError:
            continue
    return None


def empty_integrity() -> dict:
    return {name: 0 for name in INTEGRITY_COUNTERS} | {
        "unusable": 0,
        COMPLETION_LINES: 0,
    }


def merge_integrity(tallies) -> dict:
    """Sum per-file tallies into one, re-deriving ``unusable``."""

    total = empty_integrity()
    for tally in tallies:
        for name in (*INTEGRITY_COUNTERS, COMPLETION_LINES):
            total[name] += tally[name]
    total["unusable"] = total["unparseable"] + total["schema_mismatch"] + total["undatable"]
    return total


def all_file_integrity(by_file: dict[str, dict]) -> dict:
    total = merge_integrity(by_file.values())
    total["lines"] -= total.pop(COMPLETION_LINES)
    return total


def load_events(directory: Path) -> tuple[list[dict], list[str], dict[str, dict]]:
    """Read every event file. Returns (events, warnings, integrity BY FILE).

    Files are named by publish day, so all of them are read and the window is
    applied to ``observed_at`` afterwards — a straggler published on day N can
    belong to day N-3.

    The line tally is kept per file rather than as one number because an unusable
    line has no usable observation time by construction, so the file is the finest
    scope it can be attributed to. It is the tally's *numerator* that this buys a
    scope for — see ``scoped_integrity`` for why the denominator cannot be the
    file's line count either (legB r2c P1-2, legA/legB r3c P1-1).
    """

    warnings: list[str] = []
    by_file: dict[str, dict] = {}
    if not directory.is_dir():
        return [], [f"no event log directory at {directory}"], by_file
    events: list[dict] = []
    for path in sorted(directory.glob("*.jsonl")):
        integrity = by_file.setdefault(path.name, empty_integrity())
        for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            line = line.strip()
            if not line:
                continue
            integrity["lines"] += 1
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                integrity["unparseable"] += 1
                warnings.append(f"{path.name}:{lineno}: unparseable line skipped")
                continue
            if not isinstance(event, dict):
                # A bare scalar or list is valid JSON and not a record. This is
                # reachable from the very interleaving MALFORMED_LINE_CEILING
                # exists for — a fragment of one writer's line can parse on its
                # own — and it used to raise AttributeError on the `.get` below,
                # aborting the run before any criterion was evaluated. Absorbed
                # into the same unusable tally as every other junk line so the
                # scope fix above still gets to be judged (legA r3c P2-2).
                integrity["schema_mismatch"] += 1
                warnings.append(
                    f"{path.name}:{lineno}: line is a JSON "
                    f"{type(event).__name__}, not an object, skipped"
                )
                continue
            schema = event.get("schema")
            if schema != SCHEMA:
                integrity["schema_mismatch"] += 1
                warnings.append(
                    f"{path.name}:{lineno}: schema {schema!r} is not {SCHEMA!r}, skipped"
                )
                continue
            if event_time(event) is None:
                integrity["undatable"] += 1
                warnings.append(f"{path.name}:{lineno}: no usable observation time, skipped")
                continue
            if str(event.get("site") or "").startswith("completion_"):
                integrity[COMPLETION_LINES] += 1
            event[SOURCE_FILE_KEY] = path.name
            events.append(event)
        integrity["unusable"] = (
            integrity["unparseable"] + integrity["schema_mismatch"] + integrity["undatable"]
        )
    return events, warnings, by_file


def apply_window(events: list[dict], days: int | None) -> list[dict]:
    """Keep the newest ``days`` days of OBSERVATION time.

    The old shape sliced the newest N *files*, which is publish time and a
    different axis from the ``window_days`` criterion it fed (legA P2-7).
    """

    if days is None or not events:
        return events
    lifecycle_times = [event_time(event) for event in events if is_axis_a_event(event)]
    if not lifecycle_times:
        return events
    floor = max(lifecycle_times) - timedelta(days=days)
    return [event for event in events if event_time(event) > floor]


def turn_key(event: dict) -> tuple:
    return (
        event.get("host"),
        event.get("process_generation"),
        event.get("runtime_ptr"),
        event.get("channel_id"),
        event.get("turn_id"),
    )


def is_axis_a_event(event: dict) -> bool:
    return event.get("site") in {"bridge_entry", "stream_loop", "loop_exit"} and isinstance(
        event.get("axis_a"), dict
    )


def completion_scope_counts(events: list[dict]) -> dict:
    return dict(
        Counter(
            f"{event.get('site')}:{event.get('scope')}:{event.get('scope_reason')}"
            for event in events
            if str(event.get("site") or "").startswith("completion_")
        )
    )


def segment_events(events: list[dict]) -> list[dict]:
    """Split the input into contiguous single-fingerprint runs, oldest first.

    Walked in GLOBAL observation-time order, not per fingerprint. A run ends where
    the input itself witnesses that the dial moved: the next sample carries a
    different ``cohort_fingerprint``. That interleaving is the canonical
    discriminator, and it is already in the input — bucketing by fingerprint first
    threw it away, which merged two rollout windows separated by a dial excursion
    shorter than the gap below into one promotion case (legA/legB r2c P1-1: 120
    turns over 4 days plus 100 turns over 4 days, with a 22-hour excursion between
    them, read as one 220-turn 8-day window and passed every floor).

    What the interleaving actually witnesses is narrower than "the dial moved":
    it is "the observed population disagreed about the fingerprint".
    ``cohort_fingerprint`` carries ``mode`` and ``percent`` and not the host, so
    while a config rollout is part-way across hosts, two hosts' samples alternate
    second by second and shred the window into many one-sample runs, leaving a
    target far smaller than the real one. The direction is fail-closed — the
    floors under-count and refuse promotion — so it is a declared domain rather
    than a defect, but it is a domain (legA r3c P2-3).

    ``SEGMENT_GAP_HOURS`` remains as a secondary heuristic for the cases the
    input *cannot* witness: the dial left the observing set entirely so nothing
    was recorded, or samples did exist during the excursion and were all lost
    while stranded (low reachability, since eviction bounds unpublished turns at
    one per channel — legA r3c P2-4). Either way there is no interleaved sample
    to find. It is a judgement about
    how long a channel may go quiet and still be one window — not a derived bound.
    The r2 docstring argued it from "a gap this wide contains at least one calendar
    day with no sample"; that property is true (a gap > 48h strictly contains some
    calendar day) but it does not discriminate, because a 25-hour gap can contain
    an empty calendar day too and is deliberately NOT split, and the
    ``window_days`` floor counts distinct days without requiring them to be
    consecutive (legA r2c §6). The comparison is ``>=`` so that a gap of exactly
    the threshold splits rather than merges.
    """

    ordered = sorted(
        (
            (event_time(event), str(event.get("cohort_fingerprint")), event)
            for event in events
            if is_axis_a_event(event)
        ),
        key=lambda item: (item[0], item[1]),
    )
    gap = timedelta(hours=SEGMENT_GAP_HOURS)
    segments: list[dict] = []
    run: list[tuple[datetime, dict]] = []
    current: str | None = None
    for moment, fingerprint, event in ordered:
        if run and (fingerprint != current or moment - run[-1][0] >= gap):
            segments.append(build_segment(current, run))
            run = []
        current = fingerprint
        run.append((moment, event))
    if run:
        segments.append(build_segment(current, run))
    return segments


def build_segment(fingerprint: str, run: list[tuple[datetime, dict]]) -> dict:
    return {
        "cohort_fingerprint": fingerprint,
        "first_observed": run[0][0].isoformat(),
        "last_observed": run[-1][0].isoformat(),
        "events": [event for _, event in run],
    }


def tally(events: list[dict]) -> dict:
    """Promotion counts over an already axis-A-only segment."""

    events = [event for event in events if is_axis_a_event(event)]
    days = {event_time(event).date().isoformat() for event in events}
    turns = {turn_key(event) for event in events}
    sites = Counter()
    entry_verdicts = Counter()
    rowless_turns: set[tuple] = set()
    range_shapes = Counter()
    stream = Counter()
    unmeasured = Counter()
    # Provenance is a property of the publication, so it is counted per turn: all
    # three of a turn's site records are written by one call and carry one value.
    # A record without the field is `unattributed` rather than assumed — there are
    # no such records in a v2 log, and guessing would be the fail-open reading.
    reason_of_turn: dict[tuple, str] = {}

    for event in events:
        site = event.get("site")
        sites[site] += 1
        reason_of_turn.setdefault(
            turn_key(event), str(event.get("publish_reason") or "unattributed")
        )
        axis_a = event.get("axis_a") or {}
        if site == "bridge_entry":
            entry_verdicts[f"{axis_a.get('old')}->{axis_a.get('new')}"] += 1
            if axis_a.get("rowless_continuation"):
                rowless_turns.add(turn_key(event))
        elif site == "stream_loop":
            for field in (
                "ticks",
                "old_ended_lifecycle",
                "new_ended_lifecycle",
                "diff",
                "new_stricter",
            ):
                stream[field] += int(axis_a.get(field) or 0)
        elif site == "loop_exit":
            range_shapes[axis_a.get("lease_range_shape")] += 1
            # S7a fields. Absent until that slice lands; counted, never inferred.
            for field in UNMEASURED_UNTIL_S7A:
                if field not in axis_a:
                    unmeasured[field] += 1

    reasons = Counter(reason_of_turn.values())
    published = sum(reasons.values())
    return {
        "days": sorted(days),
        "turn_samples": len(turns),
        "sites": dict(sites),
        "entry_verdict_transitions": dict(entry_verdicts),
        "rowless_continuation_turns": len(rowless_turns),
        "stream_gate": dict(stream),
        "lease_range_shapes": dict(range_shapes),
        "unmeasured_fields": dict(unmeasured),
        "publish_reasons": dict(reasons),
        # Displayed, never gated (see the module docstring). This is the share of
        # the window that reached the log only because a successor turn arrived.
        "evicted_publication_share": (
            (reasons.get("evicted", 0) / published) if published else None
        ),
    }


def site_coverage(counts: dict, site: str) -> dict:
    entries = counts.get("bridge_entry", 0)
    covered = counts.get(site, 0)
    share = (covered / entries) if entries else None
    return {
        "value": covered,
        "of": entries,
        "share": share,
        "floor": SITE_COVERAGE_FLOOR,
        "met": share is not None and share >= SITE_COVERAGE_FLOOR,
    }


def criteria_for(counts: dict, stage: int, integrity: dict) -> dict:
    lines = integrity["lines"]
    unusable_share = (integrity["unusable"] / lines) if lines else None
    turn_floor = STAGE_TURN_FLOOR[stage]
    return {
        "window_days": {
            "value": len(counts["days"]),
            "floor": WINDOW_DAY_FLOOR,
            "met": len(counts["days"]) >= WINDOW_DAY_FLOOR,
        },
        "turn_samples": {
            "value": counts["turn_samples"],
            "floor": turn_floor,
            "met": counts["turn_samples"] >= turn_floor,
        },
        "new_stricter": {
            "value": counts["stream_gate"].get("new_stricter", 0),
            "must_be": 0,
            "met": counts["stream_gate"].get("new_stricter", 0) == 0,
        },
        "loop_exit_coverage": site_coverage(counts["sites"], "loop_exit"),
        "stream_coverage": site_coverage(counts["sites"], "stream_loop"),
        "line_integrity": {
            "value": integrity["unusable"],
            "of": lines,
            "share": unusable_share,
            "ceiling": MALFORMED_LINE_CEILING,
            "met": unusable_share is not None and unusable_share <= MALFORMED_LINE_CEILING,
        },
    }


def scoped_integrity(target: dict | None, by_file: dict[str, dict]) -> dict:
    """The line tally charged to the target segment.

    Denominator: the target segment's own usable records, PLUS every unusable
    line in the files those records were read from. Deliberately not the whole
    line count of those files. A daily file is named by publish day and the dial
    moves mid-day, so the target's first file routinely also holds the previous
    segment's records for that day; counting them diluted the target's own
    losses below the ceiling exactly as the whole-input tally used to (legA/legB
    r3c P1-1: a target losing 20 of its own 650 lines, 3.08%, read as 0.92%
    against 1,530 cohabiting lines and flipped ``promotion_ready`` to True).
    Cohabitation is the default at segment birth, not a corner case, and it is
    worst when the target's sample is thinnest.

    An unusable line has no observation time — that is what makes it unusable —
    so it cannot be split between the segments sharing its file, and it is
    charged to the target whole. Both residuals of that choice are declared
    rather than hidden, and they point OPPOSITE ways:

    * a cohabiting segment's unusable lines are charged to this target, so a
      neighbour's corruption can fail a clean target — false-red;
    * a file holding NO usable target record is outside the scope entirely,
      which takes its unusable lines out of the numerator AND the denominator
      and so *lowers* the measured share — FAIL-OPEN, not false-red. A day's
      file that is unusable end to end shows up only in
      ``line_integrity_all_files``. Both ends of that are reachable: history
      files accumulate exactly the interleaved junk ``MALFORMED_LINE_CEILING``
      documents as expected, so 36 such lines beside a clean target read
      ``0/630 = 0.0%`` here while the whole input is 4.26%; and a mixed-version
      schema bump can lose one whole day of the target's own window, which read
      ``0/810 = 0.0%`` scoped against 55.25% unusable whole-input and left
      ``promotion_ready`` True (legA rc P1-1).

    ``out_of_scope_unusable_lines`` is what bounds the second residual: it is the
    only way to audit that exclusion from this output, and like
    ``cohabiting_usable_lines`` it is displayed, never judged. No criterion can
    see the fail-open residual, so the runbook — not this script — is what has to
    read that field and the whole-input ratio next to a green ``line_integrity``.

    Dating an unparseable line by its position between the datable lines around
    it would close both, and is a machine for S4 rather than this slice (§12-2).
    """

    files = sorted(
        {event[SOURCE_FILE_KEY] for event in target["events"] if SOURCE_FILE_KEY in event}
        if target
        else set()
    )
    scoped = merge_integrity(by_file[name] for name in files if name in by_file)
    records = len(target["events"]) if target else 0
    # Usable lines in those files belonging to some other segment. Excluded from
    # the denominator; reported so the exclusion is auditable rather than silent.
    scoped["cohabiting_usable_lines"] = (
        scoped["lines"] - scoped["unusable"] - scoped[COMPLETION_LINES] - records
    )
    # The symmetric display for the fail-open residual: unusable lines in files
    # this target has no usable record in, which leave BOTH sides of the ratio.
    # Displayed only — no criterion reads it, and none is added here.
    scoped["out_of_scope_unusable_lines"] = (
        merge_integrity(by_file.values())["unusable"] - scoped["unusable"]
    )
    scoped["lines"] = records + scoped["unusable"]
    scoped.pop(COMPLETION_LINES)
    scoped["files"] = files
    return scoped


def summarize(events: list[dict], stage: int, by_file: dict[str, dict]) -> dict:
    lifecycle_events = [event for event in events if is_axis_a_event(event)]
    segments = segment_events(lifecycle_events)
    target = segments[-1] if segments else None
    target_counts = tally(target["events"]) if target else tally([])
    target_fingerprint = target["cohort_fingerprint"] if target else None
    target_counts["completion_scopes"] = completion_scope_counts(
        event
        for event in events
        if target
        and str(event.get("cohort_fingerprint")) == target_fingerprint
        and datetime.fromisoformat(target["first_observed"])
        <= event_time(event)
        <= datetime.fromisoformat(target["last_observed"])
    )
    integrity = scoped_integrity(target, by_file)
    criteria = criteria_for(target_counts, stage, integrity)

    return {
        "stage": stage,
        "target_segment": {
            "cohort_fingerprint": target["cohort_fingerprint"] if target else None,
            "first_observed": target["first_observed"] if target else None,
            "last_observed": target["last_observed"] if target else None,
            **target_counts,
        },
        "segments": [
            {
                "cohort_fingerprint": segment["cohort_fingerprint"],
                "first_observed": segment["first_observed"],
                "last_observed": segment["last_observed"],
                "turns": len({turn_key(event) for event in segment["events"]}),
                "days": len(
                    {event_time(event).date().isoformat() for event in segment["events"]}
                ),
            }
            for segment in segments
        ],
        # Context only. Promotion is judged on the target segment above, because
        # two segments can sit at different dial positions.
        "all_segments_turn_samples": len({turn_key(event) for event in lifecycle_events}),
        "rowless_no_range_share": None,
        "line_integrity": integrity,
        # Context only, for the same reason `all_segments_turn_samples` is: the
        # whole input spans dial positions this promotion case is not about.
        "line_integrity_all_files": all_file_integrity(by_file),
        "criteria": criteria,
        "promotion_ready": all(item["met"] for item in criteria.values()),
    }


def render(summary: dict, warnings: list[str]) -> str:
    target = summary["target_segment"]
    days = target["days"]
    lines = [
        f"relay-authority axis-A rollout report (stage {summary['stage']})",
        "",
        f"  segments           : {len(summary['segments'])} "
        f"(promotion judged on the newest one only)",
    ]
    for segment in summary["segments"]:
        lines.append(
            f"    {segment['cohort_fingerprint']}  turns={segment['turns']}  "
            f"days={segment['days']}  "
            f"{segment['first_observed']} .. {segment['last_observed']}"
        )
    lines.extend(
        [
            "",
            f"  target fingerprint : {target['cohort_fingerprint']}",
            f"  observed window    : {target['first_observed']} .. {target['last_observed']}",
            f"  days in window     : {len(days)} {days[:1]}..{days[-1:]}",
            f"  distinct turns     : {target['turn_samples']}",
            f"  events by site     : {target['sites']}",
            f"  entry old->new     : {target['entry_verdict_transitions']}",
            f"  rowless turns      : {target['rowless_continuation_turns']}",
            f"  stream gate totals : {target['stream_gate']}",
            f"  lease range shapes : {target['lease_range_shapes']}",
            f"  completion scopes  : {target['completion_scopes']}",
            f"  published by       : {target['publish_reasons']}",
            f"  evicted share      : {target['evicted_publication_share']} "
            "(displayed, NOT gated — the share that needed a successor to be"
            " logged at all; S4 owns whether it becomes a floor)",
            "  rowless no_range   : unmeasured (S7a owns it — a rowless turn cannot"
            " reach loop exit before enforcement)",
            f"  integrity scope    : {len(summary['line_integrity']['files'])} file(s) of"
            f" the target segment, excluding"
            f" {summary['line_integrity']['cohabiting_usable_lines']} usable line(s)"
            f" cohabiting there and"
            f" {summary['line_integrity']['out_of_scope_unusable_lines']} unusable line(s)"
            " in files with no target record (that second exclusion is FAIL-OPEN and no"
            " criterion sees it — read it against the whole input);"
            f" whole input {summary['line_integrity_all_files']}",
        ]
    )
    if target["unmeasured_fields"]:
        lines.append(f"  unmeasured (S7a)   : {target['unmeasured_fields']}")
    lines.append("")
    lines.append("  criteria:")
    for name, item in summary["criteria"].items():
        mark = "PASS" if item["met"] else "FAIL"
        detail = ", ".join(f"{key}={value}" for key, value in item.items() if key != "met")
        lines.append(f"    [{mark}] {name}: {detail}")
    lines.append("")
    lines.append(
        f"  promotion_ready: {summary['promotion_ready']} "
        "(advisory — design §5.3 makes promotion a human decision)"
    )
    for warning in warnings:
        lines.append(f"  warning: {warning}")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--root",
        type=Path,
        default=None,
        help="AgentDesk runtime root (default: $AGENTDESK_ROOT_DIR, else ~/.adk)",
    )
    parser.add_argument(
        "--days",
        type=int,
        default=None,
        help="only keep the newest N days of observation time (default: all of them). "
        "Every file is still read: publish day and observation day are different "
        "axes, so this narrows the judged window, not the I/O",
    )
    parser.add_argument(
        "--stage",
        type=int,
        choices=sorted(STAGE_TURN_FLOOR),
        default=1,
        help="rollout stage, which selects the turn-sample floor (default: 1)",
    )
    parser.add_argument("--json", action="store_true", help="emit the summary as JSON")
    args = parser.parse_args(argv)

    root = args.root or default_root()
    events, warnings, by_file = load_events(root / "relay_authority")
    summary = summarize(apply_window(events, args.days), args.stage, by_file)
    if args.json:
        print(json.dumps({"summary": summary, "warnings": warnings}, indent=2, sort_keys=True))
    else:
        print(render(summary, warnings))
    return 0 if summary["promotion_ready"] else 1


if __name__ == "__main__":
    sys.exit(main())
