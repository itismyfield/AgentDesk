#!/usr/bin/env python3
"""Refresh the giant-file issue snapshot through the operator's authenticated gh.

CI never invokes this command. ``generate_inventory_docs.py`` consumes only the
checked-in JSON snapshot, so its result is deterministic and network-free.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
SNAPSHOT_PATH = REPO_ROOT / "scripts" / "giant_file_issue_metadata.json"
DEFAULT_GITHUB_REPOSITORY = "itismyfield/AgentDesk"


class RefreshError(RuntimeError):
    pass


def github_issue(number: int, repository: str) -> dict[str, object]:
    result = subprocess.run(
        [
            "gh",
            "issue",
            "view",
            str(number),
            "--repo",
            repository,
            "--json",
            "number,state,title",
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "unknown gh failure"
        raise RefreshError(f"could not read GitHub issue #{number}: {detail}")
    try:
        issue = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RefreshError(f"GitHub issue #{number} returned invalid JSON") from error
    state = issue.get("state") if isinstance(issue, dict) else None
    title = issue.get("title") if isinstance(issue, dict) else None
    if (
        not isinstance(issue, dict)
        or issue.get("number") != number
        or not isinstance(state, str)
        or state not in {"OPEN", "CLOSED"}
        or not isinstance(title, str)
        or not title.strip()
    ):
        raise RefreshError(f"GitHub issue #{number} returned an invalid record: {issue!r}")
    return {"number": number, "state": state.lower(), "title": title}


def refreshed_snapshot(
    payload: object,
    repository: str,
    refreshed_at: datetime,
) -> dict[str, object]:
    if not isinstance(payload, dict) or payload.get("schema_version") != 2:
        raise RefreshError("snapshot must be a schema_version 2 JSON object")
    records = payload.get("issues")
    if not isinstance(records, list):
        raise RefreshError("snapshot issues must be a list")
    ratchets = payload.get("ratchets")
    if not isinstance(ratchets, dict):
        raise RefreshError("snapshot ratchets must be an object")
    for key in ("closed_deadline_entries", "transition_list_entries"):
        value = ratchets.get(key)
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            raise RefreshError(f"snapshot ratchets.{key} must be a non-negative integer")

    refreshed_records: list[dict[str, object]] = []
    seen: set[int] = set()
    for record in records:
        if not isinstance(record, dict):
            raise RefreshError(f"snapshot issue record must be an object: {record!r}")
        number = record.get("number")
        if (
            not isinstance(number, int)
            or isinstance(number, bool)
            or number <= 0
            or number in seen
        ):
            raise RefreshError(f"snapshot issue number is invalid or duplicate: {record!r}")
        seen.add(number)
        live = github_issue(number, repository)
        refreshed_records.append({**record, "state": live["state"], "title": live["title"]})

    if refreshed_at.tzinfo is None or refreshed_at.utcoffset() is None:
        raise RefreshError("refreshed_at must be timezone-aware")
    return {
        "schema_version": 2,
        "refreshed_at": refreshed_at.astimezone(timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
        "ratchets": dict(ratchets),
        "issues": refreshed_records,
    }


def lower_ratchet_baselines(
    payload: dict[str, object],
    registry_entries: list[dict[str, str]],
    transition_paths: set[str],
) -> dict[str, object]:
    """Lower checked-in baselines to current counts, never raise them."""
    records = payload.get("issues")
    ratchets = payload.get("ratchets")
    if not isinstance(records, list) or not isinstance(ratchets, dict):
        raise RefreshError("refreshed snapshot is missing issues or ratchets")

    states: dict[int, str] = {}
    for record in records:
        if not isinstance(record, dict):
            raise RefreshError(f"snapshot issue record must be an object: {record!r}")
        number = record.get("number")
        state = record.get("state")
        if not isinstance(number, int) or isinstance(number, bool) or state not in {
            "open",
            "closed",
        }:
            raise RefreshError(f"snapshot issue record is invalid: {record!r}")
        states[number] = state

    closed_deadline_entries = 0
    for entry in registry_entries:
        if entry.get("decision", "shrink") != "shrink":
            continue
        issue_ref = entry.get("decompose_issue", "")
        if not issue_ref.startswith("#") or not issue_ref[1:].isdigit():
            raise RefreshError(
                f"registry entry has invalid decompose_issue while refreshing ratchets: {entry!r}"
            )
        issue_number = int(issue_ref[1:])
        if issue_number not in states:
            raise RefreshError(
                f"registry entry references issue #{issue_number}, absent from snapshot"
            )
        if states[issue_number] == "closed":
            closed_deadline_entries += 1

    measured = {
        "closed_deadline_entries": closed_deadline_entries,
        "transition_list_entries": len(transition_paths),
    }
    lowered: dict[str, int] = {}
    for key, current in measured.items():
        baseline = ratchets.get(key)
        if not isinstance(baseline, int) or isinstance(baseline, bool) or baseline < 0:
            raise RefreshError(f"snapshot ratchets.{key} must be a non-negative integer")
        lowered[key] = min(baseline, current)

    return {**payload, "ratchets": lowered}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Refresh checked-in giant-file issue states via read-only gh calls."
    )
    parser.add_argument(
        "--repo",
        default=DEFAULT_GITHUB_REPOSITORY,
        help=f"GitHub owner/name to query (default: {DEFAULT_GITHUB_REPOSITORY})",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        import generate_inventory_docs as inventory

        payload = json.loads(SNAPSHOT_PATH.read_text(encoding="utf-8"))
        refreshed = refreshed_snapshot(payload, args.repo, datetime.now(timezone.utc))
        try:
            _grandfathered, registry_entries, _baseline_paths = (
                inventory.load_giant_file_registry()
            )
            transition_paths = inventory.load_giant_file_closed_issue_transition_list()
        except inventory.ParseError as error:
            raise RefreshError(str(error)) from error
        refreshed = lower_ratchet_baselines(
            refreshed, registry_entries, transition_paths
        )
        SNAPSHOT_PATH.write_text(
            json.dumps(refreshed, indent=2, ensure_ascii=False) + "\n",
            encoding="utf-8",
        )
    except (OSError, json.JSONDecodeError, RefreshError) as error:
        print(f"giant-file issue metadata refresh failed: {error}", file=sys.stderr)
        return 1
    print(f"refreshed {len(refreshed['issues'])} issues in {SNAPSHOT_PATH.relative_to(REPO_ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
