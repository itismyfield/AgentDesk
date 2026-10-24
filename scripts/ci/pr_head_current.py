"""Require the failed run to belong to the current head of an open PR."""

import json
from pathlib import Path
import re
import subprocess
import sys


def pr_head_is_current(repo: str, run: dict) -> bool:
    sha = run.get("head_sha")
    pulls = run.get("pull_requests")
    if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-f]{40}", sha):
        return False
    if not isinstance(pulls, list) or not pulls:
        return False
    for pull in pulls:
        number = pull.get("number") if isinstance(pull, dict) else None
        if type(number) is not int or number <= 0:
            return False
        result = subprocess.run(
            ["gh", "api", f"repos/{repo}/pulls/{number}"],
            check=True, text=True, capture_output=True,
        )
        current = json.loads(result.stdout)
        head = current.get("head") if isinstance(current, dict) else None
        if not isinstance(head, dict) or current.get("state") != "open" or head.get("sha") != sha:
            return False
    return True


if __name__ == "__main__":
    try:
        current = pr_head_is_current(sys.argv[1], json.loads(Path(sys.argv[2]).read_text()))
    except (OSError, ValueError, TypeError, AttributeError, subprocess.CalledProcessError):
        current = False
    sys.exit(0 if current else 1)
