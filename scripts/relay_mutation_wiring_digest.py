#!/usr/bin/env python3
"""Report whether HEAD changed the resolved relay mutation wiring in ci-pr.yml vs HEAD^1.

Only subtrees that change what the gate runs are compared; any doubt reports true."""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

WORKFLOW = ".github/workflows/ci-pr.yml"
MUTATION_JOB = "relay_authority_mutations"
MIRROR_JOB = "relay-authority-contract"
# Workflow-level keys every job inherits; absent on both sides compares equal.
TOP_LEVEL_KEYS = ("env", "defaults", "permissions", "concurrency")
_ABSENT = object()


class WiringUnknown(Exception):
    """The comparison cannot be made, so the gate must run."""


def _canonical(value: object) -> object:
    # Typed keys keep `on:` (loaded as True) apart from "True" and let mixed keys sort;
    # every real value canonicalizes to a list, so the bare string marks an absent key.
    if value is _ABSENT:
        return "absent"
    if isinstance(value, dict):
        return [[f"{type(key).__name__}:{key}", _canonical(item)] for key, item in
                sorted(value.items(), key=lambda pair: f"{type(pair[0]).__name__}:{pair[0]}")]
    if isinstance(value, list):
        return [_canonical(item) for item in value]
    return [type(value).__name__, value if isinstance(value, (bool, int, float, str, type(None))) else str(value)]


def _load(repo: Path, rev: str) -> dict:
    shown = subprocess.run(["git", "show", f"{rev}:{WORKFLOW}"], cwd=repo,
                           capture_output=True, text=True)
    if shown.returncode != 0:
        raise WiringUnknown(f"git show {rev}:{WORKFLOW} failed: {shown.stderr.strip()}")
    import yaml  # Imported here so a missing PyYAML also fails closed.

    document = yaml.safe_load(shown.stdout)
    if not isinstance(document, dict):
        raise WiringUnknown(f"{rev}:{WORKFLOW} is not a mapping")
    return document


def wiring_subtrees(document: dict) -> dict[str, object]:
    """Every subtree whose resolved value the mutation gate depends on."""
    # PyYAML reads the bare `on:` key as boolean True.
    trigger = document.get(True, document.get("on", _ABSENT))
    jobs = document.get("jobs")
    if trigger is _ABSENT or not isinstance(jobs, dict):
        raise WiringUnknown("workflow lacks `on` or `jobs`")
    subtrees: dict[str, object] = {"on": trigger}
    for key in TOP_LEVEL_KEYS:
        subtrees[key] = document.get(key, _ABSENT)
    for job in (MUTATION_JOB, MIRROR_JOB):
        if not isinstance(jobs.get(job), dict):
            raise WiringUnknown(f"jobs.{job} is missing")
        subtrees[f"jobs.{job}"] = jobs[job]
    return subtrees


def digest(subtree: object) -> str:
    payload = json.dumps(_canonical(subtree), separators=(",", ":"))
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()


def wiring_changed(repo: Path, base: str = "HEAD^1", head: str = "HEAD") -> bool:
    try:
        before = {key: digest(value) for key, value in wiring_subtrees(_load(repo, base)).items()}
        after = {key: digest(value) for key, value in wiring_subtrees(_load(repo, head)).items()}
    except Exception as error:  # noqa: BLE001 - every failure must run the gate.
        print(f"WIRING_UNKNOWN reason={type(error).__name__}: {error}", flush=True)
        return True
    for key in after:
        print(f"WIRING_SUBTREE key={key} base={before[key][:12]} head={after[key][:12]}"
              f" changed={str(before[key] != after[key]).lower()}", flush=True)
    return before != after


def main() -> int:
    line = f"wiring_changed={str(wiring_changed(Path.cwd())).lower()}"
    print(line, flush=True)
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            handle.write(line + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
