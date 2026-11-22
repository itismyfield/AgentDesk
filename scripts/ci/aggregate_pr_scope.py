#!/usr/bin/env python3
"""Measure scope-first PR test coverage from downloaded nextest-pr-scope artifacts."""

import argparse
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timezone
import json
import math
from pathlib import Path
from statistics import median
import sys


OBSERVATION_NAME = "scope-observation.json"


def timestamp(value):
    parsed = datetime.fromisoformat(value)
    if parsed.tzinfo is None:
        raise ValueError("timestamp needs a UTC offset")
    return parsed.astimezone(timezone.utc)


def positive_int(value, field):
    text = str(value)
    if not text.isascii() or not text.isdecimal() or int(text) < 1:
        raise ValueError(f"{field} must be a positive integer")
    return int(text)


def exact_ids(value, field):
    if not isinstance(value, list) or any(
        not isinstance(row, list) or len(row) != 2
        or any(not isinstance(part, str) or not part for part in row)
        for row in value
    ):
        raise ValueError(f"{field} must contain [binary ID, test name] pairs")
    ids = frozenset(map(tuple, value))
    if len(ids) != len(value):
        raise ValueError(f"{field} contains duplicate test IDs")
    return ids


@dataclass(frozen=True)
class Observation:
    run: tuple
    recorded_at: datetime
    mode: str
    reason: str
    scope_ids: frozenset
    scope_seconds: float | None
    scope_state: str
    full_state: str
    scope_failure_ids: frozenset
    full_failure_ids: frozenset
    scope_miss_ids: frozenset


def read_observation(path):
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(doc, dict) or type(doc.get("version")) is not int or doc["version"] != 1:
            raise ValueError("expected a version 1 PR scope observation")
        mode, reason = doc["mode"], doc["reason"]
        scope_state, full_state = doc["scope_state"], doc["full_state"]
        if mode not in ("scoped", "full", "skipped") or not isinstance(reason, str) or not reason:
            raise ValueError("unknown mode or empty reason")
        if scope_state not in ("passed", "failed", "incomplete", "not_run"):
            raise ValueError(f"unknown scope state {scope_state!r}")
        if full_state not in ("complete", "incomplete", "not_run"):
            raise ValueError(f"unknown full state {full_state!r}")
        seconds = doc["scope_seconds"]
        if seconds is not None and (
            type(seconds) not in (int, float) or not math.isfinite(seconds) or seconds < 0
        ):
            raise ValueError("scope_seconds must be null or a finite nonnegative number")
        scope_ids, scope_failures, full_failures, misses = (
            exact_ids(doc[field], field)
            for field in ("scope_ids", "scope_failure_ids", "full_failure_ids", "scope_miss_ids")
        )
        expected = full_failures - scope_ids if mode == "scoped" else set()
        if misses != expected or scope_failures - scope_ids:
            raise ValueError("failure IDs do not match the selected scope")
        if full_state == "not_run" and full_failures:
            raise ValueError("full phase did not run but reports failures")
        if mode == "scoped" and not scope_ids:
            raise ValueError("scoped observation selected zero tests")
        if scope_state in ("passed", "not_run") and scope_failures:
            raise ValueError("scope state conflicts with scope failures")
        if mode != "scoped" and (scope_state != "not_run" or scope_failures or seconds is not None):
            raise ValueError("scope results are only valid for scoped mode")
        if mode == "skipped" and full_state != "not_run":
            raise ValueError("skipped mode cannot report full results")
        if mode == "scoped" and full_state == "complete" and scope_state != "passed":
            raise ValueError("complete scoped run requires a passing scope phase")
        run = tuple(positive_int(doc[field], field) for field in ("run_id", "run_attempt"))
        return Observation(run, timestamp(doc["recorded_at"]), mode, reason, scope_ids, seconds,
                           scope_state, full_state, scope_failures, full_failures, misses)
    except (OSError, KeyError, OverflowError, TypeError, ValueError) as exc:
        raise ValueError(f"{path}: {exc}") from exc


