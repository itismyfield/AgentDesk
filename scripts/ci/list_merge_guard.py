#!/usr/bin/env python3
"""Check two union-merged lists against their three source trees, then check order.

Local API: --base SHA --ours SHA --theirs SHA --merged WORKTREE_OR_TREE [--repo DIR].
Exit codes: 0 PASS, 1 list/three-way violation, 2 invalid or unreadable inputs.
--ci binds PR inputs to the checked-out candidate and its validated parents.
Legacy MERGE_BASE OURS THEIRS [--repo DIR] [--result REF] remains supported.
--normalize sorts a working result only after all semantic checks pass.
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from nextest_ci import PlanError, load_justifications
from statecharts_tlc import CheckError, expected_rows, parse_expected
from merge_candidate import pr_comparison_base


EXPECTED = "docs/statecharts/expected.tsv"
REASONS = "scripts/ci/test-removal-justifications.txt"
LISTS = (EXPECTED, REASONS)


class GuardError(Exception):
    """A list contract or three-way merge violation."""


class InputError(Exception):
    """Arguments, Git provenance or result inputs cannot be trusted."""


def git(repo: Path, *args: str) -> str:
    return subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True, check=True).stdout


def ci_inputs(repo: Path) -> list[str]:
    if (os.environ.get("GFP_EVENT_NAME") or os.environ.get("GITHUB_EVENT_NAME")) != "pull_request":
        return ["HEAD", "HEAD", "HEAD"]
    def event_oid(name: str, label: str) -> str:
        value = os.environ.get(name)
        if not value:
            raise InputError(f"PR list guard needs its {label} SHA")
        return git(repo, "rev-parse", "--verify", f"{value}^{{commit}}").strip()
    candidate = event_oid("GFP_CANDIDATE_SHA", "candidate")
    if candidate != git(repo, "rev-parse", "HEAD").strip():
        raise InputError("candidate SHA is not checked-out HEAD")
    event_base = event_oid("GFP_BASE_SHA", "base")
    head = event_oid("GFP_HEAD_SHA", "head")
    parents = git(repo, "rev-list", "--parents", "-n1", candidate).split()
    try:
        ours = pr_comparison_base(candidate, event_base, head, parents, lambda *args: git(repo, *args))
    except RuntimeError as error:
        raise InputError(str(error)) from error
    return [git(repo, "merge-base", ours, head).strip(), ours, head]


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
    parser.add_argument("legacy", nargs="*", metavar="REF")
    parser.add_argument("--base", help="unique merge-base of --ours and --theirs")
    parser.add_argument("--ours", help="actual first-parent commit of the merge")
    parser.add_argument("--theirs", help="second-parent commit of the merge")
    parser.add_argument("--merged", help="result worktree directory or Git tree-ish in --repo")
    parser.add_argument("--ci", action="store_true", help="resolve fixed GFP PR provenance or check current HEAD")
    parser.add_argument("--repo", type=Path, default=Path("."))
    parser.add_argument("--result", help="read the result from this ref instead of the working tree")
    parser.add_argument("--normalize", action="store_true", help="sort the working result after semantic validation")
    args = parser.parse_args(argv)
    named = [args.base, args.ours, args.theirs, args.merged]
    if args.ci:
        if args.legacy or any(named) or args.result:
            parser.error("--ci cannot be combined with explicit merge inputs")
    elif args.legacy:
        if len(args.legacy) != 3 or any(named):
            parser.error("use three legacy refs or the complete named local API")
    elif not all(named):
        parser.error("local API needs --base, --ours, --theirs and --merged")
    if args.merged and args.result:
        parser.error("--merged and --result are alternatives")
    repo = args.repo.resolve()
    try:
        inputs = ci_inputs(repo) if args.ci else args.legacy or named[:3]
        refs = [git(repo, "rev-parse", "--verify", f"{ref}^{{commit}}").strip()
                for ref in inputs]
        common = git(repo, "merge-base", "--all", refs[1], refs[2]).splitlines()
        if common != [refs[0]]:
            raise InputError(f"merge-base must be the unique common ancestor: got {refs[0]}, want {common}")
        result_root, result_ref = repo, args.result
        if args.merged:
            if Path(args.merged).is_dir():
                result_root = Path(args.merged).resolve()
            else:
                result_ref = args.merged
        if result_ref:
            result_ref = git(repo, "rev-parse", "--verify", f"{result_ref}^{{tree}}").strip()
            if args.normalize:
                raise InputError("--normalize needs a working tree result")
        updates = {}
        for path in LISTS:
            inputs = [parse(path, git(repo, "show", f"{ref}:{path}"), f"{ref}:{path}") for ref in refs]
            result = git(repo, "show", f"{result_ref}:{path}") if result_ref else (result_root / path).read_text(encoding="utf-8")
            rows = parse(path, result, f"result:{path}")
            check_result(merge_rows(*inputs, path), rows, path)
            canonical = normalized(result, rows)
            if result != canonical and not args.normalize:
                raise GuardError(f"{path}: not sorted (comments belong in the header); run with --normalize")
            updates[path] = canonical
        if args.normalize:
            for path, text in updates.items():
                (result_root / path).write_text(text, encoding="utf-8")
    except GuardError as error:
        print(f"FAIL {error}", file=sys.stderr)
        return 1
    except (InputError, OSError, UnicodeError, subprocess.CalledProcessError) as error:
        detail = f"git: {error.stderr.strip()}" if isinstance(error, subprocess.CalledProcessError) else str(error)
        print(f"FAIL input: {detail}", file=sys.stderr)
        return 2
    print(f"LIST_MERGE_GUARD_OK lists={len(LISTS)} merge_base={refs[0]} ours={refs[1]} theirs={refs[2]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
