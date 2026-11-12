#!/usr/bin/env python3
"""Check two union-merged lists against their three source trees, then check order.

Usage: list_merge_guard.py MERGE_BASE OURS THEIRS [--repo DIR] [--normalize]
The result defaults to the working tree; --result REF checks a committed tree.
Historical inputs may be unsorted. --normalize writes only after all semantic
checks pass, moving comments to the header and preserving every data row.
"""

from __future__ import annotations

import argparse
from pathlib import Path
import subprocess
import sys
import tempfile

from nextest_ci import PlanError, load_justifications
from statecharts_tlc import CheckError, expected_rows, parse_expected


EXPECTED = "docs/statecharts/expected.tsv"
REASONS = "scripts/ci/test-removal-justifications.txt"
LISTS = (EXPECTED, REASONS)


class GuardError(Exception):
    """An invalid input or a merge that cannot be accepted automatically."""


def git(repo: Path, *args: str) -> str:
    result = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True)
    if result.returncode:
        raise GuardError(f"git {' '.join(args)}: {result.stderr.strip()}")
    return result.stdout


def data_lines(text: str) -> list[str]:
    return [line for line in text.splitlines() if line.strip() and not line.lstrip().startswith("#")]


def parse(path: str, text: str, source: str) -> dict:
    try:
        if path == EXPECTED:
            parsed = parse_expected(text, source)
            rows = expected_rows(text)
        else:
            # Reuse the scope/overlap policy without changing its file-based API.
            with tempfile.TemporaryDirectory(prefix="list-parser-") as directory:
                file = Path(directory) / "justifications.txt"
                file.write_text(text, encoding="utf-8")
                parsed = load_justifications(file)
            rows = dict(zip(parsed, data_lines(text), strict=True))
    except (CheckError, PlanError, ValueError) as error:
        raise GuardError(f"{source}: {error}") from error
    if not parsed:
        raise GuardError(f"{source}: empty list is not a valid guard input")
    return rows


def key_order(key) -> tuple:
    return (key,) if isinstance(key, str) else tuple(part or "" for part in key)


def merge_rows(base: dict, ours: dict, theirs: dict, source: str) -> dict:
    deleted = (base.keys() - ours.keys()) | (base.keys() - theirs.keys())
    for key in base.keys() | ours.keys() | theirs.keys():
        before, left, right = base.get(key), ours.get(key), theirs.get(key)
        if left != before and right != before and left != right:
            kind = "delete/modify" if left is None or right is None else "different edits"
            raise GuardError(f"{source}: {kind} for {key!r}; resolve the source branches first")
    keys = (ours.keys() | theirs.keys()) - deleted
    return {key: (theirs[key] if key not in ours or ours.get(key) == base.get(key) else ours[key]) for key in keys}


def check_result(wanted: dict, actual: dict, source: str) -> None:
    missing, extra = wanted.keys() - actual.keys(), actual.keys() - wanted.keys()
    if missing or extra:
        raise GuardError(f"{source}: missing keys={sorted(missing, key=key_order)!r}; "
                         f"unexpected keys={sorted(extra, key=key_order)!r}")
    changed = [key for key in wanted if wanted[key] != actual[key]]
    if changed:
        raise GuardError(f"{source}: merged rows differ for {sorted(changed, key=key_order)!r}")


def normalized(text: str, rows: dict) -> str:
    comments = [line for line in text.splitlines() if line.lstrip().startswith("#")]
    return "\n".join(comments + [rows[key] for key in sorted(rows, key=key_order)]) + "\n"


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("merge_base")
    parser.add_argument("ours")
    parser.add_argument("theirs")
    parser.add_argument("--repo", type=Path, default=Path("."))
    parser.add_argument("--result", help="read the result from this ref instead of the working tree")
    parser.add_argument("--normalize", action="store_true", help="sort the working result after semantic validation")
    args = parser.parse_args(argv)
    if args.normalize and args.result:
        parser.error("--normalize needs a working tree result")
    repo = args.repo.resolve()
    try:
        refs = [git(repo, "rev-parse", "--verify", f"{ref}^{{commit}}").strip()
                for ref in (args.merge_base, args.ours, args.theirs)]
        common = git(repo, "merge-base", "--all", refs[1], refs[2]).splitlines()
        if common != [refs[0]]:
            raise GuardError(f"merge-base must be the unique common ancestor: got {refs[0]}, want {common}")
        updates = {}
        for path in LISTS:
            inputs = [parse(path, git(repo, "show", f"{ref}:{path}"), f"{ref}:{path}") for ref in refs]
            result = git(repo, "show", f"{args.result}:{path}") if args.result else (repo / path).read_text(encoding="utf-8")
            rows = parse(path, result, f"result:{path}")
            check_result(merge_rows(*inputs, path), rows, path)
            canonical = normalized(result, rows)
            if result != canonical and not args.normalize:
                raise GuardError(f"{path}: not sorted (comments belong in the header); run with --normalize")
            updates[path] = canonical
        if args.normalize:
            for path, text in updates.items():
                (repo / path).write_text(text, encoding="utf-8")
    except (GuardError, OSError, UnicodeError) as error:
        print(f"FAIL {error}", file=sys.stderr)
        return 1
    print(f"LIST_MERGE_GUARD_OK lists={len(LISTS)} merge_base={refs[0]} ours={refs[1]} theirs={refs[2]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