def collect(paths, since, until):
    attempts = {}
    for path in paths:
        records = sorted(path.rglob(OBSERVATION_NAME)) if path.is_dir() else [path]
        for record in records:
            row = read_observation(record)
            if (since and row.recorded_at < since) or (until and row.recorded_at >= until):
                continue
            if row.run in attempts and row != attempts[row.run]:
                raise ValueError(f"{record}: conflicting observations for run/attempt {row.run}")
            attempts[row.run] = row
    if not attempts:
        raise ValueError("no scope observations in the requested interval")
    return list(attempts.values())


def stats(values):
    if not values:
        return {"samples": 0, "min": None, "median": None, "max": None}
    return {"samples": len(values), "min": min(values), "median": median(values), "max": max(values)}


def ratio(numerator, denominator):
    return {"numerator": numerator, "denominator": denominator,
            "ratio": numerator / denominator if denominator else None}


def aggregate(rows):
    scoped = [row for row in rows if row.mode == "scoped"]
    complete = [row for row in scoped if row.full_state == "complete"]
    early_test_failure = [row for row in scoped if row.scope_state == "failed" and row.scope_failure_ids]
    censored = [row for row in scoped if row.full_state != "complete"]
    miss_runs = sum(bool(row.scope_miss_ids) for row in complete)
    misses = Counter(test_id for row in complete for test_id in row.scope_miss_ids)
    # Early red rules out a scope-only false green. The missing remainder still censors its test IDs.
    return {
        "attempts": len(rows),
        "modes": dict(sorted(Counter(row.mode for row in rows).items())),
        "full_reasons": dict(sorted(Counter(row.reason for row in rows if row.mode == "full").items())),
        "full_states": dict(sorted(Counter(row.full_state for row in rows).items())),
        "scope_size": stats([len(row.scope_ids) for row in scoped]),
        "scope_seconds": stats([row.scope_seconds for row in scoped if row.scope_seconds is not None]),
        "scoped_full_complete": len(complete),
        "scoped_full_censored": len(censored),
        "scoped_early_test_fail": len(early_test_failure),
        "scoped_indeterminate": len(censored) - len(early_test_failure),
        "censored_miss_observations": sum(len(row.scope_miss_ids) for row in censored),
        "misses_by_id_in_complete_runs": [[*test_id, count] for test_id, count in sorted(misses.items())],
        "miss_run_rate_after_scope_pass": ratio(miss_runs, len(complete)),
        "scope_only_false_green_rate_in_decidable_scoped_runs":
            ratio(miss_runs, len(complete) + len(early_test_failure)),
        "miss_id_rate_in_complete_scoped_runs":
            ratio(sum(misses.values()), sum(len(row.full_failure_ids) for row in complete)),
        "miss_id_rate_among_observed_scoped_failures": ratio(
            sum(len(row.scope_miss_ids) for row in scoped),
            sum(len(row.scope_failure_ids | row.full_failure_ids) for row in scoped),
        ),
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, epilog=(
        "Download artifacts named nextest-pr-scope-attempt-* before running. "
        "Rates cover available artifacts; canceled or missing runs without artifacts cannot be inferred. "
        "Use explicit offsets for the inclusive start and exclusive end of the measurement week."
    ))
    parser.add_argument("observations", nargs="+", type=Path, help="JSON files or extracted artifact directories")
    parser.add_argument("--since", type=timestamp)
    parser.add_argument("--until", type=timestamp)
    args = parser.parse_args(argv)
    try:
        if args.since and args.until and args.since >= args.until:
            raise ValueError("--since must be before --until")
        report = aggregate(collect(args.observations, args.since, args.until))
    except ValueError as exc:
        print(f"ERROR {exc}", file=sys.stderr)
        return 2
    report["recorded_at_from_inclusive"] = args.since.isoformat() if args.since else None
    report["recorded_at_until_exclusive"] = args.until.isoformat() if args.until else None
    print(json.dumps(report, sort_keys=True, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
