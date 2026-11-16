#!/usr/bin/env python3
"""Sync the required-check dependency table from its canonical PR workflow."""

import argparse
from pathlib import Path
import re

import yaml

ROOT = Path(__file__).resolve().parents[2]
BLOCK = re.compile(r"(?P<start>^(?P<indent>[ \t]*)<!-- required-gate-wiring -->\n)"
                   r".*?(?P<end>^[ \t]*<!-- /required-gate-wiring -->)", re.M | re.S)


def synchronize(workflow, document, check=False):
    jobs = yaml.safe_load(workflow.read_text())["jobs"]
    rows, pending = {}, ["required_contexts"]
    while pending:
        name = pending.pop()
        if name in rows:
            continue
        needs = jobs[name].get("needs", [])
        rows[name] = [needs] if isinstance(needs, str) else needs
        pending.extend(rows[name])

    def cell(values):
        return ", ".join(f"`{value}`" for value in values) or "none"

    lines = ["| job | needs | job-level `if` |", "|---|---|---|"] + [
        f"| `{name}` | {cell(needs)} | {cell([jobs[name]['if']] if 'if' in jobs[name] else [])} |"
        for name, needs in sorted(rows.items())]
    original = document.read_text()
    matches = list(BLOCK.finditer(original))
    if len(matches) != 1:
        raise ValueError("expected exactly one required-gate-wiring block")
    match = matches[0]
    body = "".join(match["indent"] + line + "\n" for line in lines)
    updated = original[:match.start()] + match["start"] + body + match["end"] + original[match.end():]
    if check:
        return 0 if updated == original else 1
    document.write_text(updated)
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    return synchronize(ROOT / ".github/workflows/ci-pr.yml",
                       ROOT / "docs/agent-maintenance/change-surfaces.md", args.check)


if __name__ == "__main__":
    raise SystemExit(main())
