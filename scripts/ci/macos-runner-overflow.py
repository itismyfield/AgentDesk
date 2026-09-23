#!/usr/bin/env python3
"""Print `hosted` when every self-hosted macOS runner is busy, else `self-hosted`.

A failed or unrecognized runner query prints `self-hosted`: reading an outage
as "all busy" would flood the scarce hosted pool.
"""
from __future__ import annotations

import json
import os
import sys
import urllib.request


def _state(runner: dict) -> tuple[set[str], str, bool]:
    """Labels (casefolded, as GitHub matches them), status and busy; ValueError if malformed."""
    labels, status, busy = runner.get("labels"), runner.get("status"), runner.get("busy")
    if (
        not isinstance(labels, list)
        or not all(isinstance(label, dict) and isinstance(label.get("name"), str) for label in labels)
        or status not in ("online", "offline")
        or not isinstance(busy, bool)
    ):
        raise ValueError(f"unrecognized runner entry {runner.get('name')!r}")
    return {label["name"].casefold() for label in labels}, status, busy


def decide(runners: list[dict], wanted: list[str]) -> tuple[str, str]:
    # Hosted only when every matching runner is confirmed busy or offline.
    wanted_set = {label.casefold() for label in wanted}
    eligible = [state for state in map(_state, runners) if wanted_set <= state[0]]
    idle = [state for state in eligible if state[1] == "online" and not state[2]]
    if eligible and not idle:
        return "hosted", f"all {len(eligible)} matching self-hosted runner(s) busy or offline"
    if not eligible:
        return "self-hosted", "no runner matches the labels; queueing on self-hosted"
    return "self-hosted", f"{len(idle)}/{len(eligible)} matching self-hosted runner(s) idle"


def fetch_runners(repo: str, token: str) -> list[dict]:
    request = urllib.request.Request(
        f"https://api.github.com/repos/{repo}/actions/runners?per_page=100",
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        body = json.load(response)
    # A partial list (more pages, truncation) leaves unlisted runners unknown, not busy.
    runners, total = body.get("runners"), body.get("total_count")
    if not isinstance(runners, list) or type(total) is not int or total != len(runners):
        raise ValueError(f"incomplete runner list (total_count={total!r})")
    return runners


def main() -> int:
    wanted = json.loads(os.environ["MACOS_RUNNER"])
    try:
        runners = fetch_runners(os.environ["GITHUB_REPOSITORY"], os.environ["RUNNER_QUERY_TOKEN"])
        mode, reason = decide(runners, wanted)
    except Exception as exc:  # network, auth, rate limit, malformed or incomplete body, bad runner entry
        mode, reason = "self-hosted", f"runner query failed ({type(exc).__name__}: {exc}); keeping self-hosted"
    print(reason, file=sys.stderr)
    print(mode)
    return 0


if __name__ == "__main__":
    sys.exit(main())
