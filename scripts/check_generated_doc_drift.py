#!/usr/bin/env python3
"""Report reproducible documentation drift without changing the candidate SHA."""
from __future__ import annotations

import argparse
from pathlib import Path
import subprocess
import sys

GENERATED_DOCS = (
    "ARCHITECTURE.md", "docs/generated/route-inventory.md",
    "docs/generated/worker-inventory.md", "docs/generated/env-reference.md",
    "docs/generated/cli-reference.md",
)


def check(root: Path) -> int:
    head = subprocess.run(["git", "-C", str(root), "rev-parse", "HEAD"],
                          check=True, capture_output=True, text=True).stdout.strip()
    result = subprocess.run(["git", "-C", str(root), "diff", "--exit-code", "HEAD", "--",
                             *GENERATED_DOCS], capture_output=True, text=True)
    if result.returncode not in (0, 1):
        raise RuntimeError(result.stderr.strip() or "git diff failed")
    if result.returncode == 1:
        print(f"::warning title=Generated documentation drift::candidate {head}; "
              "run python3 scripts/prepare_gate_review.py and commit corrections before review")
        print(result.stdout, end="")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args(argv)
    try:
        return check(args.repo_root)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"ERROR: generated documentation check: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
