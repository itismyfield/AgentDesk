#!/usr/bin/env python3
"""Protect writer call-site gate wiring outside ``ci-script-checks.sh``.

The PR ``Script checks`` runner job invokes this checker directly.  Keeping
the checker outside the aggregate script means removal of an aggregate writer
gate or of either tested gate's unittest command is observable even when that
removal would otherwise stop the corresponding wiring test from running.  It
also pins the aggregate invocations that run the CI hardening guard and its
fast wiring unittest, so deleting either aggregate observer alone is visible
to the external step.

This is an exact shell-command contract, not a shell parser.  Only a complete,
unindented executable line counts; comments, echoes, command suffixes, and
duplicates fail closed.  Presence does not prove unconditional execution: a
matching column-zero line can still be nested in shell control flow. The
hardening guard owns the parsed effective-execution contract; this checker pins
the guard's aggregate and external-step assertions so either observer cannot
be removed independently.
"""

from __future__ import annotations

import argparse
import sys
from dataclasses import dataclass
from pathlib import Path


CI_SCRIPT = Path("scripts/ci-script-checks.sh")
FORBIDDEN_AGGREGATE_TEXT = "--write-baseline"


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
    RequiredInvocation(
        "SQL execution surface inventory gate",
        '"$PYTHON" scripts/check_sql_execution_surface_inventory.py --check',
    ),
    RequiredInvocation(
        "SQL execution surface baseline dirty-worktree guard",
        "git diff --exit-code HEAD -- scripts/sql_execution_surface_inventory.json",
    ),
    RequiredInvocation(
        "SQL execution surface inventory unittest module",
        '"$PYTHON" -m unittest tests.test_sql_execution_surface_inventory',
    ),
    RequiredInvocation(
        "CI runner hardening gate",
        "./scripts/check-ci-runner-hardening.sh",
    ),
    RequiredInvocation(
        "fast CI wiring unittest module",
        '"$PYTHON" -m unittest tests.test_fast_check_ci_wiring',
    ),
)

REQUIRED_HARDENING_SNIPPETS = (
    '''unless execution_contract(script_check_execution, expected_script_check_execution)
  expected = JSON.generate(canonical_yaml(expected_script_check_execution))
  found = JSON.generate(canonical_yaml(script_check_execution))
  warn "#{path}: Script checks aggregate effective execution changed; expected #{expected}; found #{found}"
  exit 1
end''',
    '''unless execution_contract(writer_wiring_execution, expected_writer_wiring_execution)
  expected = JSON.generate(canonical_yaml(expected_writer_wiring_execution))
  found = JSON.generate(canonical_yaml(writer_wiring_execution))
  warn "#{path}: writer gate aggregate wiring effective execution changed; expected #{expected}; found #{found}"
  exit 1
end''',
)


def check_text(text: str) -> list[str]:
    """Return contract violations for an aggregate-script snapshot."""
    lines = text.splitlines()
    errors: list[str] = []
    positions: dict[str, int] = {}

    if FORBIDDEN_AGGREGATE_TEXT in text:
        errors.append(
            f"aggregate must not contain {FORBIDDEN_AGGREGATE_TEXT!r}; baseline repins "
            "must remain an explicit reviewed operation"
        )

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
        ("SQL execution surface inventory gate", "SQL execution surface inventory unittest module"),
    )
    for gate_label, test_label in ordered_pairs:
        if gate_label in positions and test_label in positions:
            if positions[gate_label] >= positions[test_label]:
                errors.append(f"{gate_label} must run before {test_label}")

    gate_label = "SQL execution surface inventory gate"
    dirty_label = "SQL execution surface baseline dirty-worktree guard"
    if gate_label in positions and dirty_label in positions:
        if positions[dirty_label] != positions[gate_label] + 1:
            errors.append(f"{dirty_label} must run immediately after {gate_label}")

    return errors


def check_hardening_text(text: str) -> list[str]:
    """Return contract violations for effective-execution assertions."""
    errors: list[str] = []
    for snippet in REQUIRED_HARDENING_SNIPPETS:
        count = text.count(snippet)
        if count != 1:
            errors.append(
                "Script checks effective-execution contract: expected exactly one "
                f"hardening snippet {snippet!r}, found {count}"
            )
    return errors


def check(repo_root: Path) -> list[str]:
    path = repo_root / CI_SCRIPT
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        return [f"cannot read {CI_SCRIPT}: {error}"]
    errors = check_text(text)
    hardening_path = repo_root / Path("scripts/check-ci-runner-hardening.sh")
    try:
        hardening_text = hardening_path.read_text(encoding="utf-8")
    except OSError as error:
        errors.append(f"cannot read scripts/check-ci-runner-hardening.sh: {error}")
    else:
        errors.extend(check_hardening_text(hardening_text))
    return errors


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
        f"{len(REQUIRED_INVOCATIONS)} exact aggregate invocations and "
        f"{len(REQUIRED_HARDENING_SNIPPETS)} effective-execution assertions protected"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
