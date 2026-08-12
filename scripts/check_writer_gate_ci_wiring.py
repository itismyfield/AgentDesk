#!/usr/bin/env python3
"""Protect writer call-site gate wiring outside ``ci-script-checks.sh``.

The required PR ``Script checks`` job invokes this checker directly.  Keeping
the checker outside the aggregate script means removal of an aggregate writer
gate or of either tested gate's unittest command is observable even when that
removal would otherwise stop the corresponding wiring test from running.

This is an exact shell-command contract, not a shell parser.  Only a complete,
unindented executable line counts; comments, echoes, command suffixes, and
duplicates fail closed.
"""

from __future__ import annotations

import argparse
import sys
from dataclasses import dataclass
from pathlib import Path


CI_SCRIPT = Path("scripts/ci-script-checks.sh")


@dataclass(frozen=True)
class RequiredInvocation:
    label: str
    command: str


REQUIRED_INVOCATIONS = (
    RequiredInvocation(
        "delivery-journal raw-writer gate",
        '"$PYTHON" scripts/check_delivery_journal_raw_writer.py',
    ),
    RequiredInvocation(
        "durable-frontier writer gate",
        '"$PYTHON" scripts/check_durable_frontier_writer_call_sites.py',
    ),
    RequiredInvocation(
        "durable-frontier writer unittest module",
        '"$PYTHON" -m unittest tests.test_durable_frontier_writer_call_sites',
    ),
    RequiredInvocation(
        "intake-outbox done-writer gate",
        '"$PYTHON" scripts/check_intake_outbox_done_writer_call_sites.py',
    ),
    RequiredInvocation(
        "intake-outbox done-writer unittest module",
        '"$PYTHON" -m unittest tests.test_intake_outbox_done_writer_call_sites',
    ),
)


def check_text(text: str) -> list[str]:
    """Return contract violations for an aggregate-script snapshot."""
    lines = text.splitlines()
    errors: list[str] = []
    positions: dict[str, int] = {}

    for required in REQUIRED_INVOCATIONS:
        matches = [index for index, line in enumerate(lines) if line == required.command]
        if len(matches) != 1:
            errors.append(
                f"{required.label}: expected exactly one executable invocation "
                f"{required.command!r}, found {len(matches)}"
            )
        else:
            positions[required.label] = matches[0]

    ordered_pairs = (
        ("durable-frontier writer gate", "durable-frontier writer unittest module"),
        ("intake-outbox done-writer gate", "intake-outbox done-writer unittest module"),
    )
    for gate_label, test_label in ordered_pairs:
        if gate_label in positions and test_label in positions:
            if positions[gate_label] >= positions[test_label]:
                errors.append(f"{gate_label} must run before {test_label}")

    return errors


def check(repo_root: Path) -> list[str]:
    path = repo_root / CI_SCRIPT
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        return [f"cannot read {CI_SCRIPT}: {error}"]
    return check_text(text)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parents[1],
        help="repository root (defaults to the parent of scripts/)",
    )
    args = parser.parse_args(argv)

    errors = check(args.repo_root.resolve())
    if errors:
        for error in errors:
            print(f"ERROR: writer gate CI wiring: {error}", file=sys.stderr)
        return 1

    print(
        "writer gate CI wiring check passed: "
        f"{len(REQUIRED_INVOCATIONS)} exact aggregate invocations protected"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
