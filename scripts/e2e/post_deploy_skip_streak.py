"""Count consecutive deploys whose smoke turn scenario skipped a cell not proven idle.

Keyed by scenario and cell. A skip adds one; an evaluated run (PASS/FAIL) resets to zero.
Unreadable state counts from zero: this bookkeeping never fails the smoke.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
from pathlib import Path


def record(state: Path, key: str, *, skipped: bool) -> int:
    try:
        counts = json.loads(state.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        counts = {}
    if not isinstance(counts, dict):
        counts = {}
    previous = counts.get(key)
    count = (previous if type(previous) is int and previous > 0 else 0) + 1 if skipped else 0
    if count == 0 and key not in counts:
        return 0
    counts[key] = count
    state.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=state.name + ".", dir=state.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(counts, handle, sort_keys=True)
        os.replace(tmp, state)
    except BaseException:
        Path(tmp).unlink(missing_ok=True)
        raise
    return count


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("outcome", choices=("skipped", "evaluated"))
    parser.add_argument("--state", required=True)
    parser.add_argument("--scenario", required=True)
    parser.add_argument("--cell", required=True)
    args = parser.parse_args(argv)
    try:
        count = record(Path(args.state), f"{args.scenario}:{args.cell}", skipped=args.outcome == "skipped")
    except OSError as error:
        print(f"skip streak state unavailable: {error}", file=sys.stderr)
        return 1
    print(count)
    return 0


if __name__ == "__main__":
    sys.exit(main())
