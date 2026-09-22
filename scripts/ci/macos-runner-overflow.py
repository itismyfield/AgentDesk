#!/usr/bin/env python3
"""Print `hosted` when every self-hosted macOS runner is busy, else `self-hosted`.

A failed runner query prints `self-hosted`: reading an outage as "all busy" would
flood the scarce hosted pool.
"""
from __future__ import annotations

import json
import os
import sys
import urllib.request


def decide(runners: list[dict], wanted: list[str]) -> tuple[str, str]:
    eligible = [r for r in runners if set(wanted) <= {label["name"] for label in r.get("labels", [])}]
    idle = [r for r in eligible if r.get("status") == "online" and not r.get("busy")]
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
        return json.load(response)["runners"]


def main() -> int:
    wanted = json.loads(os.environ["MACOS_RUNNER"])
    try:
        runners = fetch_runners(os.environ["GITHUB_REPOSITORY"], os.environ["RUNNER_QUERY_TOKEN"])
    except Exception as exc:  # network, auth, rate limit, malformed body
        mode, reason = "self-hosted", f"runner query failed ({type(exc).__name__}: {exc}); keeping self-hosted"
    else:
        mode, reason = decide(runners, wanted)
    print(reason, file=sys.stderr)
    print(mode)
    return 0


if __name__ == "__main__":
    sys.exit(main())
