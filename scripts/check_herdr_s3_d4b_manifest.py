#!/usr/bin/env python3
"""D4-B acceptance manifest: `--structure-only`, or a `--require now|full` verdict on `--results`.

`implemented` only means a row's lib tests are registered; a verdict needs a results file
(`{"cases": {id: "PASS"|"FAIL"}, "mutants": {name: "KILLED"|"SURVIVED"|"TIMEOUT"}}`).
`now` needs every required now row implemented and PASS, and every required mutant naming one of
them KILLED. `full` needs that for every required row and mutant, so any pending or blocked row,
any missing result and a TIMEOUT in place of a kill all fail it. The committed required rosters
are pinned in tests/test_herdr_s3_d4b_manifest.py.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "scripts/herdr_s3_d4b_manifest.json"
PROFILES = ("now", "slow", "after")
CASE_FIELDS = ("id", "profile", "piece", "status", "tests")
LEVELS = ("now", "full")


def status_ok(status: object) -> bool:
    if status in ("implemented", "pending"):
        return True
    return isinstance(status, str) and status.startswith("blocked:") and len(status) > 8


def linux_lib_tests(root: Path) -> set[str]:
    """Lib tests a linux `cargo test --lib` compiles, read from the sources src/lib.rs reaches."""
    sys.path.insert(0, str(root / "scripts/ci"))
    import rust_test_sources as sources
    return sources.compiled_lib_tests(root)


def structure_errors(manifest: dict, inventory: set[str]) -> list[str]:
    errors: list[str] = []
    for key in ("required_cases", "required_mutants"):
        roster = manifest.get(key) or []
        if not roster:
            errors.append(f"{key} is empty")
        for name in sorted({name for name in roster if roster.count(name) > 1}):
            errors.append(f"{key} lists {name} twice")
    rows: dict[str, dict] = {}
    for case in manifest.get("cases", []):
        missing = [field for field in CASE_FIELDS if field not in case]
        if missing:
            errors.append(f"case {case.get('id')}: missing {missing}")
            continue
        cid = case["id"]
        if cid in rows:
            errors.append(f"duplicate case {cid}")
        rows[cid] = case
        if case["profile"] not in PROFILES:
            errors.append(f"{cid}: profile {case['profile']!r}")
        if case["piece"] not in range(1, 7):
            errors.append(f"{cid}: piece {case['piece']!r}")
        if not status_ok(case["status"]):
            errors.append(f"{cid}: status {case['status']!r}")
        if case["status"] == "implemented" and not case["tests"]:
            errors.append(f"{cid}: implemented without a lib test")
        for test in case["tests"]:
            if test not in inventory:
                errors.append(f"{cid}: {test} is not in the lib test inventory")
    for cid in manifest.get("required_cases", []):
        if cid not in rows:
            errors.append(f"required case {cid} has no row")
    names: set[str] = set()
    for mutant in manifest.get("mutants", []):
        name = mutant.get("name")
        if name in names:
            errors.append(f"duplicate mutant {name}")
        names.add(name)
        for cid in mutant.get("cases") or [None]:
            if cid not in rows:
                errors.append(f"mutant {name}: unknown case {cid}")
    for name in manifest.get("required_mutants", []):
        if name not in names:
            errors.append(f"required mutant {name} has no row")
    return errors


def verdict(manifest: dict, results: dict, level: str) -> list[str]:
    """Why `level` is not met; empty means PASS."""
    rows = {case["id"]: case for case in manifest["cases"]}
    mutants = {mutant["name"]: mutant for mutant in manifest["mutants"]}

    required_now = {cid for cid in manifest["required_cases"] if rows[cid]["profile"] == "now"}

    def counted(cases: list[str]) -> bool:
        return level == "full" or bool(required_now.intersection(cases))

    reasons = []
    for cid in manifest["required_cases"]:
        if not counted([cid]):
            continue
        if rows[cid]["status"] != "implemented":
            reasons.append(f"{cid}: {rows[cid]['status']}")
        got = results.get("cases", {}).get(cid)
        if got != "PASS":
            reasons.append(f"{cid}: result {got or 'missing'}")
    for name in manifest["required_mutants"]:
        if not counted(mutants[name]["cases"]):
            continue
        got = results.get("mutants", {}).get(name)
        if got != "KILLED":
            reasons.append(f"mutant {name}: {got or 'missing'}")
    return reasons


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--manifest", type=Path, default=MANIFEST)
    parser.add_argument("--inventory", type=Path, help="test IDs, one per line, in place of the linux lib sources")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--structure-only", action="store_true", help="check the manifest alone")
    mode.add_argument("--require", choices=LEVELS, help="level whose FAIL sets rc 1; needs --results")
    parser.add_argument("--results", type=Path, help="results JSON for --require")
    args = parser.parse_args(argv)
    if args.require and args.results is None:
        parser.error("--require needs --results; without results nothing is accepted")
    if args.structure_only and args.results is not None:
        parser.error("--structure-only takes no --results")
    manifest = json.loads(args.manifest.read_text())
    if args.inventory is None:
        inventory = linux_lib_tests(ROOT)
    else:
        lines = args.inventory.read_text().splitlines()
        inventory = {line.strip() for line in lines if line.strip() and not line.startswith("#")}
    errors = structure_errors(manifest, inventory)
    for error in errors:
        print(f"manifest: {error}")
    if errors:
        return 1
    print("manifest: ok")
    if args.structure_only:
        return 0
    results = json.loads(args.results.read_text())
    failed = {}
    for level in LEVELS:
        reasons = verdict(manifest, results, level)
        failed[level] = bool(reasons)
        print(f"{level}: {'FAIL' if reasons else 'PASS'}")
        for reason in reasons:
            print(f"  {reason}")
    return int(failed[args.require])


if __name__ == "__main__":
    sys.exit(main())
