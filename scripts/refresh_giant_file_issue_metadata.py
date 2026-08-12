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
    if not isinstance(payload, dict) or payload.get("schema_version") != 1:
        raise RefreshError("snapshot must be a schema_version 1 JSON object")
    records = payload.get("issues")
    if not isinstance(records, list):
        raise RefreshError("snapshot issues must be a list")

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
        "schema_version": 1,
        "refreshed_at": refreshed_at.astimezone(timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
        "issues": refreshed_records,
    }


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
        payload = json.loads(SNAPSHOT_PATH.read_text(encoding="utf-8"))
        refreshed = refreshed_snapshot(payload, args.repo, datetime.now(timezone.utc))
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
