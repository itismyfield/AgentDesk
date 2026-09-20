#!/usr/bin/env python3
"""Report the tested on-disk sources; require HEAD equality in CI."""
from __future__ import annotations

import hashlib
import os
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCES = (
    "scripts/run_relay_authority_mutations.sh",
    "tests/test_relay_authority_mutations.py",
    "scripts/check_relay_mutation_sources.py",
    "scripts/ci-script-checks.sh",
)


def main() -> int:
    verify = os.environ.get("GITHUB_ACTIONS", "false") == "true"
    mismatches = []
    for relative in SOURCES:
        actual = hashlib.sha256((ROOT / relative).read_bytes()).hexdigest()
        committed = subprocess.run(
            ["git", "show", f"HEAD:{relative}"], cwd=ROOT, capture_output=True,
        )
        expected = hashlib.sha256(committed.stdout).hexdigest() if committed.returncode == 0 else "unavailable"
        print(f"MUTATION_SOURCE path={relative} sha256={actual} head_sha256={expected}", flush=True)
        if actual != expected:
            mismatches.append(relative)
    if verify and mismatches:
        print(f"ERROR mutation sources differ from HEAD: {', '.join(mismatches)}", flush=True)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
